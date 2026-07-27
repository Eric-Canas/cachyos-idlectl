#!/usr/bin/env bash
# Usage: check-commit-identity.sh [--pending|--head|--all|--range REV..REV] [--rules FILE]
#
# A commit carries two identifiers nobody edits on purpose: the author/committer
# name and e-mail, and the trailers in the message.  Both are published forever
# and neither is covered by a content scanner that only looks at files.
#
# Two layers:
#
#   shape checks (always on, need no configuration)
#     * with user.email unset, git synthesises an address from the local
#       account and the local host name.  That single default publishes a real
#       account AND a real machine in one string, so an address whose domain is
#       unqualified or in a private zone is rejected outright.
#     * an address whose domain is a literal IP, an empty identity.
#     * assistant/agent trailers in the message.
#
#   allowlist (from a file OUTSIDE the repository, so the intended public
#   identity is never itself published here)
#       ${XDG_CONFIG_HOME:-$HOME/.config}/idlectl/commit-identity.txt
#     with lines
#       name-allow  <POSIX ERE>
#       email-allow <POSIX ERE>
#
# Fails closed: no rules file, or a rules file with no name-allow / no
# email-allow line, is a FAILURE.
#
# ONE rule is deliberately SUBORDINATE to the allowlist rather than absolute:
# non-ASCII bytes in a name or address.  Its purpose is catching an identity
# nobody chose -- git's synthesised account@host, or a paragraph of the
# original non-English notes pasted into a name field.  It is NOT a rule that
# the copyright holder must misspell their own name to satisfy.  Applied
# unconditionally it makes the audit unsatisfiable for any maintainer whose
# legal name carries an accent, since the only escapes are rewriting published
# history or editing this script -- and an unsatisfiable gate is a gate that
# gets bypassed.  So it is reported only for an identity that ALREADY fails its
# allowlist, exactly as LICENSE is exempt from the orthography rule elsewhere
# in this regime: an explicitly allowed identity is by definition intended.

set -euo pipefail

