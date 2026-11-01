# touchgate

**EXPERIMENTAL: Still playing around, do not depend on its security!**

touchgate releases a Cargo workspace to crates.io, or a Python package to PyPI, from a hand-written `CHANGELOG.md`. CI publishes a release only after you approve it with a FIDO2 security key, by touch and PIN. Anything that holds your GitHub token, such as a coding agent, cannot publish on its own.

## Releasing

1. Run `touchgate prepare 0.4.0`. It dates `## Unreleased` and sets the version. Commit that and push it to the default branch. If the default branch takes changes only through pull requests, run the `touchgate` workflow with the version instead. It opens a pull request with the same change, which you merge in any way.
2. Run `touchgate approve`. It shows the commits since the last release, and the key asks for its PIN and a touch. touchgate then signs an approval of the tip of the default branch and starts `release.yml` with it. To approve another commit on the default branch, pass it: `touchgate approve <commit>`.

Prepare and approve on the same day, or the next: the release must be dated the day of the approval or the day before, in UTC. For a release prepared earlier, run `touchgate prepare 0.4.0` again, which dates it anew, and push that.

You need not wait for CI before you approve. `release.yml` checks the approval first, then waits until every check on the commit has passed. It publishes the release to crates.io or PyPI, and then makes the tag and the GitHub release, with the approval attached as `touchgate-approval.txt`.

An approval adds no commit, so CI runs once for a release: on the commit that prepares it. Fold the prepare into your last change, and it adds no CI run at all.

To sign on one machine and start the release from another, run `touchgate approve --no-start`, which prints the approval instead of starting the release, and start the release with it from the other machine:

```nu
touchgate approve --no-start | save approval.txt
gh workflow run release.yml --ref publish -F approval=@approval.txt
```

If a release fails, re-run its failed jobs from GitHub Actions. Re-running every job checks the approval again, so after 15 minutes you approve again. If the cause needs a fix, fix it on the default branch and copy any change to `release.yml` to `publish`. Then run both steps again with the same version. `touchgate prepare` moves anything added under `## Unreleased` since into that version's section, and `touchgate approve` refuses a commit with entries left there, so you may skip the prepare if nothing was added. A version with a tag has published already, and needs a newer version instead.

## Setting up a repository

1. Make a resident key on the security key, where `touchgate approve` looks for it, and install touchgate:

   ```nu
   ssh-keygen -t ed25519-sk -O resident -O verify-required -O application=ssh:signing -f ~/.ssh/id_ed25519_sk_signing
   cargo install --locked --git https://github.com/borink-org/touchgate
   ```

   `touchgate approve` also needs `gh`, logged in to an account that can start workflows in the repository.

2. Copy [`examples/touchgate.yml`](examples/touchgate.yml) to `.github/workflows/touchgate.yml` on your default branch. It checks the changelog, and prepares a release through a pull request when you start it with a version.
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
- GitHub Actions allowed to create pull requests, if you prepare releases with the `touchgate` workflow.
- A `no-changelog` label, for pull requests that need no changelog entry.

## What the release checks

`touchgate verify` checks the approval before anything else runs. The approval is a short message that names the repository, the commit to release, its version, the commit of `publish` that the release is to run from, and the time it was signed. It must meet these conditions:

- It is signed by the key in `TOUCHGATE_KEY`, with flags that show a touch and the PIN. `ssh-keygen` accepts a FIDO2 signature without these flags, so touchgate reads the flags itself.
- It is signed in the namespace `touchgate-release`. A signature covers its namespace, so a git commit signature made with the same key cannot pass as an approval.
- It names the repository of the run.
- It is at most 15 minutes old.
- It names the commit of `publish` that the release runs from. A change to `publish` after you approve stops the release.
- Its commit is on the default branch, and its `CHANGELOG.md` has no entries under `## Unreleased`. The newest release there is the version that the approval names.
- That release is dated the day of the approval, or the day before, in UTC. `touchgate prepare` writes dates in UTC. The day before admits a release prepared in the evening and approved after midnight.

touchgate runs no code from the commit that it checks. It reads `CHANGELOG.md` at the root of the repository with git.

Within its 15 minutes, anyone who can start the workflow can start it again with the same approval. That releases the same commit again, and publishing skips what the registry has already.

## Installing touchgate in a workflow

Each job installs touchgate with `borink-org/touchgate/install`, pinned to the commit of a release. The action downloads the static Linux binary of that release. It then runs `gh attestation verify`, which checks that touchgate's own `release.yml` built the binary on touchgate's `publish` branch. The action fails for a commit that is not a release, unless you set `from-source: true` to build that commit with cargo.

## A Python package on PyPI

touchgate releases to PyPI if the repository has a `pyproject.toml` at its root with a `[project]` table. Otherwise it releases to crates.io. A `pyproject.toml` that only configures tools does not count.

touchgate reads and sets `project.version` in `pyproject.toml`, and updates `uv.lock` if the repository has one. If the `build-backend` is `maturin`, the packages of the Cargo workspace get the same version, published or not. `check` fails if their versions differ. touchgate does not release pre-releases to PyPI yet, because PyPI writes their versions differently from Cargo.

`examples/pypi-release.yml` runs these commands:

- `touchgate verify` checks the approval.
- `touchgate check --build` checks the version against the changelog. It also fails if `uv build` would download `uv_build` from PyPI instead of using the copy in uv. uv downloads it when the range in `build-system.requires` does not include the running uv. The example pins uv with `UV_VERSION`, so the backend is pinned too. When you move `UV_VERSION` past the range, widen the range. A package with another backend, such as maturin, passes, because uv always downloads that backend.
- `touchgate wait-for-checks` waits for every check on the release commit to pass.
- `touchgate publish dist` checks that `dist` holds only the wheels and sdist of this version. It signs a PEP 740 attestation for each file with `pypi-attestations`, and uploads the files with `uv publish` through Trusted Publishing. It skips files that PyPI has already. It runs only in a GitHub Actions job with `id-token: write`.
- `touchgate github-release dist/*` makes the tag and the GitHub release, with the built files attached.

touchgate installs `pypi-attestations` from a lock in its own source, which fixes each dependency by the hashes of its files. A new touchgate release can update that lock.

The example builds with `uv build`. For maturin, build the wheels in the `build` job with [PyO3/maturin-action](https://github.com/PyO3/maturin-action) in a matrix, and download them all into `dist`.

## touchgate's own releases

touchgate releases itself with its own `touchgate.yml` and `release.yml`:

- `touchgate.yml` is `examples/touchgate.yml`, except that each job builds touchgate from source at the commit it runs on. CI fails if the two files differ in anything else.
- `release.yml` builds binaries for `x86_64-unknown-linux-musl` and `aarch64-unknown-linux-musl` with a pinned Rust version. A job that builds nothing attests them. The last job attaches them to the GitHub release.

touchgate publishes nothing to crates.io.
