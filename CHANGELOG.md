# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Versioning note.** While the version is `0.x`, the config format
and the D-Bus interface may change in a minor release; every such change will be listed under
**Changed** with a migration note, and an incompatible config will be rejected loudly with that note
rather than silently reinterpreted. From `1.0.0` on, the config file, the CLI surface and the D-Bus
interface `io.github.ericcanas.Idlectl1` are the public API and follow SemVer strictly.

## [Unreleased]

Nothing yet.

## [0.2.0] - 2026-07-27

### Changed

- **The DRM `fdinfo` GPU source moved from the daemon to the session agent, and `ReportSession`
  gained an argument.** `/proc/<pid>/fdinfo` is mode `0555` and still gated by `ptrace_may_access`,
  so reading *another user's* needs `CAP_SYS_PTRACE` — measured: denied under the daemon's
  `CAP_DAC_READ_SEARCH`, granted with `CAP_SYS_PTRACE`. Handing the component that can power a
  machine off the right to read any process's memory, so that video RAM can be attributed, is not a
  trade worth making. The agent runs as the session user, reads its own processes, and needs no
  capability at all — the same argument that already put `media_playing` there.

  Left as it was on purpose: **both GPU sources are still read and merged**, never one falling back
  to the other, because on a hybrid machine the integrated GPU publishes `fdinfo` for trivia while
  the discrete card publishes none. And **attribution stays in the daemon** — deciding whether a
  holder belongs to a game needs the process tree and command lines, which are world-readable, and
  an unprivileged process's claims about what is running on the machine are not something to take on
  trust. The agent reports raw `(pid, name, bytes)`; the daemon classifies.

  Where no agent runs, the source contributes nothing, exactly as if no DRM device published memory.
  It deliberately does not read `indeterminate`: an absent agent is already reported through
  `human_active`, and saying it twice would veto every sleep action on a headless machine.

  **Migration:** none for configuration files. The D-Bus method
  `io.github.ericcanas.Idlectl1.Manager.ReportSession` is now `(t idle_usec, s media_playing,
  a(ust) gpu_holders)`. Both binaries ship in the same package and are always upgraded together;
  a third-party agent implementing the old signature must add the argument.

### Fixed

- **Two contradictory `AmbientCapabilities=` lines in `idlepolicyd.service`.** The unit carried an
  empty assignment from when the bounding set was empty too, so an `AmbientCapabilities=` added
  above it would have been silently cancelled by the reset below. It changed nothing in practice —
  the service runs as uid 0, which takes its effective set from the bounding set — but a file about
  privilege should not contain two lines disagreeing with each other. Verified on the running
  daemon: `CapEff 0000000000000004`.

## [0.1.4] - 2026-07-27

### Fixed

- **The Steam detectors could not work on any ordinary install.** `useradd` creates home directories
  mode `0700`, and the daemon ran with `CapabilityBoundingSet=` empty, so root could not traverse
  `/home/<user>/` to reach `$HOME/.local/share/Steam`. Measured on a machine where Steam was plainly
  installed and a game was running: the same `ls -d /home/*/.local/share/Steam` returns the path with
  the capability and *"No such file or directory"* without it. `steam_root` in the configuration was
  no escape, because the obstacle is the home directory rather than the path.

  The cost was larger than two facts reading `unavailable`. Attribution of GPU memory to a game
  requires the Steam fact to be true, so a game holding 7280 MiB was classified as `gpu_busy_other`
  — worth twenty minutes — instead of `gpu_busy_game`, worth two hours. And `steam_downloading`, the
  veto that lets a machine finish a download overnight, could never be true at all.

  Now `CapabilityBoundingSet=CAP_DAC_READ_SEARCH` plus the matching `AmbientCapabilities=`. Read and
  search only, never `CAP_DAC_OVERRIDE`; combined with `ProtectHome=read-only` a write to a home is
  refused by the mount before permissions are consulted — measured: *"Read-only file system"*.
  Verified with a real game running: `steam_game_running` true, `steam_downloading` false with the
  staging directory named, `gpu_busy_game` true naming the process and its 7280 MiB, and
  `gpu_busy_other` false.

