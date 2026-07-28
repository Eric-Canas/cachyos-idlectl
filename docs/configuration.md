<!-- Moved out of README.md: the full configuration reference. The README keeps the
     three composition rules and the layer table, which is what a first-time reader needs. -->

# Configuration

*[← back to the README](../README.md)*

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
