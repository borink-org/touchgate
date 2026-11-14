//! The interactive `touchgate`, which prepares, approves and follows a
//! release in the terminal. It is experimental.
//!
//! It fetches the default branch and starts where the release is:
//!
//! 1. With entries under `## Unreleased`, it prepares a version that you
//!    pick in a worktree of its own, and commits it there. It pushes the
//!    commit to the default branch, or opens a pull request and follows its
//!    checks until you merge it. Your checkout stays as it is.
//! 2. With a prepared release that no tag names, it shows the commits since
//!    the last release, and signs the approval when you press `a`.
//! 3. It starts `release.yml` with the approval and follows the run.
//!
//! When it waits on you, it sends a desktop notification through the
//! terminal. ssh-keygen asks for the key's PIN on the terminal itself, and git
//! may ask for a key to sign a commit, so the screen steps aside while they
//! run. The PIN never passes through touchgate.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::changelog::{self, Changelog};
use crate::github::{self, CommitAuthor, PullRequestOutcome, Repository};
use crate::project::Project;
use crate::terminal::{Key, Line, RawTerminal, Span, Style, TabProgress, wrap_to_width};
use crate::version::{Date, Version, VersionPart};
use crate::watch::{self, Progress, PullRequestState, PullRequestView, RunView, Timing, Watched};
use crate::{ChangedCommit, Changes, ReleasePlan};
use unicode_width::UnicodeWidthStr;

/// The frames of the spinner of something under way.
const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

/// The rows of the review that are not the list of commits.
const REVIEW_CHROME: usize = 11;

/// The rows of the preparation that are not the unreleased entries or the
/// versions to pick from.
const PREPARE_CHROME: usize = 10;

/// Runs `touchgate [--key <private key>]`, where `key` means what it means
/// for `touchgate approve`.
pub fn release(key: Option<&str>) -> Result<(), Vec<String>> {
    let key = crate::signing_key(key.unwrap_or(crate::DEFAULT_KEY))?;
    let repository = Repository::of_checkout().map_err(|error| vec![error])?;
    let stage = inspect(&repository);
    let mut screen = Screen {
        repository,
        key,
        stage,
        tick: 0,
        notifications: Vec::new(),
    };
    let mut terminal = RawTerminal::enter().map_err(|error| vec![error])?;
    let result = screen.run(&mut terminal);
    drop(terminal);
    result?;
    if let Some(summary) = screen.summary() {
        println!("{summary}");
    }
    Ok(())
}

/// Where the release is.
enum Stage {
    /// Something stops the release that you have to fix, as this says.
    Blocked(String),
    /// The newest version in the changelog is released, and nothing is under
    /// `## Unreleased`.
    Released(Version),
    Preparing(Preparing),
    Merging(Merging),
    Reviewing(Reviewing),
    Following(Following),
}

/// A release to prepare, with the version and the way it lands still to
/// pick.
struct Preparing {
    /// The worktree that the release is prepared in.
    worktree: Worktree,
    /// The version to prepare again, to date it anew, if the release is not
    /// a new version.
    again: Option<Version>,
    /// The lines under `## Unreleased`.
    unreleased: Vec<String>,
    pick: Pick,
    /// Why the last try failed.
    notice: Option<String>,
}

/// The version to prepare and the way it lands, as picked so far.
struct Pick {
    choices: Vec<VersionChoice>,
    /// The index of the version picked.
    selected: usize,
    /// The index of the version that the unreleased entries suggest.
    suggested: usize,
    landing: Landing,
}

impl Pick {
    /// Returns the version picked.
    fn version(&self) -> &Version {
        &self.choices[self.selected].version
    }

    /// Changes the pick for the key `pressed`: `j` and `k` pick the next and
    /// the previous version, `g` and `G` the first and the last, and `h` and
    /// `l` a push and a pull request. The arrow keys work too, and Tab
    /// switches the landing.
    fn respond(&mut self, pressed: Key) {
        let last = self.choices.len() - 1;
        self.selected = match pressed {
            Key::Char('k') | Key::Up => self.selected.saturating_sub(1),
            Key::Char('j') | Key::Down => (self.selected + 1).min(last),
            Key::Char('g') | Key::Home => 0,
            Key::Char('G') | Key::End => last,
            _ => self.selected,
        };
        self.landing = match (pressed, self.landing) {
            (Key::Char('h') | Key::Left, _) | (Key::Tab, Landing::PullRequest) => Landing::Push,
            (Key::Char('l') | Key::Right, _) | (Key::Tab, Landing::Push) => Landing::PullRequest,
            (_, landing) => landing,
        };
    }
}

/// A version to pick, and what kind of release it is.
struct VersionChoice {
    version: Version,
    kind: &'static str,
}

/// How a prepared release reaches the default branch.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Landing {
    /// A push to the default branch.
    Push,
    /// A pull request from `release/<version>`, which you merge.
    PullRequest,
}

/// What preparing the release did.
enum Landed {
    Pushed,
    /// It opened or updated the pull request at this URL.
    PullRequest(String),
    /// The worktree already held the prepared release.
    Unchanged,
}

/// A detached worktree of the default branch as origin has it, in which a
/// release is prepared without touching your checkout. Dropping it removes
/// it.
struct Worktree {
    /// The temporary directory that holds the worktree.
    dir: PathBuf,
    /// The top directory of the worktree, in `dir`.
    path: PathBuf,
}

