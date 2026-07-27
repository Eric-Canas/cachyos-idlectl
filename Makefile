# idlectl -- packaging contract.
#
# The two calls a package recipe is expected to make, and the only two that are
# guaranteed to keep working:
#
#     make build
#     make DESTDIR="$pkgdir" prefix=/usr install
#
# THE PKGBUILD IS NOT IN THIS REPOSITORY, ON PURPOSE. makepkg's source array has to
# point at a published tarball plus its checksum, so a PKGBUILD living inside the
# tree it builds is either self-referential or permanently one release stale. The
# surveyed native packages agree: system76-power, hypridle, scx, chwd and
# CachyOS-Welcome all carry zero root PKGBUILDs. This Makefile is the interface a
# PKGBUILD is written against instead.
#
# NOTHING IS INSTALLED UNDER /etc, AND NOTHING IS ENABLED.
#
#   * The vendor configuration goes to $(vendorconfdir), which the package owns.
#     /etc/idlectl/ belongs to the administrator: never created, never modified,
#     never removed here. `make verify-install` asserts it.
#   * No systemd preset is shipped and no unit is enabled. Installing a program that
#     can suspend a machine is not the same act as permitting it to.

# ----------------------------------------------------------------------------------
# Directories. `prefix` is the autotools spelling and the one the contract above uses;
# PREFIX is accepted too, because plenty of recipes reach for it out of habit.
# ----------------------------------------------------------------------------------
PREFIX      ?= /usr
prefix      ?= $(PREFIX)
exec_prefix ?= $(prefix)
bindir      ?= $(exec_prefix)/bin
libdir      ?= $(prefix)/lib
datadir     ?= $(prefix)/share
mandir      ?= $(datadir)/man

vendorconfdir         ?= $(libdir)/idlectl
systemd_system_unitdir ?= $(libdir)/systemd/system
systemd_user_unitdir   ?= $(libdir)/systemd/user
dbus_policydir        ?= $(datadir)/dbus-1/system.d
dbus_interfacesdir    ?= $(datadir)/dbus-1/interfaces
polkit_actiondir      ?= $(datadir)/polkit-1/actions
bash_completiondir    ?= $(datadir)/bash-completion/completions
zsh_completiondir     ?= $(datadir)/zsh/site-functions
fish_completiondir    ?= $(datadir)/fish/vendor_completions.d
licensedir            ?= $(datadir)/licenses/idlectl
docdir                ?= $(datadir)/doc/idlectl

# ----------------------------------------------------------------------------------
# Tools
# ----------------------------------------------------------------------------------
CARGO   ?= cargo
INSTALL ?= install
SCDOC   ?= scdoc

# Left empty so the packager can add --locked --offline --frozen without this file
# having an opinion about network access inside a build chroot.
CARGO_FLAGS ?=

# Exported so cargo and the install rules below cannot disagree about where the
# binaries landed.
CARGO_TARGET_DIR ?= target
export CARGO_TARGET_DIR
RELEASE_DIR = $(CARGO_TARGET_DIR)/release

BINARIES  = idlectl idlepolicyd idlectl-agent
MAN_PAGES = man/idlectl.1 man/idlectl.toml.5 man/idlepolicyd.8

DBUS_NAME = io.github.ericcanas.Idlectl1

.PHONY: all build build-rust build-man install install-bin install-data install-man \
        install-completions install-doc uninstall verify-install check test fmt \
        fmt-check lint audit clean help

all: build

# ----------------------------------------------------------------------------------
# Build
# ----------------------------------------------------------------------------------
build: build-rust build-man

build-rust:
	$(CARGO) build --release $(CARGO_FLAGS)

# Fails loudly if scdoc is missing rather than skipping the man pages. A package that
# quietly ships without documentation is a package nobody notices is broken; scdoc
# belongs in makedepends.
build-man:
	@command -v $(SCDOC) >/dev/null 2>&1 || { \
	  echo "error: $(SCDOC) not found. It is required to build the man pages."; \
	  echo "       Install scdoc, or run 'make build-rust' to skip them."; \
	  exit 1; }
	$(MAKE) -C man SCDOC=$(SCDOC)

# ----------------------------------------------------------------------------------
# Install
# ----------------------------------------------------------------------------------
install: install-bin install-data install-man install-completions install-doc

install-bin:
	$(INSTALL) -Dm0755 -t "$(DESTDIR)$(bindir)/" \
	  $(addprefix $(RELEASE_DIR)/,$(BINARIES))

install-data:
	# Vendor default configuration. NOT /etc: see the header.
	$(INSTALL) -Dm0644 data/idlectl.toml \
	  "$(DESTDIR)$(vendorconfdir)/idlectl.toml"
	# systemd units. Neither is enabled by anything here.
	$(INSTALL) -Dm0644 data/idlepolicyd.service \
	  "$(DESTDIR)$(systemd_system_unitdir)/idlepolicyd.service"
	$(INSTALL) -Dm0644 data/idlectl-agent.service \
	  "$(DESTDIR)$(systemd_user_unitdir)/idlectl-agent.service"
	# D-Bus: bus policy, then the interface definition so other projects can
	# generate bindings without depending on this package.
	$(INSTALL) -Dm0644 data/$(DBUS_NAME).conf \
	  "$(DESTDIR)$(dbus_policydir)/$(DBUS_NAME).conf"
	$(INSTALL) -Dm0644 data/$(DBUS_NAME).xml \
	  "$(DESTDIR)$(dbus_interfacesdir)/$(DBUS_NAME).xml"
	# polkit actions.
	$(INSTALL) -Dm0644 data/$(DBUS_NAME).policy \
	  "$(DESTDIR)$(polkit_actiondir)/$(DBUS_NAME).policy"

