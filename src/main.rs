//! Prepares the release of a Cargo workspace or a Python package from a
//! hand-written changelog. CI publishes the release only after a person
//! approves it with a hardware key, by touch and PIN.
//!
//! See the README for the release process these commands make up.

mod actions;
mod approval;
mod changelog;
mod github;
#[cfg(feature = "tui")]
mod interactive;
mod project;
mod python;
#[cfg(feature = "tui")]
mod terminal;
mod version;
#[cfg(feature = "tui")]
mod watch;
mod workspace;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use changelog::Changelog;
use project::{Project, Registry};
use version::{Date, Version};
use workspace::Workspace;

/// How long an approval lasts, unless `--max-age` gives another time.
/// `touchgate approve` starts the release right after signing, and `verify`
/// runs first, so the approval only has to outlast the queue for a runner.
const DEFAULT_MAX_AGE: &str = "15m";

/// The name the approval is attached to the GitHub release under.
const APPROVAL_ASSET: &str = "touchgate-approval.txt";

const USAGE: &str = "\
usage:
  touchgate check [--build]
  touchgate prepare <version> [--date YYYY-MM-DD] [--pull-request]
  touchgate check-entry <base revision>
  touchgate notes [<version>]
  touchgate approve [<commit>] [--key <private key>] [--no-start]
  touchgate [--key <private key>]
  touchgate verify [--repo <dir>] [--approval <file>] [--branch <ref>]
                   [--repository <owner/name>] [--key <public key>]
                   [--workflow-commit <hash>] [--max-age <15m | 1h | 7d>]
  touchgate publish [--to crates.io] [<cargo publish arguments>...]
  touchgate publish [--to pypi] [<directory of built files>]
  touchgate github-release [--commit <hash>] [<file to attach>...]
  touchgate wait-for-checks [--commit <hash>] [--timeout <30m | 2h>]

In a release run of GitHub Actions, the release commands read the approval
from the run's `approval` input. verify takes --branch and --repository from
the run, --key from TOUCHGATE_KEY and --workflow-commit from
GITHUB_WORKFLOW_SHA.

touchgate without a command prepares, approves and follows a release in the
terminal. It is experimental, and only in a build with `--features tui`.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["check"] => check().map(|version| println!("{version}")),
        ["check", "--build"] => check_build(),
        ["prepare", version, rest @ ..] => prepare(version, rest),
        ["check-entry", base] => check_entry(base),
        ["notes"] => notes(None),
        ["notes", version] => notes(Some(version)),
        ["verify", rest @ ..] => verify(rest),
        ["publish", rest @ ..] => publish(rest),
        ["approve", rest @ ..] => approve(rest),
        #[cfg(feature = "tui")]
        [] => interactive::release(None),
        #[cfg(feature = "tui")]
        ["--key", key] => interactive::release(Some(key)),
        ["github-release", "--commit", commit, files @ ..] => github_release(commit, files),
        ["github-release", files @ ..] => github_release_of_run(files),
        ["wait-for-checks", rest @ ..] => wait_for_checks(rest),
        _ => Err(vec![USAGE.to_owned()]),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(errors) => {
            for error in errors {
                eprintln!("{error}");
            }
            ExitCode::FAILURE
        }
    }
}

/// Checks the version against the changelog, and returns the version.
///
/// The changelog must have the shape that [`Changelog::parse`] describes.
/// Everything the project releases must be at one version, and the newest
/// released section of the changelog must be that version.
fn check() -> Result<Version, Vec<String>> {
    check_project(&Project::load()?)
}

/// Does what [`check`] does for `project`.
fn check_project(project: &Project) -> Result<Version, Vec<String>> {
    let version = project.version()?;
    let text = read(&changelog_path(project))?;
    let changelog = Changelog::parse(&text)?;
    match changelog.newest_release() {
        Some((newest, _)) if *newest == version => Ok(version),
        Some((newest, _)) => Err(vec![format!(
            "CHANGELOG.md: the newest release is {newest}, but the project is at {version}"
        )]),
        None => Err(vec!["CHANGELOG.md: no released section".to_owned()]),
    }
}

