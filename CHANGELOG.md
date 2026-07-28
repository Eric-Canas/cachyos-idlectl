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

## [0.5.0] - 2026-07-28

A lease now names the process holding it. A minor release and not a patch, because the D-Bus
interface changes — see the migration note under **Changed**.

### Added

- **`idlectl lease list` and `idlectl status` show the holder's pid** ([FACT-13b]). A lease is the
  only thing that can hold a machine awake with nothing in the configuration to point at, and until
  now the only identity it carried was a uid — which every process that user owns shares. Measured
  need: a lease called `eval-flake` held a machine awake, and finding the process behind it took a
  walk over `/proc/*/fd` and an `ss -xp` cross-reference of socket inodes.

  The pid is a **diagnostic and never an identity**, so it is never shown bare. The daemon records
  the holder's process start time beside the pid and re-checks both at the moment of reporting, which
  turns the two dishonest answers into states:

  | Shown | Meaning |
  |---|---|
  | `pid 3878 (idlectl)` | Still the process that took the lease. |
  | `pid 3878 (gone)` | Exited, yet the lease stands — a child inherited the descriptor. |
  | `pid 3878 (recycled)` | Live, but a **different** process now. Deliberately unnamed. |
  | `pid unknown` | The bus did not answer for the caller's connection. |

  `recycled` is not a hypothetical tidied away: authorization in this daemon is performed against a
  bus name rather than a pid *precisely* because pids are recycled. A diagnostic can survive that
  objection where a decision cannot, but only if it pays for it — an unqualified pid next to a held
  lease reads as an instruction, and the obvious next step lands on a bystander.

- **`idlectl status` says when the client and the daemon are different builds.** Installing the
  package does not restart the daemon, and every answer on that screen comes from the process that is
  *running*. Previously the only clue was output that did not match the release notes.

### Changed

- **`ListLeases` now returns `a(ssttuuss)`** — `(who, why, acquired_usec, expires_usec, uid,
  holder_pid, holder_state, holder_comm)`. `holder_pid` is 0 when unknown, and `holder_state` is one
  of `alive`, `gone`, `recycled`, `unknown`.

  **Migration.** The CLI and the daemon ship in the same package, so a normal upgrade needs nothing —
  but `pacman` does not restart the daemon, so until `systemctl restart idlepolicyd` runs, the new
  `idlectl` is talking to the old interface. It says so on the first line of `status` and prints
  `UNREADABLE` where the lease list would go, rather than an empty section: "nothing is holding this
  machine awake" and "something might be and I cannot see it" are the two answers that command exists
  to tell apart, and `unwrap_or_default()` rendered them identically. Third-party clients reading the
  five-field tuple must widen it.

- `idlectl lease list` and the `holding this machine awake` block of `idlectl status` gained a column.
  `--json` gained `holder_pid`, `holder_state` and `holder_comm` as three separate fields, so a
  caller that wants to act on the pid has to read the state and one that only prints it need not
  parse a sentence.

### Fixed

- **The shipped D-Bus introspection XML declared `ListLeases` as `a(sttu)`** while the implementation
  returned `a(ssttu)` — one `s` short, with a comment beside it correctly listing all five fields.
  Anything generating bindings from that file, which is what a static introspection XML is *for*, got
  a signature that could never decode a reply. Now `a(ssttuuss)`, and the preflight gate that parses
  every tracked XML checks it parses but cannot check that it is true; only reading it against the
  implementation does.

## [0.4.7] - 2026-07-28

Four defects, all found by running a real machine on this daemon for the first time rather than
alongside it. The first one is the one that matters.

### Fixed

- **A session that started before its compositor advertised a blanking protocol never blanked
  again.** `idlectl-agent` decided its blanking capability once, during `connect`, from two Wayland
  round trips. Measured on a cold boot of a KDE session: the agent started one second after the user
  manager, KWin had not yet advertised `org_kde_kwin_dpms`, and everything else worked perfectly —
  the agent registered, reported idle correctly, and told the daemon `can_blank=false`. The panel
  then stayed lit for the entire session, on a machine whose panel is an OLED television. The
  previous boot had won the same race, which is why three days of co-existence testing never showed
  it.

  The capability is no longer a value captured at start-up. The registry listener installs a blanker
  whenever the protocol turns up, however late, `can_blank` is read on every report, and the agent
  re-registers when the answer changes so the daemon learns. A compositor restart is covered by the
  same path. `[OBS-9]`.

