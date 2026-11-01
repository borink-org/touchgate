#!/usr/bin/env bash
# Puts touchgate on the PATH for the rest of the job, as `action.yml`
# describes. The runner has checked out the whole touchgate commit that the
# action is pinned to, at `_actions/borink-org/touchgate/<commit>`.
set -euo pipefail

repo=borink-org/touchgate
source=$(dirname "$GITHUB_ACTION_PATH")
bin="$RUNNER_TEMP/touchgate/bin"

if [[ $FROM_SOURCE == true ]]; then
  CARGO_TARGET_DIR="$RUNNER_TEMP/touchgate/target" \
    cargo install --locked --path "$source" --root "$RUNNER_TEMP/touchgate"
  echo "$bin" >> "$GITHUB_PATH"
  exit 0
fi

# The release is the tag `v<version>`, which must name the pinned commit.
commit=$(basename "$source")
version=$(sed -n 's/^version = "\(.*\)"$/\1/p' "$source/Cargo.toml" | head -n 1)
tagged=$(gh api "repos/$repo/commits/v$version" --jq .sha 2>/dev/null) || tagged=""
if [[ $tagged != "$commit" ]]; then
  echo "::error::$commit is not the commit of a touchgate release. Pin the full commit that a release tag names, or set from-source: true"
  exit 1
elif [[ $RUNNER_OS != Linux ]]; then
  echo "::error::touchgate releases binaries for Linux only, not for $RUNNER_OS"
  exit 1
fi

file="touchgate-$(uname -m)-unknown-linux-musl"
gh release download "v$version" --repo "$repo" --pattern "$file" --dir "$bin" --clobber
gh attestation verify "$bin/$file" --repo "$repo" \
  --signer-workflow "$repo/.github/workflows/release.yml" \
  --source-ref refs/heads/publish --deny-self-hosted-runners
mv "$bin/$file" "$bin/touchgate"
chmod +x "$bin/touchgate"
echo "$bin" >> "$GITHUB_PATH"