/// Does what [`check`] does, before a release is built. For each Python
/// package, also checks that `uv build` builds it with the copy of `uv_build`
/// in the uv on the `PATH`, as [`PythonPackage::check_build_backend`]
/// describes.
///
/// [`PythonPackage::check_build_backend`]: python::PythonPackage::check_build_backend
fn check_build() -> Result<(), Vec<String>> {
    let version = check()?;
    for package in Project::load()?.python_packages() {
        println!("{}", package.check_build_backend()?);
    }
    println!("{version}");
    Ok(())
}

/// Reads the release run that the command runs in.
fn release_run() -> Result<actions::ReleaseRun, Vec<String>> {
    actions::ReleaseRun::load().map_err(|error| vec![error])
}

/// Turns `## Unreleased` into the section of `version`, dated `--date` or
/// today, and sets `version` on what the project releases. With
/// `--pull-request`, also commits that on `release/<version>`, pushes it and
/// opens a pull request.
///
/// If the changelog dates `version` already and no tag names its release,
/// prepares it again after a release that failed: moves any entries of
/// `## Unreleased` into its section, dated `--date` or today.
fn prepare(version: &str, args: &[&str]) -> Result<(), Vec<String>> {
    let version = Version::parse(version).map_err(|error| vec![error])?;
    let (mut date, mut pull_request) = (None, false);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match *flag {
            "--date" => {
                let text = args.next().ok_or_else(|| vec![USAGE.to_owned()])?;
                date = Some(Date::parse(text).map_err(|error| vec![error])?);
            }
            "--pull-request" => pull_request = true,
            _ => return Err(vec![USAGE.to_owned()]),
        }
    }
    let root = project::repository_root()?;
    let prepared = prepare_release(&root, version, date.unwrap_or_else(Date::today))?;
    println!("{}", prepared.version);
    if pull_request {
        let project = Project::load()?;
        let outcome = github::pull_request(
            &root,
            &prepared.version,
            prepared.preparation,
            &changelog_in_repository(&project)?,
            &project.registries(),
            github::CommitAuthor::ActionsBot,
        )
        .map_err(|error| vec![error])?;
        match outcome {
            github::PullRequestOutcome::NoChange => println!(
                "{} needs no change: approve the default branch with `touchgate approve`",
                prepared.version
            ),
            github::PullRequestOutcome::Opened(url) => println!("{url}"),
            github::PullRequestOutcome::Updated(_) => {}
        }
    }
    Ok(())
}

/// A version that [`prepare_release`] prepared, and how.
struct PreparedRelease {
    version: Version,
    preparation: github::Preparation,
}

/// Does what [`prepare`] does to the checkout whose top directory is
/// `root`, without committing it.
fn prepare_release(
    root: &Path,
    version: Version,
    date: Date,
) -> Result<PreparedRelease, Vec<String>> {
    let project = Project::load_at(root)?;
    let current = check_project(&project)?;
    let path = changelog_path(&project);
    let text = read(&path)?;
    let write = |text: String| {
        std::fs::write(&path, text).map_err(|error| vec![format!("{}: {error}", path.display())])
    };
    let preparation = if current == version {
        if github::is_released(&version).map_err(|error| vec![error])? {
            return Err(vec![format!(
                "{version} is released already. Prepare a newer version"
            )]);
        }
        if let Some(again) = Changelog::parse(&text)?
            .release_again(&version, date)
            .map_err(|error| vec![error])?
        {
            write(again)?;
        }
        github::Preparation::Again
    } else {
        write(
            Changelog::parse(&text)?
                .release(&version, date)
                .map_err(|error| vec![error])?,
        )?;
        project.set_version(&version)?;
        github::Preparation::New
    };
    let checked = check_project(&Project::load_at(root)?)?;
    assert_eq!(checked, version);
    Ok(PreparedRelease {
        version,
        preparation,
    })
}