- **Every ordinary suspend logged a warning about the sleep mechanism refusing.** The in-flight
  guard of `[ACT-2]` was held for the duration of the D-Bus call, and that is not the duration of the
  transition: logind returns as soon as it has *accepted* the request, while the machine takes
  seconds to go down running sleep hooks. The next evaluation landed inside that window, found the
  deadline still passed, issued the same action again, and got back:

  ```
  WARN the sleep mechanism refused action="suspend"
       error="org.freedesktop.login1.OperationInProgress: Action suspend already in progress"
  ```

  A warning that fires on every healthy suspend is a warning nobody reads, and `[ACT-3]` means it to
  be read. The guard now holds until logind announces the resume, or for two minutes if no sleep ever
  materialises — an accepted transition that was quietly abandoned must not wedge the daemon into
  never trying again.

- **The age of a remote session was its timestamp, not its age.** logind's `TimestampMonotonic` was
  handed straight to an elapsed-duration formatter, so every session reported the same figure —
  uptime minus time spent suspended — and the *later* of two sessions reported the *larger* one. On a
  machine with 14h37m of suspend behind it, a session opened two minutes earlier read as
  `opened 8h24m ago`. `[FACT-10]` exists so that a session-scope veto is recognisable instead of
  looking like a broken detector; printed that way it did the opposite. It now reads the realtime
  `Timestamp`, and the property that the number must shrink as a session gets newer has a test.

### Added

- **`doctor` reports whether each action has a mechanism at all**, not just `screen_off`, by asking
  logind rather than working it out. On a machine whose only swap is zram and which has no `resume=`
  on its kernel command line, logind answers `na` to `CanHibernate` and knew so all along, while
  `doctor` said nothing and a forced hibernate came back with `SleepVerbNotSupported: Not enough
  suitable swap space`. Extends `[ACT-13]` to the three actions logind performs.

  `hibernate` being unavailable does not make the report unhealthy: most machines cannot hibernate
  and that is not a fault. `suspend` or `poweroff` being unavailable does — an idle daemon that
  cannot rest the machine has nothing left to offer.

## [0.4.6] - 2026-07-28

### Fixed

- **`idlectl status | head` no longer panics.** Rust sets `SIGPIPE` to `SIG_IGN` before `main`, so
  writing to a closed pipe returns `EPIPE` and the `println!` machinery panics with a backtrace:

  ```
  thread 'main' panicked at library/std/src/io/stdio.rs:1166:9:
  failed printing to stdout: Broken pipe (os error 32)
  ```

  Piping into `head`, `grep -q` or `less` and quitting early is an entirely ordinary thing to type
  — it appears in this project's own documentation — and a diagnostic tool that produces a
  backtrace when you do it is a diagnostic tool people stop trusting.

  The usual fix is to restore the default signal handler, which needs `unsafe`; every crate here
  is `#![forbid(unsafe_code)]`, so the write is checked instead and a closed pipe exits **zero**.
  Zero rather than `128 + SIGPIPE` because the reader closed the pipe having got what it wanted,
  which is not this program failing. Any other write error — a full disk on a redirected stdout —
  is still reported and still exits non-zero.

## [0.4.5] - 2026-07-28

### Added

- **`status` now shows what is holding the machine awake.** A lease and a held request are the only
  two things that can keep a machine awake **without appearing anywhere in the configuration**, so
  `explain` — which walks the blocks — structurally cannot show them. Until now the answer to "why
  will this not sleep?" was `lease_held true` with no indication of *who*, and a held request was
  not mentioned at all. Both now appear on the first screen:

  ```
  holding this machine awake
    lease    backup                 uid 1000   in 1h59m  — nightly backup
    request  poweroff               uid 1000   in 5h59m  — will happen when every veto clears
  ```

  The section is printed only when there is something in it, so `status` on an idle machine is
  exactly as short as it was.

- **`Pending` property** on the manager interface, and `pending` in the JSON report. A list of zero
  or one rather than a tuple with a sentinel, so "no request" and "a request for the action whose
  name happens to be empty" are not the same wire value.

- **A logo**, in [`docs/logo/`](docs/logo/): the IEC power symbol with the vertical stroke through
  its gap replaced by a Z. `icon.svg` drops the second, smaller Z — it reads at 128px and becomes a
  smudge at 16.

