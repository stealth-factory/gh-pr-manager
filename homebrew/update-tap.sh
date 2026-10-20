#!/usr/bin/env bash
set -euo pipefail

# Writes Formula/gh-pr-manager.rb on the shared stealth-factory/homebrew-tap.
# Ported from opscope's homebrew/update-tap.sh.
#
# GH_TOKEN must have Contents write access to the tap. Never put it in a URL.
version="${1:?Usage: bash homebrew/update-tap.sh VERSION ARTIFACTS}"
artifacts="${2:?An artefact directory is required}"
script_dir=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
endpoint=repos/stealth-factory/homebrew-tap/contents/Formula/gh-pr-manager.rb

# The Contents API updates only this file, so another application's formula
# on the shared tap is untouched. The file SHA gives compare-and-swap
# protection; on a conflict, read it again rather than force-pushing.
for attempt in 1 2 3; do
  if gh api "$endpoint?ref=main" > "$work/current.json" 2> "$work/err"; then
    jq -r .content "$work/current.json" | base64 --decode > "$work/current.rb"
    previous="$work/current.rb"
  elif grep -q 'HTTP 404' "$work/err"; then
    # Unlike opscope, which needed its first formula merged by hand, the
    # first release creates the file: a PUT without a sha is a create.
    echo "Formula/gh-pr-manager.rb does not exist on the tap yet; creating it"
    echo '{}' > "$work/current.json"
    previous=""
  else
    cat "$work/err" >&2
    echo "Cannot read Formula/gh-pr-manager.rb on the tap's main branch; check the token can access stealth-factory/homebrew-tap." >&2
    exit 1
  fi
  rm -f "$work/formula.rb"
  node "$script_dir/formula.js" "$version" "$artifacts" "$work/formula.rb" ${previous:+"$previous"}
  if [ ! -f "$work/formula.rb" ] || { [ -n "$previous" ] && cmp -s "$previous" "$work/formula.rb"; }; then
    echo "Homebrew formula already matches or exceeds $version"
    exit 0
  fi
  jq --rawfile formula "$work/formula.rb" --arg version "$version" \
    '{message: ("chore: update gh-pr-manager to " + $version), branch: "main",
      content: ($formula | @base64)} + (if .sha then {sha: .sha} else {} end)' \
    "$work/current.json" > "$work/update.json"
  if gh api --method PUT "$endpoint" --input "$work/update.json" --silent; then
    echo "Updated Homebrew formula to $version"
    exit 0
  fi
  echo "Tap update failed (attempt $attempt); reading it again" >&2
done
echo "Homebrew tap update failed after three attempts" >&2
exit 1