impl Worktree {
    /// Adds a worktree of the default branch of `repository`, as fetched.
    fn of_default_branch(repository: &Repository) -> Result<Self, Vec<String>> {
        let dir = crate::approval::private_temp_dir().map_err(|error| vec![error])?;
        let path = dir.join(repository.name.rsplit('/').next().unwrap_or("checkout"));
        let added = crate::run(
            Command::new("git")
                .args(["worktree", "add", "--detach", "--quiet"])
                .arg(&path)
                .arg(crate::tracking_ref(repository)),
        );
        let worktree = Self { dir, path };
        added?;
        Ok(worktree)
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        let _ = Command::new("git")
            .args(["worktree", "remove", "--force"])
            .arg(&self.path)
            .output();
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// A release pull request that waits for its checks and for you to merge it.
struct Merging {
    version: Version,
    url: String,
    /// When touchgate opened the pull request, in seconds since the Unix epoch.
    opened: i64,
    /// The updates on the pull request, until following stops.
    updates: Option<Receiver<Watched<PullRequestView>>>,
    /// The pull request when last asked.
    view: Option<PullRequestView>,
    /// What went wrong when last asked, if anything did.
    problem: Option<String>,
    /// Whether you were told that it is ready to merge.
    told_ready: bool,
    /// Whether you were told that a check failed.
    told_failed: bool,
}

/// A prepared release, shown before you approve it.
struct Reviewing {
    plan: ReleasePlan,
    changes: Changes,
    /// The first commit shown of the list.
    scroll: usize,
    /// Whether the changelog dates the release too long ago to approve it.
    stale_date: bool,
    /// Why the last try to approve failed.
    notice: Option<String>,
}

/// An approved release whose run is followed.
struct Following {
    plan: ReleasePlan,
    /// When touchgate started the release, in seconds since the Unix epoch.
    started: i64,
    /// The updates on the run, until following stops.
    updates: Option<Receiver<Watched<RunView>>>,
    /// The run when last asked, once it has appeared.
    run: Option<RunView>,
    /// What went wrong when last asked, if anything did.
    problem: Option<String>,
}

/// What a key asks for, beyond a change to the screen.
enum Response {
    Nothing,
    /// Look at the release again, from the start.
    Inspect,
    Prepare,
    /// Prepare the reviewed release again, to date it anew.
    PrepareAgain,
    Approve,
    Quit,
}

/// Fetches the default branch and returns where its release is.
fn inspect(repository: &Repository) -> Stage {
    inspect_release(repository).unwrap_or_else(|errors| Stage::Blocked(errors.join(" ")))
}

fn inspect_release(repository: &Repository) -> Result<Stage, Vec<String>> {
    crate::fetch_release_refs(repository)?;
    let tracking = crate::tracking_ref(repository);
    let text = crate::run(Command::new("git").args(["show", &format!("{tracking}:CHANGELOG.md")]))?;
    if !changelog::unreleased_entries(&text).is_empty() {
        return preparation_of(repository, None);
    }
    let changelog = Changelog::parse(&text)?;
    let (version, _) = changelog
        .newest_release()
        .ok_or_else(|| vec!["CHANGELOG.md has no released section".to_owned()])?;
    if is_tagged(version)? {
        return Ok(Stage::Released(version.clone()));
    }
    let plan = ReleasePlan::of(repository.clone(), None)?;
    let changes = Changes::since_release(&plan.commit)?;
    let stale_date = crate::unix_time()
        .and_then(|time| plan.approval_at(time))
        .is_err();
    Ok(Stage::Reviewing(Reviewing {
        plan,
        changes,
        scroll: 0,
        stale_date,
        notice: None,
    }))
}

/// Returns whether the tag of the release of `version` exists, as fetched.
fn is_tagged(version: &Version) -> Result<bool, Vec<String>> {
    let tag = format!("refs/tags/{}", github::release_tag(version));
    Command::new("git")
        .args(["rev-parse", "--quiet", "--verify", &tag])
        .stdout(Stdio::null())
        .status()
        .map(|status| status.success())
        .map_err(|error| vec![format!("git: {error}")])
}

/// Returns the preparation of the release of the default branch as origin
/// has it, in a worktree of its own. With `again`, prepares that version
/// again, to date it anew.
fn preparation_of(repository: &Repository, again: Option<Version>) -> Result<Stage, Vec<String>> {
    let worktree = Worktree::of_default_branch(repository)?;
    let project = Project::load_at(&worktree.path)?;
    let current = project.version()?;
    let text = crate::read(&crate::changelog_path(&project))?;
    let unreleased: Vec<String> = changelog::unreleased_lines(&text)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let (choices, suggested) = match &again {
        Some(version) => (
            vec![VersionChoice {
                version: version.clone(),
                kind: "dated anew",
            }],
            0,
        ),
        None => version_choices(&current, &unreleased),
    };
    let landing = match github::requires_pull_request(&repository.default_branch) {
        Ok(true) => Landing::PullRequest,
        _ => Landing::Push,
    };
    Ok(Stage::Preparing(Preparing {
        worktree,
        again,
        unreleased,
        pick: Pick {
            choices,
            selected: suggested,
            suggested,
            landing,
        },
        notice: None,
    }))
}

/// Returns whether a tracked file in the checkout at `dir` has changed.
fn has_uncommitted_changes(dir: &Path) -> Result<bool, Vec<String>> {
    let changed = crate::run(Command::new("git").arg("-C").arg(dir).args([
        "status",
        "--porcelain",
        "--untracked-files=no",
    ]))?;
    Ok(!changed.is_empty())
}

/// Returns the versions to pick after `current`, and the index of the one
/// that the `unreleased` lines suggest. The final release of a pre-release
/// comes first, and is suggested.
fn version_choices(current: &Version, unreleased: &[String]) -> (Vec<VersionChoice>, usize) {
    let mut choices: Vec<VersionChoice> = current
        .final_release()
        .map(|version| VersionChoice {
            version,
            kind: "final release",
        })
        .into_iter()
        .collect();
    let first_raised = choices.len();
    let parts = [VersionPart::Patch, VersionPart::Minor, VersionPart::Major];
    choices.extend(parts.iter().map(|part| VersionChoice {
        version: current.raised(*part),
        kind: part.name(),
    }));
    let suggested = if first_raised > 0 {
        0
    } else {
        let part = suggested_part(current, unreleased);
        first_raised + parts.iter().position(|each| *each == part).unwrap_or(0)
    };
    (choices, suggested)
}

/// Returns the part of `current` that the groups of the `unreleased` lines
/// suggest raising. A removal or a change breaks compatibility, and an
/// addition or a deprecation adds to the interface.
fn suggested_part(current: &Version, unreleased: &[String]) -> VersionPart {
    let has = |heading: &str| unreleased.iter().any(|line| line.trim() == heading);
    if has("### Removed") || has("### Changed") {
        if current.is_stable() {
            VersionPart::Major
        } else {
            VersionPart::Minor
        }
    } else if has("### Added") || has("### Deprecated") {
        VersionPart::Minor
    } else {
        VersionPart::Patch
    }
}

/// Takes the updates from `updates` that have arrived, and drops the receiver
/// once the follower has stopped.
fn arrived<T>(updates: &mut Option<Receiver<Watched<T>>>) -> Vec<Watched<T>> {
    let mut arrived = Vec::new();
    let mut stopped = false;
    if let Some(receiver) = updates {
        loop {
            match receiver.try_recv() {
                Ok(update) => arrived.push(update),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => {
                    stopped = true;
                    break;
                }
            }
        }
    }
    if stopped {
        *updates = None;
    }
    arrived
}

/// A desktop notification to send, as its title and body.
type Notification = (String, String);

struct Screen {
    repository: Repository,
    /// The signing key.
    key: PathBuf,
    stage: Stage,
    /// The frames drawn so far, for the spinner.
    tick: usize,
    /// The notifications not sent yet.
    notifications: Vec<Notification>,
}

impl Screen {
    /// Draws the screen and answers keys until you quit.
    fn run(&mut self, terminal: &mut RawTerminal) -> Result<(), Vec<String>> {
        let failed = |error| vec![error];
        loop {
            if self.receive_updates() {
                self.after_merge();
            }
            for (title, body) in std::mem::take(&mut self.notifications) {
                terminal.notify(&title, &body).map_err(failed)?;
            }
            let (columns, rows) = terminal.size();
            terminal
                .draw(&self.lines(columns, rows, now()))
                .map_err(failed)?;
            terminal
                .show_progress(self.tab_progress())
                .map_err(failed)?;
            for pressed in terminal.read_keys().map_err(failed)? {
                match self.respond(pressed, rows) {
                    Response::Nothing => {}
                    Response::Inspect => self.stage = inspect(&self.repository),
                    Response::Prepare => self.prepare(terminal),
                    Response::PrepareAgain => self.prepare_again(),
                    Response::Approve => self.approve(terminal),
                    Response::Quit => return Ok(()),
                }
            }
            self.tick += 1;
        }
    }

    /// Changes the screen for the key `pressed` on a screen of `rows`, and
    /// returns what else the key asks for.
    ///
    /// The keys follow vim: `j` and `k` move down and up, `h` and `l` left
    /// and right, `g` and `G` to the top and the bottom, and Ctrl with `d`
    /// and `u` by half a page, with `f` and `b` by a page. The arrow and
    /// paging keys work too. Esc does nothing, since a vim user presses it out
    /// of habit.
    fn respond(&mut self, pressed: Key, rows: usize) -> Response {
        if matches!(pressed, Key::Char('q') | Key::Ctrl('c')) {
            return Response::Quit;
        }
        match &mut self.stage {
            Stage::Blocked(_) | Stage::Released(_) => match pressed {
                Key::Char('r') => Response::Inspect,
                _ => Response::Nothing,
            },
            Stage::Preparing(preparing) => {
                preparing.pick.respond(pressed);
                match pressed {
                    Key::Enter => Response::Prepare,
                    Key::Char('r') => Response::Inspect,
                    _ => Response::Nothing,
                }
            }
            Stage::Reviewing(reviewing) => {
                let page = rows.saturating_sub(REVIEW_CHROME).max(1);
                let half = page.div_ceil(2);
                let last = reviewing.changes.commits.len().saturating_sub(page);
                let scroll = reviewing.scroll;
                reviewing.scroll = match pressed {
                    Key::Char('a') | Key::Enter => return Response::Approve,
                    Key::Char('p') if reviewing.stale_date => return Response::PrepareAgain,
                    Key::Char('r') => return Response::Inspect,
                    Key::Char('k') | Key::Up => scroll.saturating_sub(1),
                    Key::Char('j') | Key::Down => scroll + 1,
                    Key::Ctrl('u') => scroll.saturating_sub(half),
                    Key::Ctrl('d') => scroll + half,
                    Key::Ctrl('b') | Key::PageUp => scroll.saturating_sub(page),
                    Key::Ctrl('f') | Key::PageDown | Key::Char(' ') => scroll + page,
                    Key::Char('g') | Key::Home => 0,
                    Key::Char('G') | Key::End => last,
                    _ => scroll,
                }
                .min(last);
                Response::Nothing
            }
            Stage::Merging(_) | Stage::Following(_) => Response::Nothing,
        }
    }

    /// Prepares the picked version and lands it as picked. If that fails,
    /// starts the preparation over in a new worktree, and says why.
    fn prepare(&mut self, terminal: &mut RawTerminal) {
        let Stage::Preparing(preparing) = &self.stage else {
            return;
        };
        let version = preparing.pick.version().clone();
        let landing = preparing.pick.landing;
        let dir = preparing.worktree.path.clone();
        match self.prepare_and_land(terminal, &dir, &version, landing) {
            Ok(Landed::Pushed | Landed::Unchanged) => self.stage = inspect(&self.repository),
            Ok(Landed::PullRequest(url)) => {
                let (sender, receiver) = mpsc::channel();
                let branch = github::release_branch(&version);
                std::thread::spawn(move || watch::follow_pull_request(branch, sender));
                self.stage = Stage::Merging(Merging {
                    version,
                    url,
                    opened: now(),
                    updates: Some(receiver),
                    view: None,
                    problem: None,
                    told_ready: false,
                    told_failed: false,
                });
            }
            Err(error) => {
                let Stage::Preparing(old) = &self.stage else {
                    return;
                };
                let (again, selected) = (old.again.clone(), old.pick.selected);
                self.stage = match preparation_of(&self.repository, again) {
                    Ok(Stage::Preparing(mut fresh)) => {
                        fresh.pick.selected = selected.min(fresh.pick.choices.len() - 1);
                        fresh.pick.landing = landing;
                        fresh.notice = Some(error);
                        Stage::Preparing(fresh)
                    }
                    Ok(stage) => stage,
                    Err(errors) => Stage::Blocked(format!("{error} {}", errors.join(" "))),
                };
            }
        }
    }

    /// Writes the release of `version` into the worktree at `dir`, commits
    /// it, and lands it by `landing`, with the screen stepped aside while git
    /// runs.
    fn prepare_and_land(
        &self,
        terminal: &mut RawTerminal,
        dir: &Path,
        version: &Version,
        landing: Landing,
    ) -> Result<Landed, String> {
        let failed = |errors: Vec<String>| errors.join(" ");
        let prepared =
            crate::prepare_release(dir, version.clone(), Date::today()).map_err(failed)?;
        if !has_uncommitted_changes(dir).map_err(failed)? {
            return Ok(Landed::Unchanged);
        }
        let default = &self.repository.default_branch;
        let heading = match landing {
            Landing::Push => {
                format!("Committing the {version} release and pushing it to {default}.")
            }
            Landing::PullRequest => format!(
                "Committing the {version} release and opening its pull request from {}.",
                github::release_branch(version)
            ),
        };
        terminal.suspended(|| {
            println!("{heading}");
            match landing {
                Landing::Push => {
                    github::push_release(dir, &prepared.version, prepared.preparation, default)
                        .map(|()| Landed::Pushed)
                }
                Landing::PullRequest => open_pull_request(dir, &prepared),
            }
        })?
    }

    /// Prepares the reviewed release again, to date it anew.
    fn prepare_again(&mut self) {
        let Stage::Reviewing(reviewing) = &self.stage else {
            return;
        };
        let version = reviewing.plan.version.clone();
        self.stage = preparation_of(&self.repository, Some(version))
            .unwrap_or_else(|errors| Stage::Blocked(errors.join(" ")));
    }

    /// Signs the approval and starts the release. If either fails, stays on
    /// the review and says why.
    fn approve(&mut self, terminal: &mut RawTerminal) {
        let Stage::Reviewing(reviewing) = &mut self.stage else {
            return;
        };
        match sign_and_start(terminal, &reviewing.plan, &self.key) {
            Ok(updates) => {
                self.stage = Stage::Following(Following {
                    plan: reviewing.plan.clone(),
                    started: now(),
                    updates: Some(updates),
                    run: None,
                    problem: None,
                })
            }
            Err(error) => reviewing.notice = Some(error),
        }
    }

    /// Takes in the updates on what the screen follows, and queues the
    /// notifications they call for. Returns whether the release pull request
    /// was merged.
    fn receive_updates(&mut self) -> bool {
        let notifications = &mut self.notifications;
        let mut closed = None;
        match &mut self.stage {
            Stage::Merging(merging) => {
                for update in arrived(&mut merging.updates) {
                    let view = match update {
                        Watched::View(view) => view,
                        Watched::Problem(error) | Watched::GaveUp(error) => {
                            merging.problem = Some(error);
                            continue;
                        }
                    };
                    let version = &merging.version;
                    if view.is_ready() && !merging.told_ready {
                        merging.told_ready = true;
                        notifications.push((
                            format!("The {version} pull request is ready to merge"),
                            format!("Merge #{} in any way: {}", view.number, view.url),
                        ));
                    }
                    if view.has_failed_check() && !merging.told_failed {
                        merging.told_failed = true;
                        notifications.push((
                            format!("A check failed on the {version} pull request"),
                            view.url.clone(),
                        ));
                    }
                    let state = view.state;
                    merging.problem = None;
                    merging.view = Some(view);
                    match state {
                        PullRequestState::Open => {}
                        PullRequestState::Merged => return true,
                        PullRequestState::Closed => {
                            notifications.push((
                                format!("The {version} pull request was closed"),
                                merging.url.clone(),
                            ));
                            closed = Some(format!(
                                "The pull request of {version} was closed without a merge: {}. \
                                 Press r to look again.",
                                merging.url
                            ));
                            break;
                        }
                    }
                }
            }
            Stage::Following(following) => {
                for update in arrived(&mut following.updates) {
                    match update {
                        Watched::View(view) => {
                            let was_done = following
                                .run
                                .as_ref()
                                .is_some_and(|run| run.progress.is_done());
                            if view.progress.is_done() && !was_done {
                                notifications
                                    .push(run_notification(&following.plan.version, &view));
                            }
                            following.problem = None;
                            following.run = Some(view);
                        }
                        Watched::Problem(error) => following.problem = Some(error),
                        Watched::GaveUp(error) => {
                            notifications.push((
                                format!("Lost track of the {} release", following.plan.version),
                                error.clone(),
                            ));
                            following.problem = Some(error);
                        }
                    }
                }
            }
            _ => {}
        }
        if let Some(reason) = closed {
            self.stage = Stage::Blocked(reason);
        }
        false
    }

    /// Looks at the release again after its pull request was merged, and
    /// tells you if it waits for your approval.
    fn after_merge(&mut self) {
        self.stage = inspect(&self.repository);
        if let Stage::Reviewing(reviewing) = &self.stage {
            self.notifications.push((
                format!("Approve the {} release", reviewing.plan.version),
                "Its pull request is merged. Press a in touchgate, then enter the PIN and touch \
                 the key."
                    .to_owned(),
            ));
        }
    }

    /// Returns the lines of a screen of `columns` by `rows` at the time `now`,
    /// in seconds since the Unix epoch.
    fn lines(&self, columns: usize, rows: usize, now: i64) -> Vec<Line> {
        let mut lines = vec![
            vec![
                Span::styled("touchgate  ", Style::Bold),
                Span::styled(self.repository.name.clone(), Style::Dim)
                    .linked(self.repository.url.clone()),
            ],
            vec![],
        ];
        let spinner = SPINNER[self.tick % SPINNER.len()];
        let repository = &self.repository;
        let default = &repository.default_branch;
        match &self.stage {
            Stage::Blocked(reason) => {
                push_wrapped(&mut lines, reason, Style::Failed, columns);
                lines.push(vec![]);
                lines.push(footer(&[("r", "look again"), ("q", "quit")]));
            }
            Stage::Released(version) => {
                lines.push(vec![
                    Span::styled(version.to_string(), Style::Accent)
                        .linked(release_page(repository, version)),
                    Span::plain(format!(
                        " is released, and nothing is under ## Unreleased on origin/{default}."
                    )),
                ]);
                lines.push(vec![]);
                lines.push(footer(&[("r", "look again"), ("q", "quit")]));
            }
            Stage::Preparing(preparing) => {
                preparing_lines(&mut lines, preparing, repository, columns, rows)
            }
            Stage::Merging(merging) => merging_lines(&mut lines, merging, spinner, now),
            Stage::Reviewing(reviewing) => {
                reviewing_lines(&mut lines, reviewing, repository, columns, rows)
            }
            Stage::Following(following) => {
                following_lines(&mut lines, following, repository, spinner, now)
            }
        }
        lines
    }

    /// Returns the progress to show in the terminal's tab.
    fn tab_progress(&self) -> TabProgress {
        match &self.stage {
            Stage::Blocked(_) | Stage::Released(_) | Stage::Preparing(_) => TabProgress::Hidden,
            Stage::Merging(merging) => match &merging.view {
                Some(view) if view.has_failed_check() => TabProgress::Failed,
                Some(view) if view.is_ready() => TabProgress::WaitingOnYou,
                _ => TabProgress::Busy,
            },
            Stage::Reviewing(_) => TabProgress::WaitingOnYou,
            Stage::Following(following) => match following.run.as_ref().map(|run| &run.progress) {
                Some(Progress::Passed) => TabProgress::Hidden,
                Some(Progress::Failed(_) | Progress::Skipped) => TabProgress::Failed,
                _ if following.updates.is_none() => TabProgress::Failed,
                _ => TabProgress::Busy,
            },
        }
    }

    /// Returns what to print after the screen closes, so that the shell's
    /// history keeps where the release went.
    fn summary(&self) -> Option<String> {
        match &self.stage {
            Stage::Merging(merging) => Some(format!(
                "The pull request of {} is open: {}. Run touchgate again once it is merged.",
                merging.version, merging.url
            )),
            Stage::Following(following) => {
                let version = &following.plan.version;
                Some(match &following.run {
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
                        "Started release.yml on publish for {version}. Follow it with `gh run \
                         watch`.{}",
                        following
                            .problem
                            .as_ref()
                            .map(|problem| format!(" {problem}"))
                            .unwrap_or_default()
                    ),
                })
            }
            _ => None,
        }
    }
}

/// Commits the `prepared` release in the worktree at `dir` as git's
/// configured identity, pushes it to its branch and opens its pull request.
fn open_pull_request(dir: &Path, prepared: &crate::PreparedRelease) -> Result<Landed, String> {
    let project = Project::load_at(dir).map_err(|errors| errors.join(" "))?;
    let changelog = crate::changelog_in_repository(&project).map_err(|errors| errors.join(" "))?;
    let outcome = github::pull_request(
        dir,
        &prepared.version,
        prepared.preparation,
        &changelog,
        &project.registries(),
        CommitAuthor::Configured,
    )?;
    Ok(match outcome {
        PullRequestOutcome::NoChange => Landed::Unchanged,
        PullRequestOutcome::Opened(url) | PullRequestOutcome::Updated(url) => {
            Landed::PullRequest(url)
        }
    })
}

/// Signs the approval of `plan` as of now with `key`, with the screen
/// stepped aside for the PIN, starts `release.yml` with it, and starts
/// following the run.
fn sign_and_start(
    terminal: &mut RawTerminal,
    plan: &ReleasePlan,
    key: &Path,
) -> Result<Receiver<Watched<RunView>>, String> {
    let earlier = watch::started_run_ids()?;
    let approval = crate::unix_time()
        .and_then(|time| plan.approval_at(time))
        .map_err(|errors| errors.join(" "))?;
    let heading = format!(
        "Approving release {} of {} at {}, to run from publish at {}.",
        approval.version, approval.repository, approval.commit, approval.workflow_commit
    );
    let signed = terminal.suspended(|| {
        println!("{heading}");
        approval.sign(key)
    })??;
    github::start_release(&signed)?;
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || watch::follow_run(earlier, sender));
    Ok(receiver)
}