### Changed

- **The README is 477 lines instead of 793**, and answers "what is this, how do I install it, how do
  I use it" before it argues about anything. It gained a `Usage` section with real command output,
  which it did not have; the configuration reference, the fact-by-fact measurements, the security
  model and the design rationale moved to [`docs/`](docs/) intact. Nothing was deleted.

- `Install` leads with `paru -S idlectl` and explains why `pacman` cannot fetch it on its own,
  instead of assuming the reader knows what an AUR helper is for.

## [0.4.4] - 2026-07-28

### Added

- **The daemon now reads the screen back, and says so when it disagrees** ([OBS-8]). Its record of
  the screen state was an intent — written when it asked for a change, never read back — and by
  [ACT-7] it does not re-issue an action it believes is already in effect. Every way a blank can
  fail to take therefore ended in a lit panel that nothing reported and nothing retried: a
  compositor that ignores the request, another program turning the panel back on, a compositor
  restart that leaves the protocol object dead, an output replaced by a hotplug. That is exactly
  the shape of the `0.4.3` defect, and it survived three days of side-by-side comparison because
  nothing was looking.

  Each evaluation now compares what was asked for against the sessions' `Blanked` property, which
  since `0.4.3` is written by the display server. A disagreement that survives 60 seconds — two
  agent heartbeats — produces one log record and a `DIVERGED` line in `doctor`.

  **It does not correct.** Input turns the panel back on before `resumed` has been delivered, so a
  daemon that re-issued on sight would blank the screen in the face of somebody who had just
  picked up the controller: a worse, more visible and more frequent failure than the one it fixes,
  landing on the highest-priority use case. Whether correcting is worth it is a question for
  evidence, and this is the change that produces the evidence.

## [0.4.3] - 2026-07-28

### Fixed

- **Blanking now actually reaches the compositor.** The Wayland backend handed a blank request to
  its event thread over an mpsc channel, and that thread spends its life parked in
  `blocking_dispatch` waiting for the *compositor* to speak. Nothing in a channel send wakes that
  up. So the request was only written the next time the compositor happened to send an unrelated
  event — and on a quiet seat, which is the only state in which anything ever asks for a blank, no
  event is due. Confirmed with `WAYLAND_DEBUG=1` on a KWin 6.7.3 session: across a 40-second blank
  the `org_kde_kwin_dpms.set` request never appeared on the wire at all, and the panel stayed lit.

  Requests are now issued and flushed on the thread that makes them, which is what
  `wayland-client` supports and what removes the wakeup problem instead of working around it. The
  event thread reads and only reads. Same trace after the fix: `-> org_kde_kwin_dpms@8.set(3)`
  immediately, `<- mode, (3)` back from KWin, and a panel that goes dark.

- **`Blanked` reports what the display server says, not what was asked for.** The property was set
  from the request, so the bug above read back as a screen that had been successfully turned off.
  It is now taken from the compositor's `mode` event (`org_kde_kwin_dpms` and
  `zwlr_output_power_v1` both send one), falling back to the request only on X11, where nothing is
  volunteered. `Backend::observed_blank` returns `Option<bool>` precisely so that "nothing has
  reported" stays distinguishable from "lit".

  This is the seat rule applied to the screen: an observation nobody made is the one claim this
  agent must not make. Had it been applied here from the start, the defect above would have been
  visible from the first `busctl` call rather than surviving days of co-existence testing.

## [0.4.2] - 2026-07-28

### Fixed

- **A held request's TTL is now armed as a deadline.** While anything contributes `never` the
  resolved deadline is `never` and no timer is set, so a request held under [REQ-6] could outlive
  its own TTL and only be noticed when something unrelated happened to wake the loop. This is the
  same hole the lease TTL already had a line of code for, and the comment next to it says why: a
  bound the policy engine cannot see has to be armed separately.

  Retrying was never affected: whatever holds a request is a fact, and a fact is either polled --
  in which case the sweep arms it -- or event-driven, in which case the event pokes the loop. What
  no fact reports is the passage of the TTL itself.

## [0.4.1] - 2026-07-28

### Fixed

