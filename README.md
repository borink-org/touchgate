# touchgate

Releases a Cargo workspace from a hand-written `CHANGELOG.md`, and lets CI publish to crates.io only after you approve with a FIDO2 security key, by touch and PIN. Nothing holding your GitHub token, such as a coding agent, can publish on its own.

## Release

1. Run the `prepare-release` workflow with a version. It dates `## Unreleased`, sets the version and opens a pull request.
2. Once its checks pass, approve on that branch and push:

   ```nu
   git commit --allow-empty -S -m "Approve release 0.4.0."
   ```

3. Within the hour, merge with a merge commit. The release publishes to crates.io, then tags and makes the GitHub release.

## What it checks

The merge's second parent must be the approval: same tree as the merge, at most an hour old, signed by the one key in `release.yml`, with the signature's flags showing a touch and the PIN. git and `ssh-keygen` accept FIDO2 signatures made without either, so touchgate reads those flags itself. It runs no code from the commit it checks.

## What it relies on

- Your everyday account has Maintain, never Admin, on the repository and on touchgate. A separate admin account, used only in the browser, makes the settings below.
- `release.yml` runs from a `publish` branch that a ruleset lets only the admin change, in a `release` environment limited to exactly that branch.
- crates.io trusts only `release.yml` in the `release` environment, with Trusted Publishing only on and no API tokens left.
- Immutable releases are on.

## Set up

Make a resident key on the security key:

```nu
ssh-keygen -t ed25519-sk -O resident -O verify-required -O application=ssh:signing -f ~/.ssh/id_ed25519_sk_signing
git config --global gpg.format ssh
git config --global user.signingkey ~/.ssh/id_ed25519_sk_signing.pub
```

Copy `workflows/` into `.github/workflows/`, set `TOUCHGATE_REV`, the default branch and, in `release.yml`, `RELEASE_KEY`. Commit `release.yml` to both the default branch and `publish`.