## [0.1.3] - 2026-07-27

### Fixed

- **Both GPU facts read `indeterminate` forever on an NVIDIA machine, under the shipped unit.**
  `idlepolicyd.service` allowed the NVIDIA character devices read-only. NVML does not merely read
  them: it drives the driver through ioctls on an `O_RDWR` handle, so every call failed and
  `nvidia-smi` reported *"NVIDIA-SMI has failed because it couldn't communicate with the NVIDIA
  driver"* — indistinguishable from a broken driver, and in fact a cgroup denial. Two smaller faults
  in the same allowlist: `/dev/nvidia0` was never listed, and `char-nvidia-frontend` is not a device
  class at all, so it matched nothing. systemd resolves `char-NAME` through `/proc/devices`, which
  lists `nvidia`, `nvidiactl`, `nvidia-modeset` and `nvidia-uvm`.

  Now `DeviceAllow=char-nvidia rw` and `DeviceAllow=char-nvidia-uvm rw`: by class, so a second card
  or different numbering keeps working. Verified under the unit's full sandbox including the empty
  capability set.

  The consequence was not a missing detector but a machine that never sleeps: `indeterminate` vetoes
  every sleep action, which is the correct direction to fail in and completely useless.

## [0.1.2] - 2026-07-27

### Fixed

- **`idlectl-agent.service` could never start.** The unit set `ProtectHome=yes`, which makes
  `/home`, `/root` **and `/run/user`** inaccessible and empty. `/run/user` is `XDG_RUNTIME_DIR`,
  which holds the Wayland socket and the session bus socket — the agent's only two inputs. Every
  start failed with *"no usable session backend"*, naming *"Could not find wayland compositor"* and
  X11's *"Authorization required"*, on a machine with a healthy Plasma session. The unit's own
  comment said the inputs are "sockets in XDG_RUNTIME_DIR" and then hid that directory.

  Now `ProtectHome=read-only`, which keeps what the hardening was for — the agent reads no file
  under a home directory and can write to none — while leaving the sockets reachable. Bisected with
  `systemd-run --user`: under `ProtectHome=yes` both `$XDG_RUNTIME_DIR/wayland-0` and
  `$XDG_RUNTIME_DIR/bus` were denied; under `read-only`, alone and combined with
  `ProtectSystem=strict`, both were reachable and the agent registered with `can_blank=true`.

  Invisible to every test that ran the binary directly, which is all of them. `TESTING.md` gains a
  step 0a that enables the packaged units and fails on exactly this.

## [0.1.1] - 2026-07-27

Found by packaging `0.1.0` and installing it, rather than by reading the code.

### Changed

- **`idlectl status` and `idlectl lease list` print how long is left, not a boot offset.**
  Every instant on the D-Bus interface is an absolute point on `CLOCK_BOOTTIME`, because
  that is the only clock that keeps counting across a suspend. Printing one raw meant a
  lease taken out for sixty seconds displayed as `expires at +6907s`, which is a true
  statement about the machine's boot and no answer at all to "how long have I got". Both
  now read `expires in 1m00s` and `in 12m30s  (at +7200s since boot)` — the absolute
  value kept as a parenthetical, because it is what the property carries, what the journal
  logs and what a second tool would line up against. The JSON output is unchanged.

### Fixed

- **The comment claiming the session agent links `libwayland-client` was wrong.** It
  reasoned from feature unification and concluded the C backend gets pulled in
  regardless. Measured on the packaged binary: `ldd idlectl-agent` lists libc, libgcc_s
  and the vdso and nothing else, and the string `libwayland` does not appear in it at
  all, so it is neither linked nor dlopened — wayland-client is built with its pure-Rust
  backend. This is load-bearing rather than trivia: it is why the distribution package
  needs no `wayland` dependency.

