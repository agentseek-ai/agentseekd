#!/usr/bin/env bash
#
# generate-release-notes.sh — Generate the "What's Changed" section of
# release notes from conventional commits between the previous tag and HEAD.
#
# The "Install" download table is NOT generated here — it is provided by
# the "Create release preface" step in the workflow (RELEASE_PREFACE.md),
# which builds direct download URLs from the actual published assets.
#
# Usage: bash scripts/generate-release-notes.sh <current-tag> [previous-tag]
#
#   current-tag   The tag being released (e.g. v0.1.0).
#   previous-tag  Optional. The previous tag to compare against. When
#                 omitted the script resolves it automatically: stable
#                 releases use the previous stable tag; prereleases use
#                 the immediate ancestor tag.
#
# Categories (conventional commit prefixes):
#   feat     → ✨ Features
#   fix      → 🐛 Fixes
#   ci/test  → 🔧 CI & Tests
#   chore/release/refactor/revert → 📦 Release
#   docs     → 📝 Misc
#   *        → 📝 Other Changes (non-conventional)
#
set -euo pipefail

CURRENT_TAG="${1:-}"
if [ -z "$CURRENT_TAG" ]; then
  echo "Usage: $0 <current-tag> [previous-tag]" >&2
  exit 1
fi

# ── Resolve previous tag ──────────────────────────────────────────────
# If the caller (workflow) supplied a previous tag, trust it. Otherwise
# compute it: stable releases compare against the previous stable tag;
# prereleases compare against the immediate ancestor tag.
PREV_TAG="${2:-}"

if [ -z "$PREV_TAG" ]; then
  if printf '%s' "$CURRENT_TAG" | grep -Eq '^v[0-9]+\.[0-9]+\.[0-9]+$'; then
    # Stable release — find the previous stable tag (skip rc/beta/alpha).
    while IFS= read -r candidate; do
      if [ "$candidate" = "$CURRENT_TAG" ]; then
        break
      fi
      PREV_TAG="$candidate"
    done < <(git tag -l 'v*' --sort=v:refname 2>/dev/null \
              | grep -E '^v[0-9]+\.[0-9]+\.[0-9]+$' || true)
    # Fall back to the first release tag overall (may be a prerelease).
    if [ -z "$PREV_TAG" ]; then
      PREV_TAG="$(git tag -l 'v*' --sort=v:refname 2>/dev/null | head -n 1 || true)"
      [ "$PREV_TAG" = "$CURRENT_TAG" ] && PREV_TAG=""
    fi
  else
    # Prerelease — use the most recent tag reachable from HEAD's parent
    # (matches the workflow's git describe approach), then fall back to a
    # version-sorted tag list (deterministic, unlike --sort=-creatordate
    # which is unreliable for lightweight tags created in quick succession).
    PREV_TAG="$(git describe --tags --abbrev=0 --match 'v*' HEAD^ 2>/dev/null || true)"
    [ "$PREV_TAG" = "$CURRENT_TAG" ] && PREV_TAG=""
    if [ -z "$PREV_TAG" ]; then
      while IFS= read -r tag; do
        if [ "$tag" != "$CURRENT_TAG" ]; then
          PREV_TAG="$tag"
          break
        fi
      done < <(git tag -l 'v*' --sort=-v:refname 2>/dev/null)
    fi
  fi
fi

# Use the current tag as the range endpoint when it exists in git so the
# notes stay accurate even if HEAD has moved past the tag (e.g. local
# testing). Fall back to HEAD when the tag hasn't been created yet.
if git rev-parse --verify "${CURRENT_TAG}^{commit}" >/dev/null 2>&1; then
  RANGE_END="$CURRENT_TAG"
else
  RANGE_END="HEAD"
fi

if [ -n "$PREV_TAG" ]; then
  RANGE="${PREV_TAG}..${RANGE_END}"
else
  RANGE="$RANGE_END"
fi

# ── Extract commits by conventional commit type ───────────────────────
# Output: bullet list (- description), one per line
extract() {
  local pattern="$1"
  git log --pretty=format:"%s" "$RANGE" 2>/dev/null \
    | grep -iE "^${pattern}(\(|:)" \
    | sed -E 's/^[a-zA-Z]+(\([^)]*\))?: /- /' \
    || true
}

FEATURES=$(extract "feat")
FIXES=$(extract "fix")
CI_CHANGES=$(extract "(ci|test)")
RELEASE_CHANGES=$(extract "(chore|release|refactor|revert)")
DOCS=$(extract "docs")

# Catch non-conventional commits (skip merge commits)
OTHER=$(git log --pretty=format:"%s" "$RANGE" 2>/dev/null \
  | grep -ivE "^(feat|fix|ci|test|chore|release|refactor|revert|docs)(\(|:)" \
  | grep -ivE "^Merge " \
  | sed 's/^- /- /; s/^/- /' \
  || true)

# ── Count helper (handles empty strings safely) ───────────────────────
count_lines() {
  if [ -z "$1" ]; then
    echo 0
  else
    echo "$1" | wc -l | tr -d ' '
  fi
}

feat_count=$(count_lines "$FEATURES")
fix_count=$(count_lines "$FIXES")
ci_count=$(count_lines "$CI_CHANGES")
release_count=$(count_lines "$RELEASE_CHANGES")
docs_count=$(count_lines "$DOCS")
other_count=$(count_lines "$OTHER")

# ── Build markdown ────────────────────────────────────────────────────
# The "Install" section is prepended by the workflow's RELEASE_PREFACE.md.

echo "## What's Changed"
echo ""

if [ "$feat_count" -gt 0 ]; then
  echo "### ✨ Features"
  echo ""
  echo "Summary: ${feat_count} change(s) shipped in this area."
  echo ""
  echo "$FEATURES"
  echo ""
fi

if [ "$fix_count" -gt 0 ]; then
  echo "### 🐛 Fixes"
  echo ""
  echo "Summary: ${fix_count} change(s) shipped in this area."
  echo ""
  echo "$FIXES"
  echo ""
fi

if [ "$ci_count" -gt 0 ]; then
  echo "### 🔧 CI & Tests"
  echo ""
  echo "Summary: ${ci_count} change(s) shipped in this area."
  echo ""
  echo "$CI_CHANGES"
  echo ""
fi

if [ "$release_count" -gt 0 ]; then
  echo "### 📦 Release"
  echo ""
  echo "Summary: ${release_count} change(s) shipped in this area."
  echo ""
  echo "$RELEASE_CHANGES"
  echo ""
fi

if [ "$docs_count" -gt 0 ]; then
  echo "### 📝 Misc"
  echo ""
  echo "Summary: ${docs_count} change(s) shipped in this area."
  echo ""
  echo "$DOCS"
  echo ""
fi

if [ "$other_count" -gt 0 ]; then
  echo "### 📝 Other Changes"
  echo ""
  echo "Summary: ${other_count} change(s) shipped in this area."
  echo ""
  echo "$OTHER"
  echo ""
fi

# ── Full Changelog link ───────────────────────────────────────────────
REPO="${GITHUB_REPOSITORY:-agentseek-ai/agentseekd}"
if [ -n "$PREV_TAG" ]; then
  echo "Full Changelog: https://github.com/${REPO}/compare/${PREV_TAG}...${CURRENT_TAG}"
else
  echo "Full Changelog: https://github.com/${REPO}/commits/${CURRENT_TAG}"
fi
