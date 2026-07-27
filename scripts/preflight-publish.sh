#!/usr/bin/env bash
# Usage: preflight-publish.sh [-h]
#
# The gate between this working copy and the public internet.  Run it before
# the first push, before any push that adds documentation, and before cutting a
# release tarball.
#
# This design was extracted from a private repository documenting one specific
# machine.  The failure mode being defended against is not "a secret leaked" --
# it is "a real host name, a LAN address, a MAC, an account name or a paragraph
# of the original non-English notes was copied across and nobody noticed".
#
# THE AUDIT FAILS CLOSED.  A missing scanner is a FAIL.  A missing tier-2
# literals file is a FAIL.  There is deliberately no --skip flag: a gate with a
# bypass is a gate that will be bypassed on the one evening it mattered.
#
# Every step prints PASS or FAIL and the audit always runs to the end, so one
# invocation gives the whole picture rather than the first problem only.

set -euo pipefail

PROG=${0##*/}

usage() {
  cat <<'EOF'
Usage: preflight-publish.sh [-h|--help]

Ordered publication audit.  No options, no skips: every check either passes or
fails the run.  Exit status 0 means the working copy and its whole history are
clear to publish.

Requires, and will FAIL without: git, gitleaks, shellcheck, plus
  ~/.config/idlectl/forbidden-literals.txt   (tier-2 literals)
  ~/.config/idlectl/commit-identity.txt      (author allowlist)
both of which live outside the repository on purpose.
EOF
}

case ${1:-} in
  -h | --help)
    usage
    exit 0
    ;;
  '') : ;;
  *)
    usage >&2
    exit 2
    ;;
esac

git rev-parse --is-inside-work-tree >/dev/null 2>&1 || {
  printf '%s: not inside a git work tree\n' "$PROG" >&2
  exit 2
}
ROOT=$(git rev-parse --show-toplevel)
cd "$ROOT"

SCRIPTS="$ROOT/scripts"
STEP=0
TITLE=""
FAILED=0
LAST_FAILED_STEP=-1
NON_ASCII=$'[^ -~\t]'

WORK=$(mktemp -d "${TMPDIR:-/tmp}/idlectl-preflight.XXXXXX")
trap 'rm -rf "$WORK"' EXIT HUP INT TERM

