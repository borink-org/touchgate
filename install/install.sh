#!/usr/bin/env bash
# Installs touchgate for the rest of the job, as `action.yml` describes: the
# binary of the touchgate release at $COMMIT, or at the commit this action is
# pinned to, once `gh attestation verify` shows that touchgate's release
# workflow built it on the protected `publish` branch. With $FROM_SOURCE,
# builds $COMMIT with cargo instead.
set -euo pipefail

repo=borink-org/touchgate
bin="$RUNNER_TEMP/touchgate-bin"

# A pinned action runs from `_actions/borink-org/touchgate/<ref>/install`.
commit=${COMMIT:-$(basename "$(dirname "$GITHUB_ACTION_PATH")")}
if [[ ! $commit =~ ^[0-9a-f]{40}$ ]]; then
  echo "::error::pin borink-org/touchgate/install to a full commit hash, not \`$commit\`"
  exit 1
fi

if [[ $FROM_SOURCE == true ]]; then
  cargo install --locked --root "$RUNNER_TEMP/touchgate" \
    --git "https://github.com/$repo" --rev "$commit" touchgate
  echo "$RUNNER_TEMP/touchgate/bin" >> "$GITHUB_PATH"
  exit 0
fi

if [[ $RUNNER_OS != Linux ]]; then
  echo "::error::touchgate releases binaries for Linux alone, not $RUNNER_OS"
  exit 1
fi
target="$(uname -m)-unknown-linux-musl"

# The release is the tag that names this commit, at the version the commit
# gives touchgate.
version=$(gh api "repos/$repo/contents/Cargo.toml?ref=$commit" \
  -H 'Accept: application/vnd.github.raw' |
  sed -n 's/^version = "\(.*\)"$/\1/p' | head -n 1)
tagged=$(gh api "repos/$repo/commits/v$version" --jq .sha 2>/dev/null || true)
if [[ $tagged != "$commit" ]]; then
  echo "::error::$commit is not a touchgate release: v$version names ${tagged:-no commit}. Pin the commit of a release, or set from-source"
  exit 1
fi

mkdir -p "$bin"
gh release download "v$version" --repo "$repo" \
  --pattern "touchgate-$target" --output "$bin/touchgate" --clobber
gh attestation verify "$bin/touchgate" --repo "$repo" \
  --signer-workflow "$repo/.github/workflows/release.yml" \
  --source-ref refs/heads/publish --deny-self-hosted-runners
chmod +x "$bin/touchgate"
echo "$bin" >> "$GITHUB_PATH"
echo "installed touchgate $version ($target) from $commit"
