#!/usr/bin/env bash
#
# One-way mirror: push the current state of apps/provider/ to the public
# github.com/rahulbats/doodleiq-agent repo. Develop here in the monorepo; run
# this when you want the public repo to catch up (e.g. before cutting a release).
#
# It replaces the public repo's tree wholesale with the monorepo's tracked
# files for this crate, so the public history stays a clean series of snapshots
# with none of the monorepo's private commits.
#
#   apps/provider/packaging/sync-public.sh ["commit message"]
set -euo pipefail

REMOTE="${DOODLEIQ_AGENT_REMOTE:-https://github.com/rahulbats/doodleiq-agent.git}"
here="$(cd "$(dirname "$0")" && pwd)"
crate="$(cd "$here/.." && pwd)"
msg="${1:-sync from monorepo $(date -u +%Y-%m-%dT%H:%MZ)}"

work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

git clone --quiet "$REMOTE" "$work/pub"
cd "$crate"

# Wipe everything tracked in the public repo except its .git, then copy in the
# monorepo's tracked files for this crate.
find "$work/pub" -mindepth 1 -maxdepth 1 ! -name .git -exec rm -rf {} +
git ls-files | while read -r f; do
  mkdir -p "$work/pub/$(dirname "$f")"
  cp "$f" "$work/pub/$f"
done

cd "$work/pub"
if git diff --quiet && git diff --cached --quiet && [ -z "$(git status --porcelain)" ]; then
  echo "public repo already up to date"
  exit 0
fi
git add -A
git commit --quiet -m "$msg"
git push --quiet origin HEAD
echo "pushed to $REMOTE: $msg"