/// Tags the release at `commit`, the checkout, and makes its GitHub release
/// with its section of the changelog as the notes, and `files` attached.
fn github_release(commit: &str, files: &[&str]) -> Result<(), Vec<String>> {
    let version = check()?;
    let head = run(Command::new("git").args(["rev-parse", "--verify", "HEAD^{commit}"]))?;
    if head.trim_end() != commit {
        return Err(vec![format!(
            "the checkout is {}, not the release {commit}",
            head.trim_end()
        )]);
    }
    let changelog = changelog_in_repository(&Project::load()?)?;
    github::release(commit, &version, &changelog, files).map_err(|error| vec![error])
}

/// Does what [`github_release`] does for the commit of the release run, and
/// attaches the run's approval too, so that the release keeps it.
fn github_release_of_run(files: &[&str]) -> Result<(), Vec<String>> {
    let run = release_run()?;
    let dir = approval::private_temp_dir().map_err(|error| vec![error])?;
    let path = dir.join(APPROVAL_ASSET);
    let result = std::fs::write(&path, run.approval.text())
        .map_err(|error| vec![format!("{}: {error}", path.display())])
        .and_then(|()| {
            let path = path
                .to_str()
                .ok_or_else(|| vec![format!("{} is not a UTF-8 path", path.display())])?;
            let mut files = files.to_vec();
            files.push(path);
            github_release(run.commit(), &files)
        });
    let _ = std::fs::remove_dir_all(&dir);
    result
}

/// Fails if the changes since `base` touch what the project releases but add
/// nothing under `## Unreleased`.
fn check_entry(base: &str) -> Result<(), Vec<String>> {
    let project = Project::load()?;
    let top =
        PathBuf::from(run(Command::new("git").args(["rev-parse", "--show-toplevel"]))?.trim_end());
    let changed = run(Command::new("git").args([
        "diff",
        "--name-only",
        "--end-of-options",
        &format!("{base}...HEAD"),
    ]))?;
    let touched: Vec<&str> = project
        .released_dirs()
        .into_iter()
        .filter(|(_, dir)| changed.lines().any(|path| top.join(path).starts_with(dir)))
        .map(|(name, _)| name)
        .collect();
    if touched.is_empty() {
        println!("nothing released changed");
        return Ok(());
    }

    let path = changelog_path(&project);
    let relative = path.strip_prefix(&top).unwrap_or(&path);
    let relative = relative
        .to_str()
        .ok_or_else(|| vec![format!("{} is not a UTF-8 path", relative.display())])?;
    let at_base = format!("{base}:{relative}");
    // A base from before the changelog existed has no entries.
    let exists = Command::new("git")
        .args(["cat-file", "-e", &at_base])
        // A missing file is an answer here, not an error to show.
        .stderr(Stdio::null())
        .status()
        .map_err(|error| vec![format!("git: {error}")])?
        .success();
    let before = if exists {
        run(Command::new("git").args(["show", &at_base]))?
    } else {
        String::new()
    };
    let after = read(&path)?;
    // Preparing a release moves the entries under `## Unreleased` into the
    // new version's section, and adds none.
    if let Some(version) = prepared_version(&before, &after) {
        println!("this prepares the release of {version}");
        return Ok(());
    }
    let before = changelog::unreleased_entries(&before);
    if changelog::unreleased_entries(&after)
        .iter()
        .any(|line| !before.contains(line))
    {
        Ok(())
    } else {
        Err(vec![format!(
            "CHANGELOG.md: this changes {} but adds nothing under `## Unreleased`",
            touched.join(", ")
        )])
    }
}