/// Returns the notification of a release run of `version` that finished.
fn run_notification(version: &Version, run: &RunView) -> Notification {
    let title = match &run.progress {
        Progress::Passed => format!("Released {version}"),
        Progress::Failed(conclusion) => {
            format!("The release run of {version} ended with {conclusion}")
        }
        _ => format!("The release run of {version} was skipped"),
    };
    (title, run.url.clone())
}

/// Adds `text` to `lines` in `style`, wrapped to `columns`.
fn push_wrapped(lines: &mut Vec<Line>, text: &str, style: Style, columns: usize) {
    for line in wrap_to_width(text, columns) {
        lines.push(vec![Span::styled(line, style)]);
    }
}

/// Returns a line that names `keys`, each a key and what it does.
fn footer(keys: &[(&str, &str)]) -> Line {
    let mut line = Vec::new();
    for (index, (key, action)) in keys.iter().enumerate() {
        if index > 0 {
            line.push(Span::plain("  "));
        }
        line.push(Span::styled(*key, Style::Bold));
        line.push(Span::styled(format!(" {action}"), Style::Dim));
    }
    line
}

/// Returns the URL of `path` among the GitHub pages of `repository`, such
/// as `commit/<hash>`.
fn page(repository: &Repository, path: &str) -> String {
    format!("{}/{path}", repository.url)
}

