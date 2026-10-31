//! Prepares the release of a Cargo workspace or a Python package from a
//! hand-written changelog, and lets CI publish it only after a person approved
//! it with a hardware key, by touch and PIN.
//!
//! See the README for the release process these commands make up.

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

/// How long an approval lasts unless `--max-age` says otherwise: long enough
/// to merge once the pull request's checks pass, and short enough that an
/// approval left unmerged is soon worthless.
const DEFAULT_MAX_AGE: &str = "1h";

const USAGE: &str = "\
usage:
  touchgate check
  touchgate check-backend
  touchgate prepare <version> [--date YYYY-MM-DD] [--pull-request]
  touchgate check-entry <base revision>
  touchgate notes [<version>]
  touchgate verify --repo <dir> --commit <hash> --branch <ref> --key <public key>
                   --workflow-commit <hash>
                   [--max-age <30m | 1h | 7d>]
  touchgate publish [<cargo publish arguments>... | <directory of built files>]
  touchgate approve [--key <private key>] [--no-push]
  touchgate start --commit <hash>
  touchgate github-release --commit <hash> [<file to attach>...]
  touchgate wait-for-checks --commit <hash> [--timeout <30m | 2h>]";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let result = match args.as_slice() {
        ["check"] => check().map(|version| println!("{version}")),
        ["check-backend"] => check_backend(),
        ["prepare", version, rest @ ..] => prepare(version, rest),
        ["check-entry", base] => check_entry(base),
        ["notes"] => notes(None),
        ["notes", version] => notes(Some(version)),
        ["verify", rest @ ..] => verify(rest),
        ["publish", rest @ ..] => publish(rest),
        ["approve", rest @ ..] => approve(rest),
        ["start", "--commit", commit] => github::start(commit).map_err(|error| vec![error]),
        ["github-release", "--commit", commit, files @ ..] => github_release(commit, files),
        ["wait-for-checks", "--commit", commit] => wait_for_checks(commit, DEFAULT_CHECKS_TIMEOUT),
        ["wait-for-checks", "--commit", commit, "--timeout", timeout] => {
            wait_for_checks(commit, timeout)
        }
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

/// Checks that the changelog has the shape [`Changelog::parse`] describes,
/// that what the project releases is at one version, and that the newest
/// released section of the changelog is that version. Returns the version.
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

/// Checks that `uv build` builds a Python package with the backend built into
/// uv, if it builds with `uv_build`, as [`PythonPackage::check_build_backend`]
/// describes.
///
/// [`PythonPackage::check_build_backend`]: python::PythonPackage::check_build_backend
fn check_backend() -> Result<(), Vec<String>> {
    match Project::load()? {
        Project::Crates(_) => Err(vec![
            "check-backend checks a Python package, and touchgate.json releases to crates.io"
                .to_owned(),
        ]),
        Project::Python { package, .. } => {
            println!("{}", package.check_build_backend()?);
            Ok(())
        }
    }
}