/// Returns the version that a change from the changelog `before` to `after`
/// prepares, if it does. Preparing a version moves the entries under
/// `## Unreleased` into its section, and adds none there. The version is new,
/// or one prepared before whose release has not published, as
/// `touchgate prepare` prepares it again: with entries moved, or dated anew.
fn prepared_version(before: &str, after: &str) -> Option<Version> {
    let changelog = Changelog::parse(after).ok()?;
    let (version, date) = changelog.newest_release()?;
    let newest_before = Changelog::parse(before).ok().and_then(|changelog| {
        changelog
            .newest_release()
            .map(|(version, date)| (version.clone(), date))
    });
    let entries = changelog::unreleased_entries(before);
    let notes = changelog.notes(version).unwrap_or_default();
    let moved = changelog::unreleased_entries(after).is_empty()
        && entries
            .iter()
            .all(|entry| notes.lines().any(|line| line == *entry));
    match newest_before {
        Some((newest_before, _)) if newest_before > *version => None,
        Some((newest_before, date_before)) if newest_before == *version => {
            (moved && (!entries.is_empty() || date > date_before)).then(|| version.clone())
        }
        _ => Some(version.clone()),
    }
}

/// Prints the section of `version`, or of the current version, for the notes
/// of a release.
fn notes(version: Option<&str>) -> Result<(), Vec<String>> {
    let project = Project::load()?;
    let version = match version {
        Some(version) => Version::parse(version).map_err(|error| vec![error])?,
        None => project.version()?,
    };
    let text = read(&changelog_path(&project))?;
    let notes = Changelog::parse(&text)?
        .notes(&version)
        .ok_or_else(|| vec![format!("CHANGELOG.md: no section for {version}")])?;
    print!("{notes}");
    Ok(())
}

/// Checks a release's approval with [`approval::verify`], and prints the
/// commit it releases. In GitHub Actions, also sets the step outputs
/// `commit` and `version`, which the later jobs check out and release.
/// Reads no workspace, since the tree is not trusted yet.
fn verify(args: &[&str]) -> Result<(), Vec<String>> {
    let (mut repo, mut approval_file, mut branch, mut repository) = (None, None, None, None);
    let (mut key, mut workflow, mut max_age) = (None, None, None);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let slot = match *flag {
            "--repo" => &mut repo,
            "--approval" => &mut approval_file,
            "--branch" => &mut branch,
            "--repository" => &mut repository,
            "--key" => &mut key,
            "--workflow-commit" => &mut workflow,
            "--max-age" => &mut max_age,
            _ => return Err(vec![USAGE.to_owned()]),
        };
        *slot = Some(*args.next().ok_or_else(|| vec![USAGE.to_owned()])?);
    }
    // What no flag gives comes from the release run, read only if needed.
    let (run_approval, run_branch, run_repository) =
        if approval_file.is_none() || branch.is_none() || repository.is_none() {
            let run = release_run()?;
            (Some(run.approval), Some(run.branch), Some(run.repository))
        } else {
            (None, None, None)
        };
    let signed = match approval_file {
        Some(path) => approval::SignedApproval::parse(&read(Path::new(path))?)
            .map_err(|error| vec![format!("{path}: {error}")])?,
        None => run_approval.ok_or_else(|| vec![USAGE.to_owned()])?,
    };
    let branch = branch
        .map(str::to_owned)
        .or(run_branch)
        .ok_or_else(|| vec![USAGE.to_owned()])?;
    let repository = repository
        .map(str::to_owned)
        .or(run_repository)
        .ok_or_else(|| vec![USAGE.to_owned()])?;
    let key = match key {
        Some(key) => key.to_owned(),
        None => actions::approval_key().map_err(|error| vec![error])?,
    };
    let workflow = match workflow {
        Some(workflow) => workflow.to_owned(),
        None => actions::workflow_commit().map_err(|error| vec![error])?,
    };
    if !approval::is_hash(&workflow) {
        return Err(vec![format!("`{workflow}` is not a full commit hash")]);
    }
    let max_age_seconds =
        approval::parse_age(max_age.unwrap_or(DEFAULT_MAX_AGE)).map_err(|error| vec![error])?;
    let expected = approval::Expected {
        key: &key,
        repository: &repository,
        branch: &branch,
        workflow_commit: &workflow,
        max_age_seconds,
    };
    approval::verify(Path::new(repo.unwrap_or(".")), &signed, &expected)
        .map_err(|error| vec![error])?;
    let approved = &signed.approval;
    println!("{}", approved.commit);
    actions::set_step_outputs(&[
        ("commit", &approved.commit),
        ("version", &approved.version.to_string()),
    ])
    .map_err(|error| vec![error])
}

