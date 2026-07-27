#!/usr/bin/env bash
# Usage: check-tracked-paths.sh [--tracked|--staged] [--require-ignored]
#                               [--write-exclude] [-h]
#
# Guards the SHAPE of the published file set.  Three independent jobs, split by
# AUDIENCE, because they are not properties of the same thing:
#
#   1. no tracked path may match the denylist below (assistant tooling, build
#      output, packaging scratch, key material, editor droppings);
#   2. .gitignore must NOT name assistant tooling -- a line reading ".claude/"
#      in a published file is itself evidence of the toolchain, which is the
#      whole reason those paths live in .git/info/exclude instead;
#   3. every assistant tooling path must nevertheless BE ignored.  `git
#      check-ignore` does not care which file did it, so requirement 2 and
#      requirement 3 are not in conflict: one file must not name them, some
#      file must.
#
# Checks 1 and 2 are properties of the PUBLISHED TREE: they hold in any clone,
# they are what actually protect the repository, and they run everywhere,
# including CI.
#
# Check 3 is a property of the LOCAL WORKING COPY only.  The sanctioned home
# for those patterns is .git/info/exclude, which is deliberately never cloned,
# so a fresh checkout can never satisfy check 3 -- running it in CI produces a
# permanently red job for a condition no contributor can fix, and a gate that
# is always red is a gate that gets switched off.  It therefore runs ONLY when
# asked for with --require-ignored (which scripts/preflight-publish.sh and the
# pre-commit hook both pass), and it announces itself when it does not run.
#
# Needs no configuration and no secrets, so checks 1 and 2 also run in CI.

set -euo pipefail

PROG=${0##*/}

usage() {
  cat <<'EOF'
Usage: check-tracked-paths.sh [options]

Options:
  --tracked          check every tracked path (default)
  --staged           check only paths staged for the next commit (pre-commit
                     hook)
  --require-ignored  additionally require that every assistant-tooling path is
                     ignored by something (check 3).  A property of the local
                     working copy, not of the published tree: the patterns live
                     in .git/info/exclude, which is never cloned, so this can
                     never hold in CI.  Off by default for that reason.
  --write-exclude    append any missing assistant-tooling pattern to
                     .git/info/exclude, then re-check.  Local-only file;
                     nothing it contains is ever pushed.  Implies
                     --require-ignored.
  -h, --help         this text

Exit status: 0 clean, 1 findings, 2 usage or configuration error.
EOF
}

die() {
  printf '%s: %s\n' "$PROG" "$1" >&2
  exit 2
}

MODE='tracked'
WRITE_EXCLUDE=0
REQUIRE_IGNORED=0

while [ $# -gt 0 ]; do
  case $1 in
    --tracked) MODE='tracked' ;;
    --staged) MODE='staged' ;;
    --require-ignored) REQUIRE_IGNORED=1 ;;
    --write-exclude)
      WRITE_EXCLUDE=1
      REQUIRE_IGNORED=1
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
  shift || true
done

git rev-parse --is-inside-work-tree >/dev/null 2>&1 || die "not inside a git work tree"
ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"

FINDINGS=0
fail() {
  printf 'FAIL  %s\n' "$1" >&2
  FINDINGS=$((FINDINGS + 1))
}

