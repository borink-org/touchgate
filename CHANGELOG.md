# Changelog

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
