//! Follows the run of `release.yml` that `touchgate release` starts, through
//! `gh`.
//!
//! `gh workflow run` does not say which run it started. The run is the one
//! that appears afterwards and was not listed before.

use std::sync::mpsc::Sender;
use std::time::Duration;

use crate::github::gh;

/// How long to look for the run before giving up.
const APPEAR_TIMEOUT: Duration = Duration::from_secs(120);
/// How often to ask whether the run has appeared.
const APPEAR_INTERVAL: Duration = Duration::from_secs(2);
/// How often to ask how the run is going.
const VIEW_INTERVAL: Duration = Duration::from_secs(3);

/// How far a run or a job has come.
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
    /// Reads GitHub's `status` and `conclusion` of a run or a job.
    fn of(status: &str, conclusion: &str) -> Self {
        match (status, conclusion) {
            ("completed", "success" | "neutral") => Self::Passed,
            ("completed", "skipped") => Self::Skipped,
            ("completed", "") => Self::Failed("no conclusion".to_owned()),
            ("completed", conclusion) => Self::Failed(conclusion.to_owned()),
            ("in_progress", _) => Self::Running,
            _ => Self::Queued,
        }
    }

    /// Whether the run or job has finished.
    pub fn is_done(&self) -> bool {
        matches!(self, Self::Passed | Self::Skipped | Self::Failed(_))
    }
}

/// A job of the run.
#[derive(Debug, PartialEq)]
pub struct JobView {
    pub name: String,
    pub progress: Progress,
    /// The step the job is at, while it runs.
    pub step: Option<String>,
    pub url: String,
}

/// How the run is going.
#[derive(Debug, PartialEq)]
pub struct RunView {
    pub url: String,
    pub progress: Progress,
    pub jobs: Vec<JobView>,
}

/// What [`follow`] sends while it follows the run.
pub enum RunUpdate {
    View(RunView),
    /// Something went wrong that following may get past, such as a failed
    /// call to `gh`.
    Problem(String),
    /// Following stopped before the run finished.
    GaveUp(String),
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

/// Reads the output of `gh run view --json url,status,conclusion,jobs`.
fn parse_run_view(json: &str) -> Result<RunView, String> {
    let run: serde_json::Value =
        serde_json::from_str(json).map_err(|error| format!("gh run view: {error}"))?;
    let text =
        |value: &serde_json::Value, name: &str| value[name].as_str().unwrap_or_default().to_owned();
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

/// Finds the run of `release.yml` that is not among `earlier`, the runs
/// listed before it started, and sends `updates` on how it goes until it
/// finishes. Stops early once no one receives the updates.
pub fn follow(earlier: Vec<u64>, updates: Sender<RunUpdate>) {
    let Some(id) = find_new_run(&earlier, &updates) else {
        return;
    };
    loop {
        let (update, done) = match view_run(id) {
            Ok(view) => {
                let done = view.progress.is_done();
                (RunUpdate::View(view), done)
            }
            Err(error) => (RunUpdate::Problem(error), false),
        };
        if updates.send(update).is_err() || done {
            return;
        }
        std::thread::sleep(VIEW_INTERVAL);
    }
}

/// Waits for a run of `release.yml` that is not among `earlier`, and returns
/// its ID. Returns `None` if none appears in time, or no one receives the
/// updates.
fn find_new_run(earlier: &[u64], updates: &Sender<RunUpdate>) -> Option<u64> {
    let started = std::time::Instant::now();
    loop {
        match started_run_ids() {
            Ok(ids) => {
                if let Some(id) = new_run(earlier, &ids) {
                    return Some(id);
                }
            }
            Err(error) => updates.send(RunUpdate::Problem(error)).ok()?,
        }
        if started.elapsed() > APPEAR_TIMEOUT {
            let _ = updates.send(RunUpdate::GaveUp(format!(
                "no new run of release.yml appeared within {} seconds",
                APPEAR_TIMEOUT.as_secs()
            )));
            return None;
        }
        std::thread::sleep(APPEAR_INTERVAL);
    }
}

/// Returns the newest run among `ids`, newest first, that is not among
/// `earlier`.
fn new_run(earlier: &[u64], ids: &[u64]) -> Option<u64> {
    ids.iter().copied().find(|id| !earlier.contains(id))
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
}
