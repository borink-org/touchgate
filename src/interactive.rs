//! The interactive `touchgate`, which prepares, approves and follows a
//! release in the terminal. It is experimental.
//!
//! It fetches the default branch and starts where the release is:
//!
//! 1. With entries under `## Unreleased`, it prepares a version that you
//!    pick, and commits it. It pushes the commit to the default branch, or
//!    opens a pull request and follows its checks until you merge it.
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
use crate::watch::{self, Progress, PullRequestState, PullRequestView, RunView, Watched};
use crate::{Changes, ReleasePlan};

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
    /// The lines under `## Unreleased`.
    unreleased: Vec<String>,
    choices: Vec<VersionChoice>,
    /// The index of the version picked.
    selected: usize,
    /// The index of the version that the unreleased entries suggest.
    suggested: usize,
    landing: Landing,
    /// Why the last try failed.
    notice: Option<String>,
}

/// A version to pick, and what kind of release it is.
struct VersionChoice {
    version: Version,
    kind: &'static str,
}

/// How a prepared release reaches the default branch.
#[derive(Clone, Copy, PartialEq)]
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
    /// The checkout already held the prepared release.
    Unchanged,
}

/// Why preparing a release failed.
enum Failure {
    /// It failed before it changed the checkout.
    Unchanged(String),
    /// It failed after it changed the checkout, which keeps the change.
    Changed(String),
}