PROG=${0##*/}

usage() {
  cat <<'EOF'
Usage: check-commit-identity.sh [mode] [--rules FILE]

Modes:
  --pending          identity that the NEXT commit would carry (default;
                     this is the one a pre-commit hook wants)
  --msg FILE         a commit message file (commit-msg hook)
  --head             the identity and message of HEAD
  --range REV..REV   every commit in a revision range
  --all              every commit reachable from every ref

Options:
  --rules FILE   allowlist file, overriding
                 $XDG_CONFIG_HOME/idlectl/commit-identity.txt
                 (env: IDLECTL_COMMIT_IDENTITY)
  -h, --help     this text

Exit status: 0 clean, 1 findings, 2 usage or configuration error.
EOF
}

die() {
  printf '%s: %s\n' "$PROG" "$1" >&2
  exit 2
}

redact_home() {
  case $1 in
    "$HOME"/*) printf '~%s' "${1#"$HOME"}" ;;
    *) printf '%s' "$1" ;;
  esac
}

MODE='pending'
RANGE=""
MSG_FILE=""
RULES=${IDLECTL_COMMIT_IDENTITY:-"${XDG_CONFIG_HOME:-$HOME/.config}/idlectl/commit-identity.txt"}

while [ $# -gt 0 ]; do
  case $1 in
    --pending) MODE='pending' ;;
    # Quoted: to shellcheck an unquoted `MODE=head` looks like an attempt to
    # capture the output of head(1) (SC2209), and CI runs it at --severity=style.
    --head) MODE='head' ;;
    --all) MODE='all' ;;
    --msg)
      MODE='msg'
      shift || true
      [ $# -gt 0 ] || die "--msg needs a file argument"
      MSG_FILE=$1
      ;;
    --range)
      MODE='range'
      shift || true
      [ $# -gt 0 ] || die "--range needs a revision range"
      RANGE=$1
      ;;
    --rules)
      shift || true
      [ $# -gt 0 ] || die "--rules needs a file argument"
      RULES=$1
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

# --------------------------------------------------------------------------
# Allowlist
# --------------------------------------------------------------------------
[ -f "$RULES" ] || die "identity rules file not found: $(redact_home "$RULES")
Write one or more lines of the form:
  name-allow  <POSIX ERE>
  email-allow <POSIX ERE>
It must stay OUTSIDE the repository."
[ -r "$RULES" ] || die "identity rules file is not readable: $(redact_home "$RULES")"

NAME_ALLOW=()
EMAIL_ALLOW=()
lineno=0
# SC2094 fires on redact_home "$RULES" inside a loop redirected from "$RULES",
# reading it as read-and-write of one file in a single pipeline.  Nothing here
# writes: redact_home only rewrites the PATH STRING for a diagnostic, so the
# warning is a false positive on all four call sites in this loop.
# shellcheck disable=SC2094
while IFS= read -r raw || [ -n "$raw" ]; do
  lineno=$((lineno + 1))
  line=${raw%$'\r'}
  case $line in
    '' | '#'*) continue ;;
  esac
  # Split on the first RUN of blanks, not on the first single space, and strip
  # the padding.  The format documented above and in --help is column-aligned
  # ("name-allow  <ERE>", two spaces, against "email-allow <ERE>", one), and
  # `${line#* }` keeps that second space -- turning the rule into the ERE
  # " ^Name$", a literal space before a start-of-string anchor, which cannot
  # match anything.  The effect was silent: a name-allow rule written exactly
  # the way this script documents it was accepted, reported as a valid ERE, and
  # then never matched the identity it was written for.
  kind=${line%%[[:space:]]*}
  value=${line#"$kind"}
  value=${value#"${value%%[![:space:]]*}"}
  value=${value%"${value##*[![:space:]]}"}
  [ -n "$value" ] || die "$(redact_home "$RULES"):$lineno has no value"
  case $kind in
    name-allow) NAME_ALLOW+=("$value") ;;
    email-allow) EMAIL_ALLOW+=("$value") ;;
    *) die "$(redact_home "$RULES"):$lineno: unknown key '$kind' (expected name-allow or email-allow)" ;;
  esac
  # grep exits 1 for "no match" and 2 or more for "that is not a regex".
  rc=0
  printf '' | grep -Eq -e "$value" 2>/dev/null || rc=$?
  [ "$rc" -lt 2 ] || die "$(redact_home "$RULES"):$lineno is not a valid POSIX ERE"
done <"$RULES"

[ "${#NAME_ALLOW[@]}" -gt 0 ] || die "$(redact_home "$RULES") has no name-allow rule"
[ "${#EMAIL_ALLOW[@]}" -gt 0 ] || die "$(redact_home "$RULES") has no email-allow rule"

# --------------------------------------------------------------------------
# Shape rules
# --------------------------------------------------------------------------
# Written with escaped dots so that this file does not itself look like a host
# name to the tier-1 scanner.
PRIVATE_ZONE='\.(lan|local|localdomain|internal|intranet|home|home\.arpa|ts\.net)$'
LITERAL_IP='^\[?[0-9]{1,3}(\.[0-9]{1,3}){3}\]?$'
ASSISTANT_TRAILER='(co-authored-by|signed-off-by|generated[ -]with|assisted[ -]by).*(claude|anthropic|copilot|cursor|codex|devin|aider|codeium|windsurf|gemini|chatgpt|openai)'
# Everything outside printable ASCII, tab excepted.
NON_ASCII=$'[^ -~\t]'

FINDINGS=0
fail() {
  printf 'FAIL  %s\n' "$1" >&2
  FINDINGS=$((FINDINGS + 1))
}

mask_email() {
  local addr=$1 localpart domain
  case $addr in
    *@*)
      localpart=${addr%%@*}
      domain=${addr#*@}
      printf '%.1s***@%s' "$localpart" "$domain"
      ;;
    *) printf '%s' "$addr" ;;
  esac
}

matches_any() {
  # $1 = subject, rest = patterns
  local subject=$1
  shift
  local p
  for p in "$@"; do
    if printf '%s\n' "$subject" | grep -Eq -e "$p"; then
      return 0
    fi
  done
  return 1
}

check_identity() {
  local where=$1 role=$2 name=$3 email=$4 domain

  if [ -z "$name" ]; then
    fail "$where: empty $role name"
  fi
  if [ -z "$email" ]; then
    fail "$where: empty $role e-mail"
    return
  fi

  # Checked before the address is picked apart, so that one bad domain does not
  # hide a second problem in the name.
  #
  # The non-ASCII test lives INSIDE this branch on purpose (see the header): it
  # exists to describe an identity that is already unaccounted for, not to
  # forbid a spelling the maintainer chose.  An allowed name is intended, and
  # an intended name that carries an accent is correct.
  if ! matches_any "$name" "${NAME_ALLOW[@]}"; then
    fail "$where: $role name '$name' matches no name-allow rule"
    if printf '%s' "$name" | LC_ALL=C grep -q "$NON_ASCII"; then
      fail "$where: $role name also carries non-ASCII bytes -- if this identity is intended, add a name-allow rule for it; if it is not, git synthesised it and user.name needs setting"
    fi
  fi

  case $email in
    *@*) domain=${email#*@} ;;
    *)
      fail "$where: $role e-mail '$(mask_email "$email")' has no domain"
      return
      ;;
  esac

  if printf '%s\n' "$domain" | grep -Eq -e "$LITERAL_IP"; then
    fail "$where: $role e-mail domain is a literal IP address -- that is a real host"
    return
  fi
  if printf '%s\n' "$domain" | grep -Eqi -e "$PRIVATE_ZONE"; then
    fail "$where: $role e-mail domain '$domain' is a private DNS zone -- git synthesised it from the local host name. Set user.email."
    return
  fi
  case $domain in
    *.*) : ;;
    *)
      fail "$where: $role e-mail domain '$domain' is an unqualified host name -- git synthesised it from the local host name. Set user.email."
      return
      ;;
  esac

  if ! matches_any "$email" "${EMAIL_ALLOW[@]}"; then
    fail "$where: $role e-mail '$(mask_email "$email")' matches no email-allow rule"
    if printf '%s' "$email" | LC_ALL=C grep -q "$NON_ASCII"; then
      fail "$where: $role e-mail also carries non-ASCII bytes -- if this address is intended, add an email-allow rule for it"
    fi
  fi
}

check_message() {
  local where=$1 file=$2
  if grep -Eqi -e "$ASSISTANT_TRAILER" -- "$file"; then
    fail "$where: commit message carries an assistant/agent trailer"
  fi
  if LC_ALL=C grep -q "$NON_ASCII" -- "$file"; then
    fail "$where: commit message contains non-ASCII bytes"
  fi
}

parse_ident() {
  # "Name <mail@example.org> 1700000000 +0100" -> name and email on two lines
  local ident=$1 name email
  name=${ident%% <*}
  email=${ident#*<}
  email=${email%%>*}
  printf '%s\n%s\n' "$name" "$email"
}

WORK=$(mktemp -d "${TMPDIR:-/tmp}/idlectl-ident.XXXXXX")
trap 'rm -rf "$WORK"' EXIT HUP INT TERM

check_commit() {
  local sha=$1 short an ae cn ce
  short=${sha:0:12}
  git show -s --format='%an%n%ae%n%cn%n%ce' "$sha" >"$WORK/id"
  {
    IFS= read -r an || :
    IFS= read -r ae || :
    IFS= read -r cn || :
    IFS= read -r ce || :
  } <"$WORK/id"
  check_identity "commit $short" author "$an" "$ae"
  if [ "$an" != "$cn" ] || [ "$ae" != "$ce" ]; then
    check_identity "commit $short" committer "$cn" "$ce"
  fi
  git show -s --format='%B' "$sha" >"$WORK/msg"
  check_message "commit $short" "$WORK/msg"
}

case $MODE in
  pending)
    parse_ident "$(git var GIT_AUTHOR_IDENT)" >"$WORK/a"
    parse_ident "$(git var GIT_COMMITTER_IDENT)" >"$WORK/c"
    {
      IFS= read -r pan || :
      IFS= read -r pae || :
    } <"$WORK/a"
    {
      IFS= read -r pcn || :
      IFS= read -r pce || :
    } <"$WORK/c"
    check_identity "pending commit" author "$pan" "$pae"
    if [ "$pan" != "$pcn" ] || [ "$pae" != "$pce" ]; then
      check_identity "pending commit" committer "$pcn" "$pce"
    fi
    ;;
  msg)
    [ -f "$MSG_FILE" ] || die "commit message file not found: $(redact_home "$MSG_FILE")"
    check_message "commit message" "$MSG_FILE"
    ;;
  head)
    git rev-parse --verify -q HEAD >/dev/null || die "no HEAD yet"
    check_commit "$(git rev-parse HEAD)"
    ;;
  range)
    git rev-list "$RANGE" >"$WORK/list" || die "bad revision range: $RANGE"
    while IFS= read -r sha; do
      [ -n "$sha" ] || continue
      check_commit "$sha"
    done <"$WORK/list"
    ;;
  all)
    git rev-list --all >"$WORK/list" 2>/dev/null || : >"$WORK/list"
    while IFS= read -r sha; do
      [ -n "$sha" ] || continue
      check_commit "$sha"
    done <"$WORK/list"
    ;;
  *) die "internal: unhandled mode $MODE" ;;
esac

if [ "$FINDINGS" -gt 0 ]; then
  printf '%s: %s finding(s)\n' "$PROG" "$FINDINGS" >&2
  exit 1
fi

printf '%s: clean (mode=%s)\n' "$PROG" "$MODE"
