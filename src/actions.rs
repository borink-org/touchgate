//! Reads the release run that a command runs in. That run is `release.yml`,
//! started by hand with the merge commit of a release. The release commands
//! read their values from here when you pass no flags.
//!
//! Every value comes from GitHub and none from an earlier step. A step cannot
//! set a `GITHUB_*` variable for the steps after it. The event file holds the
//! inputs that the run was started with.

use std::path::PathBuf;

/// The variable that names the file with the event that started the run.
const EVENT_PATH: &str = "GITHUB_EVENT_PATH";

/// The variable with the commit of the workflow file that runs.
const WORKFLOW_SHA: &str = "GITHUB_WORKFLOW_SHA";

/// The variable that holds the public key of the approval key. A
/// `release.yml` sets it in its `env`, so the copy on `publish` fixes it.
pub const KEY_VARIABLE: &str = "TOUCHGATE_KEY";

/// The workflow input that holds the merge commit of the release.
const COMMIT_INPUT: &str = "commit";

/// What the run was started for.
pub struct ReleaseRun {
    /// The merge commit of the release, the `commit` input.
    pub commit: String,
    /// The repository's default branch, as `origin/<name>`.
    pub branch: String,
}

impl ReleaseRun {
    /// Reads the event that started the run, which must be a start by hand
    /// with a `commit` input.
    pub fn load() -> Result<Self, String> {
        let path = std::env::var_os(EVENT_PATH)
            .map(PathBuf::from)
            .ok_or_else(|| {
                format!(
                    "{EVENT_PATH} is not set. Outside a GitHub Actions release run, pass the \
                 values as flags"
                )
            })?;
        let text = std::fs::read_to_string(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        Self::parse(&text).map_err(|error| format!("{}: {error}", path.display()))
    }

    fn parse(event: &str) -> Result<Self, String> {
        let event: serde_json::Value =
            serde_json::from_str(event).map_err(|error| error.to_string())?;
        let commit = event["inputs"][COMMIT_INPUT].as_str().ok_or_else(|| {
            format!("no `{COMMIT_INPUT}` input: start release.yml by hand with the merge commit")
        })?;
        let branch = event["repository"]["default_branch"]
            .as_str()
            .ok_or("no `repository.default_branch`")?;
        Ok(Self {
            commit: commit.to_owned(),
            branch: format!("origin/{branch}"),
        })
    }
}

/// Returns the commit of the workflow file that runs.
pub fn workflow_commit() -> Result<String, String> {
    std::env::var(WORKFLOW_SHA).map_err(|_| {
        format!("{WORKFLOW_SHA} is not set. Outside GitHub Actions, pass --workflow-commit")
    })
}

/// Returns the public approval key that `release.yml` sets.
pub fn approval_key() -> Result<String, String> {
    std::env::var(KEY_VARIABLE).map_err(|_| {
        format!("{KEY_VARIABLE} is not set. Set it in the `env` of release.yml, or pass --key")
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_the_release_run() {
        let run = ReleaseRun::parse(
            r#"{ "inputs": { "commit": "abc" }, "repository": { "default_branch": "main" } }"#,
        )
        .unwrap();
        assert_eq!(run.commit, "abc");
        assert_eq!(run.branch, "origin/main");
        let error = ReleaseRun::parse(r#"{ "repository": { "default_branch": "main" } }"#)
            .err()
            .unwrap();
        assert!(error.contains("no `commit` input"), "{error}");
    }
}
