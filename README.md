# touchgate

touchgate releases a Cargo workspace from a hand-written changelog, and lets CI publish to crates.io only after a person approved the release with a FIDO2 security key, by touch and PIN. No one edits a version by hand, and nothing with access to your GitHub account, such as a coding agent, can publish on its own.

## The release

1. Start the `prepare-release` workflow with a version. It turns `## Unreleased` in `CHANGELOG.md` into the section of that version, sets the version on every published package and every requirement on one, and opens a pull request.
2. Approve: check out the pull request's branch, look it over, and add an empty commit signed with your security key. The key asks for its PIN and a touch.

   ```nu
   git commit --allow-empty -S -m "Approve release 0.4.0."
   git push
   ```

3. Merge the pull request with a merge commit.
4. `start-release` starts `release.yml` on the `publish` branch. It checks the approval, publishes every published package to crates.io, and only then tags the commit and makes the GitHub release, with the changelog section as its notes.

A release that fails halfway can run again: packages on crates.io already are skipped, and so is a GitHub release that exists.

## What the approval proves

`release.yml` publishes a merge commit on the default branch only if:

- its second parent is a commit whose subject starts with `Approve release`,
- that commit has the same tree as the merge, so the signature covers exactly what is published,
- that commit is signed in git's namespace by the one key `release.yml` names, and `ssh-keygen -Y verify` accepts the signature,
- the signature's authenticator flags say the key was touched and its PIN entered,
- and the approval is at most 7 days old by its committer time, which the signature covers, so an approval you abandoned cannot be merged and released later.

The last check is why touchgate exists. `git verify-commit` and `ssh-keygen -Y verify` accept a FIDO2 signature made without a touch or a PIN: the allowed-signers format has no option that demands either, and a FIDO2 key asked for a silent assertion signs without one. The authenticator signs its flags byte along with the message, so touchgate can read it and cannot be fooled by a changed one.

The check runs no code from the commit it checks, because until it passes, a `rust-toolchain.toml`, a `.cargo/config.toml` or a build script in the commit could fake a pass. touchgate is built before the checkout and outside it. It reads commits as raw objects with `git cat-file` and parses them itself, with replacement objects and the user's and system's git configuration turned off, so neither a `refs/replace/` ref nor a setting such as `log.showSignature` changes what it reads. It leaves the cryptography to `ssh-keygen`.

After the check, `release.yml` keeps each step to the least it needs:

- The build, which runs the build scripts and procedural macros of every dependency, runs in a job that cannot get a crates.io token. The job that publishes compiles nothing.
- Anyone who can push can create tags and releases. If the tag or the GitHub release of the version exists already and does not point at the release commit, the workflow fails instead of reusing it.

## What it relies on

The check is only as strong as the settings around it. crates.io trusts whatever GitHub vouches for, so the guarantee holds if nothing your everyday tools can reach is able to change what GitHub vouches for:

- **An administrator account that your tools never hold.** A second GitHub account owns the organization, signs in only in the browser, and keeps its credentials in a password manager. Your everyday account, whose token your tools and agents use, has the Maintain or Write role on the repository, never Admin. Anyone with Admin can change the environment and the rulesets below.
- **A protected `publish` branch.** It holds the trusted copy of `release.yml`. A ruleset on it restricts creation, updates and deletion, and blocks force pushes, with only the administrator allowed to bypass it. GitHub cannot protect a single file on its free plan, so the branch carries the file.
- **A `release` environment** whose deployment branches are the `publish` branch alone. A copy of `release.yml` on any other branch, rewritten or not, gets no crates.io token.
- **crates.io trusted publishing** for each published package, with the repository, the workflow `release.yml` and the environment `release`. Turn on **Trusted Publishing only** for each package, and revoke every API token, including the one `cargo login` left in `~/.cargo/credentials.toml`. A token with the trusted-publishing scope could turn the setting back off.

- **The touchgate repository itself**, if it is yours, administered the same way. `release.yml` pins a touchgate commit, which no push can change, but moving the pin trusts the new code: read the change before the administrator commits it to the `publish` branch.
- **Immutable releases**, a repository setting, so that a published GitHub release and its tag cannot be edited or deleted afterwards.

Your everyday account can still change every other workflow, push to the default branch and open pull requests. What it cannot do is make the signature, change the code that checks it, or change what crates.io trusts. It can still get in the way: start runs that fail, or delay a release by queueing runs behind it.

## Set up the key

touchgate needs a FIDO2 SSH key: `sk-ssh-ed25519` or `sk-ecdsa-sha2-nistp256`. A resident key lives on the security key itself, so any machine can fetch it.

```nu
(
  ssh-keygen -t ed25519-sk
    -O resident
    -O verify-required
    -O application=ssh:signing
    -C "release signing"
    -f ~/.ssh/id_ed25519_sk_signing
)
```

On another machine, `ssh-keygen -K` writes the handle and public key of every resident key on the security key.

Point git at the key. Leave `commit.gpgsign` off, or every commit asks for the PIN and a touch.

```nu
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519_sk_signing.pub
```

## Set up a repository

1. Copy the four workflows in `workflows/` into `.github/workflows/`. In each, set `TOUCHGATE_REV` to a touchgate commit and replace `main` with your default branch. In `release.yml`, set `RELEASE_KEY` to the contents of your `.pub` file.
2. Commit `release.yml` on the default branch too: GitHub dispatches a workflow only if the default branch has it. The copy on the `publish` branch is the one that runs.
3. As the administrator: create the `publish` branch from the default branch, add its ruleset, and create the `release` environment limited to it by its exact name, not a pattern. Turn on immutable releases. Under Actions settings, allow GitHub Actions to create pull requests. Allow merge commits. Lower your everyday account to Maintain.
4. On crates.io: add the trusted publisher for each published package, turn on Trusted Publishing only, and revoke API tokens.
5. Create a `no-changelog` label for pull requests that have nothing to say in the changelog.

To change `release.yml` later, the administrator pushes the change to the `publish` branch, and the same change goes to the default branch.

## The changelog

`touchgate check` holds on every commit. `CHANGELOG.md` at the root of the workspace opens with `# Changelog`. Each section is headed `## Unreleased`, which may only come first, or `## X.Y.Z - YYYY-MM-DD`, with versions falling and dates not rising down the file. A section may divide its entries under the headings of [Keep a Changelog](https://keepachangelog.com): Added, Changed, Deprecated, Removed, Fixed and Security. No heading is empty. Each paragraph and list item is one line. The newest released section is the version every published package is at.

touchgate releases every published package of a workspace together, at one version.

## Commands

| Command | What it does |
|---|---|
| `touchgate check` | Checks the changelog and the versions, and prints the version. |
| `touchgate prepare <version> [--date YYYY-MM-DD]` | Releases `## Unreleased` as `<version>`, dated today in UTC unless given, and sets the version. |
| `touchgate check-entry <base>` | Fails if the changes since `<base>` touch a published package but add nothing under `## Unreleased`. |
| `touchgate notes [<version>]` | Prints the section of a version, for the notes of a GitHub release. |
| `touchgate verify --repo <dir> --commit <hash> --branch <ref> --key <public key> [--max-age-days <days>]` | Checks a release's approval, at most 7 days old unless given, and prints the approval commit. |
| `touchgate publish [<cargo publish arguments>]` | Publishes the packages whose version is not on crates.io yet. |

## License

Licensed under either the [Apache License, Version 2.0](LICENSE-APACHE) or the [MIT license](LICENSE-MIT), at your option.
