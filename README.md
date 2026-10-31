# touchgate

**EXPERIMENTAL: Still playing around, do not depend on its security!**

Releases a Cargo workspace to crates.io, or a Python package to PyPI, from a hand-written `CHANGELOG.md`, and lets CI publish only after you approve with a FIDO2 security key, by touch and PIN. Nothing holding your GitHub token, such as a coding agent, can publish on its own.

## Release

1. Run the `prepare-release` workflow with a version. It dates `## Unreleased`, sets the version and opens a pull request.
2. Once its checks pass, check out the branch and run `touchgate approve`. It signs an empty `Approve release 0.4.0.` commit with the key, which asks for its PIN and a touch, and pushes it. Your git signing settings stay as they are.
3. Within the hour, merge with a merge commit. Once every check on the merge has passed, the release publishes to crates.io or PyPI, then tags and makes the GitHub release.

## What it checks

The merge's second parent must be the approval: same tree as the merge, at most an hour old, signed by the one key in `release.yml`, with the signature's flags showing a touch and the PIN, and naming the `publish` commit the release runs from, so a change to `publish` after you approve stops the release. git and `ssh-keygen` accept FIDO2 signatures made without either, so touchgate reads those flags itself. It runs no code from the commit it checks.

## What it relies on

- Your everyday account has Maintain, never Admin, on the repository and on touchgate. A separate admin account, used only in the browser, makes the settings below.
- `release.yml` runs from a `publish` branch that a ruleset lets only the admin change, in a `release` environment limited to exactly that branch.
- crates.io or PyPI trusts only `release.yml` in the `release` environment, with Trusted Publishing only and no API tokens left.
- Immutable releases are on.

## Set up

Make a resident key on the security key, where `touchgate approve` looks for it, and install touchgate:

```nu
ssh-keygen -t ed25519-sk -O resident -O verify-required -O application=ssh:signing -f ~/.ssh/id_ed25519_sk_signing
cargo install --locked --git https://github.com/borink-org/touchgate
```

Add two workflows that call touchgate's, pinned to a touchgate commit. `.github/workflows/touchgate.yml`, with your default branch:

```yaml
name: touchgate
on:
  workflow_dispatch:
    inputs:
      version:
        description: The version to release, such as 0.4.0
        required: true
        type: string
  push:
    branches: [main]
  pull_request:
    types: [opened, synchronize, reopened, labeled, unlabeled]
permissions: {}
jobs:
  touchgate:
    uses: borink-org/touchgate/.github/workflows/touchgate.yml@<commit>
    permissions:
      contents: write
      pull-requests: write
      actions: write
    with:
      version: ${{ inputs.version }}
```

`.github/workflows/release.yml`, with the contents of your `.pub` file, committed to both the default branch and `publish`:

```yaml
name: release
on:
  workflow_dispatch:
    inputs:
      commit:
        required: true
        type: string
permissions: {}
jobs:
  release:
    uses: borink-org/touchgate/.github/workflows/release.yml@<commit>
    permissions:
      contents: write
      id-token: write
      checks: read
    with:
      commit: ${{ inputs.commit }}
      key: sk-ssh-ed25519@openssh.com AAAA...
```

Allow GitHub Actions to create pull requests, and add a `no-changelog` label for pull requests with nothing to say in the changelog.

## A Python package on PyPI

Add a `touchgate.json` at the root of the repository:

```json
{ "registry": "pypi" }
```

touchgate then releases the package of `pyproject.toml`, whose `project.version` it reads and sets, and moves `uv.lock` along if there is one. For a Rust extension built with maturin, add `"extension": "maturin"`: the packages of the Cargo workspace then move with the Python package's version, whether they are published or not, and `check` fails if they disagree. touchgate does not release pre-releases to PyPI yet, since PyPI writes them another way than Cargo. Without a `touchgate.json`, touchgate releases the Cargo workspace to crates.io.

Use [`examples/pypi-release.yml`](examples/pypi-release.yml) as `.github/workflows/release.yml`, with your touchgate commit and key, and keep `touchgate.yml` as above. PyPI accepts no trusted publisher in a reusable workflow, and the attestations name the workflow that defines the job that publishes, so that job lives in your `release.yml`. Every step in it that does more than build or upload is a touchgate command:

- `touchgate verify` checks the approval, as for crates.io.
- `touchgate check` checks the version against the changelog before the build.
- `touchgate wait-for-checks` waits for every check on the merge.
- `touchgate publish dist` checks that `dist` holds only this version's wheels and sdist, signs a PEP 740 attestation for each with a pinned `pypi-attestations`, and uploads them with `uv publish` through Trusted Publishing, skipping files PyPI has already. It runs only in GitHub Actions, in a job with `id-token: write`. Updating touchgate updates how it signs and uploads.
- `touchgate github-release --commit <hash> dist/*` tags the release and attaches the built files.

The example builds with `uv build`. For maturin, build the wheels in the `build` job with [PyO3/maturin-action](https://github.com/PyO3/maturin-action) instead, in a matrix, and download them all into `dist`.

Each job installs touchgate with `cargo install --locked`, including the job that publishes, so touchgate's own locked dependencies build there. Their versions are fixed by touchgate's `Cargo.lock`.
