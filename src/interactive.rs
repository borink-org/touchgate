//! `touchgate release`, an experimental interactive release. It shows what
//! `touchgate approve` would sign and the commits since the last release.
//! On a key press, it signs the approval, starts `release.yml` with it and
//! follows the run's jobs until the run finishes.
//!
//! ssh-keygen asks for the key's PIN on the terminal itself, so the screen
//! steps aside while it signs. The PIN never passes through touchgate.

use std::path::Path;
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::release_run::{self, Progress, RunUpdate, RunView};
use crate::terminal::{Key, Line, RawTerminal, Span, Style, TabProgress};
use crate::{Changes, ReleasePlan};

/// The frames of the spinner of something under way.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The rows of the review screen that are not the list of commits.
const REVIEW_CHROME: usize = 10;

/// Runs `touchgate release [<commit>] [--key <private key>]`, whose arguments
/// mean what they mean for `touchgate approve`.
pub fn release(args: &[&str]) -> Result<(), Vec<String>> {
    let (mut key, mut revision) = (crate::DEFAULT_KEY, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match *arg {
            "--key" => key = *args.next().ok_or_else(|| vec![crate::USAGE.to_owned()])?,
            flag if flag.starts_with('-') => return Err(vec![crate::USAGE.to_owned()]),
            commit if revision.is_none() => revision = Some(commit),
            _ => return Err(vec![crate::USAGE.to_owned()]),
        }
    }
    let key = crate::signing_key(key)?;
    let plan = ReleasePlan::of(revision)?;
    let changes = Changes::since_release(&plan.commit)?;
    let mut screen = Screen {
        plan,
        changes,
        stage: Stage::Review {
            scroll: 0,
            notice: None,
        },
        tick: 0,
    };
    let mut terminal = RawTerminal::enter().map_err(|error| vec![error])?;
    let result = screen.run(&mut terminal, &key);
    drop(terminal);
    result?;
    if let Some(summary) = screen.summary() {
        println!("{summary}");
    }
    Ok(())
}

/// Where the release is.
enum Stage {
    /// The release is shown, not yet approved.
    Review {
        /// The first commit shown of the list.
        scroll: usize,
        /// Why the last try to approve failed.
        notice: Option<String>,
    },
    /// The release is approved and started, and its run followed.
    Following {
        /// The updates on the run, until following stops.
        updates: Option<Receiver<RunUpdate>>,
        /// How the run went when last asked, once it has appeared.
        run: Option<RunView>,
        /// What went wrong when last asked, if anything did.
        problem: Option<String>,
    },
}

/// What a key asks for.
enum Response {
    Nothing,
    Approve,
    Quit,
}

struct Screen {
    plan: ReleasePlan,
    changes: Changes,
    stage: Stage,
    /// The frames drawn so far, for the spinner.
    tick: usize,
}

impl Screen {
    /// Draws the screen and answers keys until the person quits.
    fn run(&mut self, terminal: &mut RawTerminal, key: &Path) -> Result<(), Vec<String>> {
        loop {
            self.receive_updates();
            let (_, rows) = terminal.size();
            terminal
                .draw(&self.lines(rows))
                .and_then(|()| terminal.show_progress(self.tab_progress()))
                .map_err(|error| vec![error])?;
            for pressed in terminal.read_keys().map_err(|error| vec![error])? {
                match self.respond(pressed, rows) {
                    Response::Nothing => {}
                    Response::Approve => self.approve(terminal, key),
                    Response::Quit => return Ok(()),
                }
            }
            self.tick += 1;
        }
    }

    /// Changes the screen for the key `pressed` on a screen of `rows`, and
    /// returns what else the key asks for.
    fn respond(&mut self, pressed: Key, rows: usize) -> Response {
        if matches!(pressed, Key::Char('q') | Key::Escape | Key::Ctrl('c')) {
            return Response::Quit;
        }
        let Stage::Review { scroll, .. } = &mut self.stage else {
            return Response::Nothing;
        };
        let page = rows.saturating_sub(REVIEW_CHROME).max(1);
        let last = self.changes.log.lines().count().saturating_sub(page);
        *scroll = match pressed {
            Key::Char('a') | Key::Enter => return Response::Approve,
            Key::Up | Key::Char('k') => scroll.saturating_sub(1),
            Key::Down | Key::Char('j') => *scroll + 1,
            Key::PageUp => scroll.saturating_sub(page),
            Key::PageDown | Key::Char(' ') => *scroll + page,
            Key::Home => 0,
            Key::End => last,
            _ => *scroll,
        }
        .min(last);
        Response::Nothing
    }

