#!/usr/bin/env bash
#
# idlectl -- remove a source install.
#
# For a package installed from the AUR, use pacman instead:  sudo pacman -Rns idlectl
#
# Two things this deliberately does NOT do:
#
#   * It does not remove anything under /etc/idlectl. The package never created those
#     files, so it has no business deleting them, and a configuration you spent an
#     afternoon on should survive an uninstall you might be doing to reinstall.
#   * It does not stop at the first missing file. A partial install is exactly when an
#     uninstaller is needed most.
#
set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
DRY_RUN=0

say()  { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==> warning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m==> error:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
	cat <<'EOF'
Usage: uninstall.sh [--prefix DIR] [--dry-run]

Removes the files packaging/install.sh installed. Leaves /etc/idlectl alone.
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
		--prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
		--dry-run) DRY_RUN=1; shift ;;
		-h|--help) usage; exit 0 ;;
		*) die "unknown option $1 (try --help)" ;;
	esac
done

[ "$(id -u)" -ne 0 ] || die "do not run this as root; it will use sudo for the removals."

DBUS_NAME="io.github.ericcanas.Idlectl1"

# The same list the Makefile installs, in the same order. Kept here rather than shelling
# out to `make uninstall` because an uninstaller that needs the source tree is useless to
# anybody who has already deleted it.
FILES=(
	"$PREFIX/bin/idlectl"
	"$PREFIX/bin/idlepolicyd"
	"$PREFIX/bin/idlectl-agent"
	"$PREFIX/lib/idlectl/idlectl.toml"
	"$PREFIX/lib/systemd/system/idlepolicyd.service"
	"$PREFIX/lib/systemd/user/idlectl-agent.service"
	"$PREFIX/share/dbus-1/system.d/$DBUS_NAME.conf"
	"$PREFIX/share/dbus-1/interfaces/$DBUS_NAME.xml"
	"$PREFIX/share/polkit-1/actions/$DBUS_NAME.policy"
	"$PREFIX/share/man/man1/idlectl.1"
	"$PREFIX/share/man/man5/idlectl.toml.5"
	"$PREFIX/share/man/man8/idlepolicyd.8"
	"$PREFIX/share/bash-completion/completions/idlectl"
	"$PREFIX/share/zsh/site-functions/_idlectl"
	"$PREFIX/share/fish/vendor_completions.d/idlectl.fish"
	"$PREFIX/share/licenses/idlectl/LICENSE"
	"$PREFIX/share/doc/idlectl/README.md"
	# Symlinks install.sh suggests for a non-/usr prefix. Removed if they point at this
	# install and left alone otherwise, below.
	"/usr/share/dbus-1/system.d/$DBUS_NAME.conf"
	"/usr/share/polkit-1/actions/$DBUS_NAME.policy"
)

run() {
	if [ "$DRY_RUN" -eq 1 ]; then
		printf '  would run: %s\n' "$*"
	else
		"$@"
	fi
}

# ----------------------------------------------------------------------------------
# Stop it before removing it, and unblank first.
#
# Order matters. A daemon killed while a screen is blanked leaves a dark panel with
# nothing left running that knows how to bring it back, and the user's next move is to
# assume the machine crashed. Stopping the agent cleanly unblanks on its way out.
# ----------------------------------------------------------------------------------
if systemctl is-enabled --quiet idlepolicyd.service 2>/dev/null \
	|| systemctl is-active --quiet idlepolicyd.service 2>/dev/null; then
	say "stopping and disabling idlepolicyd.service"
	run sudo systemctl disable --now idlepolicyd.service || true
fi
if systemctl --user is-active --quiet idlectl-agent.service 2>/dev/null; then
	say "stopping and disabling the session agent"
	run systemctl --user disable --now idlectl-agent.service || true
fi

removed=0
skipped=0
for path in "${FILES[@]}"; do
	if [ -L "$path" ]; then
		target=$(readlink -f -- "$path" 2>/dev/null || true)
		case "$target" in
			"$PREFIX"/*) ;;
			# A symlink pointing somewhere else belongs to a different install, most
			# likely a packaged one. Deleting it would break that install and blame
			# would land on pacman.
			*) warn "leaving $path: it points at $target, not at $PREFIX"; skipped=$((skipped + 1)); continue ;;
		esac
	elif [ ! -e "$path" ]; then
		continue
	fi

	if owner=$(pacman -Qoq "$path" 2>/dev/null) && [ -n "$owner" ]; then
		warn "leaving $path: it belongs to the pacman package '$owner'"
		skipped=$((skipped + 1))
		continue
	fi

	run sudo rm -f -- "$path"
	removed=$((removed + 1))
done

# Only if empty. `rmdir` refusing is the desired outcome when something else lives there.
for dir in "$PREFIX/lib/idlectl" "$PREFIX/share/licenses/idlectl" "$PREFIX/share/doc/idlectl"; do
	[ -d "$dir" ] && run sudo rmdir --ignore-fail-on-non-empty -- "$dir" || true
done

run sudo systemctl daemon-reload || true

say "removed $removed file(s), skipped $skipped"
if [ -d /etc/idlectl ]; then
	cat <<EOF

/etc/idlectl still exists and was not touched. The package never created it, so this
script does not delete it. Remove it yourself if you are done:

    sudo rm -r /etc/idlectl
EOF
fi
