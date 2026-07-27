# Testing

`idlectl` decides whether a machine keeps running. A bug here does not produce a wrong pixel; it
ends somebody's session. The test strategy is split accordingly: everything that can be made
deterministic is a unit test with a fake clock, and everything that cannot is a written manual
protocol that a human runs on real hardware before a release.

- [What CI covers](#what-ci-covers)
- [What CI provably cannot cover](#what-ci-provably-cannot-cover)
- [Before you test suspend](#before-you-test-suspend)
- [The manual protocol](#the-manual-protocol)
- [Release checklist](#release-checklist)
- [Reporting a failure](#reporting-a-failure)

---

## What CI covers

Automated, on every push:

- **The composition algebra**, against an injected clock. The engine is a pure function from
  (config, facts, clock origins, now) to a resolved instant per action, and that function is tested
  exhaustively — including the normative resume case (see the spec), commutativity and
  associativity of the composition, `never` as an absorbing element, and the empty-set defaults.
- **The omission rule**: a true block that sets no key for an action contributes nothing to that
  action — silence is not `never`. The vector is `[while.always] screen_off = "10m", suspend =
  "30m"` plus a true `[while.steam_downloading] suspend = "never"`, which must resolve `suspend` to
  `+infinity` and `screen_off` to the human-input origin plus 10 minutes.
- **The ceiling model**: one class, absolute. A `[when]` ceiling defeats a `never` floor, the
  human-presence floor and the implicit floors, and can only ever pull an action earlier — no
  configuration may produce a resolved instant later than its own floor.
- **Action depth**: `screen_off` composes with a sleep action in the same evaluation; among
  `suspend`, `hibernate` and `poweroff` only the shallowest due one is issued, and an action already
  in effect is not re-issued until its deadline is re-armed.
- **Config parsing and validation**: unknown keys, unknown fact and condition names, unparseable
  durations (including a bare integer, which must be rejected with a missing-unit diagnostic),
  out-of-range values, the three-layer merge order with drop-ins winning, and degraded mode — a file
  that fails to parse is dropped whole, the other layers survive, the three sleep actions are vetoed
  and `screen_off` is not. Every invalid input must produce a diagnostic and a veto, never a panic
  and never a dead daemon.
- **Fact state machine**: `true` / `false` / `indeterminate` / `unavailable` transitions; the
  asymmetry that `indeterminate` vetoes sleeping but not `screen_off`; that the doubt veto applies
  machine-wide whether or not a block references the doubtful fact; that a fact disabled with
  `[facts.<name>] enabled = false` reports `unavailable` and contributes no veto; and that an
  indeterminate condition is false *as a selector*, in `[when]` as well as `[while]`.
- **D-Bus surface**, against a private `dbus-daemon` started by the test harness: introspection
  matches the shipped XML, read-only methods answer, state-changing methods refuse without
  authorisation.
- **Detector parsers** against captured fixtures — real recorded output from `loginctl`,
  `systemd-inhibit --list`, MPRIS property dumps, GPU query output — so a format change is caught
  even though the live source is absent.
- `cargo fmt --check`, `cargo clippy -- -D warnings`, `cargo deny check`, and the no-tokio check
  (see [CONTRIBUTING.md](CONTRIBUTING.md)).

Everything a detector reads is behind a trait with a fake implementation. If a new detector cannot
be faked, it cannot be tested, and that is a review objection.

---

## What CI provably cannot cover

This section is deliberately blunt. A green CI badge on this project means the algebra is right. It
does not mean the machine wakes up.

**Real S3 / S4.** CI runners are containers or VMs. They do not enter S3, so nothing in CI exercises
the kernel suspend and resume path, the behaviour of `CLOCK_BOOTTIME` across a genuine suspend, or
whether a timer armed before sleeping still fires after waking. The single most expensive bug this
project was built to fix — a timer that did not re-arm after a resume — is invisible to CI by
construction. Firmware differences between machines make it worse: a laptop, a NUC and a desktop
board can each get suspend subtly wrong in different ways.

**A real compositor.** `human_active` reads `ext-idle-notify-v1` or the KDE idle protocol from a
live Wayland session with real input devices. There is no Wayland session, no seat, and no input
device in CI. The measured fact that logind's `IdleHint` sits permanently `no` on a Plasma 6 Wayland
session is exactly the class of thing you only learn on hardware.

**A real GPU.** Attributing GPU utilisation to a game process versus to something else requires the
actual driver, an actual busy process, and the vendor query tool. CI tests the parser against
recorded output; it cannot tell you the driver started reporting a different unit.

**A real Steam download.** Steam's behaviour is the whole reason `steam_downloading` exists — most
notably that it declares no logind inhibitor while downloading. Reproducing that needs Steam, an
account, and a large game. It is not automatable and it must be re-checked whenever Steam changes.

**Panels, wake sources and firmware.** Whether the screen actually goes dark, whether an OLED panel
is protected, whether a wake-on-LAN packet or an RTC alarm actually wakes the board — all hardware,
all manual.

**polkit in a real seat.** CI can prove an unauthorised call is refused. It cannot prove that the
authorisation prompt appears where a desktop user expects it.

If you are reviewing a change to the decision path, assume CI proved nothing about the parts above,
and run the protocol.

---

## Before you test suspend

You are about to deliberately put a machine to sleep, on a build whose job is putting machines to
sleep. Arrange in advance:

1. **A second way in.** Physical access, a serial console, or a shell open from another machine on
   the same network. Do not test suspend on a host you can only reach one way.
2. **A way to wake it that does not depend on the code under test** — the physical power button is
   the reliable one.
3. **Nothing unsaved.** Assume every test may lose the session.
4. **A journal follower** running somewhere you can still read after the resume:

```sh
journalctl -fu idlepolicyd
```

Run the whole protocol against a config you wrote for it, not against your daily config. Use short
timeouts so a run takes minutes rather than hours — but keep them *proportional*, because several
tests are about which timeout wins, not about how long it is.

Test config used by the protocol below:

```toml
# /etc/idlectl/conf.d/99-protocol.toml
# Drop-ins are the last layer, so this overrides the vendor file at
# /usr/lib/idlectl/idlectl.toml and anything in /etc/idlectl/idlectl.toml.

[while.always]
clock      = "human_input"
screen_off = "1m"
suspend    = "3m"

[while.human_active]
suspend    = "never"
hibernate  = "never"
poweroff   = "never"

[while.steam_game_running]
screen_off = "never"
suspend    = "never"
poweroff   = "never"

[while.steam_downloading]
suspend    = "never"
poweroff   = "never"

[while.after_resume]
clock      = "resume"
suspend    = "2m"

[when.always]
screen_off = "2m"
```

Run `idlectl check-config` before you start. Every condition name above is a shipped fact name; an
unknown one is a fatal config error by design, so a typo here ends the protocol at step 0 rather
than quietly testing a different policy.

Note the shape:

- `after_resume` (2m) is *shorter* than `always` (3m), which is what makes test 2 meaningful.
- `[while.steam_game_running] screen_off = "never"` is deliberately paired with
  `[when.always] screen_off = "2m"`. Under the v1 ceiling model — one class, absolute — the ceiling
  wins, so the panel goes dark two minutes into a game while the machine keeps running. That is the
  behaviour tests 1, 4 and 8 assert. If a future change makes a ceiling anything less than absolute,
  those tests must fail loudly rather than be quietly re-interpreted.
- This config sets `suspend = "never"` under `[while.steam_game_running]`, unlike the vendor default
  (`2h`), because the protocol needs an unambiguous veto to observe. Do not copy it into a daily
  config: a game left running on the couch would then keep the machine awake forever.

---

## The manual protocol

Record, for each test: date, machine, kernel, session type (`echo $XDG_SESSION_TYPE`), desktop
environment and version, `idlectl --version`, and the observed result. Paste the record into the PR.

### 0a. The packaged units actually start

Do this from the **installed package**, with `systemctl`, before anything else. Running the three
binaries by hand exercises none of the unit files, and the unit files are where a sandbox directive
can make a program that works perfectly fail every start.

```sh
sudo systemctl enable --now idlepolicyd.service
systemctl --user enable --now idlectl-agent.service
systemctl is-active idlepolicyd.service
systemctl --user is-active idlectl-agent.service
```

**Expect:** both `active`, and the agent's first journal line naming a backend, e.g.
`session backend ready backend="wayland (KDE), ext-idle-notify-v1, org_kde_kwin_dpms"`.

**Fail if:** the agent reports *"no usable session backend"* on a machine with a running compositor.
That is a sandbox fault, not a protocol fault: bisect it with

```sh
systemd-run --user --wait --pipe -p ProtectHome=yes \
  /bin/sh -c 'ls $XDG_RUNTIME_DIR/wayland-0 $XDG_RUNTIME_DIR/bus'
```

`ProtectHome=yes` makes `/home`, `/root` **and `/run/user`** inaccessible, and `/run/user` is where
both of the agent's sockets live. This shipped broken in 0.1.1 and was invisible to every test that
started the binary directly.

Then run `idlectl doctor` and read the *reason* beside every fact, not just its state. A detector
that works from a shell and reports `indeterminate` under the unit is a sandbox fault too, and the
same bisection finds it. Two that shipped this way: `DeviceAllow=… r` on the NVIDIA nodes, which
makes NVML fail with *"couldn't communicate with the NVIDIA driver"* because it needs `rw` (fixed in
0.1.3); and an empty `CapabilityBoundingSet=`, which stops root traversing a `0700` home and so
hides a Steam install that is plainly there.

### 0. Baseline

```sh
idlectl check-config
idlectl doctor
idlectl status
idlectl explain suspend
```

**Expect:** `check-config` exits 0. `doctor` lists the other candidate owners of power it can read —
logind's `IdleAction` and `IdleActionSec`, any process owning a known desktop power-management D-Bus
name, any running `swayidle` / `hypridle` / `xautolock` — and on a machine prepared per *Conflicts*
in the README, none of them is armed. If one is, fix that first: the rest of the protocol is
meaningless with two owners. `doctor` also lists every `[when]` ceiling in the effective
configuration as a standing hazard (this config has one), every disabled fact, and every block that
cannot act. `status` shows every fact with a state and a source. `explain` shows each true block
with its clock, its origin instant and its deadline, and names the winner.

**Fail if:** any enabled fact is `indeterminate` on a healthy machine (and `doctor` must exit
non-zero when one is), or `explain` shows a deadline whose arithmetic does not match the printed
origin plus timeout, or `doctor` does not name the `[when.always]` ceiling from the protocol config.

Note: `local_service_busy` ships disabled and reports `unavailable` until you give it a counters
endpoint. That is expected here, and it is what test 6 turns on.

### 1. Plain idle → suspend

Stop touching the machine. Watch the journal.

**Expect:** screen off at ~1 minute, suspend at ~3 minutes, both preceded by a log line naming the
action and the winning block. `screen_off` is not a power-state change, so its firing does not delay
the suspend.

**Fail if:** either fires early, or fires with no explanation logged, or the screen-off floor (1m)
and the screen-off ceiling (2m) resolve to anything other than the earlier of the two, 1m, without a
log line saying which applied.

### 2. The resume test — the important one

This is the normative case. Run it on every release.

1. Leave the machine idle long past the base timeout — ideally hours, and at minimum well beyond
   `[while.always] suspend`. The point is to accumulate idle *before* the sleep.
2. Let it suspend, or suspend it by hand.
3. Wake it with the power button. **Write down the wall-clock second of the wake.**
4. Within a few seconds, from your second shell:

```sh
idlectl explain suspend
```

**Expect:** the resolved instant is `resume + 2m` — the `after_resume` block wins, *even though it
carries the shorter timeout*, because composition is over deadlines and `always`'s deadline is hours
in the past. The machine then stays awake for two full minutes before suspending again.

**Fail if:** `explain` resolves to now, or the machine suspends within seconds of waking. That is
the exact measured incident this rule exists to prevent — wake at 08:35:45, `SUSPENDING` at
08:35:46. If you see a one-second gap between resume and suspend in the journal, composition
regressed from instants to durations. Stop and file it as a blocker.

Also check, in the same run:

```sh
journalctl -u idlepolicyd -b | grep -iE 'resume|boottime'
```

**Expect:** the resume was observed and the `resume` clock origin was updated. A resume the daemon
did not notice is the same bug wearing a different hat.

### 3. The human-presence floor

With somebody actually typing on the machine, from another shell:

```sh
idlectl rest --now
```

**Expect:** refusal, with `human_active` named as the vetoing floor, and a journal line. The machine
does not sleep. (`rest` with no `--action` means `suspend` — nothing here should ever propose
`poweroff`.)

Then:

```sh
idlectl rest --now --force
```

**Expect:** it sleeps, and the journal records the override at warning level, naming the requester
*and* every floor it defeated with each floor's reason. An override that is not logged is a failure
even when the behaviour is right.

### 4. `rest --now` satisfies two blocks and no others

Start a Steam game. From another shell:

```sh
idlectl rest --now
```

**Expect:** the machine stays awake. `rest --now` satisfies exactly `[while.always]` and
`[while.after_resume]`; `[while.steam_game_running]` is evaluated normally on its own clock and its
`never` still stands.

**Expect also:** the panel still goes dark at the `[when.always]` ceiling of 2 minutes despite
`[while.steam_game_running] screen_off = "never"`, and the log record names the ceiling, its file
and the floor it defeated. That is the absolute-ceiling rule; if the panel stays lit, the ceiling
model regressed and this test must be reported as such rather than adjusted.

**Fail if:** the game is suspended out from under you. This is a regression against the shipped
system this project was extracted from, and it is a release blocker.

### 5. Steam download, with no inhibitor

Start a large download in Steam. Then, from another shell:

```sh
systemd-inhibit --list
idlectl status
```

**Expect:** `systemd-inhibit --list` shows **nothing from Steam** — that is the point — while
`idlectl status` shows `steam_downloading = true`. Leave the machine untouched past the base
timeout: it must not suspend.

**Fail if:** the machine sleeps mid-download, or `steam_downloading` is false while a download is
visibly in flight. Re-run this test after every Steam client update; it is the detector most likely
to be broken by somebody else's release.

### 6. Running is not in use

`local_service_busy` ships disabled, so turn it on for this test. Start a long-lived local service
that idles (a model server is the canonical case), point the fact at its counters endpoint, and add
a block that reads it:

```toml
# appended to /etc/idlectl/conf.d/99-protocol.toml
[facts.local_service_busy]
enabled      = true
counters_url = "http://127.0.0.1:8080/metrics"
idle_window  = "2m"

[while.local_service_busy]
suspend   = "never"
hibernate = "never"
poweroff  = "never"
```

Reload, confirm `idlectl status` shows the fact as `false` rather than `indeterminate` (an
unreachable endpoint reads `indeterminate` and would veto everything, which is a different test),
then do not send the service any requests and leave the machine alone past the base timeout.

**Expect:** `local_service_busy = false` and the machine suspends normally. The service being
`active` must not keep it awake — and neither must its holding 12 GB of VRAM while doing nothing.

Then send it a request and immediately check:

**Expect:** `local_service_busy = true`, and the machine stays awake — until the counters stop
moving for `idle_window`, after which it may sleep again.

Undo the `enabled = true` before running the rest of the protocol.

### 7. Indeterminate is a veto on sleep, not on the screen

Make one detector unreadable (revoke access to its source, stop the session agent, or point a
detector at a path that does not exist). Pick a fact **no block in the protocol config mentions** —
that is the point of this test.

**Expect:** the affected fact reads `indeterminate`; a journal line says which detector failed and
why; `suspend`, `hibernate` and `poweroff` are vetoed *even though no block names that fact*;
`explain` names the indeterminate fact as the reason for the floor; `doctor` exits non-zero naming
it; and `screen_off` **still fires on schedule**.

**Fail if:** the screen stays lit. One broken detector burning an OLED panel overnight is the
failure mode this asymmetry exists to prevent. Equally, fail if the machine suspends anyway — doubt
must veto sleep. And fail if the veto only appears once you add a block referencing the fact: the
doubt veto is machine-wide, not a property of the blocks you happened to write.

Then, with the same detector still broken:

```sh
idlectl explain suspend        # names the doubt floor
idlectl rest --now             # must NOT sleep
idlectl rest --now --force     # must sleep
```

**Expect:** `rest --now` issues no transition and reports the doubt floor as the reason, because the
doubt floor is not a block and `--now` does not satisfy it. `rest --now --force` issues the
transition and logs the doubt floor among the floors it defeated. This is the case people actually
reach for `--force` in — a machine wedged awake by a dead detector — so a `--force` that cannot
clear it is a release blocker.

Finally, check the disabled-fact escape hatch: set `[facts.<name>] enabled = false` for the broken
detector and reload.

**Expect:** the fact reads `unavailable`, its detector does not run, the veto is gone, the machine
sleeps on schedule, and `doctor` lists the fact as disabled. A fact switched off must never become
an invisible hole.

### 8. The ceiling

Set `[while.always] screen_off = "never"` while leaving `[when.always] screen_off = "2m"`.

**Expect:** the screen still goes off at 2 minutes; the journal names the ceiling, the file it came
from and the `never` floor it defeated; and `doctor` listed that ceiling as a standing hazard before
it ever fired.

**Fail if:** the ceiling is silent in any of those three places. A ceiling is the only construct in
the file that can act against a floor, so an unannounced one is a defect even when the timing is
right.

### 9. Nothing is silent

After the full run:

```sh
journalctl -u idlepolicyd -b --no-pager | less
```

**Expect:** every decision, every veto, every detector failure and every override appears. There is
a periodic line showing the resolved deadline per action, so you can reconstruct any decision after
the fact.

**Fail if:** anything above happened without a corresponding log line, and especially if the unit
was skipped without saying so — a `systemd` unit carrying a `Condition*` once turned an entire
feature into dead code that logged nothing at all. Verify the unit actually ran:

```sh
systemctl show idlepolicyd -p ConditionResult -p ExecMainStartTimestamp
```

### 10. Restart and reload

```sh
sudo systemctl restart idlepolicyd
idlectl explain suspend
```

**Expect:** clock origins are re-established sanely after a restart, and the daemon does not act on
a stale deadline inherited from before. A restart must never cause an immediate action.

Then edit the config and reload:

```sh
sudo systemctl reload idlepolicyd     # or: idlectl reload
```

**Expect:** the new config is in effect and `explain` reflects it.

Then break it on purpose — put an unparseable duration in the drop-in — and reload again.

**Expect:** the daemon stays up; the offending **file** is dropped as a whole and the other layers
stay in effect, so the vendor policy still applies; `suspend`, `hibernate` and `poweroff` are held
off while the fault stands; `screen_off` is **not** held off; and `doctor` exits non-zero naming the
key, the file and the offending text. Never applied partially, never fatal, never a machine with no
policy at all.

**Fail if:** the daemon exits, or the whole configuration including the vendor layer is discarded,
or the panel stays lit because a config typo vetoed `screen_off` too.

Now the harder half of the same case: fix nothing, and **reboot**. There is no previous good policy
to fall back on after a cold start, which is exactly the state this rule was written for.

**Expect:** the daemon comes up, drops the bad file, applies the rest, and vetoes sleep with the
fault named. If the machine boots with no policy at all, that is a blocker.

### 11. It does not escalate

Add `poweroff = "5m"` to `[while.always]` alongside its `suspend = "3m"`, reload, and leave the
machine alone.

**Expect:** the machine **suspends** at 3 minutes and never powers itself off. Only the shallowest
due sleep action is issued, and `poweroff` is not a substitute for `suspend`.

**Expect also:** `idlectl rest --action poweroff` does power it off. The deep action is available;
it just has to be asked for by name.

**Fail if:** the machine powers off on the schedule. An unattended `poweroff` ends every session on
the box, and on the hardware this policy came from it hung about one time in four — leaving a
machine only a physical trip could revive.

Remember to remove the `poweroff` line afterwards.

---

## Release checklist

Before tagging:

- [ ] CI green, including `clippy -D warnings`, `cargo deny` and the no-tokio check.
- [ ] Tests 0–11 run on at least one real desktop, results pasted into the release PR.
- [ ] Test 2 (the resume test) run at least twice, once after a long real idle accumulation.
- [ ] Test 5 re-run against the current Steam client.
- [ ] Test 7's `--force`-against-the-doubt-floor step run, since it is the path a wedged machine is
      recovered by.
- [ ] Tested on both a Wayland and an X11 session, or the unsupported one stated in the notes.
- [ ] Tested once on a session where `screen_off` is `unavailable` (no agent running), confirming
      the daemon never proposes it and `doctor` lists the affected blocks as inert.
- [ ] `idlectl doctor` run on a machine with a competing idle owner still enabled, to confirm it is
      named in the report.
- [ ] Upgrade path checked: install the previous version, then the new one, and confirm the existing
      config still parses or fails loudly with a migration message.
- [ ] `CHANGELOG.md` moved out of `[Unreleased]`.

---

## Reporting a failure

Include:

```sh
idlectl --version
idlectl doctor
idlectl status --json
idlectl explain suspend --json
journalctl -u idlepolicyd -b --no-pager
uname -r; echo "$XDG_SESSION_TYPE"
```

Plus the whole effective configuration — `/usr/lib/idlectl/idlectl.toml`,
`/etc/idlectl/idlectl.toml` and every `/etc/idlectl/conf.d/*.toml` — and, for anything involving a
resume, the wall-clock times of the wake and of the action, to the second. That second is usually
the whole diagnosis. Send all three layers, not just the one you edited: which layer a key came from
is half of most config bugs.

Redact hostnames, usernames and addresses before pasting — none of them are needed to reproduce a
policy bug.