redact_home() {
  case $1 in
    "$HOME"/*) printf '~%s' "${1#"$HOME"}" ;;
    *) printf '%s' "$1" ;;
  esac
}

step() {
  STEP=$((STEP + 1))
  TITLE=$1
}
ok() { printf '[%02d] PASS  %s\n' "$STEP" "$TITLE"; }
# A pass with a reason.  An empty input set and a clean scan are both exit 0,
# and reporting them identically is how a step that examines nothing gets read
# as a step that found nothing.
ok_because() { printf '[%02d] PASS  %s\n' "$STEP" "$1"; }
bad() {
  printf '[%02d] FAIL  %s\n' "$STEP" "$TITLE"
  while [ $# -gt 0 ]; do
    printf '          %s\n' "$1"
    shift
  done
  # Counted once per step, however many reasons that step had, so that the
  # closing "N of M checks FAILED" is arithmetic anyone can verify.
  if [ "$STEP" != "$LAST_FAILED_STEP" ]; then
    FAILED=$((FAILED + 1))
    LAST_FAILED_STEP=$STEP
  fi
}
indent_file() { sed 's/^/          | /' <"$1"; }

# run <title> <command...>
run() {
  local rc=0
  step "$1"
  shift
  "$@" >"$WORK/out" 2>&1 || rc=$?
  if [ "$rc" -eq 0 ]; then
    ok
  else
    bad "command exited $rc"
    indent_file "$WORK/out"
  fi
}

is_binary_file() {
  local total stripped
  total=$(LC_ALL=C head -c 65536 -- "$1" | wc -c | tr -d ' ')
  stripped=$(LC_ALL=C head -c 65536 -- "$1" | LC_ALL=C tr -d '\000' | wc -c | tr -d ' ')
  [ "$total" != "$stripped" ]
}

printf '%s: publication audit of %s\n\n' "$PROG" "$(redact_home "$ROOT")"

# ---------------------------------------------------------------------------
step "repository is a normal, non-bare work tree with a resolvable root"
if [ "$(git rev-parse --is-bare-repository)" = true ]; then
  bad "this is a bare repository"
elif [ ! -d "$ROOT/.git" ] && [ ! -f "$ROOT/.git" ]; then
  bad "no .git at the resolved root"
else
  ok
fi

# ---------------------------------------------------------------------------
step "required tools are installed"
MISSING_TOOLS=""
for t in git gitleaks shellcheck grep sed awk find mktemp; do
  command -v "$t" >/dev/null 2>&1 || MISSING_TOOLS="$MISSING_TOOLS $t"
done
if [ -n "$MISSING_TOOLS" ]; then
  bad "missing:$MISSING_TOOLS" \
    "A missing scanner is a FAIL, never a skip -- an audit that quietly does" \
    "nothing is worse than no audit at all." \
    "Arch:   sudo pacman -S --needed git gitleaks shellcheck" \
    "macOS:  brew install git gitleaks shellcheck"
else
  ok
fi

# ---------------------------------------------------------------------------
step "the leak-prevention regime itself is present and executable"
MISSING_FILES=""
for f in .gitignore .gitleaks.toml \
  scripts/check-tracked-paths.sh \
  scripts/check-forbidden-literals.sh \
  scripts/check-commit-identity.sh; do
  [ -f "$f" ] || MISSING_FILES="$MISSING_FILES $f"
done
NOT_EXEC=""
for f in scripts/check-tracked-paths.sh scripts/check-forbidden-literals.sh \
  scripts/check-commit-identity.sh scripts/preflight-publish.sh; do
  if [ -f "$f" ] && [ ! -x "$f" ]; then NOT_EXEC="$NOT_EXEC $f"; fi
done
if [ -n "$MISSING_FILES" ] || [ -n "$NOT_EXEC" ]; then
  [ -z "$MISSING_FILES" ] || bad "missing:$MISSING_FILES"
  [ -z "$NOT_EXEC" ] || bad "not executable:$NOT_EXEC (chmod +x)"
else
  ok
fi

# ---------------------------------------------------------------------------
step "tier-2 literals file exists, is usable, and lives OUTSIDE the repository"
LITERALS=${IDLECTL_FORBIDDEN_LITERALS:-"${XDG_CONFIG_HOME:-$HOME/.config}/idlectl/forbidden-literals.txt"}
if [ ! -f "$LITERALS" ]; then
  bad "not found: $(redact_home "$LITERALS")" \
    "Tier 1 (.gitleaks.toml) only knows identifier SHAPES; it cannot know that" \
    "one particular host name is yours.  Without tier 2 this audit is blind to" \
    "exactly the strings it exists to catch, so the absence is a FAIL." \
    "Create it with one POSIX ERE per line."
elif [ ! -s "$LITERALS" ]; then
  bad "is empty: $(redact_home "$LITERALS")"
else
  LIT_DIR=$(cd "$(dirname "$LITERALS")" && pwd -P)
  case "$LIT_DIR/" in
    "$ROOT"/*) bad "the literals file is INSIDE the repository: $(redact_home "$LITERALS")" ;;
    *) ok ;;
  esac
fi

# ---------------------------------------------------------------------------
step "author allowlist exists and lives OUTSIDE the repository"
IDENT=${IDLECTL_COMMIT_IDENTITY:-"${XDG_CONFIG_HOME:-$HOME/.config}/idlectl/commit-identity.txt"}
if [ ! -f "$IDENT" ]; then
  bad "not found: $(redact_home "$IDENT")" \
    "Needs name-allow / email-allow lines (POSIX EREs)."
else
  IDENT_DIR=$(cd "$(dirname "$IDENT")" && pwd -P)
  case "$IDENT_DIR/" in
    "$ROOT"/*) bad "the identity allowlist is INSIDE the repository: $(redact_home "$IDENT")" ;;
    *) ok ;;
  esac
fi

# ---------------------------------------------------------------------------
# --require-ignored: this is the LOCAL audit, so it is the right place -- and
# the only place -- for the check that assistant tooling is ignored.  CI runs
# the same script without the flag, because .git/info/exclude is never cloned.
run "tracked path shapes, .gitignore purity, assistant paths ignored" \
  "$SCRIPTS/check-tracked-paths.sh" --tracked --require-ignored

# ---------------------------------------------------------------------------
# Repeated here explicitly: the requirement is that these paths ARE ignored,
# and `git check-ignore` does not care whether .gitignore or .git/info/exclude
# did it.  That indifference is the whole point -- it lets the published file
# stay silent about the toolchain while the local one does the work.
step "assistant tooling paths are ignored (git check-ignore, any source)"
UNIGNORED=""
for p in .claude/ .cursor/ .cursorrules .aider.conf.yml .codeium/ .windsurf/ \
  .continue/ .roo/ .specstory/ .opencode/ .junie/ .amazonq/ .kiro/ .goose/ \
  .taskmaster/ .mcp.json CLAUDE.md AGENTS.md GEMINI.md; do
  git check-ignore -q -- "$p" 2>/dev/null || UNIGNORED="$UNIGNORED $p"
done
if [ -n "$UNIGNORED" ]; then
  bad "not ignored:$UNIGNORED" \
    "Add them to .git/info/exclude (NOT .gitignore):" \
    "  scripts/check-tracked-paths.sh --write-exclude"
else
  ok
fi

# ---------------------------------------------------------------------------
step "working tree is clean (nothing staged, modified, or untracked)"
git status --porcelain >"$WORK/status"
if [ -s "$WORK/status" ]; then
  bad "publish from a clean tree; an untracked file is one 'git add -A' from being public"
  indent_file "$WORK/status"
else
  ok
fi

# ---------------------------------------------------------------------------
step "text in the audit file set carries no non-English orthography"
# Two different things hide under "non-ASCII", and conflating them is how a
# gate gets switched off:
#
#   an accented Latin letter or inverted punctuation  -> a LEAK.  The notes this
#     design came from are in another language; one stray "n with a tilde" is
#     evidence of a copied paragraph, not a typo.  Fatal.
#
#   an em dash, an arrow, a curly quote  -> typography.  Not a leak.  Worth
#     fixing, because ASCII survives terminals, roff and diffs better, but
#     failing a publication audit over punctuation trains people to bypass it.
#     Advisory.
#
# The split is done on the UTF-8 encoding of Latin-1 Supplement through Latin
# Extended-A, while typography sits at lead byte \342 and is left alone.  The
# two holes in the \303 range are U+00D7 and U+00F7, the multiplication and
# division signs: they sit in the middle of the Latin block but they are
# arithmetic, and a spec that writes "8 x 3" with the real sign is not a leak.
#
# awk rather than grep because BSD grep refuses a high-byte pattern even under
# LC_ALL=C, and this has to run on the author's machine too.
# THE AUDIT FILE SET, shared by steps 09, 10 and 18.
#
# --cached AND --others --exclude-standard: tracked files PLUS untracked ones
# that are not ignored.  A plain `git ls-files` sees only what is already
# committed, and the state this audit exists for -- a working copy about to be
# published for the first time -- is exactly the state in which almost nothing
# is committed yet.  Measured on this repository before the fix: 36 of 38 files
# were untracked, so three content steps reported PASS having opened LICENSE
# and README.md and nothing else.
#
# It also makes the two scanner tiers agree on their input: gitleaks already
# scans untracked files (`--no-git --source .`), so anything narrower here
# meant the second opinion was formed from a smaller set of facts than the
# first.  Step 08 fails on a dirty tree anyway, but three green content checks
# beside it read as "the content was audited", and it had not been.
git ls-files -z --cached --others --exclude-standard >"$WORK/fileset"
: >"$WORK/orthography"
: >"$WORK/nonascii"
while IFS= read -r -d '' f; do
  [ -f "$f" ] || continue
  if [ -L "$f" ]; then continue; fi
  if is_binary_file "$f"; then continue; fi
  case $f in
    # The one file whose job is to carry a person's legal name exactly as
    # spelled.  A licence that mangles the copyright holder is a worse
    # document than one with a non-ASCII byte in it.
    LICENSE | LICENSE.* | LICENCE | LICENCE.* | COPYING | COPYING.* | LICENSES/*) : ;;
    *)
      LC_ALL=C awk -v name="$f" \
        '/\302[\241\277]|\303[\200-\226\230-\266\270-\277]|[\304\305][\200-\277]/ { print name ":" FNR }' \
        "$f" >>"$WORK/orthography" || :
      ;;
  esac
  if LC_ALL=C grep -n "$NON_ASCII" -- "$f" >"$WORK/hits" 2>/dev/null; then
    while IFS= read -r hit; do
      printf '%s:%s\n' "$f" "${hit%%:*}" >>"$WORK/nonascii"
    done <"$WORK/hits"
  fi
done <"$WORK/fileset"
if [ -s "$WORK/orthography" ]; then
  bad "accented Latin letters or inverted punctuation (locations only; content is not echoed)"
  indent_file "$WORK/orthography"
else
  ok
fi
if [ -s "$WORK/nonascii" ]; then
  printf '     NOTE  %s line(s) contain non-ASCII typography (em dashes, arrows,\n' \
    "$(wc -l <"$WORK/nonascii" | tr -d ' ')"
  printf '           curly quotes). Not a leak and not a failure; ASCII simply\n'
  printf '           travels better through terminals, man pages and diffs.\n'
fi

# ---------------------------------------------------------------------------
step "does anything here name a real machine?"
# Independent of gitleaks on purpose: two implementations of the same idea fail
# in different places.  Locations only -- the content is never echoed, because
# if the pattern did match, echoing it is the leak.
HEURISTICS=(
  # The middle class admits dots: a mesh-VPN name is host.tailnet.ts.net, and a
  # rule that only spans one label never reaches the suffix.
  #
  # The trailing class excludes '_' as well as alphanumerics, because a DNS
  # label cannot contain an underscore but an identifier can: without that,
  # this project's own [while.local_service_busy] config table reads as a
  # host called something.local.
  'private DNS zone host name|(^|[^[:alnum:]_.])[[:alnum:]]([-.[:alnum:]]{0,61}[[:alnum:]])?\.(lan|local|localdomain|internal|intranet|home|ts\.net)([^[:alnum:]_-]|$)'
  # No trailing context on the address rules: an address at the end of a
  # sentence is followed by a full stop, and requiring a non-dot there is how a
  # detector silently misses the most common way an address gets written down.
  'RFC1918 address|(^|[^0-9.])(10\.[0-9]{1,3}\.[0-9]{1,3}\.[0-9]{1,3}|172\.(1[6-9]|2[0-9]|3[01])\.[0-9]{1,3}\.[0-9]{1,3}|192\.168\.[0-9]{1,3}\.[0-9]{1,3})'
  'shared address space (mesh VPN)|(^|[^0-9.])100\.(6[4-9]|[7-9][0-9]|1[01][0-9]|12[0-7])\.[0-9]{1,3}\.[0-9]{1,3}'
  'MAC address|(^|[^:[:alnum:]])[0-9a-fA-F]{2}(:[0-9a-fA-F]{2}){5}([^:[:alnum:]]|$)'
  # The flag group must admit a BARE flag as well as a flag with an argument.
  # Demanding whitespace after the argument, as `([-][[:alnum:]]+[[:space:]]+
  # [^[:space:]]+[[:space:]]+)*` did, misses `rsync -av user@host:/path`
  # entirely: there the target IS the token that would have been -av's
  # argument, so neither zero nor one repetition matches.  Same defect, same
  # fix, as idlectl-remote-shell-invocation in .gitleaks.toml -- these two are
  # meant to be independent implementations of one idea, not one bug written
  # twice.
  'remote shell to a named account|(^|[^[:alnum:]_])(ssh|scp|sftp|rsync|mosh)[[:space:]]+(-{1,2}[[:alnum:]-]+([=[:space:]][^[:space:]]+)?[[:space:]]+)*[[:alnum:]_][[:alnum:]._-]*@'
  'home directory of a named account|/(home|Users)/[[:alnum:]_][[:alnum:]._-]*'
  'private key file reference|(\.ssh/[[:alnum:]._-]+|(^|[^[:alnum:]_])id_(ed25519|rsa|ecdsa|dsa)([^[:alnum:]]|$))'
)
# Same index as HEURISTICS; a line matching the exception is dropped from that
# rule's hits.  Only the documentation ranges get one, and only line-wide --
# tier 1 allowlists per MATCH instead, so a line carrying both a placeholder
# and a real value is still caught there.  Two implementations, different
# blind spots, on purpose.
HEURISTIC_EXCEPT=(
  ''
  ''
  ''
  '00:00:[5][eE]:00:53:[0-9a-fA-F]{2}|[aA][aA]:[bB][bB]:[cC][cC]:[dD][dD]:[eE][eE]:[fF][fF]'
  ''
  ''
  ''
)
: >"$WORK/machine"
while IFS= read -r -d '' f; do
  [ -f "$f" ] || continue
  if [ -L "$f" ]; then continue; fi
  if is_binary_file "$f"; then continue; fi
  case $f in
    # The shape files: every one of these is a list of the patterns below, so
    # scanning them with themselves reports nothing but self-matches.
    .gitleaks.toml | scripts/preflight-publish.sh | scripts/check-tracked-paths.sh | scripts/check-commit-identity.sh) continue ;;
  esac
  i=0
  while [ "$i" -lt "${#HEURISTICS[@]}" ]; do
    h=${HEURISTICS[i]}
    label=${h%%|*}
    pattern=${h#*|}
    except=${HEURISTIC_EXCEPT[i]}
    i=$((i + 1))
    grep -nE -e "$pattern" -- "$f" >"$WORK/hits" 2>/dev/null || continue
    if [ -n "$except" ]; then
      grep -vE -e "$except" "$WORK/hits" >"$WORK/hits.kept" 2>/dev/null || :
      mv -f "$WORK/hits.kept" "$WORK/hits"
    fi
    while IFS= read -r hit; do
      printf '%s:%s: %s\n' "$f" "${hit%%:*}" "$label" >>"$WORK/machine"
    done <"$WORK/hits"
  done
done <"$WORK/fileset"
if [ -s "$WORK/machine" ]; then
  bad "the following locations look like they name a real machine or account" \
    "Approved placeholders: 192.0.2.x / 198.51.100.x / 203.0.113.x (RFC 5737)," \
    "2001:db8:: (RFC 3849), 00:00:5E:00:53:xx (RFC 7042), example.com," \
    "*.invalid (RFC 2606), and <user> or %i for an account."
  indent_file "$WORK/machine"
else
  ok
fi

# ---------------------------------------------------------------------------
run "tier-2 literals: tracked working tree" \
  "$SCRIPTS/check-forbidden-literals.sh" --tree
run "tier-2 literals: reachable history, refs and reflog" \
  "$SCRIPTS/check-forbidden-literals.sh" --history
run "tier-2 literals: every object, including blobs orphaned by an amend" \
  "$SCRIPTS/check-forbidden-literals.sh" --objects

# ---------------------------------------------------------------------------
run "tier-1 shapes: working tree, including untracked files" \
  gitleaks detect --no-git --source . --config .gitleaks.toml --redact --exit-code 1
run "tier-1 shapes: full history on every ref" \
  gitleaks detect --source . --config .gitleaks.toml --redact --exit-code 1 --log-opts=--all

# ---------------------------------------------------------------------------
run "commit identities and message trailers across all history" \
  "$SCRIPTS/check-commit-identity.sh" --all

# ---------------------------------------------------------------------------
step "remote URLs carry no credentials and no private host"
git remote -v >"$WORK/remotes" 2>/dev/null || : >"$WORK/remotes"
if [ -s "$WORK/remotes" ] &&
  grep -Eq -e '://[^/[:space:]]*:[^/@[:space:]]+@' -e '\.(lan|local|internal|intranet)(:|/|[[:space:]])' "$WORK/remotes"; then
  bad "a remote URL embeds a password or points at a private host"
  indent_file "$WORK/remotes"
else
  ok
fi

# ---------------------------------------------------------------------------
step "shell scripts are shellcheck-clean"
: >"$WORK/sh"
while IFS= read -r -d '' f; do
  case $f in
    *.sh | *.bash) printf '%s\n' "$f" >>"$WORK/sh" ;;
    *)
      if [ -f "$f" ] && head -c 64 -- "$f" 2>/dev/null | grep -Eq '^#!.*(ba)?sh'; then
        printf '%s\n' "$f" >>"$WORK/sh"
      fi
      ;;
  esac
done <"$WORK/fileset"
if [ ! -s "$WORK/sh" ]; then
  # NOT a plain ok: this repository ships four shell scripts, so an empty set
  # here means the file set was built wrongly, not that the scripts are clean.
  ok_because "no shell scripts in the audit file set (nothing was examined)"
elif command -v shellcheck >/dev/null 2>&1; then
  rc=0
  tr '\n' '\0' <"$WORK/sh" |
    xargs -0 shellcheck --severity=style --external-sources >"$WORK/out" 2>&1 || rc=$?
  if [ "$rc" -eq 0 ]; then
    ok
  else
    bad "shellcheck exited $rc"
    indent_file "$WORK/out"
  fi
else
  bad "shellcheck is not installed (see the tools step)"
fi

# ---------------------------------------------------------------------------
step "Rust sources are rustfmt-clean"
if [ ! -f Cargo.toml ]; then
  ok
elif ! command -v cargo >/dev/null 2>&1; then
  bad "Cargo.toml exists but cargo is not installed"
else
  rc=0
  cargo fmt --all -- --check >"$WORK/out" 2>&1 || rc=$?
  if [ "$rc" -eq 0 ]; then
    ok
  else
    bad "cargo fmt --all -- --check exited $rc"
    indent_file "$WORK/out"
  fi
fi

# ---------------------------------------------------------------------------
# Every XML file this package installs is machine-parsed by something that
# fails SILENTLY.
#
# Measured, and it cost an hour: a `--` typed inside an XML comment is illegal,
# polkit's parser abandoned the surrounding <action> without a word in any log,
# and `idlectl lease` then refused every unprivileged caller with "Action ... is
# not registered".  The file looked perfect in an editor.  D-Bus bus policy is
# worse: a malformed .conf means the daemon cannot take its bus name at all.
#
# So this step parses them rather than reading them.  xmllint where available,
# Python's expat otherwise; if neither exists the step FAILS rather than passing,
# because a gate that silently skips is the class of thing this whole script
# exists to catch.
step "installed XML parses (polkit actions, bus policy, introspection)"
xml_files=$(git ls-files 'data/*.xml' 'data/*.conf' 'data/*.policy' 2>/dev/null || true)
if [ -z "$xml_files" ]; then
  ok_because "no XML data files are tracked"
elif command -v xmllint >/dev/null 2>&1; then
  rc=0
  # shellcheck disable=SC2086
  xmllint --noout $xml_files >"$WORK/out" 2>&1 || rc=$?
  if [ "$rc" -eq 0 ]; then ok; else
    bad "xmllint rejected an installed file"
    indent_file "$WORK/out"
  fi
elif command -v python3 >/dev/null 2>&1; then
  rc=0
  # shellcheck disable=SC2086
  python3 - $xml_files >"$WORK/out" 2>&1 <<'PYEOF' || rc=$?
import sys, xml.dom.minidom
bad = 0
for path in sys.argv[1:]:
    try:
        xml.dom.minidom.parse(path)
    except Exception as err:
        print(f"{path}: {err}")
        bad = 1
sys.exit(bad)
PYEOF
  if [ "$rc" -eq 0 ]; then ok; else
    bad "an installed XML file does not parse"
    indent_file "$WORK/out"
  fi
else
  bad "neither xmllint nor python3 is available to parse the installed XML"
fi

# ---------------------------------------------------------------------------
printf '\n'
if [ "$FAILED" -gt 0 ]; then
  printf '%s: %s of %s checks FAILED -- do not publish.\n' "$PROG" "$FAILED" "$STEP" >&2
  exit 1
fi
printf '%s: all %s checks passed.\n' "$PROG" "$STEP"
# The backticks are prose, quoting a command for the reader; nothing is meant
# to expand here (SC2016).
# shellcheck disable=SC2016
printf '%s: reminder -- `git gc --prune=now` before publishing only AFTER this\n' "$PROG"
printf '%s: audit, never before: the --objects scan is what sees orphaned blobs.\n' "$PROG"