# --------------------------------------------------------------------------
# Denylist of path shapes that must never be tracked.
# Each entry is "<ERE>|<reason>".  The ERE itself contains '|', so the split is
# on the LAST one: pattern=${entry%|*}, reason=${entry##*|}.
# --------------------------------------------------------------------------
DENY=(
  '(^|/)\.claude(/|$)|assistant tooling'
  '(^|/)\.cursor(rules)?(/|$)|assistant tooling'
  '(^|/)\.aider[^/]*$|assistant tooling'
  '(^|/)\.codeium(/|$)|assistant tooling'
  '(^|/)\.windsurf(rules)?(/|$)|assistant tooling'
  '(^|/)\.continue(/|$)|assistant tooling'
  '(^|/)\.roo(/|$)|assistant tooling'
  '(^|/)\.specstory(/|$)|assistant tooling'
  '(^|/)\.opencode(/|$)|assistant tooling'
  '(^|/)\.junie(/|$)|assistant tooling'
  '(^|/)\.amazonq(/|$)|assistant tooling'
  '(^|/)\.kiro(/|$)|assistant tooling'
  '(^|/)\.goose(/|$)|assistant tooling'
  '(^|/)\.crush(/|$)|assistant tooling'
  '(^|/)\.taskmaster(/|$)|assistant tooling'
  '(^|/)\.mcp\.json$|assistant tooling'
  '(^|/)CLAUDE\.md$|assistant tooling'
  '(^|/)AGENTS\.md$|assistant tooling'
  '(^|/)GEMINI\.md$|assistant tooling'
  '(^|/)\.github/copilot-instructions\.md$|assistant tooling'
  '(^|/)\.ssh(/|$)|SSH material'
  '(^|/)id_(rsa|dsa|ecdsa|ed25519)(_sk)?(\.pub)?$|SSH key file'
  '(^|/)[^/]+_(rsa|dsa|ecdsa|ed25519)(_sk)?(\.pub)?$|SSH key file'
  '(^|/)(known_hosts|authorized_keys|\.netrc|\.pgpass)$|credential store'
  '(^|/)[^/]+\.(pem|key|p12|pfx|jks|keystore|kdbx)$|key material'
  '(^|/)\.env(\.|$)|environment file'
  '(^|/)\.envrc$|environment file'
  '(^|/)target(/|$)|build output'
  '(^|/)pkg(/|$)|makepkg staging directory'
  '(^|/)PKGBUILD$|the PKGBUILD lives in the separate AUR repository'
  '(^|/)\.SRCINFO$|packaging metadata belongs to the AUR repository'
  '[^/]+\.pkg\.tar\.[a-z]+$|built package'
  '[^/]+\.(profraw|profdata)$|coverage data'
  '(^|/)\.DS_Store$|macOS metadata'
  '(^|/)\._[^/]+$|macOS resource fork'
  '(^|/)\.idea(/|$)|editor state'
  '[^/]+\.(swp|swo|orig|rej|bak)$|editor or merge droppings'
  '[^/]+~$|editor backup'
  '(^|/)\.gitmodules$|no submodules: they publish another repository URL'
  '(^|/)man/[^/]+\.[1-8]$|generated roff -- author the scdoc source instead'
)

# Assistant tooling patterns that must be ignored by *something*.
ASSISTANT_PATHS=(
  '.claude/'
  '.cursor/'
  '.cursorrules'
  '.aider.conf.yml'
  '.aider.chat.history.md'
  '.codeium/'
  '.windsurf/'
  '.continue/'
  '.roo/'
  '.specstory/'
  '.opencode/'
  '.junie/'
  '.amazonq/'
  '.kiro/'
  '.goose/'
  '.crush/'
  '.taskmaster/'
  '.mcp.json'
  'CLAUDE.md'
  'AGENTS.md'
  'GEMINI.md'
)

# --------------------------------------------------------------------------
# 1. denylist over the file set
# --------------------------------------------------------------------------
LIST=$(mktemp "${TMPDIR:-/tmp}/idlectl-paths.XXXXXX")
trap 'rm -f "$LIST"' EXIT HUP INT TERM

if [ "$MODE" = staged ]; then
  git diff --cached --name-only --diff-filter=ACMR -z >"$LIST"
else
  git ls-files -z >"$LIST"
fi