/// Returns the URL of the GitHub release of `version`.
fn release_page(repository: &Repository, version: &Version) -> String {
    page(
        repository,
        &format!("releases/tag/{}", github::release_tag(version)),
    )
}

/// Returns the URL of the changelog of `repository` at `revision`, a commit
/// or a branch.
fn changelog_page(repository: &Repository, revision: &str) -> String {
    page(repository, &format!("blob/{revision}/CHANGELOG.md"))
}

/// Returns a commit `subject` as spans, with each `#<number>` linked to that
/// issue or pull request of `repository`. GitHub sends the URL of an issue to
/// the pull request of that number.
fn subject_spans(subject: &str, repository: &Repository) -> Vec<Span> {
    let bytes = subject.as_bytes();
    let is_word = |at: usize| bytes.get(at).is_some_and(u8::is_ascii_alphanumeric);
    let mut spans = Vec::new();
    let (mut plain, mut at) = (0, 0);
    while at < bytes.len() {
        let digits = bytes[at + 1..]
            .iter()
            .take_while(|byte| byte.is_ascii_digit())
            .count();
        let end = at + 1 + digits;
        let starts = at == 0 || !is_word(at - 1);
        if bytes[at] == b'#' && starts && digits > 0 && !is_word(end) {
            if plain < at {
                spans.push(Span::plain(&subject[plain..at]));
            }
            let number = &subject[at + 1..end];
            spans.push(
                Span::styled(&subject[at..end], Style::Accent)
                    .linked(page(repository, &format!("issues/{number}"))),
            );
            (plain, at) = (end, end);
        } else {
            at += 1;
        }
    }
    if plain < subject.len() {
        spans.push(Span::plain(&subject[plain..]));
    }
    spans
}

