#!/usr/bin/env bash
#
# idlectl -- source installer for Arch-based systems.
#
#   curl -fsSL https://raw.githubusercontent.com/Eric-Canas/cachyos-idlectl/main/packaging/install.sh | bash
#
# THIS IS THE SECOND-CHOICE ROUTE. The first is the AUR:
#
#     paru -S idlectl        # or yay, or makepkg -si in a clone of the AUR repo
#
# A packaged install is owned by pacman, upgrades with the system, and can be removed
# completely. This script builds from source and installs into a prefix that pacman does
# not know about, which is a fine thing to do deliberately and a bad thing to end up with
# by accident. If an AUR helper is present it says so and stops, rather than quietly
# giving you the worse of the two.
#
# WHAT IT WILL NOT DO
#
#   * It will not touch anything under /etc. The package owns no configuration; that
#     directory is yours, and an installer that wrote there could revert a decision you
#     made on the next run.
#   * It will not enable any unit. Installing a program that can suspend a machine is
#     not the same act as permitting it to, and the difference is the whole reason the
#     packaging has no preset.
#   * It will not install over a pacman-managed copy. Two owners of the same file is how
#     an upgrade silently reverts a build, and pacman wins that fight without telling you.
#
set -euo pipefail

PREFIX="${PREFIX:-/usr/local}"
REPO="${IDLECTL_REPO:-https://github.com/Eric-Canas/cachyos-idlectl.git}"
REF="${IDLECTL_REF:-main}"
DRY_RUN=0
FORCE=0

say()  { printf '\033[1m==>\033[0m %s\n' "$*"; }
warn() { printf '\033[1;33m==> warning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31m==> error:\033[0m %s\n' "$*" >&2; exit 1; }

usage() {
	cat <<'EOF'
Usage: install.sh [OPTIONS]

  --prefix DIR   install prefix (default: /usr/local; the AUR package uses /usr)
  --ref REF      git ref to build (default: main)
  --dry-run      print what would happen and change nothing
  --force        install even when an AUR helper is available
  -h, --help     this

Environment: PREFIX, IDLECTL_REPO, IDLECTL_REF override the same settings.

After installing, nothing is running yet. That is deliberate:

    sudo systemctl enable --now idlepolicyd.service     # the decider
    systemctl --user enable --now idlectl-agent.service # in a graphical session
    idlectl doctor                                      # what works here

Read `idlectl doctor` before enabling anything else. In particular it names every
OTHER candidate owner of this machine's power state -- your desktop's own suspend
timer, logind's IdleAction, a running swayidle -- and two things deciding when a
machine sleeps is worse than either one deciding badly.
EOF
}

while [ $# -gt 0 ]; do
	case "$1" in
		--prefix) PREFIX="${2:?--prefix needs a directory}"; shift 2 ;;
		--ref)    REF="${2:?--ref needs a git ref}"; shift 2 ;;
		--dry-run) DRY_RUN=1; shift ;;
		--force)  FORCE=1; shift ;;
		-h|--help) usage; exit 0 ;;
		*) die "unknown option $1 (try --help)" ;;
	esac
done

run() {
	if [ "$DRY_RUN" -eq 1 ]; then
		printf '  would run: %s\n' "$*"
	else
		"$@"
	fi
}

# ----------------------------------------------------------------------------------
# Refuse to run as root.
#
# Not squeamishness: the build has to happen as an unprivileged user because cargo
# fetches and compiles third-party code, and doing that as root for a `curl | bash`
# script is precisely the shape of thing nobody should get used to. Only the install
# step is elevated, and it is elevated one command at a time so you can see it.
# ----------------------------------------------------------------------------------
[ "$(id -u)" -ne 0 ] || die "do not run this as root. It will ask for sudo only for the install step."

command -v pacman >/dev/null 2>&1 \
	|| die "this installer targets Arch-based systems (CachyOS, Arch, EndeavourOS). \
On anything else, build with 'make build' and install with 'make DESTDIR=... prefix=... install'."

# ----------------------------------------------------------------------------------
# The AUR is better. Say so before doing the worse thing.
# ----------------------------------------------------------------------------------
if [ "$FORCE" -eq 0 ]; then
	for helper in paru yay pikaur trizen; do
		if command -v "$helper" >/dev/null 2>&1; then
			cat >&2 <<EOF
$helper is installed, and a packaged install is better than this one in every way that
matters: pacman owns the files, an upgrade replaces them, and 'pacman -Rns idlectl'
removes them completely. This script installs into $PREFIX where pacman cannot see it.

    $helper -S idlectl