## [0.1.0] - 2026-07-27

The first release.

### Added

The runtime, on top of the M0 specification and pure engine: `idlepolicyd` (the resident decider),
`idlectl` (the client), `idlectl-agent` (the per-session reporter), the eleven detectors, leases,
polkit authorization and the full D-Bus surface. Verified on a real CachyOS machine rather than
reasoned about — the notes below record what that verification changed.

- **Resume is detected from `CLOCK_BOOTTIME - CLOCK_MONOTONIC`, not from a hook or a signal.**
  That difference is the total time this boot has spent suspended: the kernel maintains it, a cold
  boot zeroes it, and it survives a daemon restart. It is also the only mechanism that notices a
  sleep entered by writing `/sys/power/state` directly, which emits no `PrepareForSleep` and runs no
  sleep hook. Measured with `rtcwake -m mem`: `PM: suspend entry (deep)`, delta 23.5 s, and the
  daemon logged the resume with no signal involved. `after_resume` therefore needs no state file at
  all, and neither does anything else — the package creates no runtime directory.
- **Both GPU sources are read and merged, rather than one falling back to the other.** On a hybrid
  machine an integrated AMD GPU publishes DRM `fdinfo` for trivia while the discrete NVIDIA card
  publishes none, so a fallback chain concludes the generic source works and never asks the card the
  games run on. Measured: with the merge, a model server holding 11 353 MiB appeared immediately;
  with the fallback chain it would have read `false` with a game running.
- **`media_playing` is reported by the session agent, not read by the daemon.** A root daemon cannot
  connect to a user session bus at all — measured, the bus authenticates by uid over `EXTERNAL` and
  closes the connection. This is a privilege reduction as well as the only thing that works: the
  privileged half of the project now touches no session bus anywhere.
- **`org_kde_kwin_dpms` is supported alongside `wlr-output-power-management-v1`.** KWin implements
  `ext-idle-notify-v1` (version 2, confirmed on a Plasma session) and the wlroots power protocol not
  at all. Without the KDE backend, `screen_off` would have been permanently unavailable on the
  desktop this is most likely to be installed on.
- **A released lease wakes the loop.** A held lease contributes `never` and so arms no timer;
  without a watcher on the descriptor, a lease released by its holder exiting was never noticed and
  held the machine awake for the rest of the boot. Measured, then fixed with a watcher thread per
  lease and a timer armed at the earliest TTL.

- Specification of the policy engine in `docs/spec.md`, normative over the implementation.
- Composition rule: a config is a set of `[while.<condition>]` blocks, each giving a timeout per
  action; for each action independently the longest deadline among currently-true blocks that set a
  key for that action wins. `never` is absorbing, the maximum over the empty set is `never`, a block
  that sets no key for an action contributes nothing to it, and order never matters.
