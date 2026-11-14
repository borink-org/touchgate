//! Follows what the interactive `touchgate` waits for on GitHub, through
//! `gh`: the checks of the release pull request until it is merged or closed,
//! and the run of `release.yml` until it finishes.
//!
//! Each follower runs on a thread of its own and sends what it sees as
//! [`Watched`] updates. It stops once no one receives them.

use std::sync::mpsc::Sender;
use std::time::{Duration, Instant};

use crate::github::gh;

/// How long to look for the run of `release.yml` before giving up.
const RUN_APPEAR_TIMEOUT: Duration = Duration::from_secs(120);
/// How often to ask whether the run has appeared.
const RUN_APPEAR_INTERVAL: Duration = Duration::from_secs(2);
/// How often to ask how the run is going.
const RUN_INTERVAL: Duration = Duration::from_secs(3);
/// How often to ask how the pull request is going.
const PULL_REQUEST_INTERVAL: Duration = Duration::from_secs(10);

/// How far a run, a job or a check has come.
#[derive(Clone, Debug, PartialEq)]
pub enum Progress {
    /// Not started, or waiting for a runner.
    Queued,
    Running,
    Passed,
    Skipped,
    /// Finished without passing, with GitHub's conclusion, such as `failure`
    /// or `cancelled`.
    Failed(String),
}

impl Progress {
    /// Reads GitHub's `status` and `conclusion` of a run, a job or a check
    /// run, in either case.
    fn of(status: &str, conclusion: &str) -> Self {
        let (status, conclusion) = (status.to_lowercase(), conclusion.to_lowercase());
        match (status.as_str(), conclusion.as_str()) {
            ("completed", "success" | "neutral") => Self::Passed,
            ("completed", "skipped") => Self::Skipped,
            ("completed", "") => Self::Failed("no conclusion".to_owned()),
            ("completed", _) => Self::Failed(conclusion),
            ("in_progress", _) => Self::Running,
            _ => Self::Queued,
        }
    }

    /// Reads the `state` of a commit status, the older kind of check.
    fn of_status(state: &str) -> Self {
        match state.to_lowercase().as_str() {
            "success" => Self::Passed,
            state @ ("failure" | "error") => Self::Failed(state.to_owned()),
            _ => Self::Running,
        }
    }

    /// Whether the run, job or check has finished.
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Passed | Self::Skipped | Self::Failed(_))
    }
}

/// What a follower sends.
pub enum Watched<T> {
    /// How the followed thing is, when last asked.
    View(T),
    /// Something went wrong that following may get past, such as a failed
    /// call to `gh`.
    Problem(String),
    /// Following stopped before the followed thing finished.
    GaveUp(String),
}

/// Sends what `view` returns every `interval` until `is_done` holds for it,
/// or until no one receives the updates.
fn keep_watching<T>(
    view: impl Fn() -> Result<T, String>,
    is_done: impl Fn(&T) -> bool,
    interval: Duration,
    updates: &Sender<Watched<T>>,
) {
    loop {
        let (update, done) = match view() {
            Ok(view) => {
                let done = is_done(&view);
                (Watched::View(view), done)
            }
            Err(error) => (Watched::Problem(error), false),
        };
        if updates.send(update).is_err() || done {
            return;
        }
        std::thread::sleep(interval);
    }
}

/// A job of the release run.
#[derive(Debug, PartialEq)]
pub struct JobView {
    pub name: String,
    pub progress: Progress,
    /// The step the job is at, while it runs.
    pub step: Option<String>,
    pub url: String,
}

/// How the release run is going.
#[derive(Debug, PartialEq)]
pub struct RunView {
    pub url: String,
    pub progress: Progress,
    pub jobs: Vec<JobView>,
}

/// Returns the IDs of the latest runs of `release.yml` that a person started
/// on `publish`.
pub fn started_run_ids() -> Result<Vec<u64>, String> {
    let ids = gh(&[
        "run",
        "list",
        "--workflow",
        "release.yml",
        "--branch",
        "publish",
        "--event",
        "workflow_dispatch",
        "--limit",
        "20",
        "--json",
        "databaseId",
        "--jq",
        ".[].databaseId",
    ])?;
    ids.lines()
        .map(|id| {
            id.parse()
                .map_err(|_| format!("gh run list: `{id}` is not a run ID"))
        })
        .collect()
}

/// Finds the run of `release.yml` that is not among `earlier`, the runs
/// listed before it started, and sends `updates` on how it goes until it
/// finishes. `gh workflow run` does not say which run it started, so the run
/// is the one that appears afterwards.
pub fn follow_run(earlier: Vec<u64>, updates: Sender<Watched<RunView>>) {
    if let Some(id) = find_new_run(&earlier, &updates) {
        keep_watching(
            || view_run(id),
            |view| view.progress.is_done(),
            RUN_INTERVAL,
            &updates,
        );
    }
}