/// Turns `## Unreleased` into the section of `version`, dated `--date` or
/// today, and sets `version` on what the project releases. With
/// `--pull-request`, also commits that on `release/<version>`, pushes it and
/// opens a pull request.
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
    check()?;
    let project = Project::load()?;
    let path = changelog_path(&project);
    let text = read(&path)?;
    let released = Changelog::parse(&text)?
        .release(&version, date)
        .map_err(|error| vec![error])?;
    std::fs::write(&path, released)
        .map_err(|error| vec![format!("{}: {error}", path.display())])?;
    project.set_version(&version)?;
    let checked = check()?;
    assert_eq!(checked, version);
    println!("{version}");
    if pull_request {
        github::pull_request(
            &version,
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
/// approval commit. Reads no workspace, since the tree is not trusted yet.
fn verify(args: &[&str]) -> Result<(), Vec<String>> {
    let (mut repo, mut commit, mut branch, mut key, mut workflow, mut max_age) =
        (None, None, None, None, None, None);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let slot = match *flag {
            "--repo" => &mut repo,
            "--commit" => &mut commit,
            "--branch" => &mut branch,
            "--key" => &mut key,
            "--workflow-commit" => &mut workflow,
            "--max-age" => &mut max_age,
            _ => return Err(vec![USAGE.to_owned()]),
        };
        *slot = Some(*args.next().ok_or_else(|| vec![USAGE.to_owned()])?);
    }
    let (Some(repo), Some(commit), Some(branch), Some(key), Some(workflow)) =
        (repo, commit, branch, key, workflow)
    else {
        return Err(vec![USAGE.to_owned()]);
    };
    if !approval::is_hash(workflow) {
        return Err(vec![format!("`{workflow}` is not a full commit hash")]);
    }
    let max_age =
        approval::parse_age(max_age.unwrap_or(DEFAULT_MAX_AGE)).map_err(|error| vec![error])?;
    let approval = approval::verify(Path::new(repo), commit, key, branch, workflow, max_age)
        .map_err(|error| vec![error])?;
    println!("{approval}");
    Ok(())
}

/// How long `wait-for-checks` waits unless `--timeout` says otherwise.
const DEFAULT_CHECKS_TIMEOUT: &str = "2h";

/// Waits for every check on `commit` to pass, as [`github::wait_for_checks`]
/// describes.
fn wait_for_checks(commit: &str, timeout: &str) -> Result<(), Vec<String>> {
    let timeout = approval::parse_age(timeout).map_err(|error| vec![error])?;
    github::wait_for_checks(commit, timeout).map_err(|error| vec![error])
}

/// The release key unless `--key` names another: the handle `ssh-keygen`
/// writes for a key on a security key, beside its `.pub`.
const DEFAULT_KEY: &str = "~/.ssh/id_ed25519_sk_signing";

/// Signs the approval of the release branch checked out, and pushes it.
///
/// The key is given to this one `git commit`, so git's own signing settings
/// stay as they are. Nothing from the branch runs: the version comes from the
/// branch name, since running `cargo` would run code not yet approved.
fn approve(args: &[&str]) -> Result<(), Vec<String>> {
    let (mut key, mut push) = (DEFAULT_KEY.to_owned(), true);
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        match *flag {
            "--key" => key = (*args.next().ok_or_else(|| vec![USAGE.to_owned()])?).to_owned(),
            "--no-push" => push = false,
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

    let branch = run(Command::new("git").args(["symbolic-ref", "--short", "HEAD"]))?;
    let branch = branch.trim_end();
    let version = branch.strip_prefix("release/").ok_or_else(|| {
        vec![format!(
            "`{branch}` is not a release branch, release/<version>"
        )]
    })?;
    let version = Version::parse(version).map_err(|error| vec![error])?;

    if !run(Command::new("git").args(["status", "--porcelain"]))?.is_empty() {
        return Err(vec![
            "the working tree has changes. An approval covers the commit, so commit or drop them first"
                .to_owned(),
        ]);
    }
    // The branch name parsed as `release/<version>`, so it is a plain ref
    // name and cannot read as an option.
    let tracking = format!("refs/remotes/origin/{branch}");
    run(Command::new("git").args([
        "fetch",
        "--quiet",
        "origin",
        &format!("+refs/heads/{branch}:{tracking}"),
    ]))?;
    let head = run(Command::new("git").args(["rev-parse", "--verify", "HEAD^{commit}"]))?;
    let remote = run(Command::new("git").args([
        "rev-parse",
        "--verify",
        &format!("{tracking}^{{commit}}"),
    ]))?;
    if head != remote {
        return Err(vec![format!(
            "HEAD is not origin/{branch}. Approve exactly what is pushed: pull or push first"
        )]);
    }

    // The release is to run from publish as it is now; a change to it after
    // this stops the release until it is approved again.
    run(Command::new("git").args([
        "fetch",
        "--quiet",
        "origin",
        "+refs/heads/publish:refs/remotes/origin/publish",
    ]))?;
    let publish = run(Command::new("git").args([
        "rev-parse",
        "--verify",
        "refs/remotes/origin/publish^{commit}",
    ]))?;
    let publish = publish.trim_end();

    println!(
        "Approving release {version} at {}, to run from publish at {publish}.",
        head.trim_end()
    );
    let message = format!(
        "{} {version}.\n\n{}: {publish}\n",
        approval::APPROVAL,
        approval::WORKFLOW_TRAILER
    );
    approval::sign_approval(Path::new(&key), branch, head.trim_end(), &message)
        .map_err(|error| vec![error])?;
    if push {
        let status = Command::new("git")
            .args(["push", "origin"])
            .arg(format!("HEAD:refs/heads/{branch}"))
            .status()
            .map_err(|error| vec![format!("git: {error}")])?;
        if !status.success() {
            return Err(vec!["git push failed".to_owned()]);
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

/// The changelog's path in the git repository, which a link on GitHub names.
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

/// The changelog, at the root of the project.
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

/// The output of the program `name`, which must be UTF-8.
fn utf8(bytes: Vec<u8>, name: &str) -> Result<String, String> {
    String::from_utf8(bytes).map_err(|_| format!("{name} wrote output that is not UTF-8"))
}

fn read(path: &Path) -> Result<String, Vec<String>> {
    std::fs::read_to_string(path).map_err(|error| vec![format!("{}: {error}", path.display())])
}
