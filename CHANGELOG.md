# Changelog

## 0.2.0 - 2026-11-01

### Changed

- `touchgate approve` approves a commit on the default branch, its tip unless you name another, and starts `release.yml` itself. It signs a message that names the repository, the commit, its version, the commit of `publish` and the time, in the SSH namespace `touchgate-release`. A release needs no release branch, approval commit or merge, so CI runs once for it.
- `release.yml` takes the input `approval` instead of `commit`. `touchgate verify` reads the commit from the approval, and sets it as the step output `commit` for the jobs that check it out. Copy the examples again, and update `publish`.
- An approval lasts 15 minutes unless `--max-age` says otherwise, instead of an hour.
- `touchgate approve` and `touchgate verify` refuse a release that the changelog dates another day than the approval or the day before, in UTC.
- `touchgate github-release` in a release run attaches the run's approval to the GitHub release, as `touchgate-approval.txt`.
- The pull request of `touchgate prepare --pull-request` may be merged in any way, and is titled `Prepare the 0.4.0 release.`.
- `touchgate check-entry` passes for a change that prepares a release, whatever its branch is called.
- `touchgate prepare` prepares a version again if the changelog dates it but no tag names it. It moves any entries of `## Unreleased` into that version's section, and dates the section anew. After a release that failed before it published, or one whose date has passed, prepare the same version again and approve again.

### Removed

- `touchgate start`, and the `start` job of `examples/touchgate.yml`.
- Option `touchgate approve --no-push`. Option `--no-start` prints the approval instead of starting the release.

## 0.1.0 - 2026-11-01

### Added

- Releases of touchgate itself. Each release has static Linux binaries for x86_64 and aarch64, with GitHub artifact attestations.
- Action `borink-org/touchgate/install`. It downloads the binary of the touchgate release that it is pinned to, checks its attestation with `gh attestation verify`, and puts it on the `PATH`. With `from-source: true`, it builds the pinned commit with cargo instead.
- Releases of a Python package to PyPI, for a repository whose root `pyproject.toml` has a `[project]` table. `touchgate publish` uploads a PEP 740 attestation with each file.
- Option `touchgate check --build`. It also checks that `uv build` would use the `uv_build` in uv instead of downloading one.
- In a release run, `verify`, `wait-for-checks` and `github-release` read the commit, the default branch, the workflow commit and the key from the run when you pass no flag for them.
- Examples `examples/touchgate.yml`, `examples/crates-release.yml` and `examples/pypi-release.yml`, to copy into a repository.

### Removed

- The reusable workflows `touchgate.yml` and `release.yml`. Copy `examples/touchgate.yml` to `.github/workflows/touchgate.yml`, and `examples/crates-release.yml` to `.github/workflows/release.yml`.

## 0.0.0 - 2026-10-31

### Added

- touchgate before its first release, installed from git at a commit.