/// Returns the line that names the release of `version`. With `dated`, it
/// also names the day the changelog dates it, and links the changelog to
/// that URL.
fn release_line(version: &Version, dated: Option<(Date, String)>) -> Line {
    let mut line = vec![
        Span::plain("Release   "),
        Span::styled(version.to_string(), Style::Accent),
    ];
    if let Some((date, changelog)) = dated {
        line.push(Span::styled(format!("  dated {date} in "), Style::Dim));
        line.push(Span::styled("CHANGELOG.md", Style::Dim).linked(changelog));
    }
    line
}

fn preparing_lines(
    lines: &mut Vec<Line>,
    preparing: &Preparing,
    repository: &Repository,
    columns: usize,
    rows: usize,
) {
    let default = &repository.default_branch;
    let room = rows.saturating_sub(PREPARE_CHROME + preparing.pick.choices.len());
    lines.push(vec![
        Span::styled(
            if preparing.unreleased.is_empty() {
                "Nothing is under ## Unreleased"
            } else {
                "Unreleased"
            },
            Style::Bold,
        ),
        Span::styled(" in ", Style::Dim),
        Span::styled("CHANGELOG.md", Style::Dim).linked(changelog_page(repository, default)),
        Span::styled(format!(" on {default}"), Style::Dim),
    ]);
    // Entries are wrapped, with their continuation lines indented.
    let mut unreleased = Vec::new();
    for line in &preparing.unreleased {
        let style = if line.starts_with('#') {
            Style::Accent
        } else {
            Style::Plain
        };
        for (index, part) in wrap_to_width(line, columns.saturating_sub(4).max(20))
            .into_iter()
            .enumerate()
        {
            let indent = if index == 0 { "  " } else { "    " };
            unreleased.push(vec![Span::plain(indent), Span::styled(part, style)]);
        }
    }
    let shown = if unreleased.len() > room {
        room.saturating_sub(1)
    } else {
        room
    };
    let hidden = unreleased.len().saturating_sub(shown);
    lines.extend(unreleased.into_iter().take(shown));
    if hidden > 0 {
        lines.push(vec![Span::styled(
            format!("  … {hidden} more lines"),
            Style::Dim,
        )]);
    }
    lines.push(vec![]);
    for (index, choice) in preparing.pick.choices.iter().enumerate() {
        let picked = index == preparing.pick.selected;
        let mut line = vec![
            Span::plain(if index == 0 {
                "Version   "
            } else {
                "          "
            }),
            if picked {
                Span::styled("● ", Style::Accent)
            } else {
                Span::styled("○ ", Style::Dim)
            },
            Span::styled(
                choice.version.to_string(),
                if picked { Style::Bold } else { Style::Plain },
            ),
            Span::styled(format!("  {}", choice.kind), Style::Dim),
        ];
        if index == preparing.pick.suggested && preparing.pick.choices.len() > 1 {
            line.push(Span::styled(", suggested", Style::Dim));
        }
        lines.push(line);
    }
    let version = &preparing.pick.choices[preparing.pick.selected].version;
    let mut landing = vec![Span::plain("Lands by  ")];
    for (each, text) in [
        (Landing::Push, format!("a push to {default}")),
        (
            Landing::PullRequest,
            format!("a pull request from {}", github::release_branch(version)),
        ),
    ] {
        let (mark, style) = if each == preparing.pick.landing {
            (Span::styled("● ", Style::Accent), Style::Bold)
        } else {
            (Span::styled("○ ", Style::Dim), Style::Plain)
        };
        landing.push(mark);
        landing.push(match each {
            Landing::Push => {
                Span::styled(text, style).linked(page(repository, &format!("tree/{default}")))
            }
            Landing::PullRequest => Span::styled(text, style),
        });
        landing.push(Span::plain("   "));
    }
    lines.push(landing);
    lines.push(vec![]);
    if let Some(notice) = &preparing.notice {
        push_wrapped(lines, notice, Style::Failed, columns);
    }
    let action = format!("prepare {version}");
    let mut keys = vec![("enter", action.as_str())];
    if preparing.pick.choices.len() > 1 {
        keys.push(("j/k", "version"));
    }
    keys.extend([("h/l", "push or pull request"), ("q", "quit")]);
    lines.push(footer(&keys));
}