install-man: build-man
	$(INSTALL) -Dm0644 man/idlectl.1      "$(DESTDIR)$(mandir)/man1/idlectl.1"
	$(INSTALL) -Dm0644 man/idlectl.toml.5 "$(DESTDIR)$(mandir)/man5/idlectl.toml.5"
	$(INSTALL) -Dm0644 man/idlepolicyd.8  "$(DESTDIR)$(mandir)/man8/idlepolicyd.8"

# Completions are checked into the tree by hand rather than generated at build time,
# the way paru does it: generating them would mean running the freshly built binary
# during packaging, which cross-building forbids. Guarded with wildcard so this
# Makefile keeps working while they are being written.
install-completions:
ifneq ($(wildcard completions/idlectl.bash),)
	$(INSTALL) -Dm0644 completions/idlectl.bash \
	  "$(DESTDIR)$(bash_completiondir)/idlectl"
endif
ifneq ($(wildcard completions/_idlectl),)
	$(INSTALL) -Dm0644 completions/_idlectl \
	  "$(DESTDIR)$(zsh_completiondir)/_idlectl"
endif
ifneq ($(wildcard completions/idlectl.fish),)
	$(INSTALL) -Dm0644 completions/idlectl.fish \
	  "$(DESTDIR)$(fish_completiondir)/idlectl.fish"
endif

install-doc:
	$(INSTALL) -Dm0644 LICENSE "$(DESTDIR)$(licensedir)/LICENSE"
ifneq ($(wildcard README.md),)
	$(INSTALL) -Dm0644 README.md "$(DESTDIR)$(docdir)/README.md"
endif

# Asserts the two invariants a reviewer would otherwise have to take on trust.
# Run it after `make DESTDIR=... install`.
verify-install:
	@if [ -e "$(DESTDIR)/etc" ]; then \
	  echo "FAIL: the package installed something under /etc:"; \
	  find "$(DESTDIR)/etc" -mindepth 1 -maxdepth 3; \
	  exit 1; \
	fi
	@if find "$(DESTDIR)$(systemd_system_unitdir)" \
	         "$(DESTDIR)$(systemd_user_unitdir)" \
	         -name '*.wants' -o -name '*.preset' 2>/dev/null | grep -q .; then \
	  echo "FAIL: the package enabled a unit or shipped a preset."; \
	  exit 1; \
	fi
	@echo "OK: nothing under /etc, no unit enabled, no preset shipped."

uninstall:
	rm -f $(addprefix "$(DESTDIR)$(bindir)"/,$(BINARIES))
	rm -f "$(DESTDIR)$(vendorconfdir)/idlectl.toml"
	rm -f "$(DESTDIR)$(systemd_system_unitdir)/idlepolicyd.service"
	rm -f "$(DESTDIR)$(systemd_user_unitdir)/idlectl-agent.service"
	rm -f "$(DESTDIR)$(dbus_policydir)/$(DBUS_NAME).conf"
	rm -f "$(DESTDIR)$(dbus_interfacesdir)/$(DBUS_NAME).xml"
	rm -f "$(DESTDIR)$(polkit_actiondir)/$(DBUS_NAME).policy"
	rm -f "$(DESTDIR)$(mandir)/man1/idlectl.1"
	rm -f "$(DESTDIR)$(mandir)/man5/idlectl.toml.5"
	rm -f "$(DESTDIR)$(mandir)/man8/idlepolicyd.8"
	rm -f "$(DESTDIR)$(bash_completiondir)/idlectl"
	rm -f "$(DESTDIR)$(zsh_completiondir)/_idlectl"
	rm -f "$(DESTDIR)$(fish_completiondir)/idlectl.fish"
	rmdir --ignore-fail-on-non-empty "$(DESTDIR)$(vendorconfdir)" 2>/dev/null || true
	# /etc/idlectl is deliberately left alone: the package never created it.

# ----------------------------------------------------------------------------------
# Development
# ----------------------------------------------------------------------------------
check: test fmt-check lint

test:
	$(CARGO) test --workspace $(CARGO_FLAGS)

fmt:
	$(CARGO) fmt --all

fmt-check:
	$(CARGO) fmt --all -- --check

lint:
	$(CARGO) clippy --workspace --all-targets $(CARGO_FLAGS) -- -D warnings

audit:
	$(CARGO) deny check

clean:
	$(CARGO) clean
	$(MAKE) -C man clean

help:
	@echo "idlectl"
	@echo
	@echo "  make build                                 build binaries and man pages"
	@echo "  make DESTDIR=DIR prefix=/usr install       stage an install"
	@echo "  make DESTDIR=DIR verify-install            assert nothing landed in /etc"
	@echo
	@echo "  make check     test + fmt-check + lint"
	@echo "  make audit     cargo deny (licences, advisories, banned crates)"
	@echo "  make clean"
