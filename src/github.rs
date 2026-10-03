//! The steps of a release that talk to GitHub, through the `gh` CLI that
//! GitHub's runners carry. `gh` finds the repository from the checkout, and
//! its token from `GH_TOKEN`.

use std::path::Path;
use std::process::Command;
use std::time::{Duration, Instant};

use crate::approval;
use crate::version::Version;

/// The branch a prepared release is proposed from.
fn release_branch(version: &Version) -> String {
    format!("release/{version}")
}

/// Starts `release.yml` on the `publish` branch if the merge `commit` brings
/// in an approval. Whether the approval holds is for that workflow to check.
pub fn start(commit: &str) -> Result<(), String> {
    match approval::merged_approval(Path::new("."), commit)? {
        None => {
            println!("{commit} merges no approval");
            Ok(())
        }
        Some(approval) => {
            println!("{commit} merges the approval {approval}; starting the release");
            gh(&[
                "workflow",
                "run",
                "release.yml",
                "--ref",
                "publish",
                "-f",
                &format!("commit={commit}"),
            ])
            .map(drop)
        }
    }
}

/// The GitHub release a version gets: what [`release`] makes, and what the
/// pull request of [`pull_request`] shows ahead of it.
struct GitHubRelease {
    /// The tag, which is also the title.
    tag: String,
    notes: String,
    pre_release: bool,
}

impl GitHubRelease {
    /// The release of `version`, whose notes point at `changelog`, the
    /// changelog's path in the repository, as it reads at the tag.
    fn of(version: &Version, changelog: &str) -> Result<Self, String> {
        let tag = format!("v{version}");
        let url = gh(&["repo", "view", "--json", "url", "--jq", ".url"])?;
        let name = changelog.rsplit('/').next().unwrap_or(changelog);
        let notes = format!("See [{name}]({}/blob/{tag}/{changelog}).", url.trim_end());
        Ok(Self {
            tag,
            notes,
            pre_release: version.is_pre_release(),
        })
    }

    /// The release as the pull request describes it.
    fn preview(&self) -> String {
        let kind = if self.pre_release {
            "a pre-release"
        } else {
            "the latest release"
        };
        let quoted: String = self
            .notes
            .lines()
            .map(|line| format!("> {line}\n"))
            .collect();
        format!(
            "Once published, the merge commit is tagged `{tag}` and gets this GitHub release, as {kind}:\n\n\
             - Title: `{tag}`\n\
             - Notes:\n\n{quoted}",
            tag = self.tag
        )
    }
}

/// Commits the prepared release on its branch, pushes it, and opens a pull
/// request for it, or updates the one that is open. `changelog` is the
/// changelog's path in the repository.
pub fn pull_request(version: &Version, changelog: &str) -> Result<(), String> {
    let release = GitHubRelease::of(version, changelog)?;
    let branch = release_branch(version);
    git(&["switch", "--create", &branch])?;
    git(&[
        "-c",
        "user.name=github-actions[bot]",
        "-c",
        "user.email=41898282+github-actions[bot]@users.noreply.github.com",
        "commit",
        "--all",
        "--no-verify",
        "--message",
        &format!("Prepare the {version} release."),
    ])?;
    git(&[
        "push",
        "--force",
        "origin",
        &format!("HEAD:refs/heads/{branch}"),
    ])?;

    let body = format!(
        "This sets every published package to {version} and dates its section of the changelog.

To approve the release, check out this branch, look it over, and once the checks pass run `touchgate approve`. It signs an empty commit with the release key, which asks for its PIN and a touch, and pushes it.

```
git fetch origin
git switch {branch}
touchgate approve
```

The approval names the commit of the `publish` branch the release is to run from; if that branch changes before the release, approve again. Pushing the approval also starts this pull request's checks, which a pull request opened by a workflow does not start on its own. An approval lasts an hour, so merge once they pass; if the hour runs out, approve again.

Merge with a merge commit, never by squashing or rebasing, which would drop the signature. The release then publishes to crates.io, and tags and releases {version} on GitHub once that succeeds. If the default branch moves before the merge, merge it into this branch and approve again, since a release publishes only the exact tree that was approved.

## The GitHub release

{preview}",
        preview = release.preview()
    );
    let title = format!("Release {version}.");
    match gh(&[
        "pr", "create", "--head", &branch, "--title", &title, "--body", &body,
    ]) {
        Ok(url) => println!("{url}"),
        Err(_) => {
            gh(&["pr", "edit", &branch, "--title", &title, "--body", &body])?;
        }
    }
    Ok(())
}

/// Tags `commit` and makes its GitHub release, as [`GitHubRelease::of`]
/// describes it.
///
/// Anyone who can push can make tags and releases, so a tag or a release that
/// exists already must point at `commit`, or this fails rather than dress up
/// someone else's. A release this made before, on a run that failed later,
/// counts as done.
pub fn release(commit: &str, version: &Version, changelog: &str) -> Result<(), String> {
    if !approval::is_hash(commit) {
        return Err(format!("`{commit}` is not a full commit hash"));
    }
    let release = GitHubRelease::of(version, changelog)?;
    let tag = &release.tag;
    let existing = tagged_commit(tag)?;
    if let Some(existing) = &existing
        && existing != commit
    {
        return Err(format!(
            "{tag} names {existing}, not the release {commit}. Delete it and run again"
        ));
    }

    match gh(&[
        "release", "view", tag, "--json", "isDraft", "--jq", ".isDraft",
    ]) {
        Ok(draft) => {
            if draft.trim() == "true" || existing.is_none() {
                return Err(format!(
                    "a release for {tag} exists that touchgate did not make. Delete it and run again"
                ));
            }
            println!("{tag} is released already");
            return Ok(());
        }
        Err(error) if error.contains("release not found") => {}
        Err(error) => return Err(error),
    }

    let mut args = vec![
        "release",
        "create",
        tag,
        "--target",
        commit,
        "--title",
        tag,
        "--notes",
        &release.notes,
    ];
    if release.pre_release {
        args.push("--prerelease");
    }
    gh(&args)?;

    match tagged_commit(tag)? {
        Some(tagged) if tagged == commit => Ok(()),
        tagged => Err(format!(
            "{tag} names {} after the release, not {commit}",
            tagged.as_deref().unwrap_or("nothing")
        )),
    }
}