fn merging_lines(lines: &mut Vec<Line>, merging: &Merging, spinner: &'static str, now: i64) {
    lines.push(release_line(&merging.version, None));
    let view = merging.view.as_ref();
    lines.push(vec![
        Span::plain("Pull      "),
        Span::styled(
            match view {
                Some(view) => format!("#{} {}", view.number, view.title),
                None => merging.url.clone(),
            },
            Style::Accent,
        )
        .linked(merging.url.clone()),
    ]);
    lines.push(vec![]);
    let checks: Vec<TaskRow> = view
        .iter()
        .flat_map(|view| &view.checks)
        .map(|check| TaskRow {
            name: &check.name,
            url: &check.url,
            progress: &check.progress,
            step: None,
            timing: check.timing,
        })
        .collect();
    lines.extend(task_lines(&checks, spinner, now));
    if view.is_some_and(|view| !view.checks.is_empty()) {
        lines.push(vec![]);
    }
    lines.push(match view {
        Some(view) if view.has_failed_check() => vec![Span::styled(
            "✗ A check failed. Push a fix to the release branch, or close the pull request.",
            Style::Failed,
        )],
        Some(view) if view.is_ready() => vec![Span::styled(
            "● Ready to merge. Merge it on GitHub in any way, and touchgate goes on.",
            Style::Accent,
        )],
        Some(_) => waiting(spinner, "Waiting for the checks", now - merging.opened),
        None => waiting(
            spinner,
            "Asking GitHub about the pull request",
            now - merging.opened,
        ),
    });
    if let Some(problem) = &merging.problem {
        lines.push(vec![Span::styled(problem.replace('\n', " "), Style::Dim)]);
    }
    lines.push(vec![]);
    lines.push(footer(&[("q", "quit; the pull request stays open")]));
}

