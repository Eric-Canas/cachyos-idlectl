# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog 1.1.0](https://keepachangelog.com/en/1.1.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Versioning note.** Nothing has been released yet. While the version is `0.x`, the config format
and the D-Bus interface may change in a minor release; every such change will be listed under
**Changed** with a migration note, and an incompatible config will be rejected loudly with that note
rather than silently reinterpreted. From `1.0.0` on, the config file, the CLI surface and the D-Bus
interface `io.github.ericcanas.Idlectl1` are the public API and follow SemVer strictly.

## [Unreleased]

Work towards the first release, `0.1.0`. This section describes what that release will contain; no
tag exists yet.

### Added

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

[Unreleased]: https://github.com/Eric-Canas/cachyos-idlectl/commits/main