/// How a finished check run may end and still let a release through.
const PASSING: [&str; 3] = ["success", "neutral", "skipped"];

/// Waits until every check run on `commit` has finished, and fails if one did
/// not pass, or if some are still running after `timeout_seconds`. The jobs of
/// this workflow run are left out, so a release run on the same commit does
/// not wait for itself. GitHub's older commit statuses are not read.
pub fn wait_for_checks(commit: &str, timeout_seconds: i64) -> Result<(), String> {
    if !approval::is_hash(commit) {
        return Err(format!("`{commit}` is not a full commit hash"));
    }
    let own_run = std::env::var("GITHUB_RUN_ID")
        .ok()
        .map(|id| format!("/actions/runs/{id}/"));
    let started = Instant::now();
    loop {
        let pages = gh(&[
            "api",
            "--paginate",
            "--slurp",
            &format!("repos/{{owner}}/{{repo}}/commits/{commit}/check-runs?per_page=100"),
        ])?;
        let pages: serde_json::Value =
            serde_json::from_str(&pages).map_err(|error| format!("gh api: {error}"))?;
        let mut pending = Vec::new();
        let mut failed = Vec::new();
        let mut passed = 0;
        for run in pages
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|page| page["check_runs"].as_array())
            .flatten()
        {
            let url = run["details_url"].as_str().unwrap_or_default();
            if own_run.as_deref().is_some_and(|own| url.contains(own)) {
                continue;
            }
            let name = run["name"].as_str().unwrap_or("a check").to_owned();
            match (run["status"].as_str(), run["conclusion"].as_str()) {
                (Some("completed"), Some(conclusion)) if PASSING.contains(&conclusion) => {
                    passed += 1
                }
                (Some("completed"), conclusion) => failed.push(format!(
                    "{name} ({})",
                    conclusion.unwrap_or("no conclusion")
                )),
                _ => pending.push(name),
            }
        }
        if !failed.is_empty() {
            return Err(format!(
                "checks on {commit} did not pass: {}",
                failed.join(", ")
            ));
        }
        if pending.is_empty() && passed > 0 {
            println!("all {passed} checks on {commit} passed");
            return Ok(());
        }
        if started.elapsed() > Duration::from_secs(timeout_seconds as u64) {
            return Err(format!(
                "checks on {commit} are still not done: {}",
                if pending.is_empty() {
                    "none has started".to_owned()
                } else {
                    pending.join(", ")
                }
            ));
        }
        println!(
            "waiting for {}",
            if pending.is_empty() {
                "the first check".to_owned()
            } else {
                pending.join(", ")
            }
        );
        std::thread::sleep(Duration::from_secs(30));
    }
}

/// The commit `tag` names, following an annotated tag to its commit, or
/// `None` if there is no such tag. Any answer but the tag or a 404 is an
/// error.
fn tagged_commit(tag: &str) -> Result<Option<String>, String> {
    let reference = match gh(&[
        "api",
        &format!("repos/{{owner}}/{{repo}}/git/ref/tags/{tag}"),
    ]) {
        Ok(reference) => reference,
        Err(error) if error.contains("HTTP 404") => return Ok(None),
        Err(error) => return Err(error),
    };
    let reference: serde_json::Value =
        serde_json::from_str(&reference).map_err(|error| format!("gh api: {error}"))?;
    let object = &reference["object"];
    let (Some(kind), Some(sha)) = (object["type"].as_str(), object["sha"].as_str()) else {
        return Err(format!("gh api: {tag} has no object"));
    };
    let sha = if kind == "tag" {
        let annotated = gh(&["api", &format!("repos/{{owner}}/{{repo}}/git/tags/{sha}")])?;
        let annotated: serde_json::Value =
            serde_json::from_str(&annotated).map_err(|error| format!("gh api: {error}"))?;
        annotated["object"]["sha"]
            .as_str()
            .ok_or_else(|| format!("gh api: the tag object of {tag} has no commit"))?
            .to_owned()
    } else {
        sha.to_owned()
    };
    if !approval::is_hash(&sha) {
        return Err(format!("gh api: {tag} names `{sha}`"));
    }
    Ok(Some(sha))
}

/// Runs `gh`, and returns its standard output, or its standard error as the
/// error.
fn gh(args: &[&str]) -> Result<String, String> {
    output("gh", Command::new("gh").args(args))
}

fn git(args: &[&str]) -> Result<String, String> {
    output("git", Command::new("git").args(args))
}

fn output(name: &str, command: &mut Command) -> Result<String, String> {
    let output = command
        .output()
        .map_err(|error| format!("{name}: {error}"))?;
    if output.status.success() {
        crate::utf8(output.stdout, name)
    } else {
        let stderr = crate::utf8(output.stderr, name)?;
        Err(format!("{name} failed: {}", stderr.trim()))
    }
}