- **`local_service_busy` no longer treats a stopped service as doubt.** Any failure to read
  `counters_url` produced `INDETERMINATE`, including a refused connection — so pointing the fact at
  a service that is started on demand and stops itself when idle vetoed every sleep action for as
  long as the machine was up. The fact was unusable for exactly the kind of service it was written
  for, and the only safe setting was to leave it unconfigured, which also means never getting the
  veto it exists to provide.

  A refused connection now reads FALSE: nothing is listening, so no work is in flight anywhere on
  that port. Everything else — a timeout, an unparseable reply, a non-200 — stays `INDETERMINATE`
  under [FACT-34], which is about a service that is *up* while its counter endpoint is off, where
  the daemon really cannot tell. New: [FACT-45].

## [0.4.0] - 2026-07-28

Both changes here come from one question: can a machine running this do everything the system it
was extracted from did? Two answers were no.

### Fixed

- **An action asked for by name is no longer traded for a shallower one.** `rest --now` makes the
  base schedule contribute `now` for *every* action it names, so `suspend` is due at the same
  instant as the `poweroff` somebody asked for — and the shallowest-wins rule of [ACT-7] then
  picked the suspend. `idlectl rest --action poweroff` therefore **suspended the machine and
  reported the request unsatisfied**: the caller was told no, the machine went to sleep, and a
  relay that recorded it as powered off had no way to know better. [ACT-7b] names that command as
  a supported route to a deeper action than the schedule would ever pick, so the promise was not
  being kept. A requested action is now the only one that may be performed in that evaluation; if
  it is held, nothing happens. New: [ACT-7c], [TEST-26].

  The same applied to `rest --force --action poweroff` on an already idle machine, where it
  mattered more: a deliberate, authenticated, logged request quietly doing something shallower.

### Added

- **`idlectl rest --pending <duration>`** — ask, and be remembered if the answer is "not yet"
  ([REQ-6], previously specified but not implemented). The daemon holds the request and carries it
  out the moment the last veto clears, giving up at the TTL. Without it, a relay that finds a game
  running has to own a timer, a retry budget and its own copy of "is it still worth asking" —
  three things the daemon already has.

  A held request is not a weaker one: every retry evaluates exactly the floors the original did, so
  a download that starts afterwards refuses it just as surely as one already running would have.
  What is remembered is the asking, never the answer. It is dropped when somebody uses the machine
  ([REQ-7]); a resume is not somebody using the machine ([REQ-8]), which is only reliable because
  0.3.0 stopped the idle clock from moving on a wake. New: [TEST-27].

  `idlectl rest --cancel` forgets it and `idlectl doctor` shows it while it is held. It lives in
  memory: `idlepolicyd.service` documents that the daemon writes no runtime state anywhere, and
  that property is worth more than surviving its own restart. The prior system kept this in `/run`
  because it was not a daemon — a timer ran a script every five minutes, so all of its state had to
  outlive the process.

- D-Bus: `RestPending(s action, t ttl_usec) -> b performed_now` and `CancelPending() -> b had_one`,
  both under the existing `io.github.ericcanas.Idlectl1.rest` polkit action. Holding a request
  reaches nothing `Rest()` does not; it only keeps trying. Existing signatures are unchanged.

### Documentation

- README: **how to let a machine with nobody in front of it be asked to rest.** The shipped polkit
  default requires authentication from anywhere that is not the seat, which is every ssh session,
  so a relay got `AccessDenied` and the README did not say what to do about it — the single most
  likely deployment could not be completed by reading it. Now it carries the rule file, and says to
  grant `.rest` and never `.rest-forced`.

## [0.3.0] - 2026-07-28

Both entries here are one defect seen twice: the agent's idle clock was being reset by things
that are not a human. Found by running the daemon in `--dry-run` alongside an existing idle
watcher on the same machine and comparing what each decided. Over that window the daemon
proposed 47 actions, all of them `screen_off`, and never once proposed a sleep, while the other
watcher suspended the machine 13 times on the same evidence.

### Fixed

- **Restarting the session agent no longer restarts every countdown measured from the
  human-input clock.** The agent reported "idle for zero seconds" before it had observed a
  single transition, which states that input just happened. Measured with nobody in the room:
  restarting the agent moved the daemon's `human_input` origin from +25823 s to +26299 s —
  exactly the uptime at the restart — while an independent `swayidle` timestamp watching the
  same seat did not move at all. A package upgrade was enough to trigger it.

  `ext-idle-notify-v1` reports transitions and has no request for the current idle time, and the
  compositor will not fill the gap either: KWin under Wayland answers
  `org.freedesktop.ScreenSaver.GetSessionIdleTime` with `not supported on this platform`. So the
  agent now carries its last-input instant across its own restarts in `$XDG_RUNTIME_DIR`, which
  survives suspend and is cleared by a cold boot, and refuses to adopt it when nothing was
  watching the seat for more than 90 s. Where there is nothing credible to carry, it reports
  unknown rather than zero — the state that vetoes sleep — until the first transition arrives.
  New: [CLK-14], [CLK-15], [TEST-25].

