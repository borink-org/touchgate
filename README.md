# touchgate

**EXPERIMENTAL: Still playing around, do not depend on its security!**

touchgate releases a Cargo workspace to crates.io, or a Python package to PyPI, from a hand-written `CHANGELOG.md`. CI publishes a release only after you approve it with a FIDO2 security key, by touch and PIN. Anything that holds your GitHub token, such as a coding agent, cannot publish on its own.

## Releasing

1. Run the `touchgate` workflow with a version. It dates `## Unreleased`, sets the version and opens a pull request.
2. When the checks of the pull request pass, check out its branch and run `touchgate approve`. The key asks for its PIN and a touch. touchgate then signs an empty commit `Approve release 0.4.0.` and pushes it. Your git signing settings do not change.
3. Merge the pull request with a merge commit within the hour.

When every check on the merge has passed, `release.yml` publishes the release to crates.io or PyPI. It then makes the tag and the GitHub release.

If the release fails, fix the cause on the default branch and copy any change to `release.yml` to `publish`. Then run the same three steps with the same version. touchgate opens the pull request again with an empty commit, because the changelog dates that version already. A failure in a step of a release run that has not changed can also be run again from GitHub Actions.

## Setting up a repository

1. Make a resident key on the security key, where `touchgate approve` looks for it, and install touchgate:

   ```nu
   ssh-keygen -t ed25519-sk -O resident -O verify-required -O application=ssh:signing -f ~/.ssh/id_ed25519_sk_signing
   cargo install --locked --git https://github.com/borink-org/touchgate
   ```

2. Copy [`examples/touchgate.yml`](examples/touchgate.yml) to `.github/workflows/touchgate.yml` on your default branch. It prepares releases, checks the changelog, and starts `release.yml` when a merge brings in an approval.
3. Copy [`examples/crates-release.yml`](examples/crates-release.yml) to `.github/workflows/release.yml`. For a Python package, copy [`examples/pypi-release.yml`](examples/pypi-release.yml) instead. Set `TOUCHGATE_KEY` in it to the contents of your `.pub` file.
4. In both files, replace the commit in `borink-org/touchgate/install@<commit>` with the commit of a touchgate release. The release's tag on GitHub names that commit.
5. Commit both files to the default branch, and copy `release.yml` to a `publish` branch.
6. With the admin account, make the settings in [Settings](#settings).

Both workflow files are yours to change. Each step that does more than check out, build or upload runs a touchgate command. To update touchgate, change the commit in the install steps, and bring over any other change to the examples.

## Settings

Your everyday account has Maintain on the repository, not Admin. A separate admin account, which signs in only in the browser, makes these settings:

- A ruleset that lets only the admin create, update or delete the `publish` branch, and that blocks force pushes.
- A `release` environment that admits only the `publish` branch.
- A trusted publisher on crates.io or PyPI for `release.yml` in the `release` environment. Delete every API token for the package.
- Immutable releases.
- GitHub Actions allowed to create pull requests.
- A `no-changelog` label, for pull requests that need no changelog entry.

## What the release checks

`touchgate verify` checks the merge commit before anything else runs. The second parent of the merge must be the approval, and the approval must meet these conditions:

- It has the same tree as the merge.
- It is at most an hour old.
- It is signed by the key in `TOUCHGATE_KEY`, with flags that show a touch and the PIN. git and `ssh-keygen` accept a FIDO2 signature without these flags, so touchgate reads the flags itself.
- It names the commit of `publish` that the release runs from. A change to `publish` after you approve stops the release.

touchgate runs no code from the commit that it checks.

## Installing touchgate in a workflow

Each job installs touchgate with `borink-org/touchgate/install`, pinned to the commit of a release. The action downloads the static Linux binary of that release. It then runs `gh attestation verify`, which checks that touchgate's own `release.yml` built the binary on touchgate's `publish` branch. The action fails for a commit that is not a release, unless you set `from-source: true` to build that commit with cargo.

## A Python package on PyPI

touchgate releases to PyPI if the repository has a `pyproject.toml` at its root with a `[project]` table. Otherwise it releases to crates.io. A `pyproject.toml` that only configures tools does not count.

touchgate reads and sets `project.version` in `pyproject.toml`, and updates `uv.lock` if the repository has one. If the `build-backend` is `maturin`, the packages of the Cargo workspace get the same version, published or not. `check` fails if their versions differ. touchgate does not release pre-releases to PyPI yet, because PyPI writes their versions differently from Cargo.

`examples/pypi-release.yml` runs these commands:

- `touchgate verify` checks the approval.
- `touchgate check --build` checks the version against the changelog. It also fails if `uv build` would download `uv_build` from PyPI instead of using the copy in uv. uv downloads it when the range in `build-system.requires` does not include the running uv. The example pins uv with `UV_VERSION`, so the backend is pinned too. When you move `UV_VERSION` past the range, widen the range. A package with another backend, such as maturin, passes, because uv always downloads that backend.
- `touchgate wait-for-checks` waits for every check on the merge to pass.
- `touchgate publish dist` checks that `dist` holds only the wheels and sdist of this version. It signs a PEP 740 attestation for each file with `pypi-attestations`, and uploads the files with `uv publish` through Trusted Publishing. It skips files that PyPI has already. It runs only in a GitHub Actions job with `id-token: write`.
- `touchgate github-release dist/*` makes the tag and the GitHub release, with the built files attached.

touchgate installs `pypi-attestations` from a lock in its own source, which fixes each dependency by the hashes of its files. A new touchgate release can update that lock.

The example builds with `uv build`. For maturin, build the wheels in the `build` job with [PyO3/maturin-action](https://github.com/PyO3/maturin-action) in a matrix, and download them all into `dist`.

## touchgate's own releases

touchgate releases itself with its own `touchgate.yml` and `release.yml`:

- `touchgate.yml` is `examples/touchgate.yml`, except that each job builds touchgate from source at the commit it runs on. CI fails if the two files differ in anything else.
- `release.yml` builds binaries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` with a pinned Rust version. A job that builds nothing attests them. The last job attaches them to the GitHub release.

touchgate publishes nothing to crates.io.
