//! Reads the release run that a command runs in. That run is `release.yml`,
//! started by hand with the signed approval of a release. The release
//! commands read their values from here when you pass no flags.
//!
//! Every value comes from GitHub and none from an earlier step. A step cannot
//! set a `GITHUB_*` variable for the steps after it. The event file holds the
//! inputs that the run was started with.

use std::path::PathBuf;

use crate::approval::SignedApproval;

/// The variable that names the file with the event that started the run.
const EVENT_PATH: &str = "GITHUB_EVENT_PATH";

/// The variable with the commit of the workflow file that runs.
const WORKFLOW_SHA: &str = "GITHUB_WORKFLOW_SHA";

/// The variable that names the file a step writes its outputs to.
const OUTPUT_PATH: &str = "GITHUB_OUTPUT";

/// The variable that holds the public key of the approval key. A
/// `release.yml` sets it in its `env`, so the copy on `publish` fixes it.
pub const KEY_VARIABLE: &str = "TOUCHGATE_KEY";

/// The workflow input that holds the signed approval of the release.
const APPROVAL_INPUT: &str = "approval";

/// What the run was started for.
pub struct ReleaseRun {
    /// The approval the run was started with, the `approval` input. Reading
    /// it checks its form only: `touchgate verify` checks the signature.
    pub approval: SignedApproval,
    /// The repository's default branch, as `origin/<name>`.
    pub branch: String,
    /// The repository, as `owner/name`.
    pub repository: String,
}

impl ReleaseRun {
    /// Reads the event that started the run, which must be a start by hand
    /// with an `approval` input.
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
        let approval = event["inputs"][APPROVAL_INPUT].as_str().ok_or_else(|| {
            format!("no `{APPROVAL_INPUT}` input: start release.yml with `touchgate approve`")
        })?;
        let repository = &event["repository"];
        let branch = repository["default_branch"]
            .as_str()
            .ok_or("no `repository.default_branch`")?;
        let name = repository["full_name"]
            .as_str()
            .ok_or("no `repository.full_name`")?;
        Ok(Self {
            approval: SignedApproval::parse(approval)?,
            branch: format!("origin/{branch}"),
            repository: name.to_owned(),
        })
    }

    /// Returns the commit the run releases.
    pub fn commit(&self) -> &str {
        &self.approval.approval.commit
    }
}

/// Sets the outputs `name=value` of the step that runs, if it runs in GitHub
/// Actions, so that later jobs can read them.
pub fn set_step_outputs(outputs: &[(&str, &str)]) -> Result<(), String> {
    let Some(path) = std::env::var_os(OUTPUT_PATH) else {
        return Ok(());
    };
    let mut text = String::new();
    for (name, value) in outputs {
        if value.contains('\n') {
            return Err(format!("the output {name} spans lines"));
        }
        text.push_str(&format!("{name}={value}\n"));
    }
    std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .and_then(|mut file| std::io::Write::write_all(&mut file, text.as_bytes()))
        .map_err(|error| format!("{}: {error}", PathBuf::from(path).display()))
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

    const APPROVAL: &str = "touchgate release approval\nrepository: o/r\ncommit: \
        aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\nversion: 0.4.0\nworkflow-commit: \
        bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\ntime: 1793000000\n\
        -----BEGIN SSH SIGNATURE-----\nU1NI\n-----END SSH SIGNATURE-----\n";

    #[test]
    fn reads_the_release_run() {
        let event = serde_json::json!({
            "inputs": { "approval": APPROVAL },
            "repository": { "default_branch": "main", "full_name": "o/r" },
        });
        let run = ReleaseRun::parse(&event.to_string()).unwrap();
        assert_eq!(run.commit(), "a".repeat(40));
        assert_eq!(run.branch, "origin/main");
        assert_eq!(run.repository, "o/r");
        let error = ReleaseRun::parse(r#"{ "repository": { "default_branch": "main" } }"#)
            .err()
            .unwrap();
        assert!(error.contains("no `approval` input"), "{error}");
    }
}