- **The agent's idle time is now measured on `CLOCK_BOOTTIME`.** It was `std::time::Instant`,
  which is `CLOCK_MONOTONIC` and stops while the machine is suspended, while the daemon anchors
  origins on `CLOCK_BOOTTIME`. The two halves of one subtraction were on different timelines.
  Measured on a machine up for 7 h 24 m of which 2 h 16 m were spent suspended: `CLOCK_BOOTTIME`
  26649 s against `CLOCK_MONOTONIC` 18471 s, a gap of 8178 s. The visible symptom was the daemon
  recording a seat nobody had touched in seventy minutes as "input in session 1 3m ago" after
  every resume, and holding `suspend` at `never` for the following five minutes. New: [CLK-13].

The X11 backend needed neither fix: `MIT-SCREEN-SAVER` exposes the idle counter itself, so there
is nothing to reconstruct and nothing to carry.

### Changed

- `idlectl-agent` now depends on `idlectl-policy` for `BootInstant` and on `rustix` for one
  `clock_gettime` call. It still contains no policy: the type is shared so that the instant the
  agent reports and the origin the daemon computes cannot drift onto different clocks.

## [0.2.1] - 2026-07-27

### Fixed

- **The agent's own sandbox made the GPU source it had just been given blind.** `0.2.0` moved the
  DRM `fdinfo` walk into the session agent to avoid granting the daemon `CAP_SYS_PTRACE`, and then
  the agent reported zero holders on a machine with two processes publishing DRM memory. The cause
  is one layer deeper than a wrong directive.

  An unprivileged user manager cannot give a unit a mount namespace without an unprivileged **user**
  namespace as well, and `CapabilityBoundingSet=` needs one too, since dropping your own bounding
  set requires `CAP_SETPCAP` — without it the unit dies at `status=218/CAPABILITIES`. Measured on a
  live session: a plain user unit runs in `user:[4026531837]`, the initial namespace; add
  `PrivateTmp=yes` and it becomes `user:[4026532846]`, a child. From inside that child, opening
  `/proc/<pid>/fdinfo` of a process **owned by the same user** is *"Permission denied"* —
  `ptrace_may_access` wants `CAP_SYS_PTRACE` in the *target's* user namespace, and same-uid does not
  survive a namespace boundary.

  Bisected one directive at a time: `ProtectSystem=`, `ProtectHome=`, `PrivateTmp=`,
  `ProtectKernelTunables=`, `ProtectKernelLogs=`, `ProtectControlGroups=`, `ProtectHostname=`,
  `ProtectClock=` and `CapabilityBoundingSet=` each took the count from 2 to 0. `IPAddressDeny=`
  fails outright, needing a BPF program the user manager has not been delegated.

  So the agent unit now carries only directives that need no namespace: `NoNewPrivileges`,
  `RestrictSUIDSGID`, `RestrictRealtime`, `RestrictNamespaces`, `LockPersonality`,
  `MemoryDenyWriteExecute`, `UMask`, `RestrictAddressFamilies=AF_UNIX`, `SystemCallArchitectures`
  and the two `SystemCallFilter` lines. Given up: a private `/tmp`, a read-only `/usr`, an
  inaccessible `/home`. Kept: everything that constrains a bug in a parser. The process already runs
  with the user's own credentials, holds no capability either way, and the privileged half takes
  none of its claims on trust.

  `CapabilityBoundingSet=` was asserting something already true — a process started by a user
  manager has an empty capability set — at the price of a namespace that broke the agent's only GPU
  source.

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

[Unreleased]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.2.1...HEAD
[0.2.1]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.4...v0.2.0
[0.1.4]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.3...v0.1.4
[0.1.3]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.2...v0.1.3
[0.1.2]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/Eric-Canas/cachyos-idlectl/compare/v0.1.0...v0.1.1
[0.1.0]: https://github.com/Eric-Canas/cachyos-idlectl/releases/tag/v0.1.0
