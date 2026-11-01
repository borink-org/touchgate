//! Prepares the release of a Cargo workspace or a Python package from a
//! hand-written changelog. CI publishes the release only after a person
//! approves it with a hardware key, by touch and PIN.
//!
//! See the README for the release process these commands make up.

mod actions;
mod approval;
mod changelog;
mod github;
mod project;
mod python;
mod version;
mod workspace;

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

use changelog::Changelog;
use project::Project;
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
  touchgate verify [--repo <dir>] [--approval <file>] [--branch <ref>]
                   [--repository <owner/name>] [--key <public key>]
                   [--workflow-commit <hash>] [--max-age <15m | 1h | 7d>]
  touchgate publish [<cargo publish arguments>... | <directory of built files>]
  touchgate github-release [--commit <hash>] [<file to attach>...]
  touchgate wait-for-checks [--commit <hash>] [--timeout <30m | 2h>]

In a release run of GitHub Actions, the release commands read the approval
from the run's `approval` input. verify takes --branch and --repository from
the run, --key from TOUCHGATE_KEY and --workflow-commit from
GITHUB_WORKFLOW_SHA.";

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
    let project = Project::load()?;
    let version = project.version()?;
    let text = read(&changelog_path(&project))?;
    let changelog = Changelog::parse(&text)?;
    match changelog.newest_release() {
        Some((newest, _)) if *newest == version => Ok(version),
        Some((newest, _)) => Err(vec![format!(
            "CHANGELOG.md: the newest release is {newest}, but the project is at {version}"
        )]),
        None => Err(vec!["CHANGELOG.md: no released section".to_owned()]),
    }
}

/// Does what [`check`] does, before a release is built. For a Python package,
/// also checks that `uv build` builds it with the copy of `uv_build` in the
/// uv on the `PATH`, as [`PythonPackage::check_build_backend`] describes.
///
/// [`PythonPackage::check_build_backend`]: python::PythonPackage::check_build_backend
fn check_build() -> Result<(), Vec<String>> {
    let version = check()?;
    if let Project::Python { package, .. } = Project::load()? {
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
    let date = date.unwrap_or_else(Date::today);
    let current = check()?;
    let project = Project::load()?;
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
    let checked = check()?;
    assert_eq!(checked, version);
    println!("{version}");
    if pull_request {
        github::pull_request(
            &version,
            preparation,
            &changelog_in_repository(&project)?,
            project.registry(),
        )
        .map_err(|error| vec![error])?;
    }
    Ok(())
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
    let (mut key, mut revision, mut start) = (DEFAULT_KEY.to_owned(), None, true);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match *arg {
            "--key" => key = (*args.next().ok_or_else(|| vec![USAGE.to_owned()])?).to_owned(),
            "--no-start" => start = false,
            flag if flag.starts_with('-') => return Err(vec![USAGE.to_owned()]),
            commit if revision.is_none() => revision = Some(commit),
            _ => return Err(vec![USAGE.to_owned()]),
        }
    }
    if let Some(rest) = key.strip_prefix("~/") {
        let home = std::env::var("HOME").map_err(|_| vec!["HOME is not set".to_owned()])?;
        key = format!("{home}/{rest}");
    }
    if !Path::new(&key).is_file() {
        return Err(vec![format!("{key}: no such key. Pass the key with --key")]);
    }

    let repository = github::Repository::of_checkout().map_err(|error| vec![error])?;
    let default = &repository.default_branch;
    let tracking = format!("refs/remotes/origin/{default}");
    // The release is to run from publish as it is now; a change to it after
    // this stops the release until it is approved again.
    run(Command::new("git").args([
        "fetch",
        "--quiet",
        "--tags",
        "origin",
        &format!("+refs/heads/{default}:{tracking}"),
        "+refs/heads/publish:refs/remotes/origin/publish",
    ]))?;
    let commit = run(Command::new("git").args([
        "rev-parse",
        "--verify",
        "--end-of-options",
        &format!("{}^{{commit}}", revision.unwrap_or(&tracking)),
    ]))?;
    let commit = commit.trim_end();
    let workflow_commit = run(Command::new("git").args([
        "rev-parse",
        "--verify",
        "refs/remotes/origin/publish^{commit}",
    ]))?;
    let workflow_commit = workflow_commit.trim_end();
    let (version, date) = approval::approvable_release(Path::new("."), commit, &tracking)
        .map_err(|error| vec![error])?;
    let time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| vec![error.to_string()])?
        .as_secs() as i64;
    approval::check_release_date(&version, date, time).map_err(|error| vec![error])?;

    show_changes(commit)?;
    eprintln!(
        "Approving release {version} of {} at {commit}, to run from publish at \
         {workflow_commit}.",
        repository.name
    );
    let signed = approval::Approval {
        repository: repository.name,
        commit: commit.to_owned(),
        version,
        workflow_commit: workflow_commit.to_owned(),
        time,
    }
    .sign(Path::new(&key))
    .map_err(|error| vec![error])?;
    if start {
        github::start_release(&signed).map_err(|error| vec![error])
    } else {
        print!("{}", signed.text());
        Ok(())
    }
}

/// Shows on standard error the commits that `commit` adds to the release
/// tagged last before it, so that the approval covers what the person saw.
fn show_changes(commit: &str) -> Result<(), Vec<String>> {
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
            let range = format!("{tag}..{commit}");
            eprintln!("Since {tag}:");
            eprint!(
                "{}",
                run(Command::new("git").args(["log", "--oneline", "--no-decorate", &range]))?
            );
            eprint!(
                "{}",
                run(Command::new("git").args(["diff", "--shortstat", &tag, commit]))?
            );
        }
        None => {
            eprintln!("No release is tagged before {commit}. Its last commits:");
            eprint!(
                "{}",
                run(Command::new("git").args([
                    "log",
                    "--oneline",
                    "--no-decorate",
                    "-20",
                    commit
                ]))?
            );
        }
    }
    Ok(())
}

/// Publishes what the project releases to its registry, skipping what is
/// there already, so that a release that failed halfway can run again.
///
/// To crates.io, `args` go to `cargo publish`. To PyPI, `args` is the
/// directory of the built files, `dist` unless it names another.
fn publish(args: &[&str]) -> Result<(), Vec<String>> {
    let project = Project::load()?;
    project.version()?;
    match &project {
        Project::Crates(workspace) => publish_crates(workspace, args),
        Project::Python { package, .. } => {
            let dist = match args {
                [] => "dist",
                [dist] => dist,
                _ => return Err(vec![USAGE.to_owned()]),
            };
            package.publish(Path::new(dist))
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
/// GitHub names.
fn changelog_in_repository(project: &Project) -> Result<String, Vec<String>> {
    let top =
        PathBuf::from(run(Command::new("git").args(["rev-parse", "--show-toplevel"]))?.trim_end());
    let path = changelog_path(project);
    let relative = path.strip_prefix(&top).unwrap_or(&path);
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
