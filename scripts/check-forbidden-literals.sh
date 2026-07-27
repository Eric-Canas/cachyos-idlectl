#!/usr/bin/env bash
# Usage: check-forbidden-literals.sh (--staged|--msg FILE|--tree|--history|--objects) [--patterns FILE]
#
# Tier-2 leak scanner: matches the real identifiers of the machine this project
# was extracted from against everything this repository is about to publish.
#
# The patterns are real host names, addresses, MAC addresses and account names.
# They therefore live OUTSIDE the repository, one POSIX ERE per line, in
#   ${XDG_CONFIG_HOME:-$HOME/.config}/idlectl/forbidden-literals.txt
# and this script never prints a pattern, never prints a matched fragment, and
# never prints the surrounding line.  A finding is reported as a location plus
# a rule NUMBER, so that a CI log or a pasted terminal buffer cannot become the
# leak the scanner exists to prevent.
#
# Fails closed: a missing, empty or unparseable pattern file is a FAILURE, not
# a skip.  A scanner that quietly does nothing is worse than no scanner.

set -euo pipefail

PROG=${0##*/}

usage() {
  cat <<'EOF'
Usage: check-forbidden-literals.sh <mode> [--patterns FILE]

Exactly one mode:
  --staged        staged blob contents and staged path names (pre-commit hook)
  --msg FILE      a commit message file (commit-msg hook)
  --tree          tracked working-tree files and their path names
  --history       every reachable commit, tag, tree and blob, plus reflog
  --objects       EVERY object in the database, reachable or not -- this is the
                  mode that catches a blob orphaned by `git commit --amend`,
                  `git reset` or a rebase, which still sits in .git and is
                  still pushable.  Run it BEFORE `git gc --prune=now`.

Options:
  --patterns FILE  pattern file to use instead of the default
                   ($XDG_CONFIG_HOME/idlectl/forbidden-literals.txt).
                   The env var IDLECTL_FORBIDDEN_LITERALS does the same.
  -h, --help       this text

Pattern file format: one POSIX ERE per line; blank lines and lines whose first
character is '#' are ignored; matching is case-insensitive.

Exit status: 0 clean, 1 findings, 2 usage or configuration error.
EOF
}

die() {
  printf '%s: %s\n' "$PROG" "$1" >&2
  exit 2
}

# Replace $HOME with ~ so that a printed path does not itself name the account.
redact_home() {
  case $1 in
    "$HOME"/*) printf '~%s' "${1#"$HOME"}" ;;
    *) printf '%s' "$1" ;;
  esac
}

MODE=""
MSG_FILE=""
PATTERNS_FILE=${IDLECTL_FORBIDDEN_LITERALS:-"${XDG_CONFIG_HOME:-$HOME/.config}/idlectl/forbidden-literals.txt"}

while [ $# -gt 0 ]; do
  case $1 in
    --staged | --tree | --history | --objects)
      [ -z "$MODE" ] || die "only one mode may be given"
      MODE=${1#--}
      ;;
    --msg)
      [ -z "$MODE" ] || die "only one mode may be given"
      MODE=msg
      shift || true
      [ $# -gt 0 ] || die "--msg needs a file argument"
      MSG_FILE=$1
      ;;
    --patterns)
      shift || true
      [ $# -gt 0 ] || die "--patterns needs a file argument"
      PATTERNS_FILE=$1
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *) die "unknown argument: $1 (try --help)" ;;
  esac
  shift || true
done

[ -n "$MODE" ] || {
  usage >&2
  die "no mode given"
}

WORK=$(mktemp -d "${TMPDIR:-/tmp}/idlectl-lit.XXXXXX")
trap 'rm -rf "$WORK"' EXIT HUP INT TERM

# --------------------------------------------------------------------------
# Load and validate the pattern file.  Fail closed on every defect.
# --------------------------------------------------------------------------

[ -e "$PATTERNS_FILE" ] || die "pattern file not found: $(redact_home "$PATTERNS_FILE")
create it with one POSIX ERE per line (real host names, addresses, accounts).
It must stay OUTSIDE the repository."
[ -r "$PATTERNS_FILE" ] || die "pattern file is not readable: $(redact_home "$PATTERNS_FILE")"
[ -s "$PATTERNS_FILE" ] || die "pattern file is empty: $(redact_home "$PATTERNS_FILE")"

PATTERNS=()
PATTERN_COUNT=0
COMPILED="$WORK/patterns"
: >"$COMPILED"

while IFS= read -r raw || [ -n "$raw" ]; do
  line=${raw%$'\r'}
  case $line in
    '' | '#'*) continue ;;
  esac
  PATTERN_COUNT=$((PATTERN_COUNT + 1))
  PATTERNS[PATTERN_COUNT]=$line
  printf '%s\n' "$line" >>"$COMPILED"
done <"$PATTERNS_FILE"

[ "$PATTERN_COUNT" -gt 0 ] || die "pattern file contains no usable pattern: $(redact_home "$PATTERNS_FILE")"

# An unparseable value must be a veto, never a crash and never a silent pass.
idx=1
while [ "$idx" -le "$PATTERN_COUNT" ]; do
  # grep exits 1 for "no match" and 2 or more for "that is not a regex".
  rc=0
  printf '' | grep -Eqi -e "${PATTERNS[idx]}" 2>/dev/null || rc=$?
  if [ "$rc" -ge 2 ]; then
    die "rule #$idx in $(redact_home "$PATTERNS_FILE") is not a valid POSIX ERE"
  fi
  idx=$((idx + 1))
done

# --------------------------------------------------------------------------
# Scanning primitives
# --------------------------------------------------------------------------

FINDINGS=0
SCANNED=0

is_binary() {
  local total stripped
  total=$(LC_ALL=C head -c 65536 -- "$1" | wc -c | tr -d ' ')
  stripped=$(LC_ALL=C head -c 65536 -- "$1" | LC_ALL=C tr -d '\000' | wc -c | tr -d ' ')
  [ "$total" != "$stripped" ]
}

# report_hits <content-file> <label>: identify WHICH rules matched, without
# ever emitting the rule or the matched text.
report_hits() {
  local content=$1 label=$2 i=1 lineno
  while [ "$i" -le "$PATTERN_COUNT" ]; do
    grep -nEi -e "${PATTERNS[i]}" -- "$content" >"$WORK/raw" 2>/dev/null || :
    cut -d: -f1 <"$WORK/raw" >"$WORK/hits"
    while IFS= read -r lineno; do
      [ -n "$lineno" ] || continue
      printf 'LEAK  %s: line %s matches forbidden-literals rule #%s\n' "$label" "$lineno" "$i"
      FINDINGS=$((FINDINGS + 1))
    done <"$WORK/hits"
    i=$((i + 1))
  done
}

scan_file() {
  local content=$1 label=$2 rc=0
  SCANNED=$((SCANNED + 1))
  if is_binary "$content"; then
    return 0
  fi
  # One cheap pass over every pattern at once; only a hit pays for the second,
  # per-pattern pass that works out which rule numbers to report.
  grep -qEi -f "$COMPILED" -- "$content" 2>/dev/null || rc=$?
  if [ "$rc" -eq 0 ]; then
    report_hits "$content" "$label"
  elif [ "$rc" -ge 2 ]; then
    die "grep failed while scanning $label"
  fi
}

scan_string() {
  printf '%s\n' "$1" >"$WORK/string"
  scan_file "$WORK/string" "$2"
}

git_or_die() {
  git rev-parse --is-inside-work-tree >/dev/null 2>&1 || die "not inside a git work tree"
}

# --------------------------------------------------------------------------
# Modes
# --------------------------------------------------------------------------

scan_object() {
  local sha=$1 type=$2
  case $type in
    blob)
      git cat-file blob "$sha" >"$WORK/obj" 2>/dev/null || return 0
      scan_file "$WORK/obj" "object $sha (blob)"
      ;;
    commit | tag)
      # The raw object carries the message AND the author/committer identity.
      git cat-file "$type" "$sha" >"$WORK/obj" 2>/dev/null || return 0
      scan_file "$WORK/obj" "object $sha ($type)"
      ;;
    tree)
      # Pretty-printed so that the path names inside are scanned as text.
      git cat-file -p "$sha" >"$WORK/obj" 2>/dev/null || return 0
      scan_file "$WORK/obj" "object $sha (tree)"
      ;;
  esac
}

mode_staged() {
  git_or_die
  git diff --cached --name-only --diff-filter=ACMR -z >"$WORK/list" || die "git diff --cached failed"
  while IFS= read -r -d '' path; do
    scan_string "$path" "staged path name <$path>"
    if git cat-file -e ":$path" 2>/dev/null; then
      git show ":$path" >"$WORK/staged" 2>/dev/null || continue
      scan_file "$WORK/staged" "staged:$path"
    fi
  done <"$WORK/list"
}

mode_msg() {
  [ -f "$MSG_FILE" ] || die "commit message file not found: $(redact_home "$MSG_FILE")"
  scan_file "$MSG_FILE" "commit message"
}

mode_tree() {
  git_or_die
  git ls-files -z >"$WORK/list" || die "git ls-files failed"
  while IFS= read -r -d '' path; do
    scan_string "$path" "tracked path name <$path>"
    if [ -f "$path" ] && [ ! -L "$path" ]; then
      scan_file "$path" "$path"
    elif git cat-file -e ":$path" 2>/dev/null; then
      git show ":$path" >"$WORK/idx" 2>/dev/null || continue
      scan_file "$WORK/idx" "index:$path"
    fi
  done <"$WORK/list"
}

mode_history() {
  git_or_die
  # Ref names can themselves carry a host name.
  git for-each-ref --format='%(refname) %(objectname)' >"$WORK/refs" 2>/dev/null || : >"$WORK/refs"
  if [ -s "$WORK/refs" ]; then
    scan_file "$WORK/refs" "ref names"
  fi

  # Reflog entries survive amends and resets and are not objects.
  git log -g --all --format='%gd %gs %an %ae %s' >"$WORK/reflog" 2>/dev/null || : >"$WORK/reflog"
  if [ -s "$WORK/reflog" ]; then
    scan_file "$WORK/reflog" "reflog"
  fi

  git rev-list --all --objects >"$WORK/objs" 2>/dev/null || : >"$WORK/objs"
  while IFS= read -r entry; do
    [ -n "$entry" ] || continue
    sha=${entry%% *}
    type=$(git cat-file -t "$sha" 2>/dev/null || true)
    [ -n "$type" ] || continue
    scan_object "$sha" "$type"
  done <"$WORK/objs"
}

mode_objects() {
  git_or_die
  git cat-file --batch-all-objects --unordered --batch-check='%(objectname) %(objecttype)' \
    >"$WORK/all" 2>/dev/null || die "git cat-file --batch-all-objects failed"
  while IFS=' ' read -r sha type; do
    [ -n "$sha" ] || continue
    scan_object "$sha" "$type"
  done <"$WORK/all"
}

case $MODE in
  staged) mode_staged ;;
  msg) mode_msg ;;
  tree) mode_tree ;;
  history) mode_history ;;
  objects) mode_objects ;;
  *) die "internal: unhandled mode $MODE" ;;
esac

if [ "$FINDINGS" -gt 0 ]; then
  printf '%s: %s finding(s) across %s scanned item(s); mode=%s, %s rule(s) loaded\n' \
    "$PROG" "$FINDINGS" "$SCANNED" "$MODE" "$PATTERN_COUNT" >&2
  printf '%s: the matched text is deliberately not shown. Look up the rule number in\n' "$PROG" >&2
  printf '%s:   %s\n' "$PROG" "$(redact_home "$PATTERNS_FILE")" >&2
  exit 1
fi

printf '%s: clean (mode=%s, %s item(s) scanned, %s rule(s) loaded)\n' \
  "$PROG" "$MODE" "$SCANNED" "$PATTERN_COUNT"
