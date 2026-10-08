#!/usr/bin/env bash
# scripts/release.sh — cut an aviary release and publish it to the Homebrew tap.
#
#   scripts/release.sh 0.2.0             bump · commit · tag · push · update the tap
#   scripts/release.sh 0.2.0 --dry-run   preflight + print every step, change nothing
#   scripts/release.sh 0.2.0 --tap-only  the tag already exists on GitHub: just redo the tap
#   scripts/release.sh 0.2.0 --verify    … then brew upgrade + brew test on this machine
#   scripts/release.sh 0.2.0 --yes       no confirmation prompts before pushing
#
# What it does, in order:
#   1. preflight    clean tree, on main and not behind origin, tag free,
#                   cargo test + clippy clean, tap checkout present and clean
#   2. bump         Cargo.toml version (+ Cargo.lock via cargo build), --version agrees
#   3. tag          commit "Release vX.Y.Z", annotated tag vX.Y.Z, push main + tag
#   4. tarball      wait for GitHub to serve archive/refs/tags/vX.Y.Z.tar.gz, sha256 it
#   5. tap          rewrite url + sha256 in Formula/aviary.rb, commit "aviary X.Y.Z", push
#   6. verify       (--verify) brew update · upgrade/reinstall · brew test · aviary --version
#
# Env:
#   AVIARY_TAP_DIR   the homebrew-aviary checkout (default: ../homebrew-aviary beside this repo)
#
# Everything talks to GitHub over plain git/curl — no gh account juggling.
set -euo pipefail

# ------------------------------------------------------------------ helpers
bold=$(tput bold 2>/dev/null || true); dim=$(tput dim 2>/dev/null || true); reset=$(tput sgr0 2>/dev/null || true)
step() { printf '\n%s▸ %s%s\n' "$bold" "$*" "$reset"; }
note() { printf '  %s\n' "$*"; }
die()  { printf '\n  ✗ %s\n' "$*" >&2; exit 1; }
run()  { # echo a command; execute it unless --dry-run
  printf '  %s$ %s%s\n' "$dim" "$*" "$reset"
  if [[ $DRY -eq 0 ]]; then "$@"; fi
}
confirm() {
  [[ $YES -eq 1 || $DRY -eq 1 ]] && return 0
  read -r -p "  $1 [y/N] " answer
  [[ $answer == y || $answer == Y ]] || die "stopped — nothing pushed"
}