fn reviewing_lines(
    lines: &mut Vec<Line>,
    reviewing: &Reviewing,
    repository: &Repository,
    columns: usize,
    rows: usize,
) {
    let plan = &reviewing.plan;
    let changes = &reviewing.changes;
    let commit_page = |hash: &str| page(repository, &format!("commit/{hash}"));
    lines.push(release_line(
        &plan.version,
        Some((plan.date, changelog_page(repository, &plan.commit))),
    ));
    lines.push(vec![
        Span::plain("Commit    "),
        Span::styled(plan.commit.clone(), Style::Accent).linked(commit_page(&plan.commit)),
    ]);
    lines.push(vec![
        Span::plain("Runs from "),
        Span::plain("publish").linked(page(repository, "tree/publish")),
        Span::plain(" at "),
        Span::styled(plan.workflow_commit.clone(), Style::Accent)
            .linked(commit_page(&plan.workflow_commit)),
    ]);
    lines.push(vec![]);
    let count = changes.commits.len();
    let commits = format!(
        "{count} commit{}, newest first",
        if count == 1 { "" } else { "s" }
    );
    lines.push(match &changes.previous {
        Some(tag) => vec![
            Span::styled("Since ", Style::Bold),
            Span::styled(tag.clone(), Style::Bold)
                .linked(page(repository, &format!("releases/tag/{tag}"))),
            Span::styled(format!(": {commits}  "), Style::Bold),
            Span::styled(changes.shortstat.trim().to_owned(), Style::Dim).linked(page(
                repository,
                &format!("compare/{tag}...{}", plan.commit),
            )),
        ],
        None => vec![Span::styled(
            format!("No release is tagged before this commit. The last {commits}:"),
            Style::Bold,
        )],
    });
    let page_rows = rows.saturating_sub(REVIEW_CHROME).max(1);
    let shown: Vec<&ChangedCommit> = changes
        .commits
        .iter()
        .skip(reviewing.scroll)
        .take(page_rows)
        .collect();
    let age_width = shown
        .iter()
        .map(|commit| commit.age.len())
        .max()
        .unwrap_or(0);
    for commit in shown {
        let mut line = vec![
            Span::plain("  "),
            Span::styled(commit.short.clone(), Style::Accent).linked(commit_page(&commit.hash)),
            Span::styled(format!("  {:<age_width$}  ", commit.age), Style::Dim),
        ];
        line.extend(subject_spans(&commit.subject, repository));
        if commit.hash == plan.commit {
            line.push(Span::styled("  ← the commit to release", Style::Dim));
        }
        lines.push(line);
    }
    let page = page_rows;
    lines.push(vec![]);
    if reviewing.stale_date {
        let text = format!(
            "CHANGELOG.md dates {} {}, and an approval must come that day or the next, in UTC. \
             Press p to date it anew.",
            plan.version, plan.date
        );
        push_wrapped(lines, &text, Style::Running, columns);
    }
    if let Some(notice) = &reviewing.notice {
        push_wrapped(lines, notice, Style::Failed, columns);
    }
    let scroll = format!(
        "scroll ({}–{} of {count})",
        reviewing.scroll + 1,
        (reviewing.scroll + page).min(count)
    );
    let mut keys = vec![("a", "approve with the security key")];
    if reviewing.stale_date {
        keys.push(("p", "date anew"));
    }
    if count > page {
        keys.push(("j/k", scroll.as_str()));
    }
    keys.push(("q", "quit"));
    lines.push(footer(&keys));
}

fn following_lines(
    lines: &mut Vec<Line>,
    following: &Following,
    repository: &Repository,
    spinner: &'static str,
    now: i64,
) {
    let run = following.run.as_ref();
    lines.push(release_line(&following.plan.version, None));
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
    let jobs: Vec<TaskRow> = run
        .iter()
        .flat_map(|run| &run.jobs)
        .map(|job| TaskRow {
            name: &job.name,
            url: &job.url,
            progress: &job.progress,
            step: job.step.as_deref(),
            timing: job.timing,
        })
        .collect();
    lines.extend(task_lines(&jobs, spinner, now));
    // How long the run took, or has taken so far, as GitHub times it.
    let took = run
        .and_then(|run| run.timing.elapsed(now))
        .unwrap_or(now - following.started);
    if run.is_some_and(|run| !run.jobs.is_empty()) {
        lines.push(vec![]);
    }
    let version = &following.plan.version;
    lines.push(match run.map(|run| &run.progress) {
        Some(Progress::Passed) => vec![
            Span::styled("✓ Released ", Style::Passed),
            Span::styled(version.to_string(), Style::Passed)
                .linked(release_page(repository, version)),
            Span::styled(format!(" in {}.", format_elapsed(took)), Style::Passed),
        ],
        Some(Progress::Failed(conclusion)) => vec![Span::styled(
            format!(
                "✗ The release run ended with {conclusion} after {}.",
                format_elapsed(took)
            ),
            Style::Failed,
        )],
        Some(Progress::Skipped) => {
            vec![Span::styled("The release run was skipped.", Style::Failed)]
        }
        _ if following.updates.is_none() => {
            vec![Span::styled("Following the run stopped.", Style::Failed)]
        }
        Some(Progress::Running) => waiting(spinner, "Running", took),
        Some(Progress::Queued) => waiting(spinner, "Queued", took),
        None => waiting(spinner, "Waiting for the run to appear", took),
    });
    if let Some(problem) = &following.problem {
        lines.push(vec![Span::styled(problem.replace('\n', " "), Style::Dim)]);
    }
    lines.push(vec![]);
    let done = run.is_some_and(|run| run.progress.is_done());
    lines.push(footer(&[(
        "q",
        if done {
            "quit"
        } else {
            "quit; the run goes on without touchgate"
        },
    )]));
}

/// Returns the time in seconds since the Unix epoch, for the timers.
fn now() -> i64 {
    crate::unix_time().unwrap_or_default()
}

/// Returns `seconds` as GitHub Actions writes a duration, such as `45s`,
/// `1m 4s` or `2h 5m`.
fn format_elapsed(seconds: i64) -> String {
    match seconds {
        ..60 => format!("{seconds}s"),
        60..3_600 => format!("{}m {}s", seconds / 60, seconds % 60),
        _ => format!("{}h {}m", seconds / 3_600, seconds % 3_600 / 60),
    }
}

/// A job of the run or a check on the pull request, as a row of the screen.
struct TaskRow<'a> {
    name: &'a str,
    url: &'a str,
    progress: &'a Progress,
    /// The step it is at, for a job that runs.
    step: Option<&'a str>,
    timing: Timing,
}

/// Returns the lines of `rows` at the time `now`: each mark and name, then
/// how long it ran, in one column, then its step.
fn task_lines(rows: &[TaskRow], spinner: &'static str, now: i64) -> Vec<Line> {
    let name_width = rows.iter().map(|row| row.name.width()).max().unwrap_or(0);
    rows.iter()
        .map(|row| {
            let elapsed = row
                .timing
                .elapsed(now)
                .map(format_elapsed)
                .unwrap_or_default();
            let padding = name_width - row.name.width() + 2;
            let mut line = vec![
                Span::plain("  "),
                progress_mark(row.progress, spinner),
                Span::plain(" "),
                Span::plain(row.name).linked(row.url),
                Span::styled(format!("{:padding$}{elapsed:>7}", ""), Style::Dim),
            ];
            if let Some(step) = row.step {
                line.push(Span::styled(format!("  {step}"), Style::Dim));
            }
            line
        })
        .collect()
}