while IFS= read -r -d '' path; do
  for entry in "${DENY[@]}"; do
    pattern=${entry%|*}
    reason=${entry##*|}
    if printf '%s\n' "$path" | grep -Eq -e "$pattern"; then
      fail "tracked path must not be published: $path  ($reason)"
      break
    fi
  done

  # A tracked file over 1 MiB in a daemon repository is almost always an
  # accident, and accidents are how binaries carrying metadata get published.
  if [ -f "$path" ] && [ ! -L "$path" ]; then
    size=$(wc -c <"$path" | tr -d ' ')
    if [ "$size" -gt 1048576 ]; then
      fail "tracked file is larger than 1 MiB ($size bytes): $path"
    fi
  fi

  # A symlink that leaves the work tree publishes an absolute path from the
  # author's machine.
  if [ -L "$path" ]; then
    target=$(readlink "$path")
    case $target in
      /* | *../*) fail "tracked symlink escapes the work tree: $path -> $target" ;;
    esac
  fi
done <"$LIST"

# --------------------------------------------------------------------------
# 2. .gitignore purity
# --------------------------------------------------------------------------
if [ -f .gitignore ]; then
  lineno=0
  while IFS= read -r raw || [ -n "$raw" ]; do
    lineno=$((lineno + 1))
    line=${raw%$'\r'}
    case $line in
      '' | '#'*) continue ;;
    esac
    for entry in "${DENY[@]}"; do
      reason=${entry##*|}
      [ "$reason" = 'assistant tooling' ] || continue
      pattern=${entry%|*}
      if printf '%s\n' "$line" | grep -Eq -e "$pattern"; then
        fail ".gitignore:$lineno names an assistant tooling path. A published \
ignore rule is itself evidence that the tool was used; move the entry to \
.git/info/exclude, which is never pushed."
        break
      fi
    done
  done <.gitignore
else
  fail ".gitignore is missing"
fi

# --------------------------------------------------------------------------
# 3. every assistant tooling path must actually be ignored
#
# LOCAL WORKING COPY ONLY.  The only sanctioned home for these patterns is
# .git/info/exclude, and git never clones that file, so this check cannot pass
# in a fresh checkout however correct the repository is.  It runs on request.
# --------------------------------------------------------------------------
if [ "$REQUIRE_IGNORED" -eq 1 ]; then
  MISSING=()
  for p in "${ASSISTANT_PATHS[@]}"; do
    if ! git check-ignore -q -- "$p" 2>/dev/null; then
      MISSING+=("$p")
    fi
  done

  if [ "${#MISSING[@]}" -gt 0 ] && [ "$WRITE_EXCLUDE" -eq 1 ]; then
    EXCLUDE=$(git rev-parse --git-path info/exclude)
    mkdir -p "$(dirname "$EXCLUDE")"
    {
      printf '\n# --- local-only ignores (never pushed) ---\n'
      for p in "${MISSING[@]}"; do printf '%s\n' "$p"; done
    } >>"$EXCLUDE"
    printf '%s: appended %s pattern(s) to %s\n' "$PROG" "${#MISSING[@]}" "${EXCLUDE#"$ROOT"/}"
    STILL=()
    for p in "${MISSING[@]}"; do
      git check-ignore -q -- "$p" 2>/dev/null || STILL+=("$p")
    done
    MISSING=("${STILL[@]+"${STILL[@]}"}")
  fi

  if [ "${#MISSING[@]}" -gt 0 ]; then
    for p in "${MISSING[@]}"; do
      fail "assistant tooling path is not ignored: $p"
    done
    printf '%s: fix with: %s --write-exclude\n' "$PROG" "$0" >&2
  fi
else
  # A skipped check announces itself, in the log format of whatever is reading.
  # Silence here would be indistinguishable from a check that passed, which is
  # the failure this whole regime exists to avoid.
  if [ -n "${CI:-}" ]; then
    printf '::notice::check 3 (assistant tooling paths are ignored) did not run: it is a property of the local working copy, whose .git/info/exclude is never cloned. It runs before every push, in scripts/preflight-publish.sh.\n'
  else
    printf '%s: note -- check 3 (assistant tooling paths are ignored) not requested; pass --require-ignored to run it\n' "$PROG"
  fi
fi

# --------------------------------------------------------------------------
if [ "$FINDINGS" -gt 0 ]; then
  printf '%s: %s finding(s)\n' "$PROG" "$FINDINGS" >&2
  exit 1
fi

printf '%s: clean (mode=%s, require-ignored=%s)\n' "$PROG" "$MODE" "$REQUIRE_IGNORED"