/// How long `wait-for-checks` waits unless `--timeout` says otherwise.
const DEFAULT_CHECKS_TIMEOUT: &str = "2h";

/// Waits for every check on `--commit`, or on the release run's commit, as
/// [`github::wait_for_checks`] describes.
fn wait_for_checks(args: &[&str]) -> Result<(), Vec<String>> {
    let (mut commit, mut timeout) = (None, None);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let slot = match *flag {
            "--commit" => &mut commit,
            "--timeout" => &mut timeout,
            _ => return Err(vec![USAGE.to_owned()]),
        };
        *slot = Some(*args.next().ok_or_else(|| vec![USAGE.to_owned()])?);
    }
    let commit = match commit {
        Some(commit) => commit.to_owned(),
        None => release_run()?.commit().to_owned(),
    };
    let timeout = approval::parse_age(timeout.unwrap_or(DEFAULT_CHECKS_TIMEOUT))
        .map_err(|error| vec![error])?;
    github::wait_for_checks(&commit, timeout).map_err(|error| vec![error])
}

/// The release key unless `--key` names another: the handle `ssh-keygen`
/// writes for a key on a security key, beside its `.pub`.
const DEFAULT_KEY: &str = "~/.ssh/id_ed25519_sk_signing";

/// Signs the approval of a commit on the default branch, `origin/<default>`
/// unless the arguments name another, and starts `release.yml` with it.
/// With `--no-start`, prints the approval instead, alone on standard output.
///
/// Nothing from the commit runs: the version comes from its changelog, read
/// with git, since running `cargo` would run code not yet approved.
fn approve(args: &[&str]) -> Result<(), Vec<String>> {
    let (mut key, mut revision, mut start) = (DEFAULT_KEY, None, true);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match *arg {
            "--key" => key = *args.next().ok_or_else(|| vec![USAGE.to_owned()])?,
            "--no-start" => start = false,
            flag if flag.starts_with('-') => return Err(vec![USAGE.to_owned()]),
            commit if revision.is_none() => revision = Some(commit),
            _ => return Err(vec![USAGE.to_owned()]),
        }
    }
    let key = signing_key(key)?;
    let repository = github::Repository::of_checkout().map_err(|error| vec![error])?;
    fetch_release_refs(&repository)?;
    let url = repository.url.clone();
    let plan = ReleasePlan::of(repository, revision)?;
    let approval = plan.approval_at(unix_time()?)?;

    show_changes(&Changes::since_release(&approval.commit)?, &approval.commit);
    eprintln!(
        "Approving release {} of {} at {}, to run from publish at {}.",
        approval.version, approval.repository, approval.commit, approval.workflow_commit
    );
    let signed = approval.sign(&key).map_err(|error| vec![error])?;
    if start {
        github::start_release(&signed).map_err(|error| vec![error])?;
        println!(
            "Started release.yml on publish. Follow it with `gh run watch`, or at {}/actions",
            url
        );
        Ok(())
    } else {
        print!("{}", signed.text());
        Ok(())
    }
}

/// Returns the path of the signing key `key`, with a leading `~/` read as the
/// home directory, and fails if there is no such file.
fn signing_key(key: &str) -> Result<PathBuf, Vec<String>> {
    let path = match key.strip_prefix("~/") {
        Some(rest) => {
            let home = std::env::var("HOME").map_err(|_| vec!["HOME is not set".to_owned()])?;
            PathBuf::from(format!("{home}/{rest}"))
        }
        None => PathBuf::from(key),
    };
    if path.is_file() {
        Ok(path)
    } else {
        Err(vec![format!(
            "{}: no such key. Pass the key with --key",
            path.display()
        )])
    }
}

/// What an approval would release, before it is signed: everything an
/// [`approval::Approval`] names but the time.
#[derive(Clone)]
struct ReleasePlan {
    /// The repository as `owner/name`.
    repository: String,
    /// The full hash of the commit to release, on the default branch.
    commit: String,
    version: Version,
    /// The day the changelog dates the release.
    date: Date,
    /// The commit of the `publish` branch the release is to run from.
    workflow_commit: String,
}