/// Waits for a run of `release.yml` that is not among `earlier`, and returns
/// its ID. Returns `None` if none appears in time, or no one receives the
/// updates.
fn find_new_run(earlier: &[u64], updates: &Sender<Watched<RunView>>) -> Option<u64> {
    let started = Instant::now();
    loop {
        match started_run_ids() {
            Ok(ids) => {
                if let Some(id) = new_run(earlier, &ids) {
                    return Some(id);
                }
            }
            Err(error) => updates.send(Watched::Problem(error)).ok()?,
        }
        if started.elapsed() > RUN_APPEAR_TIMEOUT {
            let _ = updates.send(Watched::GaveUp(format!(
                "no new run of release.yml appeared within {} seconds",
                RUN_APPEAR_TIMEOUT.as_secs()
            )));
            return None;
        }
        std::thread::sleep(RUN_APPEAR_INTERVAL);
    }
}

/// Returns the newest run among `ids`, newest first, that is not among
/// `earlier`.
fn new_run(earlier: &[u64], ids: &[u64]) -> Option<u64> {
    ids.iter().copied().find(|id| !earlier.contains(id))
}

/// Asks GitHub how the run `id` is going.
fn view_run(id: u64) -> Result<RunView, String> {
    let json = gh(&[
        "run",
        "view",
        &id.to_string(),
        "--json",
        "url,status,conclusion,jobs",
    ])?;
    parse_run_view(&json)
}

/// Returns the text of the field `name` of `value`, or an empty text.
fn text(value: &serde_json::Value, name: &str) -> String {
    value[name].as_str().unwrap_or_default().to_owned()
}

/// Reads the output of `gh run view --json url,status,conclusion,jobs`.
fn parse_run_view(json: &str) -> Result<RunView, String> {
    let run: serde_json::Value =
        serde_json::from_str(json).map_err(|error| format!("gh run view: {error}"))?;
    let jobs = run["jobs"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|job| {
            let progress = Progress::of(&text(job, "status"), &text(job, "conclusion"));
            let step = (progress == Progress::Running)
                .then(|| {
                    job["steps"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|step| step["status"] == "in_progress")
                        .map(|step| text(step, "name"))
                })
                .flatten();
            JobView {
                name: text(job, "name"),
                progress,
                step,
                url: text(job, "url"),
            }
        })
        .collect();
    Ok(RunView {
        url: text(&run, "url"),
        progress: Progress::of(&text(&run, "status"), &text(&run, "conclusion")),
        jobs,
    })
}

/// Whether a pull request is open, merged or closed without a merge.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum PullRequestState {
    Open,
    Merged,
    Closed,
}

/// A check on the pull request.
#[derive(Debug, PartialEq)]
pub struct CheckView {
    pub name: String,
    pub progress: Progress,
    pub url: String,
}

/// How the release pull request is going.
#[derive(Debug, PartialEq)]
pub struct PullRequestView {
    pub number: u64,
    pub title: String,
    pub url: String,
    pub state: PullRequestState,
    pub checks: Vec<CheckView>,
    /// Whether GitHub would merge it as it is, which it says only once the
    /// checks it requires have passed.
    pub mergeable: bool,
}

impl PullRequestView {
    /// Whether the pull request is open, no check is pending or failed, and
    /// at least one passed or GitHub would merge it.
    pub fn is_ready(&self) -> bool {
        self.state == PullRequestState::Open
            && self
                .checks
                .iter()
                .all(|check| matches!(check.progress, Progress::Passed | Progress::Skipped))
            && (self.mergeable || !self.checks.is_empty())
    }

    /// Whether a check on the pull request failed.
    pub fn has_failed_check(&self) -> bool {
        self.checks
            .iter()
            .any(|check| matches!(check.progress, Progress::Failed(_)))
    }
}

/// Sends `updates` on the pull request from `branch` until it is merged or
/// closed.
pub fn follow_pull_request(branch: String, updates: Sender<Watched<PullRequestView>>) {
    keep_watching(
        || view_pull_request(&branch),
        |view| view.state != PullRequestState::Open,
        PULL_REQUEST_INTERVAL,
        &updates,
    );
}

/// Asks GitHub how the pull request from `branch` is going.
fn view_pull_request(branch: &str) -> Result<PullRequestView, String> {
    let json = gh(&[
        "pr",
        "view",
        branch,
        "--json",
        "number,title,url,state,mergeStateStatus,statusCheckRollup",
    ])?;
    parse_pull_request_view(&json)
}