/// A release pull request that waits for its checks and for you to merge it.
struct Merging {
    version: Version,
    url: String,
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
        return preparing(repository, None);
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

/// Returns the preparation of the release in the checkout, which must be on
/// the default branch as origin has it. With `again`, prepares that version
/// again, to date it anew.
fn preparing(repository: &Repository, again: Option<Version>) -> Result<Stage, Vec<String>> {
    if let Some(reason) = checkout_problem(repository)? {
        return Ok(Stage::Blocked(reason));
    }
    let project = Project::load()?;
    let current = project.version()?;
    let text = crate::read(&crate::changelog_path(&project))?;
    let unreleased: Vec<String> = changelog::unreleased_lines(&text)
        .into_iter()
        .map(str::to_owned)
        .collect();
    let (choices, suggested) = match again {
        Some(version) => (
            vec![VersionChoice {
                version,
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
        unreleased,
        choices,
        selected: suggested,
        suggested,
        landing,
        notice: None,
    }))
}

/// Returns why the checkout cannot prepare a release, if it cannot. It must
/// be on the default branch, with no uncommitted changes and no commits that
/// origin does not have. A checkout behind origin is fast-forwarded.
fn checkout_problem(repository: &Repository) -> Result<Option<String>, Vec<String>> {
    let default = &repository.default_branch;
    let branch = Command::new("git")
        .args(["symbolic-ref", "--quiet", "--short", "HEAD"])
        .output()
        .map_err(|error| vec![format!("git: {error}")])?;
    let branch = String::from_utf8_lossy(&branch.stdout).trim().to_owned();
    if branch != *default {
        let on = if branch.is_empty() {
            "a detached HEAD".to_owned()
        } else {
            branch
        };
        return Ok(Some(format!(
            "The checkout is on {on}. Switch to {default} to prepare a release, then press r."
        )));
    }
    if has_uncommitted_changes()? {
        return Ok(Some(
            "The checkout has uncommitted changes. Commit or stash them, then press r.".to_owned(),
        ));
    }
    let tracking = crate::tracking_ref(repository);
    let counts = crate::run(Command::new("git").args([
        "rev-list",
        "--left-right",
        "--count",
        &format!("HEAD...{tracking}"),
    ]))?;
    let (ahead, behind) = counts.trim().split_once('\t').unwrap_or(("0", "0"));
    if ahead != "0" {
        return Ok(Some(format!(
            "{default} has {ahead} commit(s) that origin does not have. Push them, then press r."
        )));
    }
    if behind != "0" {
        crate::run(Command::new("git").args(["merge", "--ff-only", "--quiet", &tracking]))?;
    }
    Ok(None)
}

/// Returns whether a tracked file in the checkout has changed.
fn has_uncommitted_changes() -> Result<bool, Vec<String>> {
    let changed =
        crate::run(Command::new("git").args(["status", "--porcelain", "--untracked-files=no"]))?;
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
            terminal.draw(&self.lines(columns, rows)).map_err(failed)?;
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
    fn respond(&mut self, pressed: Key, rows: usize) -> Response {
        if matches!(pressed, Key::Char('q') | Key::Escape | Key::Ctrl('c')) {
            return Response::Quit;
        }
        match &mut self.stage {
            Stage::Blocked(_) | Stage::Released(_) => match pressed {
                Key::Char('r') => Response::Inspect,
                _ => Response::Nothing,
            },
            Stage::Preparing(preparing) => {
                let last = preparing.choices.len() - 1;
                match pressed {
                    Key::Up | Key::Char('k') => {
                        preparing.selected = preparing.selected.saturating_sub(1)
                    }
                    Key::Down | Key::Char('j') => {
                        preparing.selected = (preparing.selected + 1).min(last)
                    }
                    Key::Char('l') | Key::Tab => {
                        preparing.landing = match preparing.landing {
                            Landing::Push => Landing::PullRequest,
                            Landing::PullRequest => Landing::Push,
                        }
                    }
                    Key::Enter => return Response::Prepare,
                    Key::Char('r') => return Response::Inspect,
                    _ => {}
                }
                Response::Nothing
            }
            Stage::Reviewing(reviewing) => {
                let page = rows.saturating_sub(REVIEW_CHROME).max(1);
                let last = reviewing.changes.log.lines().count().saturating_sub(page);
                let scroll = reviewing.scroll;
                reviewing.scroll = match pressed {
                    Key::Char('a') | Key::Enter => return Response::Approve,
                    Key::Char('p') if reviewing.stale_date => return Response::PrepareAgain,
                    Key::Char('r') => return Response::Inspect,
                    Key::Up | Key::Char('k') => scroll.saturating_sub(1),
                    Key::Down | Key::Char('j') => scroll + 1,
                    Key::PageUp => scroll.saturating_sub(page),
                    Key::PageDown | Key::Char(' ') => scroll + page,
                    Key::Home => 0,
                    Key::End => last,
                    _ => scroll,
                }
                .min(last);
                Response::Nothing
            }
            Stage::Merging(_) | Stage::Following(_) => Response::Nothing,
        }
    }

    /// Prepares the picked version and lands it as picked. If that fails
    /// before anything changed, stays on the preparation and says why.
    fn prepare(&mut self, terminal: &mut RawTerminal) {
        let Stage::Preparing(preparing) = &self.stage else {
            return;
        };
        let version = preparing.choices[preparing.selected].version.clone();
        let landing = preparing.landing;
        match self.prepare_and_land(terminal, &version, landing) {
            Ok(Landed::Pushed | Landed::Unchanged) => self.stage = inspect(&self.repository),
            Ok(Landed::PullRequest(url)) => {
                let (sender, receiver) = mpsc::channel();
                let branch = github::release_branch(&version);
                std::thread::spawn(move || watch::follow_pull_request(branch, sender));
                self.stage = Stage::Merging(Merging {
                    version,
                    url,
                    updates: Some(receiver),
                    view: None,
                    problem: None,
                    told_ready: false,
                    told_failed: false,
                });
            }
            Err(Failure::Unchanged(error)) => {
                if let Stage::Preparing(preparing) = &mut self.stage {
                    preparing.notice = Some(error);
                }
            }
            Err(Failure::Changed(error)) => {
                self.stage = Stage::Blocked(format!(
                    "{error} The checkout keeps what was prepared. Land it yourself, or undo it, \
                     then press r."
                ));
            }
        }
    }

    /// Writes the release of `version` into the checkout, commits it, and
    /// lands it by `landing`, with the screen stepped aside while git runs.
    fn prepare_and_land(
        &self,
        terminal: &mut RawTerminal,
        version: &Version,
        landing: Landing,
    ) -> Result<Landed, Failure> {
        let changed = |errors: Vec<String>| Failure::Changed(errors.join(" "));
        // Preparing writes the changelog first, so a failure after it leaves
        // a change; one before it, such as a check, leaves none.
        let prepared =
            crate::prepare_release(version.clone(), Date::today()).map_err(|errors| {
                match has_uncommitted_changes() {
                    Ok(false) => Failure::Unchanged(errors.join(" ")),
                    _ => changed(errors),
                }
            })?;
        if !has_uncommitted_changes().map_err(changed)? {
            return Ok(Landed::Unchanged);
        }
        let default = &self.repository.default_branch;
        let heading = match landing {
            Landing::Push => {
                format!("Committing the {version} release and pushing it to {default}.")
            }
            Landing::PullRequest => format!(
                "Committing the {version} release on {} and opening its pull request.",
                github::release_branch(version)
            ),
        };
        let landed = terminal
            .suspended(|| {
                println!("{heading}");
                match landing {
                    Landing::Push => {
                        github::push_release(&prepared.version, prepared.preparation, default)
                            .map(|()| Landed::Pushed)
                    }
                    Landing::PullRequest => open_pull_request(&prepared),
                }
            })
            .map_err(Failure::Changed)?;
        landed.map_err(Failure::Changed)
    }

    /// Prepares the reviewed release again, to date it anew.
    fn prepare_again(&mut self) {
        let Stage::Reviewing(reviewing) = &self.stage else {
            return;
        };
        let version = reviewing.plan.version.clone();
        self.stage = preparing(&self.repository, Some(version))
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

    /// Returns the lines of a screen of `columns` by `rows`.
    fn lines(&self, columns: usize, rows: usize) -> Vec<Line> {
        let mut lines = vec![
            vec![
                Span::styled("touchgate", Style::Bold),
                Span::styled(format!("  {}", self.repository.name), Style::Dim),
            ],
            vec![],
        ];
        let spinner = SPINNER[self.tick % SPINNER.len()];
        let default = &self.repository.default_branch;
        match &self.stage {
            Stage::Blocked(reason) => {
                push_wrapped(&mut lines, reason, Style::Failed, columns);
                lines.push(vec![]);
                lines.push(footer(&[("r", "look again"), ("q", "quit")]));
            }
            Stage::Released(version) => {
                lines.push(vec![
                    Span::styled(version.to_string(), Style::Accent),
                    Span::plain(format!(
                        " is released, and nothing is under ## Unreleased on origin/{default}."
                    )),
                ]);
                lines.push(vec![]);
                lines.push(footer(&[("r", "look again"), ("q", "quit")]));
            }
            Stage::Preparing(preparing) => {
                preparing_lines(&mut lines, preparing, default, columns, rows)
            }
            Stage::Merging(merging) => merging_lines(&mut lines, merging, spinner),
            Stage::Reviewing(reviewing) => reviewing_lines(&mut lines, reviewing, columns, rows),
            Stage::Following(following) => following_lines(&mut lines, following, spinner),
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

/// Commits the `prepared` release on its branch as the checkout's identity,
/// pushes it and opens its pull request.
fn open_pull_request(prepared: &crate::PreparedRelease) -> Result<Landed, String> {
    let project = Project::load().map_err(|errors| errors.join(" "))?;
    let changelog = crate::changelog_in_repository(&project).map_err(|errors| errors.join(" "))?;
    let outcome = github::pull_request(
        &prepared.version,
        prepared.preparation,
        &changelog,
        &project.registries(),
        CommitAuthor::Checkout,
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

/// Returns the line that names the release of `version`, and the day the
/// changelog dates it if it does.
fn release_line(version: &Version, date: Option<Date>) -> Line {
    let mut line = vec![
        Span::plain("Release   "),
        Span::styled(version.to_string(), Style::Accent),
    ];
    if let Some(date) = date {
        line.push(Span::styled(
            format!("  dated {date} in CHANGELOG.md"),
            Style::Dim,
        ));
    }
    line
}

fn preparing_lines(
    lines: &mut Vec<Line>,
    preparing: &Preparing,
    default: &str,
    columns: usize,
    rows: usize,
) {
    let room = rows.saturating_sub(PREPARE_CHROME + preparing.choices.len());
    lines.push(vec![Span::styled(
        if preparing.unreleased.is_empty() {
            "Nothing is under ## Unreleased."
        } else {
            "Unreleased"
        },
        Style::Bold,
    )]);
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
    for (index, choice) in preparing.choices.iter().enumerate() {
        let picked = index == preparing.selected;
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
        if index == preparing.suggested && preparing.choices.len() > 1 {
            line.push(Span::styled(", suggested", Style::Dim));
        }
        lines.push(line);
    }
    let version = &preparing.choices[preparing.selected].version;
    lines.push(vec![
        Span::plain("Lands by  "),
        Span::styled(
            match preparing.landing {
                Landing::Push => format!("a push to {default}"),
                Landing::PullRequest => {
                    format!("a pull request from {}", github::release_branch(version))
                }
            },
            Style::Bold,
        ),
    ]);
    lines.push(vec![]);
    if let Some(notice) = &preparing.notice {
        push_wrapped(lines, notice, Style::Failed, columns);
    }
    let action = format!("prepare {version}");
    let other = match preparing.landing {
        Landing::Push => "pull request instead",
        Landing::PullRequest => "push instead",
    };
    let mut keys = vec![("enter", action.as_str())];
    if preparing.choices.len() > 1 {
        keys.push(("↑↓", "version"));
    }
    keys.extend([("l", other), ("q", "quit")]);
    lines.push(footer(&keys));
}

fn merging_lines(lines: &mut Vec<Line>, merging: &Merging, spinner: &'static str) {
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
    for check in view.iter().flat_map(|view| &view.checks) {
        lines.push(vec![
            Span::plain("  "),
            progress_mark(&check.progress, spinner),
            Span::plain(" "),
            Span::plain(check.name.clone()).linked(check.url.clone()),
        ]);
    }
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
        Some(_) => vec![
            Span::styled(spinner, Style::Running),
            Span::plain(" Waiting for the checks"),
        ],
        None => vec![
            Span::styled(spinner, Style::Running),
            Span::plain(" Asking GitHub about the pull request"),
        ],
    });
    if let Some(problem) = &merging.problem {
        lines.push(vec![Span::styled(problem.replace('\n', " "), Style::Dim)]);
    }
    lines.push(vec![]);
    lines.push(footer(&[("q", "quit; the pull request stays open")]));
}

fn reviewing_lines(lines: &mut Vec<Line>, reviewing: &Reviewing, columns: usize, rows: usize) {
    let plan = &reviewing.plan;
    let changes = &reviewing.changes;
    lines.push(release_line(&plan.version, Some(plan.date)));
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
    for line in changes.log.lines().skip(reviewing.scroll).take(page) {
        let (hash, subject) = line.split_once(' ').unwrap_or((line, ""));
        lines.push(vec![
            Span::plain("  "),
            Span::styled(hash.to_owned(), Style::Accent),
            Span::plain(format!(" {subject}")),
        ]);
    }
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
        keys.push(("↑↓", scroll.as_str()));
    }
    keys.push(("q", "quit"));
    lines.push(footer(&keys));
}

fn following_lines(lines: &mut Vec<Line>, following: &Following, spinner: &'static str) {
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
    let version = &following.plan.version;
    lines.push(match run.map(|run| &run.progress) {
        Some(Progress::Passed) => vec![Span::styled(
            format!("✓ Released {version}."),
            Style::Passed,
        )],
        Some(Progress::Failed(conclusion)) => vec![Span::styled(
            format!("✗ The release run ended with {conclusion}."),
            Style::Failed,
        )],
        Some(Progress::Skipped) => {
            vec![Span::styled("The release run was skipped.", Style::Failed)]
        }
        _ if following.updates.is_none() => {
            vec![Span::styled("Following the run stopped.", Style::Failed)]
        }
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