impl ReleasePlan {
    /// Reads the release of `revision` in `repository`, the tip of the
    /// default branch unless it names another commit on it, as
    /// [`fetch_release_refs`] fetched it.
    fn of(repository: github::Repository, revision: Option<&str>) -> Result<Self, Vec<String>> {
        let tracking = tracking_ref(&repository);
        let commit = run(Command::new("git").args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{}^{{commit}}", revision.unwrap_or(&tracking)),
        ]))?;
        let commit = commit.trim_end().to_owned();
        let workflow_commit = run(Command::new("git").args([
            "rev-parse",
            "--verify",
            "refs/remotes/origin/publish^{commit}",
        ]))?;
        let (version, date) = approval::approvable_release(Path::new("."), &commit, &tracking)
            .map_err(|error| vec![error])?;
        Ok(Self {
            repository: repository.name,
            commit,
            version,
            date,
            workflow_commit: workflow_commit.trim_end().to_owned(),
        })
    }

    /// Returns the approval of the release signed at `time`, in seconds since
    /// the Unix epoch. Fails if the changelog dates the release another day
    /// than `time` or the day before.
    fn approval_at(&self, time: i64) -> Result<approval::Approval, Vec<String>> {
        approval::check_release_date(&self.version, self.date, time)
            .map_err(|error| vec![error])?;
        Ok(approval::Approval {
            repository: self.repository.clone(),
            commit: self.commit.clone(),
            version: self.version.clone(),
            workflow_commit: self.workflow_commit.clone(),
            time,
        })
    }
}

/// Fetches the default branch of `repository`, `publish` and the tags from
/// origin.
fn fetch_release_refs(repository: &github::Repository) -> Result<(), Vec<String>> {
    // The release is to run from publish as it is now; a change to it after
    // this stops the release until it is approved again.
    run(Command::new("git").args([
        "fetch",
        "--quiet",
        "--tags",
        "origin",
        &format!(
            "+refs/heads/{}:{}",
            repository.default_branch,
            tracking_ref(repository)
        ),
        "+refs/heads/publish:refs/remotes/origin/publish",
    ]))?;
    Ok(())
}

/// Returns the ref that tracks the default branch of `repository` on origin.
fn tracking_ref(repository: &github::Repository) -> String {
    format!("refs/remotes/origin/{}", repository.default_branch)
}

/// Returns the time in seconds since the Unix epoch.
fn unix_time() -> Result<i64, Vec<String>> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| vec![error.to_string()])?
        .as_secs() as i64)
}

/// The commits that a commit adds to the release tagged last before it, as
/// git prints them.
struct Changes {
    /// The tag of that release, if one is tagged before the commit.
    previous: Option<String>,
    /// The commits since that release, or the last 20 if there is none,
    /// newest first.
    commits: Vec<ChangedCommit>,
    /// `git diff --shortstat` since that release, or nothing if there is none.
    shortstat: String,
}

impl Changes {
    /// Reads the changes that `commit` adds to the release tagged before it.
    fn since_release(commit: &str) -> Result<Self, Vec<String>> {
        let previous = Command::new("git")
            .args(["describe", "--tags", "--abbrev=0", "--match", "v*"])
            .arg(format!("{commit}^"))
            .stderr(Stdio::null())
            .output()
            .map_err(|error| vec![format!("git: {error}")])?;
        let previous = String::from_utf8(previous.stdout)
            .ok()
            .filter(|_| previous.status.success())
            .map(|tag| tag.trim_end().to_owned());
        match previous {
            Some(tag) => {
                let commits = ChangedCommit::log(&[&format!("{tag}..{commit}")])?;
                let shortstat =
                    run(Command::new("git").args(["diff", "--shortstat", &tag, commit]))?;
                Ok(Self {
                    previous: Some(tag),
                    commits,
                    shortstat,
                })
            }
            None => Ok(Self {
                previous: None,
                commits: ChangedCommit::log(&["-20", commit])?,
                shortstat: String::new(),
            }),
        }
    }
}