    /// Signs the approval and starts the release. If either fails, stays on
    /// the review and says why.
    fn approve(&mut self, terminal: &mut RawTerminal, key: &Path) {
        match self.sign_and_start(terminal, key) {
            Ok(updates) => {
                self.stage = Stage::Following {
                    updates: Some(updates),
                    run: None,
                    problem: None,
                }
            }
            Err(error) => {
                if let Stage::Review { notice, .. } = &mut self.stage {
                    *notice = Some(error);
                }
            }
        }
    }

    /// Signs the approval of the release as of now, with the screen stepped
    /// aside for the PIN, starts `release.yml` with it, and starts following
    /// the run.
    fn sign_and_start(
        &self,
        terminal: &mut RawTerminal,
        key: &Path,
    ) -> Result<Receiver<RunUpdate>, String> {
        let earlier = release_run::started_run_ids()?;
        let approval = crate::unix_time()
            .and_then(|time| self.plan.approval_at(time))
            .map_err(|errors| errors.join("\n"))?;
        let heading = format!(
            "Approving release {} of {} at {}, to run from publish at {}.",
            approval.version, approval.repository, approval.commit, approval.workflow_commit
        );
        let signed = terminal.suspended(|| {
            println!("{heading}");
            approval.sign(key)
        })??;
        crate::github::start_release(&signed)?;
        let (sender, receiver) = mpsc::channel();
        std::thread::spawn(move || release_run::follow(earlier, sender));
        Ok(receiver)
    }

    /// Takes in the updates on the run that have arrived.
    fn receive_updates(&mut self) {
        let Stage::Following {
            updates,
            run,
            problem,
        } = &mut self.stage
        else {
            return;
        };
        let Some(receiver) = updates else {
            return;
        };
        loop {
            match receiver.try_recv() {
                Ok(RunUpdate::View(view)) => {
                    *run = Some(view);
                    *problem = None;
                }
                Ok(RunUpdate::Problem(error)) => *problem = Some(error),
                Ok(RunUpdate::GaveUp(error)) => {
                    *problem = Some(error);
                    *updates = None;
                    return;
                }
                Err(TryRecvError::Empty) => return,
                Err(TryRecvError::Disconnected) => {
                    *updates = None;
                    return;
                }
            }
        }
    }

    /// Returns the lines of the screen, for a screen of `rows`.
    fn lines(&self, rows: usize) -> Vec<Line> {
        let plan = &self.plan;
        let mut lines = vec![
            vec![
                Span::styled("touchgate release", Style::Bold),
                Span::styled(format!("  {}", plan.repository), Style::Dim),
            ],
            vec![],
            vec![
                Span::plain("Release   "),
                Span::styled(plan.version.to_string(), Style::Accent),
                Span::styled(format!("  dated {} in CHANGELOG.md", plan.date), Style::Dim),
            ],
        ];
        match &self.stage {
            Stage::Review { scroll, notice } => {
                self.review_lines(&mut lines, *scroll, notice.as_deref(), rows)
            }
            Stage::Following {
                updates,
                run,
                problem,
            } => self.following_lines(&mut lines, updates.is_some(), run.as_ref(), problem),
        }
        lines
    }

    fn review_lines(
        &self,
        lines: &mut Vec<Line>,
        scroll: usize,
        notice: Option<&str>,
        rows: usize,
    ) {
        let plan = &self.plan;
        let changes = &self.changes;
        lines.push(vec![
            Span::plain("Commit    "),
            Span::styled(plan.commit.clone(), Style::Accent),
        ]);
        lines.push(vec![
            Span::plain("Runs from "),
            Span::plain("publish at "),
            Span::styled(plan.workflow_commit.clone(), Style::Accent),
        ]);
        lines.push(vec![]);
        let count = changes.log.lines().count();
        lines.push(match &changes.previous {
            Some(tag) => vec![
                Span::styled(
                    format!(
                        "Since {tag}: {count} commit{}",
                        if count == 1 { "" } else { "s" }
                    ),
                    Style::Bold,
                ),
                Span::styled(format!("  {}", changes.shortstat.trim()), Style::Dim),
            ],
            None => vec![Span::styled(
                "No release is tagged before this commit. Its last commits:",
                Style::Bold,
            )],
        });
        let page = rows.saturating_sub(REVIEW_CHROME).max(1);
        for line in changes.log.lines().skip(scroll).take(page) {
            let (hash, subject) = line.split_once(' ').unwrap_or((line, ""));
            lines.push(vec![
                Span::plain("  "),
                Span::styled(hash.to_owned(), Style::Accent),
                Span::plain(format!(" {subject}")),
            ]);
        }
        lines.push(vec![]);
        if let Some(notice) = notice {
            lines.push(vec![Span::styled(notice.replace('\n', " "), Style::Failed)]);
        }
        let mut footer = vec![
            Span::styled("a", Style::Bold),
            Span::styled(" approve with the security key  ", Style::Dim),
        ];
        if count > page {
            footer.push(Span::styled("↑↓", Style::Bold));
            footer.push(Span::styled(
                format!(
                    " scroll ({}–{} of {count})  ",
                    scroll + 1,
                    (scroll + page).min(count)
                ),
                Style::Dim,
            ));
        }
        footer.push(Span::styled("q", Style::Bold));
        footer.push(Span::styled(" quit", Style::Dim));
        lines.push(footer);
    }

