# idlectl

`idlectl` decides when a Linux machine may turn its screen off, suspend, hibernate or power off,
based on what is actually happening on it — who is logged in, what is playing, what is downloading,
what the GPU is doing. It replaces the *policy* half of your desktop environment's power settings
and of logind's `IdleActionSec` with one resident root daemon (`idlepolicyd`), one config file, and
a CLI that can explain every decision before it happens.

```toml
# /etc/idlectl/idlectl.toml

[while.always]
screen_off = "10m"
suspend    = "30m"

[while.human_active]              # somebody is at the keyboard
suspend    = "never"
hibernate  = "never"
poweroff   = "never"

[while.steam_game_running]        # a Steam game is up
screen_off = "never"
suspend    = "2h"
poweroff   = "never"

[while.media_playing]             # something is playing over MPRIS
screen_off = "never"
suspend    = "2h"
poweroff   = "never"

[while.after_resume]              # counted from the resume instant, not from last input
clock      = "resume"
suspend    = "5m"
```

Every block that is currently true proposes a deadline per action. For each action the **longest**
proposal wins, `never` beats everything, an action key a block does not set contributes nothing to
that action, and the default over zero true blocks is to do nothing. Order never matters, and there
is no priority list to get wrong.

> **Status: `0.4.3`, and working.** The daemon, the CLI and the session agent are implemented, and
> everything below has been exercised on a real CachyOS machine: the eleven facts, the decision
> loop, leases, held requests, polkit authorization, resume detection across a genuine `deep`
> suspend, and — since `0.4.3` — blanking on KWin, verified by watching the panel go dark rather
> than by trusting the call to return. The wlroots backend is implemented and compiled but has not
> been run against a wlroots compositor. Not yet on the AUR — until it is, build the package from
> the PKGBUILD (see [Install](#install)) or run `packaging/install.sh`.
>
> It has spent days running in `--dry-run` beside the system it was extracted from, on the same
> machine, with the two sets of decisions compared in the journal. That is where every bug fixed
> since `0.1.0` came from, and none of them were visible to the test suite: they only appear once
> the shipped units are actually started, and the last one was only visible to a human looking at
> a television. The [CHANGELOG](CHANGELOG.md) records what each one was and how it was measured.

---

## Contents

- [What it is not](#what-it-is-not)
- [Conflicts — read this before installing](#conflicts--read-this-before-installing)
- [Configuration](#configuration)
- [Requesting rest](#requesting-rest) — including relays and machines with nobody in front of them
- [Facts](#facts)
- [Explaining a decision](#explaining-a-decision)
- [Why not logind's `IdleActionSec`, or an inhibitor](#why-not-loginds-idleactionsec-or-an-inhibitor)
- [Why Rust](#why-rust)
- [Install](#install)
- [Security model](#security-model)
- [Design principles](#design-principles)
- [Scope](#scope)
- [Documentation](#documentation)
- [License](#license)

---

## What it is not

`idlectl` is **not** a CPU or power tuning tool. It never touches governors, EPP, platform profiles,
turbo, or PCIe/USB runtime power management. That layer belongs to TLP, tuned, auto-cpufreq or
`power-profiles-daemon`, and `idlectl` is designed to run alongside any of them without overlap.

`idlectl` decides *when the machine stops being used*. Nothing else.

---

## Conflicts — read this before installing

`idlectl` takes over idle policy for the whole machine. If your desktop environment keeps its own
idle timers, you now have **two owners of power**, and that is exactly the bug this project exists
to remove.

Two owners is not a cosmetic problem. On the machine this design was extracted from, the desktop's
auto-suspend timer was measured **not to re-arm after a resume**: the machine woke, the timer never
restarted, and only a full session cycle brought it back. When two components each believe they own
the suspend decision, neither one's state is trustworthy, and the failures are silent and
intermittent by nature.

So pick one owner. If you install `idlectl`, disable the others.

**Every desktop environment — logind's own idle action:**

```ini
# /etc/systemd/logind.conf.d/10-idlectl.conf
[Login]
IdleAction=ignore
```

Reboot after writing it. (Restarting `systemd-logind` on a live session is its own hazard; don't.)

**KDE Plasma** — System Settings → Power Management → Energy Saving:

- *Suspend session* → **off** in every profile.
- *Automatic Suspend* → **off**.
- *Screen Energy Saving* → your choice, but see the note below on who owns the backlight.

**GNOME:**

```sh
gsettings set org.gnome.settings-daemon.plugins.power sleep-inactive-ac-type 'nothing'
```

**Wayland compositors with an idle helper** (`hypridle`, `swayidle`): remove every `suspend`,
`hibernate` and `poweroff` line from their config, or don't run them at all.

**X11:** `xset s off -dpms`, and don't run `xautolock` with a suspend action.

**Who owns the screen.** `screen_off` is the one action you may reasonably leave to your desktop,
because a screen that blanks late costs a wasted panel-hour, not a lost session. Choose one:

- Let `idlectl` own it — disable the DE's screen blanking, keep `screen_off` in your config.
- Let the DE own it — keep the DE's screen blanking, and remove every `screen_off` key from your
  config so `idlectl` never proposes that action.

Do not do both.

**Screen lockers are not a conflict.** Locking is not a power action; run your locker on whatever
timer you like.

After installing, run `idlectl doctor`. Reporting the other candidate owners of power is a required
output of `doctor` (spec [OBS-3].11), not a best-effort extra: it prints logind's configured
`IdleAction` and `IdleActionSec`, any process currently owning a known desktop power-management
D-Bus name, and any idle-helper process it can find running (`swayidle`, `hypridle`, `xautolock`).
Each one is named as a *potential* second owner of power — `doctor` reports what it can read and
does not claim to have found every possible one.

---

## Configuration

The effective configuration is assembled from three layers, all called `idlectl.toml`, each
overriding the one before it:

| order | path                            | owner                                                     |
|-------|---------------------------------|-----------------------------------------------------------|
| 1     | `/usr/lib/idlectl/idlectl.toml` | the package — vendor defaults, always installed            |
| 2     | `/etc/idlectl/idlectl.toml`     | you                                                        |
| 3     | `/etc/idlectl/conf.d/*.toml`    | you, in ascending byte-wise order of basename, last wins   |

Drop-ins are applied *after* `/etc/idlectl/idlectl.toml`, so a drop-in wins over it — the same
contract systemd drop-ins use. Name them `NN-description.toml`.

Layer 1 is part of the resolution chain and is read on every start, so a machine with an empty
`/etc` already has a complete, safe policy. It is package-owned: do not edit it. To switch a vendor
block off, override it — `enabled = false` on the block, or `[facts.<name>] enabled = false` on the
fact it reads. An example file may also be installed at `/usr/share/idlectl/idlectl.example.toml`;
it is documentation, it is **not** read by the daemon, and nothing works differently if you never
copy it.

Durations are systemd-style time spans (`45s`, `10m`, `1h30m`) or the exact lower-case literal
`"never"`. Every duration needs a unit — including zero, which is written `"0s"`; a bare integer is
a config error, because a bare number was once read as seconds by one component and as minutes by
another. Whitespace and underscores inside a duration are ignored.

`idlectl check-config` validates the effective configuration without touching the running system.

### Blocks

A config is a set of condition blocks. Each block names a condition and gives a timeout **per
action**:

```toml
[while.<condition>]
screen_off = "10m"
suspend    = "30m"
hibernate  = "never"
poweroff   = "never"
```

**Keys you omit contribute nothing to that action — silence is not `never`.** This is what makes
per-action policy work: `[while.steam_downloading]` vetoes `suspend` while having no opinion at all
about the panel, so a six-hour download does not pin a static image on an OLED for six hours.

`[while ...]` blocks set **floors** — "do not do this before". `[when ...]` blocks set **ceilings**
— "do this by then at the latest":

```toml
[when.always]
screen_off = "20m"     # the panel goes dark 20 minutes after last input, whatever else is true
```

Ceilings exist for the case where something must happen by a certain point no matter what else is
true. Hardware that degrades while lit is the motivating one — an OLED panel — but they are not
restricted to `screen_off`: `[when.lease_held] suspend = "12h"` is the other shape, a hard stop on a
floor somebody might otherwise leave in place forever.

A ceiling is absolute. v1 ships **one** ceiling class: a ceiling defeats every floor, including a
`never`, including the human-presence floor, including the implicit floor a doubtful detector or a
broken config file raises. There is exactly one override channel in this system and this is it. That
makes ceilings the sharpest thing in the config file, so every ceiling in the effective
configuration is logged at warning level when it is loaded (naming the file it came from), is listed
by `idlectl doctor` as a standing hazard, and when it fires the log record names it, its file, and
every floor it defeated with each floor's reason.

A ceiling can only ever pull an action **earlier**. It can never delay one, so `[when ...]` is never
a way to keep a machine awake.

### Composition is over instants, not durations

This is the part that is easy to get wrong, and getting it wrong causes a specific, reproducible
disaster.

Different blocks are measured on **different clocks**. `[while.always]` counts from the last human
input; `[while.after_resume]` counts from the last resume. Taking `max(30m, 5m) = 30m` over the two
raw durations is meaningless, because they are counted from different origins. Idle counters do not
reset across a suspend, so a machine that was idle for nine hours before it slept is still "idle for
nine hours" one second after it wakes.

The resolution rule is therefore stated over absolute instants:

```
participates(block, action)  <=>  the block sets a key for that action

deadline(block, action) = origin(block.clock) + block.timeout      # "never" => +infinity

floor(action)   = MAX over deadlines of every participating, enabled, currently-true
                  [while] block, plus one +infinity term per implicit floor in force
                                                                   # empty set => +infinity
ceiling(action) = MIN over deadlines of every participating, enabled, currently-true
                  [when] block, plus the ephemeral ceiling installed by --force
                                                                   # empty set => +infinity

resolved(action) = MIN(floor(action), ceiling(action))
fire(action) when now >= resolved(action)
```

Max and min over instants are still commutative and associative, so order-independence survives
intact. All clocks are `CLOCK_BOOTTIME`, which keeps advancing across suspend.

The clocks a block may be measured on:

| clock         | origin                                              |
|---------------|-----------------------------------------------------|
| `human_input` | last human input (default for most conditions)      |
| `resume`      | last resume from suspend or hibernate               |
| `condition`   | the last false → true edge of the block's condition  |
| `boot`        | boot                                                |

Each condition has a sensible default clock; override it per block with `clock`:

```toml
[while.steam_game_running]
clock = "condition"
suspend = "4h"
```

A block on the `resume` clock on a machine that has not resumed this boot has no origin to count
from. It contributes `+infinity` to `suspend`, `hibernate` and `poweroff`, and counts `screen_off`
from boot — it does not silently fall back to the boot origin for the sleep actions and fire
immediately.

**The test this rule exists to pass.** A machine carrying 9 hours of accumulated human-idle resumes,
and 0 seconds later has both `[while.always] suspend = "30m"` and `[while.after_resume]
suspend = "5m"` true:

- `always` → `(now - 9h) + 30m` → long past.
- `after_resume` → `now + 5m`.
- `MAX` → **`now + 5m`**.

Composing the durations instead would give `max(30m, 5m)` against a nine-hour counter and suspend
immediately. That is not hypothetical: it is a measured incident — wake at 08:35:45, `SUSPENDING` in
the journal at 08:35:46.

### `idlectl` does not escalate

`screen_off` is not a power-state change: it may be applied in the same evaluation as a sleep
action, and applying it never suppresses one. Among `suspend`, `hibernate` and `poweroff`, at most
one is applied per evaluation, and it is the **shallowest** one that is due — suspend before
hibernate before poweroff. A deeper action is never a safe substitute for a shallower one: suspend
loses nothing, `poweroff` ends every session on the machine and, on the hardware this policy came
from, hung about one time in four.

The consequence is worth stating plainly rather than leaving you to discover it. With
`suspend = "30m"` and `poweroff = "8h"` both eventually due, the machine suspends at 30 minutes and
never powers itself off. If you want escalation, set `suspend = "never"` and let the deeper action
own the schedule, delegate to logind's `SuspendThenHibernate`, or ask for the deep action explicitly
with `idlectl rest --action poweroff`.

An action already in effect is not re-issued until its own deadline has been re-armed by a new
origin, so a machine whose panel is already blank does not re-fire `screen_off` forever.

### The human-presence floor

`human_active` is a first-class fact, not merely the clock other timeouts are measured on. Without
it, an override or a remote request can act while somebody is typing.

```toml
[while.human_active]
suspend   = "never"
hibernate = "never"
poweroff  = "never"
```

This ships enabled by default, and `human_active` has three states rather than two:

- **true** — human idle is below `min_idle` (default `5m`, set in `[general]`).
- **false** — the input clock has never been touched this boot. That is deliberate: a freshly-woken
  headless machine has no human on it, and this preserves the fast path for machines woken remotely
  to do a job.
- **indeterminate** — the clock is unreadable: the session agent is not running, its heartbeat is
  stale, or the idle protocol errored. It is **not** reported as true, because the detector did not
  observe a human and reporting an observation nobody made turns doubt into knowledge. The veto
  arrives anyway, and with the same force, through the doubt rule below.

Doubt keeps the machine awake either way, but only the honest state is diagnosable: `doctor` exits
non-zero and names the fact, so "this machine has been awake for three days because its agent died"
is readable off the fact table instead of being indistinguishable from a person at the keyboard.

Only a `[when]` ceiling, or the ephemeral one an explicit, named, logged `--force` installs, can
beat this floor. Nothing else does.

### Requesting rest

```sh
idlectl rest --now
```

`rest --now` satisfies exactly two blocks — the one whose condition is `always` and the one whose
condition is `after_resume`, i.e. the base schedule and the post-resume settle window — and nothing
else. Each of those two contributes `now` instead of its configured deadline, even if that deadline
is `never`. The set is fixed by condition name; there is no per-block opt-in key and an
administrator cannot enlarge it. Every other block, every ceiling and every implicit floor is
evaluated unchanged, on its own clock. A remote request to go to sleep does **not** suspend a
machine with a game running, a download in flight, or a person at the keyboard — that is a
regression the system this design was extracted from was fixed for, and it stays fixed.

`rest` with no `--action` always means `suspend`. Ending sessions is not something the machine does
on your behalf: `poweroff` has to be asked for by name, `idlectl rest --action poweroff`. The action
you name is the one you get: if it is held, nothing happens. You will never ask for a `poweroff` and
be given a `suspend`, which matters most to whatever recorded that the machine was off.

### Asking once, and being remembered

A bare request is a question asked at one instant. If a game is running, the answer is no, and
asking again is your problem:

```sh
idlectl rest --action poweroff --pending 8h
```

With `--pending` the machine keeps the request and carries it out **the moment the last veto
clears**, giving up after the TTL. This is what a relay wants — "I am done with this box, sleep when
you can" — and it exits zero when the request is held, so `&&` in a script does what its author
meant.

It weakens nothing. Every retry evaluates exactly the floors the original did: a download that
starts *after* the request refuses it just as surely as one already running would have. What is
remembered is the asking, never the answer. It is dropped early if somebody uses the machine, and
waking from sleep is not somebody using the machine — a box woken by a relay to run a job does not
cancel the request that lets it sleep again afterwards. `idlectl rest --cancel` forgets it, and
`idlectl doctor` shows it while it is held.

The request lives in the daemon's memory, so restarting the daemon forgets it. That direction is the
safe one: the machine stays awake and somebody has to ask again.

### Machines with nobody in front of them

The shipped polkit default lets anyone **at the machine** ask it to rest, and requires
authentication from anywhere else — a remote caller cannot see whether somebody is sitting at the
screen. Every ssh session is "anywhere else" as far as logind is concerned, so a relay gets
`AccessDenied` until you say otherwise. That is one file:

```js
// /etc/polkit-1/rules.d/49-idlectl-relay.rules
polkit.addRule(function(action, subject) {
    if (action.id == "io.github.ericcanas.Idlectl1.rest" &&
        subject.user == "relay") {
        return polkit.Result.YES;
    }
});
```

Grant `.rest` and not `.rest-forced`. Asking is safe to hand out: a game, a download, a held lease,
an open session and any detector that cannot answer all still refuse it. Forcing defeats every one
of those including the presence of a human at the keyboard, and nothing reached over the network
should be able to do that without authenticating.

To override a floor you must say so:

```sh
idlectl rest --force --why "maintenance window"
```

`--force` installs an ephemeral ceiling due *now* for the requested action, for the lifetime of that
one request. Because ceilings are absolute, that one mechanism defeats every floor with no special
cases: `never` floors, the human-presence floor, the implicit floor raised by a doubtful detector,
and the implicit floor raised by a broken config file. It is the answer to a machine wedged awake by
a dead detector, which is what people actually reach for it for.

`--force` overrides **policy**, never **mechanism**. One transition at a time still holds, a refusal
from the sleep mechanism is still logged and not retried immediately, and the facts are still
re-read and recomputed before the action is issued. It is a separate flag under a separate polkit
action, it is logged at warning level naming the requester and every floor it defeated with that
floor's reason, and `doctor` counts and timestamps its uses.

---

## Facts

Every fact is in one of four states:

| state           | meaning                                      | effect                                                  |
|-----------------|----------------------------------------------|---------------------------------------------------------|
| `true`          | condition holds                              | the block composes                                      |
| `false`         | condition does not hold                      | the block is ignored                                    |
| `indeterminate` | source unreadable, timed out, detector error | vetoes `suspend`/`hibernate`/`poweroff`, machine-wide |
| `unavailable`   | the capability does not exist on this host   | the condition is simply never true                      |

`indeterminate` is a veto on sleeping: any doubt means the machine stays awake. The veto is
**machine-wide, not per block** — if any enabled fact is indeterminate, `suspend`, `hibernate` and
`poweroff` are held off, whether or not you wrote a block that mentions that fact. The rule is about
the machine no longer being able to see something it normally sees; whether an operator happened to
write a block for it is beside the point. It is deliberately **not** a veto on `screen_off` — one
unreadable detector must never be able to hold an OLED panel lit all night.

A block whose condition is indeterminate is treated as false *as a selector*, in `[while]` and
`[when]` alike. Doubt may prevent an action; it may never cause one.

`unavailable` is a distinct state on purpose, and detectors are held to the difference: a capability
that is absent reports `unavailable`, never `indeterminate`. Unavailable is knowledge; indeterminate
is doubt. A machine with no Steam installed reports the Steam facts as `unavailable`, not as an
error, and those blocks are quietly never true. Detectors degrade; they do not fail the machine.

If a detector on your machine is permanently indeterminate and you would rather live without it,
that is a supported, auditable move:

```toml
[facts.local_service_busy]
enabled = false
```

A disabled fact reports `unavailable`, its detector does not run, it contributes no doubt veto, and
`doctor` lists it as disabled so it can never become an invisible hole in the policy.

Facts shipped in v1:

| fact                 | reads                                                                                           |
|----------------------|-------------------------------------------------------------------------------------------------|
| `human_active`       | compositor idle notification (`ext-idle-notify-v1` / KDE idle), via the session agent            |
| `after_resume`       | at least one resume from sleep has happened this boot                                            |
| `remote_session`     | logind sessions of remote type                                                                   |
| `lease_held`         | an explicit lease taken by a job that must not be interrupted                                     |
| `inhibitor_block`    | logind inhibitor locks in `block` mode — read as a **signal**, never trusted as enforcement       |
| `media_playing`      | MPRIS playback status                                                                            |
| `steam_game_running` | a Steam game process tree — the game, not merely the Steam client                                 |
| `steam_downloading`  | recent writes under Steam's staging tree, not a network-throughput threshold                     |
| `gpu_busy_game`      | GPU memory held by a process in a running game's ancestry — DRM `fdinfo` via the session agent, merged with `nvidia-smi` |
| `gpu_busy_other`     | the same reading, held by anything not attributable to a game                                    |
| `local_service_busy` | a long-running local service, measured by **cumulative counters**, not by `systemctl is-active`  |

`always` is the only built-in condition. Everything else in that table, `after_resume` included, is
an ordinary fact.

`after_resume` stays true from the first resume until the next cold boot; it does not expire when
the settle window does. Pair it with `clock = "resume"` and it *is* the settle window.

`local_service_busy` is the one fact that ships **off**, because there is no sensible default for
where to read counters from. Turning it on takes **both** keys, in a later layer — merging is per
key, so the URL alone leaves the shipped `enabled = false` standing and the fact silently stays off:

```toml
[facts.local_service_busy]
enabled      = true
counters_url = "http://127.0.0.1:8080/metrics"
idle_window  = "30m"
```

It asks whether a service has *served something recently*, not whether its unit is running: a model
server left up "just in case" is not a model server in use, and treating the unit's state as the
signal is what turns one start-up decision into a machine that never sleeps again. A refused
connection reads FALSE — nothing is listening, so nothing is being served — while a service that is
up and will not answer reads `indeterminate`, which vetoes.

`gpu_busy_game` and `gpu_busy_other` are split because they want different policy. Attribution is by
process ancestry — never by an executable allowlist — so a game keeps a soft, finite floor while
unattributed GPU load keeps its own. The compositor is excluded by name, not merely by falling under
the memory threshold.

Both facts read **two sources and merge them**, never one falling back to the other: DRM `fdinfo`,
which is generic across amdgpu/i915/xe/nouveau, and `nvidia-smi`, because the proprietary NVIDIA
driver publishes no memory accounting in `fdinfo` at all. On a hybrid machine a fallback chain sees
the integrated GPU's keys, concludes the generic source works, and never asks the card the games run
on. The `fdinfo` half is read by the **session agent** and reported: that path is gated by
`ptrace_may_access`, so a root daemon would need `CAP_SYS_PTRACE` — the right to read any process's
memory — to see another user's, and it does not get one to attribute video RAM. The agent reports
raw holders; the daemon attributes them. With no agent running, `nvidia-smi` still answers and the
`fdinfo` half simply contributes nothing; it never becomes doubt.

`local_service_busy` earns its own paragraph. "The service is running" is not "the service is in
use". A local model server sitting idle all night holding 12 GB of VRAM is not the machine being
used, and a detector that reads `is-active` will keep it awake forever, so this one reads cumulative
request counters between evaluations and asks whether they moved. It ships **disabled**, because the
counter endpoint it needs is off by default on the service it was written for and a shipped-enabled
unreadable detector would wedge every machine awake on install day. Give it an endpoint to turn it
on:

```toml
[facts.local_service_busy]
enabled      = true
counters_url = "http://127.0.0.1:8080/metrics"
idle_window  = "30m"
```

Leases exist for the case no detector can see: you are about to start a long job by hand.

```sh
idlectl lease acquire nightly-build --ttl 4h --why "full rebuild"
idlectl lease release nightly-build
```

Leases always expire. A lease with no TTL is a machine that never sleeps again.

---

## Explaining a decision

The point of a policy engine is that you can ask it why.

```
$ idlectl explain suspend
action: suspend
now:      22:41:07   (boottime 9h12m34s)

floors — longest wins
  [while.always]              30m    from last input  22:09:41  ->  22:39:41   (elapsed)
  [while.after_resume]        5m     from last resume 22:38:02  ->  22:43:02   <- winner
  [while.human_active]        never  condition false            ->  --
  [while.steam_game_running]  2h     condition false            ->  --

ceilings — shortest wins
  (none)

resolved: 22:43:02   -> suspend in 1m55s
```

`idlectl status` prints the current state of every fact. `idlectl explain <action>` prints the
composition above for one action, and `--json` gives the same thing machine-readably. `idlectl
doctor` reports the other candidate owners of power it can read, unreadable detectors, disabled
facts, every ceiling in the effective configuration, blocks that cannot act, and config faults; it
exits non-zero when any enabled fact is indeterminate or any config file was dropped.

Nothing is ever skipped silently. A detector that fails, a block that cannot be evaluated, a config
key that does nothing — each produces a log line. A feature that is quietly inert is worse than one
that is loudly broken.

---

## Why not logind's `IdleActionSec`, or an inhibitor

Both are the obvious answers, and both were measured to be insufficient before this project was
written.

**`IdleActionSec` is driven by `IdleHint`.** On a Plasma 6 Wayland session, `IdleHint` was measured
permanently `no` — the session never reports itself idle, so the idle action never fires, whatever
timeout you configure. Check it on your own machine before you trust it:

```sh
loginctl show-session "$(loginctl show-user "$USER" -p Display --value)" -p IdleHint -p IdleSinceHint
```

Beyond that, `IdleActionSec` is one global timeout driving one action. It cannot express "not while
a game is running", it cannot express "five minutes after a resume but thirty minutes otherwise",
and it has no notion of a screen-off action at all.

**Inhibitors cannot see what actually keeps a machine busy.** With a 150 GB Steam download in
flight, `systemd-inhibit --list` showed only NetworkManager and UPower (both in `delay` mode) and a
power-key handler. **Steam declares no inhibitor while downloading.** An inhibitor-based design is
structurally blind to one of the most common reasons a desktop must stay awake.

**Inhibitors are also not enforcement.** `systemctl poweroff` invoked without a tty returns 0 and
ignores inhibitors and sessions entirely. Anything that can call it can end your session regardless
of what locks are held.

So `idlectl` reads inhibitor locks as one fact among many — useful evidence that something asked to
stay awake — and never as the mechanism that keeps the machine alive. The mechanism is that
`idlectl` is the only component that acts.

---

## Why Rust

Look at what the existing Linux power daemons are written in, and the split is not about age or
taste. It is about ABI.

The ones written in C — `systemd-logind`, `elogind`, UPower, `power-profiles-daemon` — are C
*because they export a consumable C ABI*. Other programs link against them. That constraint is real,
and it decides the language.

`idlectl` exports no C ABI. Its entire public surface is a D-Bus interface, a CLI and a TOML file.
The C constraint does not transfer. And in that second cluster — self-contained daemons that talk
D-Bus and ship a CLI — the answer is uniformly Rust: `system76-power`, `supergfxctl`, `asusctl`,
`scx-scheds`. All of them package cleanly on Arch against the repo `rust` package, which is the
distribution reality this project has to live in.

That is the whole argument. Rust here is the boring, precedented choice for this shape of program,
not a claim about anybody's memory-safety virtue.

---

## Install

### The package (preferred)

```sh
makepkg -si            # in a clone of the packaging repository
```

The PKGBUILD lives in its own repository, not here — `makepkg` needs `source` to point at a
published tarball plus a checksum, so a PKGBUILD kept inside the source tree is either
self-referential or permanently out of date. The same is true of every comparable project surveyed.

It is not on the AUR yet. When it is, the line above becomes `paru -S idlectl` (or `yay -S idlectl`)
and nothing else changes: same package, same file layout, same two units left disabled.

```sh
sudo systemctl enable --now idlepolicyd.service
systemctl --user enable --now idlectl-agent.service
```

There is nothing to copy. The package installs the vendor policy at
`/usr/lib/idlectl/idlectl.toml`, which is layer 1 of the config chain and is in effect immediately.
Check it, then override only what you want to change:

```sh
idlectl doctor
sudoedit /etc/idlectl/idlectl.toml     # layer 2, overrides the vendor file
idlectl check-config
```

### Installer script

For a machine without an AUR helper, [`packaging/install.sh`](packaging/install.sh) builds from
source and lays down the same file layout under `/usr/local`:

```sh
curl -fsSL https://raw.githubusercontent.com/Eric-Canas/cachyos-idlectl/main/packaging/install.sh | bash
```

It is the second-choice route on purpose, and it says so: it refuses to run when an AUR helper is
present, refuses to install over a `pacman`-owned path, never writes to `/etc` and never enables a
unit. [`packaging/uninstall.sh`](packaging/uninstall.sh) removes exactly what it added. A packaged
install is owned by `pacman`, upgrades with the system and can be removed completely; this one is
not, which is fine deliberately and bad by accident.

### From source

See [CONTRIBUTING.md](CONTRIBUTING.md).

---

## Security model

**What runs as root.** Only `idlepolicyd`. It is the component that calls logind to suspend,
hibernate or power off. It opens no network sockets, ships no setuid binaries, and never execs a
command line built out of data it did not author.

**What runs unprivileged.** `idlectl-agent`, one instance per graphical session, as the session
user. Everything the root daemon cannot reach lives there: compositor idle notification, MPRIS
playback status, and the DRM `fdinfo` reading of which processes hold GPU memory — that last one
because `fdinfo` is gated by `ptrace_may_access`, so the daemon would need `CAP_SYS_PTRACE` to read
another user's, and reading any process's memory is not a power on a power daemon. What the agent
sends is raw: the daemon does the attribution, from the process tree and the command lines, which
are world-readable. The agent **reports facts and cannot command actions** — with exactly one
exception, stated here rather than buried, because the OLED story depends on it.

**The one exception: `screen_off`.** No root process can blank a Wayland output; only the
compositor, or whoever holds DRM master, can. So the agent exposes `Blank`/`Unblank` on the system
bus, callable **only by uid 0**, and the daemon invokes it. The agent commands nothing but the
screen, only inside its own session, and only when the caller is the daemon. Where no agent is
running, or the session type offers no blanking mechanism, `screen_off` reports `unavailable` **as
an action**: the daemon never proposes it, every block setting a `screen_off` key is inert, and
`doctor` lists which ones. Nothing a session says can cause the machine to sleep; it can only make
it stay awake, or stop making it stay awake.

**What root parses, and from where.** Root-owned config under `/etc/idlectl/` and
`/usr/lib/idlectl/` (`0644`, `root:root`), kernel and sysfs/procfs interfaces, and D-Bus messages
whose sender is checked. Fact reports from session agents are treated as untrusted input:
range-checked, size-bounded and time-bounded, and a malformed or unparseable report becomes
`indeterminate` — which vetoes sleeping — rather than an error that takes the daemon down. Config
values are validated up front; a file that fails to parse or carries a bad value is dropped as a
whole, the remaining layers are kept, evaluation continues, the three sleep actions are held off
until the fault is fixed, and `doctor` exits non-zero naming the key, the file and the offending
text. An unparseable value is a veto and a loud log line, never a crash and never a dead daemon. (A
single non-numeric value in a shell config once killed an entire earlier decider under `set -u`,
before one rule had been evaluated. Not again.) If the vendor layer itself is unusable, the daemon
falls back to a compiled-in `[while.always] clock = "human_input", screen_off = "15m"` and nothing
else, so a broken package cannot leave a panel lit indefinitely either.

**D-Bus.** The system bus name and interface namespace is `io.github.ericcanas.Idlectl1`. Bus policy
ships in `/usr/share/dbus-1/system.d/io.github.ericcanas.Idlectl1.conf`. Read-only methods —
`status`, `explain`, `doctor` — are open to any local user. Every method that can change state is
gated by a polkit action under the same namespace, so authorisation is administrator-configurable
rather than hard-coded:

| action id                                      | gates                                         |
|------------------------------------------------|-----------------------------------------------|
| `io.github.ericcanas.Idlectl1.rest`             | asking the machine to rest, policy respected  |
| `io.github.ericcanas.Idlectl1.rest-forced`      | `--force`, which defeats every floor          |
| `io.github.ericcanas.Idlectl1.lease`            | taking or releasing a lease for another user  |
| `io.github.ericcanas.Idlectl1.reload`           | reloading the configuration                    |

`rest` and `rest-forced` are deliberately separate ids: a credential that may *request* rest must
not thereby be able to force it.

The namespace has no hyphen and is lower-case deliberately: D-Bus restricts interface name elements
to `[A-Za-z0-9_]`, so the author's forge handle cannot appear verbatim. This is permanent — it is
baked into the bus policy filename, the polkit action ids and every client.

**Threat model, plainly.** A local unprivileged user can make the machine stay awake — by reporting
facts, holding a lease, or holding an inhibitor. That is by design, and it matches every other idle
system. A local unprivileged user must **not** be able to make the machine sleep, end another user's
session, or bypass the `human_active` floor. If you find a way to do anything in that second list,
that is a vulnerability.

**Reporting a vulnerability.** Use GitHub's private vulnerability reporting on this repository
(*Security* → *Report a vulnerability*). Please do not open a public issue for an undisclosed
vulnerability. Expect an acknowledgement within a few days: this is a single-maintainer project, not
a vendor with an on-call rota, and it is better to say so than to imply an SLA that does not exist.

---

## Design principles

1. **A machine that stays awake wrongly is a cheap error. A machine that sleeps wrongly is not.**
   Every ambiguity resolves towards staying awake. This is the invariant the whole engine is built
   around.
2. **One owner of power.** Two components with independent idle timers produce failures nobody can
   reproduce.
3. **Order never matters.** Max and min over instants; no priority list, no first-match-wins, no
   rule ordering to reason about.
4. **The default is to do nothing.** With no true blocks, nothing is due. Every action is opt-in —
   which is about the empty set of *blocks*, not about the empty set of keys inside a block: a
   block that says nothing about an action says nothing about it, and is not secretly voting
   `never`.
5. **Never fail silently.** A detector that cannot read its source, a block that cannot be
   evaluated, a config key that does nothing — all of them log. A `systemd` unit carrying a
   `Condition*` was once silently skipped, turning a whole feature into dead code that logged
   nothing at all. Loud beats convenient.
6. **Running is not in use.** State is instantaneous; usage is cumulative. Measure the counter, not
   the process.
7. **Explainable before it acts.** If you cannot ask why the machine is about to sleep, you cannot
   trust it not to.

---

## Scope

**v1 targets desktops.** Laptop power policy is deliberately out of scope: no battery policy, no lid
handling, no on-AC/on-battery rule sets. Battery facts may be *displayed* by `status`, `explain` and
`doctor` where the hardware exposes them, but no policy is built on top of them and the lid is not
touched. Doing laptops properly means AC transitions, lid-close semantics and critical-battery
handling, and half of that is worse than none of it.

Steam detectors ship first-class and enabled, because gaming desktops are the target. On a machine
with no Steam they report `unavailable`, and the corresponding blocks are never true.

Out of scope permanently: CPU frequency and power tuning, screen locking, session management, and
anything that would require shipping a kernel module.

---

## Documentation

- [`docs/spec.md`](docs/spec.md) — the normative specification. Behaviour is defined there; the code
  implements it.
- [`CONTRIBUTING.md`](CONTRIBUTING.md) — building, style, and how a change lands.
- [`TESTING.md`](TESTING.md) — the manual suspend/resume protocol, and an honest account of what CI
  cannot cover.
- [`CHANGELOG.md`](CHANGELOG.md).
- `man idlectl`, `man idlectl.toml`, `man idlepolicyd`.

## License

MIT. See [LICENSE](LICENSE).