/// A commit in [`Changes`].
#[cfg_attr(
    not(feature = "tui"),
    expect(
        dead_code,
        reason = "only the interactive touchgate shows the age and links the hash"
    )
)]
struct ChangedCommit {
    /// The full hash.
    hash: String,
    /// The hash as `git log --oneline` abbreviates it.
    short: String,
    /// How long ago it was committed, as git says it, such as `3 hours ago`.
    age: String,
    /// The subject line of its message.
    subject: String,
}

/// The separator of the fields that [`ChangedCommit::log`] asks git for.
const FIELD_SEPARATOR: char = '\u{1f}';

impl ChangedCommit {
    /// Returns the commits that `git log` lists with `args`, newest first.
    fn log(args: &[&str]) -> Result<Vec<Self>, Vec<String>> {
        let text = run(Command::new("git")
            .args(["log", "--no-decorate", "--format=%H%x1f%h%x1f%cr%x1f%s"])
            .args(args))?;
        Ok(text.lines().filter_map(Self::parse).collect())
    }

    /// Reads a line as [`ChangedCommit::log`] asks git to write it.
    fn parse(line: &str) -> Option<Self> {
        let mut fields = line.splitn(4, FIELD_SEPARATOR);
        let mut field = || fields.next().map(str::to_owned);
        Some(Self {
            hash: field()?,
            short: field()?,
            age: field()?,
            subject: field()?,
        })
    }
}

/// Shows the `changes` that `commit` adds on standard error, so that the
/// approval covers what the person saw.
fn show_changes(changes: &Changes, commit: &str) {
    match &changes.previous {
        Some(tag) => eprintln!("Since {tag}:"),
        None => eprintln!("No release is tagged before {commit}. Its last commits:"),
    }
    for commit in &changes.commits {
        eprintln!("{} {}", commit.short, commit.subject);
    }
    eprint!("{}", changes.shortstat);
}

/// Publishes what the project releases to a registry, skipping what is there
/// already, so that a release that failed halfway can run again. A project
/// that publishes to both crates.io and PyPI publishes to each in its own
/// job, so `--to` must name the registry; otherwise it may.
///
/// To crates.io, the other `args` go to `cargo publish`. To PyPI, they are
/// the directory of the built files, `dist` unless they name another.
fn publish(args: &[&str]) -> Result<(), Vec<String>> {
    let project = Project::load()?;
    project.version()?;
    let registries = project.registries();
    let (registry, args) = match (args, registries.as_slice()) {
        (["--to", name, rest @ ..], _) => {
            (Registry::parse(name).map_err(|error| vec![error])?, rest)
        }
        (_, [registry]) => (*registry, args),
        _ => {
            return Err(vec![
                "the project publishes to crates.io and PyPI. Name one with \
                 `--to crates.io` or `--to pypi`"
                    .to_owned(),
            ]);
        }
    };
    if !registries.contains(&registry) {
        return Err(vec![format!(
            "the project publishes nothing to {}",
            registry.name()
        )]);
    }
    match registry {
        Registry::CratesIo => {
            let workspace = project
                .published_crates()
                .expect("a project that publishes to crates.io has crates");
            publish_crates(workspace, args)
        }
        Registry::PyPi => {
            let dist = match args {
                [] => "dist",
                [dist] => dist,
                _ => return Err(vec![USAGE.to_owned()]),
            };
            python::publish_packages(project.python_packages(), Path::new(dist))
        }
    }
}