    fn following_lines(
        &self,
        lines: &mut Vec<Line>,
        following: bool,
        run: Option<&RunView>,
        problem: &Option<String>,
    ) {
        let spinner = SPINNER[self.tick % SPINNER.len()];
        lines.push(match run {
            Some(run) => vec![
                Span::plain("Run       "),
                Span::styled("release.yml on publish", Style::Accent).linked(run.url.clone()),
            ],
            None => vec![
                Span::plain("Run       "),
                Span::styled("release.yml on publish, started", Style::Dim),
            ],
        });
        lines.push(vec![]);
        for job in run.iter().flat_map(|run| &run.jobs) {
            let mut line = vec![
                Span::plain("  "),
                progress_mark(&job.progress, spinner),
                Span::plain(" "),
                Span::plain(job.name.clone()).linked(job.url.clone()),
            ];
            if let Some(step) = &job.step {
                line.push(Span::styled(format!("  {step}"), Style::Dim));
            }
            lines.push(line);
        }
        if run.is_some_and(|run| !run.jobs.is_empty()) {
            lines.push(vec![]);
        }
        let version = &self.plan.version;
        lines.push(match run.map(|run| &run.progress) {
            Some(Progress::Passed) => vec![Span::styled(
                format!("✓ Released {version}."),
                Style::Passed,
            )],
            Some(Progress::Failed(conclusion)) => vec![Span::styled(
                format!("✗ The release run ended: {conclusion}."),
                Style::Failed,
            )],
            Some(Progress::Skipped) => {
                vec![Span::styled("The release run was skipped.", Style::Failed)]
            }
            _ if !following => vec![Span::styled("Following the run stopped.", Style::Failed)],
            Some(Progress::Running) => vec![
                Span::styled(spinner, Style::Running),
                Span::plain(" Running"),
            ],
            Some(Progress::Queued) => vec![
                Span::styled(spinner, Style::Running),
                Span::plain(" Queued"),
            ],
            None => vec![
                Span::styled(spinner, Style::Running),
                Span::plain(" Waiting for the run to appear"),
            ],
        });
        if let Some(problem) = problem {
            lines.push(vec![Span::styled(problem.replace('\n', " "), Style::Dim)]);
        }
        lines.push(vec![]);
        let done = run.is_some_and(|run| run.progress.is_done());
        lines.push(vec![
            Span::styled("q", Style::Bold),
            Span::styled(
                if done {
                    " quit"
                } else {
                    " quit; the run goes on without touchgate"
                },
                Style::Dim,
            ),
        ]);
    }

    /// Returns the progress to show in the terminal's tab.
    fn tab_progress(&self) -> TabProgress {
        match &self.stage {
            Stage::Review { .. } => TabProgress::Hidden,
            Stage::Following { updates, run, .. } => match run.as_ref().map(|run| &run.progress) {
                Some(Progress::Passed) => TabProgress::Hidden,
                Some(Progress::Failed(_) | Progress::Skipped) => TabProgress::Failed,
                _ if updates.is_none() => TabProgress::Failed,
                _ => TabProgress::Busy,
            },
        }
    }

    /// Returns what to print after the screen closes, so that the shell's
    /// history keeps where the release went.
    fn summary(&self) -> Option<String> {
        let Stage::Following { run, problem, .. } = &self.stage else {
            return None;
        };
        let version = &self.plan.version;
        Some(match run {
            Some(run) => {
                let state = match &run.progress {
                    Progress::Passed => "passed".to_owned(),
                    Progress::Failed(conclusion) => format!("ended with {conclusion}"),
                    Progress::Skipped => "was skipped".to_owned(),
                    Progress::Queued | Progress::Running => "is still running".to_owned(),
                };
                format!("The release run of {version} {state}: {}", run.url)
            }
            None => format!(
                "Started release.yml on publish for {version}. Follow it with `gh run watch`.{}",
                problem
                    .as_ref()
                    .map(|problem| format!(" {problem}"))
                    .unwrap_or_default()
            ),
        })
    }
}

/// Returns the mark of a job's `progress`, with `spinner` for a job that
/// runs.
fn progress_mark(progress: &Progress, spinner: &'static str) -> Span {
    match progress {
        Progress::Queued => Span::styled("○", Style::Dim),
        Progress::Running => Span::styled(spinner, Style::Running),
        Progress::Passed => Span::styled("✓", Style::Passed),
        Progress::Skipped => Span::styled("–", Style::Dim),
        Progress::Failed(_) => Span::styled("✗", Style::Failed),
    }
}