/// Reads the output of `gh pr view --json
/// number,title,url,state,mergeStateStatus,statusCheckRollup`.
fn parse_pull_request_view(json: &str) -> Result<PullRequestView, String> {
    let pull_request: serde_json::Value =
        serde_json::from_str(json).map_err(|error| format!("gh pr view: {error}"))?;
    let state = match text(&pull_request, "state").as_str() {
        "OPEN" => PullRequestState::Open,
        "MERGED" => PullRequestState::Merged,
        "CLOSED" => PullRequestState::Closed,
        other => return Err(format!("gh pr view: the state `{other}` is unknown")),
    };
    let checks = pull_request["statusCheckRollup"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|check| match check["__typename"].as_str() {
            Some("StatusContext") => CheckView {
                name: text(check, "context"),
                progress: Progress::of_status(&text(check, "state")),
                url: text(check, "targetUrl"),
            },
            _ => CheckView {
                name: text(check, "name"),
                progress: Progress::of(&text(check, "status"), &text(check, "conclusion")),
                url: text(check, "detailsUrl"),
            },
        })
        .collect();
    Ok(PullRequestView {
        number: pull_request["number"].as_u64().unwrap_or_default(),
        title: text(&pull_request, "title"),
        url: text(&pull_request, "url"),
        state,
        checks,
        mergeable: matches!(
            text(&pull_request, "mergeStateStatus").as_str(),
            "CLEAN" | "HAS_HOOKS" | "UNSTABLE"
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_the_run_that_was_not_there_before() {
        assert_eq!(new_run(&[7, 5], &[9, 7, 5]), Some(9));
        assert_eq!(new_run(&[7, 5], &[7, 5]), None);
    }

    #[test]
    fn reads_a_run_and_its_jobs() {
        let json = r#"{
            "url": "https://github.com/o/r/actions/runs/1",
            "status": "in_progress",
            "conclusion": "",
            "jobs": [
                {"name": "verify", "status": "completed", "conclusion": "success",
                 "url": "https://github.com/o/r/actions/runs/1/job/2", "steps": []},
                {"name": "build", "status": "in_progress", "conclusion": "",
                 "url": "https://github.com/o/r/actions/runs/1/job/3",
                 "steps": [
                    {"name": "Set up job", "status": "completed", "conclusion": "success"},
                    {"name": "Build", "status": "in_progress", "conclusion": ""}
                 ]},
                {"name": "publish", "status": "completed", "conclusion": "cancelled",
                 "url": "https://github.com/o/r/actions/runs/1/job/4", "steps": []}
            ]
        }"#;
        let view = parse_run_view(json).unwrap();
        assert_eq!(view.progress, Progress::Running);
        let jobs: Vec<_> = view
            .jobs
            .iter()
            .map(|job| (job.name.as_str(), job.progress.clone(), job.step.as_deref()))
            .collect();
        assert_eq!(
            jobs,
            [
                ("verify", Progress::Passed, None),
                ("build", Progress::Running, Some("Build")),
                ("publish", Progress::Failed("cancelled".to_owned()), None),
            ]
        );
    }

    #[test]
    fn reads_a_pull_request_and_its_checks() {
        let json = r#"{
            "number": 12,
            "title": "Prepare the 0.3.0 release.",
            "url": "https://github.com/o/r/pull/12",
            "state": "OPEN",
            "mergeStateStatus": "BLOCKED",
            "statusCheckRollup": [
                {"__typename": "CheckRun", "name": "test", "status": "COMPLETED",
                 "conclusion": "SUCCESS", "detailsUrl": "https://x/1"},
                {"__typename": "CheckRun", "name": "lint", "status": "IN_PROGRESS",
                 "conclusion": "", "detailsUrl": "https://x/2"},
                {"__typename": "StatusContext", "context": "ci/legacy", "state": "PENDING",
                 "targetUrl": "https://x/3"}
            ]
        }"#;
        let view = parse_pull_request_view(json).unwrap();
        assert_eq!((view.number, view.state), (12, PullRequestState::Open));
        let checks: Vec<_> = view
            .checks
            .iter()
            .map(|check| (check.name.as_str(), check.progress.clone()))
            .collect();
        assert_eq!(
            checks,
            [
                ("test", Progress::Passed),
                ("lint", Progress::Running),
                ("ci/legacy", Progress::Running),
            ]
        );
        assert!(!view.is_ready());

        let passed = json
            .replace(r#""IN_PROGRESS""#, r#""COMPLETED""#)
            .replace(r#""PENDING""#, r#""SUCCESS""#)
            .replace(r#""conclusion": """#, r#""conclusion": "SKIPPED""#);
        assert!(parse_pull_request_view(&passed).unwrap().is_ready());
        let failed = passed.replace(r#""SKIPPED""#, r#""FAILURE""#);
        let failed = parse_pull_request_view(&failed).unwrap();
        assert!(!failed.is_ready() && failed.has_failed_check());
    }
}