/// Publishes every published package of `workspace` whose version is not on
/// crates.io yet.
fn publish_crates(workspace: &Workspace, cargo_args: &[&str]) -> Result<(), Vec<String>> {
    let mut existing = Vec::new();
    for package in workspace.published() {
        let url = format!(
            "https://crates.io/api/v1/crates/{}/{}",
            package.name, package.version
        );
        let status = run(Command::new("curl").args([
            "--silent",
            "--show-error",
            "--output",
            "/dev/null",
            "--write-out",
            "%{http_code}",
            "--user-agent",
            "touchgate (https://github.com/borink-org/touchgate)",
            &url,
        ]))?;
        match status.as_str() {
            "200" => {
                println!(
                    "{} {} is on crates.io already",
                    package.name, package.version
                );
                existing.push(package.name.as_str());
            }
            "404" => {}
            other => {
                return Err(vec![format!("crates.io answered {other} for {url}")]);
            }
        }
    }
    if existing.len() == workspace.published().count() {
        println!("nothing left to publish");
        return Ok(());
    }
    let mut command = Command::new("cargo");
    command.args(["publish", "--workspace"]);
    for name in existing {
        command.args(["--exclude", name]);
    }
    command.args(cargo_args);
    let status = command
        .status()
        .map_err(|error| vec![format!("cargo publish: {error}")])?;
    if status.success() {
        Ok(())
    } else {
        Err(vec![format!("cargo publish failed: {status}")])
    }
}

/// Returns the path of the changelog in the git repository, which a link on
/// GitHub names. The project's root is the top directory of its checkout.
fn changelog_in_repository(project: &Project) -> Result<String, Vec<String>> {
    let path = changelog_path(project);
    let relative = path.strip_prefix(project.root()).unwrap_or(&path);
    relative
        .to_str()
        .map(str::to_owned)
        .ok_or_else(|| vec![format!("{} is not a UTF-8 path", relative.display())])
}

/// Returns the path of the changelog, at the root of the project.
fn changelog_path(project: &Project) -> PathBuf {
    project.root().join("CHANGELOG.md")
}

/// Runs `command` and returns its standard output, or its standard error as
/// the error.
fn run(command: &mut Command) -> Result<String, Vec<String>> {
    let name = command
        .get_program()
        .to_str()
        .unwrap_or("a command")
        .to_owned();
    let output = command
        .output()
        .map_err(|error| vec![format!("{name}: {error}")])?;
    if output.status.success() {
        utf8(output.stdout, &name).map_err(|error| vec![error])
    } else {
        let stderr = utf8(output.stderr, &name).map_err(|error| vec![error])?;
        Err(vec![format!("{name} failed: {}", stderr.trim())])
    }
}

/// Returns the output of the program `name`, which must be UTF-8.
fn utf8(bytes: Vec<u8>, name: &str) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| format!("{name} wrote output that is not UTF-8"))
}

fn read(path: &Path) -> Result<String, Vec<String>> {
    std::fs::read_to_string(path).map_err(|error| vec![format!("{}: {error}", path.display())])
}

#[cfg(test)]
mod tests {
    use super::*;

    const RELEASED: &str = "# Changelog\n\n## 0.1.0 - 2026-11-01\n\n- First.\n";

    #[test]
    fn knows_a_change_that_prepares_a_release() {
        let unreleased =
            "# Changelog\n\n## Unreleased\n\n- Second.\n\n## 0.1.0 - 2026-11-01\n\n- First.\n";
        let new = "# Changelog\n\n## 0.2.0 - 2026-11-02\n\n- Second.\n\n## 0.1.0 - 2026-11-01\n\n- First.\n";
        let again = "# Changelog\n\n## 0.1.0 - 2026-11-02\n\n- First.\n- Second.\n";
        let version = |text: &str| Some(Version::parse(text).unwrap());
        assert_eq!(prepared_version(unreleased, new), version("0.2.0"));
        assert_eq!(prepared_version("", RELEASED), version("0.1.0"));
        assert_eq!(prepared_version(unreleased, again), version("0.1.0"));
        let redated = RELEASED.replace("2026-11-01", "2026-11-03");
        assert_eq!(prepared_version(RELEASED, &redated), version("0.1.0"));
        assert_eq!(prepared_version(&redated, RELEASED), None);
        // Adding an entry, or dropping one without moving it, prepares nothing.
        assert_eq!(prepared_version(RELEASED, unreleased), None);
        assert_eq!(prepared_version(unreleased, RELEASED), None);
        assert_eq!(prepared_version(new, RELEASED), None);
    }
}