# --------------------------------------------------------------------- args
DRY=0; TAP_ONLY=0; VERIFY=0; YES=0; VERSION=""
for arg in "$@"; do
  case $arg in
    --dry-run)  DRY=1 ;;
    --tap-only) TAP_ONLY=1 ;;
    --verify)   VERIFY=1 ;;
    --yes|-y)   YES=1 ;;
    -h|--help)  sed -n '2,22p' "$0"; exit 0 ;;
    -*)         die "unknown flag $arg (see --help)" ;;
    *)          VERSION=${arg#v} ;;
  esac
done
[[ -n $VERSION ]] || die "usage: scripts/release.sh X.Y.Z [--dry-run] [--tap-only] [--verify] [--yes]"
[[ $VERSION =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "version must be X.Y.Z (got '$VERSION')"
TAG="v$VERSION"

for tool in git cargo curl shasum perl; do
  command -v "$tool" >/dev/null || die "$tool is required"
done

REPO=$(git rev-parse --show-toplevel)
cd "$REPO"
TAP=${AVIARY_TAP_DIR:-"$(dirname "$REPO")/homebrew-aviary"}
FORMULA="$TAP/Formula/aviary.rb"

# owner/repo from the origin url — https://github.com/O/R(.git) or git@github.com:O/R(.git)
ORIGIN=$(git remote get-url origin)
SLUG=$(printf '%s' "$ORIGIN" | sed -E 's#^(https://github\.com/|git@github\.com:)##; s#\.git$##')
[[ $SLUG == */* ]] || die "origin is not a GitHub url: $ORIGIN"
TARBALL="https://github.com/$SLUG/archive/refs/tags/$TAG.tar.gz"

[[ $DRY -eq 1 ]] && note "${bold}dry run${reset} — printing every step, changing nothing"

# ---------------------------------------------------------------- preflight
step "preflight $TAG"
CURRENT=$(perl -ne 'print $1 and exit if /^version = "([^"]+)"/' Cargo.toml)
note "Cargo.toml is $CURRENT → $VERSION"
[[ $TAP_ONLY -eq 1 || $CURRENT != "$VERSION" ]] || die "Cargo.toml is already $VERSION — did you mean --tap-only?"

BRANCH=$(git rev-parse --abbrev-ref HEAD)
[[ $BRANCH == main ]] || die "on '$BRANCH' — releases cut from main"
[[ -z $(git status --porcelain --untracked-files=no) ]] || die "working tree has uncommitted changes — commit or stash first"
git fetch -q origin main
[[ $(git rev-list --count HEAD..origin/main) -eq 0 ]] || die "main is behind origin/main — pull first"

if [[ $TAP_ONLY -eq 1 ]]; then
  git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || die "--tap-only needs the tag $TAG to exist locally"
else
  ! git rev-parse -q --verify "refs/tags/$TAG" >/dev/null || die "tag $TAG already exists — bump higher, or --tap-only to redo the formula"
fi

[[ -f $FORMULA ]] || die "no formula at $FORMULA — set AVIARY_TAP_DIR to your homebrew-aviary checkout"
[[ -z $(git -C "$TAP" status --porcelain --untracked-files=no) ]] || die "tap checkout at $TAP has uncommitted changes"
note "tap: $TAP"

if [[ $TAP_ONLY -eq 0 ]]; then
  note "cargo test + clippy …"
  LOG=$(mktemp)
  cargo test -q >"$LOG" 2>&1 || { cat "$LOG"; rm -f "$LOG"; die "cargo test failed"; }
  cargo clippy -q --all-targets -- -D warnings >"$LOG" 2>&1 || { cat "$LOG"; rm -f "$LOG"; die "clippy is not clean"; }
  rm -f "$LOG"
  note "✓ tests and clippy clean"
fi

# --------------------------------------------------------------------- bump
if [[ $TAP_ONLY -eq 0 ]]; then
  step "bump Cargo.toml → $VERSION"
  # First `version = "…"` line only: the flag is set when a substitution
  # actually happens, not merely when a line is visited.
  BUMP="if (!\$done && s/^version = \"\\Q$CURRENT\\E\"/version = \"$VERSION\"/) { \$done = 1 }"
  if [[ $DRY -eq 1 ]]; then
    PREVIEW=$(mktemp); cp Cargo.toml "$PREVIEW"
    perl -pi -e "$BUMP" "$PREVIEW"
    note "would write: $(grep -m1 '^version = ' "$PREVIEW")"
    rm -f "$PREVIEW"
  fi
  run perl -pi -e "$BUMP" Cargo.toml
  if [[ $DRY -eq 0 ]]; then
    grep -q "^version = \"$VERSION\"" Cargo.toml || die "the version bump did not apply to Cargo.toml"
  fi
  run cargo build -q          # refreshes Cargo.lock's aviary entry
  if [[ $DRY -eq 0 ]]; then
    GOT=$(./target/debug/aviary --version)
    [[ $GOT == "aviary $VERSION" ]] || die "binary reports '$GOT', expected 'aviary $VERSION'"
    note "✓ aviary --version → $GOT"
  fi

  # ------------------------------------------------------------------- tag
  step "commit + tag $TAG"
  run git add Cargo.toml Cargo.lock
  run git commit -q -m "Release $TAG"
  run git tag -a "$TAG" -m "aviary $VERSION"
  confirm "push main and $TAG to origin ($SLUG)?"
  run git push -q origin main
  run git push -q origin "$TAG"
fi

# ------------------------------------------------------------------ tarball
step "tarball sha256"
note "$TARBALL"
if [[ $DRY -eq 1 && $TAP_ONLY -eq 0 ]]; then
  SHA="<sha256 of the tarball once $TAG is pushed>"
else
  # --tap-only: the tag is already up, so even a dry run can show the real sha.
  TMP=$(mktemp)
  trap 'rm -f "$TMP"' EXIT
  for attempt in $(seq 1 30); do
    if curl -sfL -o "$TMP" "$TARBALL"; then break; fi
    [[ $attempt -lt 30 ]] || die "GitHub never served $TARBALL — is the repo public and the tag pushed?"
    printf '  waiting for GitHub to build the tarball (%s/30)\r' "$attempt"
    sleep 2
  done
  SHA=$(shasum -a 256 "$TMP" | cut -d' ' -f1)
fi
note "sha256 $SHA"

# ---------------------------------------------------------------------- tap
step "update the tap formula"
run perl -pi -e "s#^(\\s*url \")[^\"]*(\")#\${1}$TARBALL\${2}#; s#^(\\s*sha256 \")[^\"]*(\")#\${1}$SHA\${2}#" "$FORMULA"
if [[ $DRY -eq 0 ]]; then
  grep -E '^\s*(url|sha256) ' "$FORMULA" | sed 's/^/  /'
  if [[ -z $(git -C "$TAP" status --porcelain) ]]; then
    note "formula already at $TAG — nothing to commit"
  else
    run git -C "$TAP" commit -q -am "aviary $VERSION"
    confirm "push the tap ($(git -C "$TAP" remote get-url origin))?"
    run git -C "$TAP" push -q origin main
  fi
fi

# ------------------------------------------------------------------- verify
if [[ $VERIFY -eq 1 && $DRY -eq 0 ]]; then
  step "verify via Homebrew"
  run brew update -q
  run brew upgrade jaydengarrick/aviary/aviary || run brew reinstall jaydengarrick/aviary/aviary
  run brew test jaydengarrick/aviary/aviary
  run /opt/homebrew/bin/aviary --version
fi

step "released $TAG"
note "friends: brew upgrade aviary   (first install: brew install jaydengarrick/aviary/aviary)"
[[ $VERIFY -eq 1 || $DRY -eq 1 ]] || note "optional: scripts/release.sh $VERSION --tap-only --verify  re-checks the formula end to end"