Re-run with --force if you meant to build from source anyway.
EOF
			exit 1
		fi
	done
fi

# ----------------------------------------------------------------------------------
# Never install over a pacman-managed copy.
#
# The failure this prevents is quiet and slow: you build from source into /usr, pacman
# later upgrades its own idlectl over the top, and the machine is running a binary
# neither of you chose. pacman -Qo is the authority on who owns a path.
# ----------------------------------------------------------------------------------
for binary in idlectl idlepolicyd idlectl-agent; do
	if owner=$(pacman -Qoq "$PREFIX/bin/$binary" 2>/dev/null) && [ -n "$owner" ]; then
		die "$PREFIX/bin/$binary is owned by the pacman package '$owner'. \
Remove it first ('sudo pacman -Rns $owner') or install to a different --prefix."
	fi
done

if systemctl is-active --quiet idlepolicyd.service 2>/dev/null; then
	warn "idlepolicyd is running. It will keep running the OLD binary until you restart it:"
	warn "    sudo systemctl restart idlepolicyd.service"
fi

# ----------------------------------------------------------------------------------
# Build dependencies. Named individually rather than as one list, because "install
# these six things" is a worse message than "you are missing scdoc".
# ----------------------------------------------------------------------------------
missing=()
for tool in git cargo rustc make install scdoc; do
	command -v "$tool" >/dev/null 2>&1 || missing+=("$tool")
done
if [ ${#missing[@]} -gt 0 ]; then
	die "missing build tools: ${missing[*]}
Install them with:  sudo pacman -S --needed git rust base-devel scdoc"
fi

# The MSRV is declared in Cargo.toml as rust-version, so cargo fails with a clear
# message rather than a wall of type errors. Nothing is checked here on purpose: a
# second copy of the version floor is a second thing to forget to update.

workdir=$(mktemp -d -t idlectl-build.XXXXXXXX)
cleanup() { rm -rf -- "$workdir"; }
trap cleanup EXIT

say "cloning $REPO at $REF"
run git clone --depth 1 --branch "$REF" "$REPO" "$workdir/src"

say "building (this compiles a few hundred crates the first time)"
run make -C "$workdir/src" build

say "installing into $PREFIX (sudo)"
run sudo make -C "$workdir/src" prefix="$PREFIX" install

# The Makefile asserts the two invariants a reviewer would otherwise take on trust:
# nothing under /etc, and no unit enabled. Running it here means this script is held to
# the same standard as the package.
say "verifying the install touched nothing it should not have"
run sudo make -C "$workdir/src" prefix="$PREFIX" DESTDIR= verify-install

if [ "$DRY_RUN" -eq 1 ]; then
	say "dry run: nothing was changed"
	exit 0
fi

# ----------------------------------------------------------------------------------
# A prefix other than /usr needs the D-Bus and polkit data where those daemons look,
# which is /usr/share regardless of where the binaries went. Say so rather than
# installing a copy that cannot take its bus name and leaves the user debugging it.
# ----------------------------------------------------------------------------------
if [ "$PREFIX" != "/usr" ]; then
	cat <<EOF

NOTE: you installed to $PREFIX. D-Bus and polkit only read their own directories under
/usr/share, so the bus policy and the polkit actions have landed somewhere they will not
be found, and idlepolicyd will fail to take its bus name. Link them:

    sudo ln -sf $PREFIX/share/dbus-1/system.d/io.github.ericcanas.Idlectl1.conf \\
                /usr/share/dbus-1/system.d/
    sudo ln -sf $PREFIX/share/polkit-1/actions/io.github.ericcanas.Idlectl1.policy \\
                /usr/share/polkit-1/actions/
    sudo systemctl reload dbus

Installing with --prefix /usr avoids all of this, at the cost of putting unpackaged
files where pacman expects to be the only writer.
EOF
fi

cat <<EOF

Installed. NOTHING IS RUNNING YET.

    idlectl check-config      # the shipped policy, before you enable anything
    sudo systemctl enable --now idlepolicyd.service
    systemctl --user enable --now idlectl-agent.service   # in a graphical session
    idlectl doctor            # what works here, and what else owns this machine's power

If you have a graphical session, the agent is not optional: without it the daemon cannot
tell whether somebody is at the keyboard, reports human_active as indeterminate, and
refuses to sleep. That is the safe direction, and doctor says so in one line.

Then turn your desktop's own automatic suspend OFF. Two things deciding when a machine
sleeps is worse than either one deciding badly, and 'idlectl doctor' will keep telling
you about it until you do.

Uninstall:  packaging/uninstall.sh --prefix $PREFIX
EOF