- Composition over absolute deadline instants rather than durations, so that blocks measured on
  different clocks compose meaningfully. Clocks: `human_input`, `resume`, `condition` (the last
  false → true edge of the block's own condition) and `boot` — all on `CLOCK_BOOTTIME`.
- `[when.<condition>]` ceiling blocks, resolved by shortest deadline, for hardware that degrades
  while lit. One ceiling class, absolute: a ceiling defeats every floor including `never`, may
  target any action, can only ever pull an action earlier, and is logged at load, listed by `doctor`
  as a standing hazard and named in the log record when it fires.
- Only the shallowest due sleep action is issued per evaluation — `suspend` before `hibernate`
  before `poweroff` — so the engine never escalates on its own; `screen_off` composes with them
  freely and never suppresses one.
- First-class `human_active` fact acting as a floor: `false` when the input clock has never been
  touched this boot, `indeterminate` when the clock is unreadable. Shipped enabled, with `suspend`,
  `hibernate` and `poweroff` set to `never`; only a ceiling, including the ephemeral one `--force`
  installs, beats it.
- Four-state facts — `true`, `false`, `indeterminate`, `unavailable`. Any enabled fact that is
  `indeterminate` raises a machine-wide floor on `suspend`, `hibernate` and `poweroff` whether or
  not a block references it, and deliberately **not** on `screen_off`. `unavailable` means the
  capability is absent and the condition is simply never true.
- Facts: `human_active`, `after_resume`, `remote_session`, `lease_held`, `inhibitor_block` (read as
  a signal, never trusted as enforcement), `media_playing`, `steam_game_running`,
  `steam_downloading`, `gpu_busy_game`, `gpu_busy_other` and `local_service_busy` (cumulative
  counters, not `systemctl is-active`). `always` is the only built-in condition; every unknown
  condition name is a fatal config error.
- `[facts.<name>] enabled = false` as the supported, `doctor`-reported way to switch a detector off:
  it then reports `unavailable`, does not run, and raises no doubt floor. `local_service_busy` ships
  disabled until it is given a counters endpoint.
- Steam detectors shipped first-class and enabled, degrading to `unavailable` when Steam is absent.
- `idlepolicyd`, the resident root daemon that owns every power action.
- `idlectl-agent`, an unprivileged per-session agent that reports session-scoped facts and commands
  nothing except `Blank`/`Unblank` on its own session's screen, callable only by uid 0.
- `screen_off` reports `unavailable` as an action where no agent or no blanking mechanism exists;
  blocks setting it are then inert and listed as such by `doctor`.
- `idlectl` CLI: `status`, `explain <action>` (showing each block, its clock, its origin instant,
  its deadline and the winner), `doctor`, `check-config`, `rest [--action <action>] [--now]
  [--force]`, and `lease acquire`/`release`.
- `idlectl rest --now` satisfies exactly the `always` and `after_resume` blocks — the base schedule
  and the resume settle window — by condition name, with no per-block opt-in key; every other block,
  ceiling and implicit floor is evaluated unchanged, so a remote rest request cannot suspend a
  running game. `rest` with no `--action` always means `suspend`.
- `--force` installs an ephemeral ceiling due now, so it defeats every floor including `never`, the
  human-presence floor, the doubt floor of an unreadable detector and the floor a faulty config file
  raises — while overriding no mechanism: one transition in flight, refusals logged, facts re-read
  before issuing. Separate flag, separate polkit action, logged at warning level with every floor it
  defeated.
- Leases with a mandatory TTL, for long jobs no detector can see.
- D-Bus interface `io.github.ericcanas.Idlectl1` on the system bus, with a shipped bus policy and
  polkit actions gating every state-changing method.
- `idlectl doctor` reporting of the other candidate owners of power — logind's `IdleAction` and
  `IdleActionSec`, any process owning a known desktop power-management D-Bus name, and any running
  idle helper it can detect — because two owners of power is the bug this project removes.
- Three-layer configuration, all named `idlectl.toml`, later winning: vendor
  `/usr/lib/idlectl/idlectl.toml` (package-owned, always installed, in effect on a machine with an
  empty `/etc`), admin `/etc/idlectl/idlectl.toml`, then `/etc/idlectl/conf.d/*.toml` in basename
  order. Validated on load; a file that fails to parse is dropped whole, the other layers survive,
  the three sleep actions are vetoed until the fault is fixed, `screen_off` is not, and `doctor`
  exits non-zero naming the key, the file and the offending text. Never a crash, never a machine
  with no policy — including on a first start after a bad edit.
- man pages (`idlectl`, `idlectl.toml`, `idlepolicyd`).
- `TESTING.md` with the manual suspend/resume protocol, including the normative resume case, and an
  explicit account of what CI cannot cover.

[Unreleased]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.2.0...HEAD
[0.2.0]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Eric-Canas/cachyos-idlectl/releases/tag/v0.1.0
