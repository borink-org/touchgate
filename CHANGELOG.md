# Changelog

## Unreleased

### Added

- Releases of touchgate itself, approved with touchgate, with static Linux binaries for x86_64 and aarch64 attached and attested with GitHub's artifact attestations.
- `borink-org/touchgate/install`, an action that installs the binary of the touchgate release it is pinned to, after `gh attestation verify` shows that touchgate's release workflow on `publish` built it. It replaces building touchgate with cargo in every job.
- Releases of Python packages to PyPI, with PEP 740 attestations, for a repository whose `pyproject.toml` has a `[project]` table.
- `touchgate check --build`, which also checks that `uv build` would use the `uv_build` built into uv rather than download one.
- In a release run, `verify`, `wait-for-checks` and `github-release` take the commit, the default branch, the workflow commit and the key from the run when no flag gives them.

### Changed

- The reusable workflow for a Cargo workspace is `crates-release.yml`, no longer `release.yml`, which is now touchgate's own release. Callers pinned to an earlier commit keep working.
- The jobs of the reusable `touchgate.yml` install touchgate with the install action instead of building it.

### Fixed

- `touchgate publish` uploads the PEP 740 attestations it signs for a PyPI release.

## 0.0.0 - 2026-10-31

### Added

- touchgate before its first release, installed from git at a commit.