/// Returns a line of a spinner, `text` and how long it has taken.
fn waiting(spinner: &'static str, text: &str, elapsed: i64) -> Line {
    vec![
        Span::styled(spinner, Style::Running),
        Span::plain(format!(" {text}  ")),
        Span::styled(format_elapsed(elapsed), Style::Dim),
    ]
}

/// Returns the mark of the `progress` of a job or a check, with `spinner` for
/// one that runs.
fn progress_mark(progress: &Progress, spinner: &'static str) -> Span {
    match progress {
        Progress::Queued => Span::styled("○", Style::Dim),
        Progress::Running => Span::styled(spinner, Style::Running),
        Progress::Passed => Span::styled("✓", Style::Passed),
        Progress::Skipped => Span::styled("–", Style::Dim),
        Progress::Failed(_) => Span::styled("✗", Style::Failed),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn suggestion(current: &str, lines: &[&str]) -> String {
        let current = Version::parse(current).unwrap();
        let lines: Vec<String> = lines.iter().map(|line| line.to_string()).collect();
        let (choices, suggested) = version_choices(&current, &lines);
        choices[suggested].version.to_string()
    }

    /// Returns a repository that releases from main.
    fn repository() -> Repository {
        Repository {
            name: "o/r".to_owned(),
            default_branch: "main".to_owned(),
            url: "https://github.com/o/r".to_owned(),
        }
    }

    /// Returns a screen at `stage` for a repository that releases from main.
    fn screen_at(stage: Stage) -> Screen {
        Screen {
            repository: repository(),
            key: PathBuf::from("key"),
            stage,
            tick: 0,
            notifications: Vec::new(),
        }
    }

    #[test]
    fn picks_a_version_and_a_landing_with_vim_keys() {
        let (choices, suggested) = version_choices(&Version::parse("0.2.0").unwrap(), &[]);
        let mut pick = Pick {
            choices,
            selected: suggested,
            suggested,
            landing: Landing::Push,
        };
        let mut press = |key| {
            pick.respond(key);
            (pick.version().to_string(), pick.landing)
        };
        assert_eq!(press(Key::Char('j')), ("0.3.0".to_owned(), Landing::Push));
        assert_eq!(press(Key::Char('G')).0, "1.0.0");
        assert_eq!(press(Key::Char('j')).0, "1.0.0");
        assert_eq!(press(Key::Char('g')).0, "0.2.1");
        assert_eq!(press(Key::Char('l')).1, Landing::PullRequest);
        assert_eq!(press(Key::Char('l')).1, Landing::PullRequest);
        assert_eq!(press(Key::Char('h')).1, Landing::Push);
        assert_eq!(press(Key::Tab).1, Landing::PullRequest);
    }

    #[test]
    fn scrolls_the_commits_with_vim_keys() {
        let commits = (0..40)
            .map(|n| ChangedCommit {
                hash: format!("{n:040x}"),
                short: format!("{n:07x}"),
                age: format!("{n} hours ago"),
                subject: format!("Commit {n}."),
            })
            .collect();
        let mut screen = screen_at(Stage::Reviewing(Reviewing {
            plan: ReleasePlan {
                repository: "o/r".to_owned(),
                commit: "c".repeat(40),
                version: Version::parse("0.3.0").unwrap(),
                date: Date::parse("2026-11-14").unwrap(),
                workflow_commit: "w".repeat(40),
            },
            changes: Changes {
                previous: None,
                commits,
                shortstat: String::new(),
            },
            scroll: 0,
            stale_date: false,
            notice: None,
        }));
        // 24 rows leave a page of 13 commits, so the last page starts at 27.
        let mut press = |key| {
            screen.respond(key, 24);
            let Stage::Reviewing(reviewing) = &screen.stage else {
                panic!("the screen left the review");
            };
            reviewing.scroll
        };
        assert_eq!(press(Key::Char('j')), 1);
        assert_eq!(press(Key::Ctrl('d')), 8);
        assert_eq!(press(Key::Ctrl('f')), 21);
        assert_eq!(press(Key::Ctrl('f')), 27);
        assert_eq!(press(Key::Ctrl('u')), 20);
        assert_eq!(press(Key::Ctrl('b')), 7);
        assert_eq!(press(Key::Char('G')), 27);
        assert_eq!(press(Key::Char('g')), 0);
        assert_eq!(press(Key::Char('k')), 0);
        assert!(matches!(screen.respond(Key::Escape, 24), Response::Nothing));
        assert!(matches!(
            screen.respond(Key::Char('a'), 24),
            Response::Approve
        ));
        assert!(matches!(screen.respond(Key::Char('q'), 24), Response::Quit));
    }

    #[test]
    fn links_the_issues_a_subject_names() {
        let spans = subject_spans(
            "Merge pull request #11, after #2 but not a#3 or #4b",
            &repository(),
        );
        let parts: Vec<_> = spans
            .iter()
            .map(|span| (span.text.as_str(), span.link.as_deref()))
            .collect();
        assert_eq!(
            parts,
            [
                ("Merge pull request ", None),
                ("#11", Some("https://github.com/o/r/issues/11")),
                (", after ", None),
                ("#2", Some("https://github.com/o/r/issues/2")),
                (" but not a#3 or #4b", None),
            ]
        );
    }

    #[test]
    fn writes_durations_as_github_does() {
        assert_eq!(format_elapsed(0), "0s");
        assert_eq!(format_elapsed(59), "59s");
        assert_eq!(format_elapsed(64), "1m 4s");
        assert_eq!(format_elapsed(3_599), "59m 59s");
        assert_eq!(format_elapsed(7_500), "2h 5m");
    }

    #[test]
    fn suggests_the_version_the_entries_call_for() {
        assert_eq!(suggestion("0.2.0", &["### Fixed", "- A bug."]), "0.2.1");
        assert_eq!(suggestion("0.2.0", &["### Added", "- A flag."]), "0.3.0");
        assert_eq!(suggestion("0.2.0", &["### Removed", "- A flag."]), "0.3.0");
        assert_eq!(suggestion("1.2.0", &["### Changed", "- A flag."]), "2.0.0");
        assert_eq!(
            suggestion("1.0.0-rc.1", &["### Changed", "- A flag."]),
            "1.0.0"
        );
    }
}
