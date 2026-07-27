# idlectl — Normative Specification

**Status:** Draft, normative for v1.
**Applies to:** `idlectl` (CLI), `idlepolicyd` (system daemon), `idlectl-agent` (session agent).
**Audience:** implementers. This document exists so that two independent implementations built
from it behave identically, and so that the reasoning below is not re-derived by anyone else the
expensive way.

---

## 0. How to read this document

### 0.1 Requirement keywords

The key words **MUST**, **MUST NOT**, **REQUIRED**, **SHALL**, **SHALL NOT**, **SHOULD**,
**SHOULD NOT**, **RECOMMENDED**, **MAY** and **OPTIONAL** are to be interpreted as described in
RFC 2119 and RFC 8174, and only when they appear in capitals.

### 0.2 Requirement identifiers

Every normative statement carries a stable identifier, e.g. `[COMP-4]`. Identifiers are
permanent: a requirement that is withdrawn keeps its identifier and is marked `WITHDRAWN`, it is
never reused. Tests, commit messages and bug reports SHOULD cite identifiers.

Prefixes:

| Prefix | Area |
|---|---|
| `MODEL` | Object model and vocabulary |
| `CLK` | Clocks and origins |
| `FACT` | Fact states and detectors |
| `COMP` | Composition of deadlines |
| `CEIL` | `[when]` ceilings |
| `HUM` | The `human_active` floor |
| `ACT` | Action execution |
| `REQ` | Requests, `rest`, `--force`, authorization |
| `CFG` | Configuration format and resolution |
| `OBS` | Observability: `explain`, `doctor`, logging |
| `TEST` | Mandatory conformance vectors |

### 0.3 Rationale notes

Statements marked with a trailing *(measured: …)* exist because of a specific observed failure on
real hardware. **Those rationales MUST survive any rewrite of this document.** They are the only
thing standing between a future maintainer and the reintroduction of a bug that was already paid
for once.

### 0.4 The invariant that outranks everything else

> **A machine that stays awake wrongly is a cheap error. A machine that sleeps wrongly is not.**

**[MODEL-1]** Where this specification is ambiguous, an implementation MUST resolve the ambiguity
in the direction that keeps the machine awake — with exactly one exception, the `screen_off`
action (§5.6), where the asymmetry is reversed because panel damage is cumulative and
irreversible while a dark screen is not.

---

## 1. Scope

### 1.1 What idlectl is

`idlectl` is a **power/idle policy engine**. A resident root daemon (`idlepolicyd`) decides *when*
a machine may:

- turn its display output off (`screen_off`),
- suspend to RAM (`suspend`),
- hibernate to disk (`hibernate`),
- power off (`poweroff`).

It replaces the **policy** half of desktop-environment power daemons, `logind`'s `IdleActionSec`,
and `swayidle`-style timer scripts. It does not replace the **mechanism**: the actual sleep
transition (`suspend`, `hibernate`, `poweroff`) is performed by `logind` (or an equivalent system
component). `screen_off` is not a system power transition and has a mechanism of its own, defined
in §8.6.

### 1.2 What idlectl is not

**[MODEL-2]** `idlectl` MUST NOT change CPU frequency governors, energy-performance preferences,
platform profiles, PCIe ASPM, or any other tunable. Those belong to `tlp`, `tuned`,
`power-profiles-daemon` or `auto-cpufreq`. Conflating idle *policy* with CPU *tuning* is a
category error and an implementation that does both is out of conformance.

**[MODEL-3]** v1 MUST NOT implement battery, AC, or lid policy. Power supply *facts* MAY be
collected so that `explain` and `doctor` can display them, but no block condition and no action
may depend on them in v1. Laptops are out of scope for v1; shipping half a laptop policy is worse
than shipping none.

§13 refutes the designs this project deliberately does not use, with the measurements that refute
them. That section is load-bearing: without it, someone eventually "simplifies" this project into
one of the designs that was measured not to work.

---

## 2. Object model

### 2.1 Vocabulary

| Term | Definition |
|---|---|
| **Action** | One of `screen_off`, `suspend`, `hibernate`, `poweroff`. |
| **Fact** | A named observation about the machine, in one of four states (§5). |
| **Condition** | A named predicate usable as a block selector. Every fact name is a condition; plus the single built-in `always`. `after_resume` is a fact ([FACT-45]), not a built-in. |
| **Block** | A configuration table `[while.<condition>]` or `[when.<condition>]` giving a timeout per action. |
| **Clock** | A named monotonic time base with a defined origin (§4). |
| **Timeout** | A duration, or `never`. |
| **Deadline** | An absolute instant on `CLOCK_BOOTTIME`, possibly `+∞`. |
| **Floor** | A `[while]` block. It pushes deadlines **later**. Composed with MAX. |
| **Ceiling** | A `[when]` block. It pulls deadlines **earlier**. Composed with MIN. There is exactly one ceiling class in v1 and every ceiling is absolute (§6). |
| **Resolved instant** | The single instant, per action, at which the action becomes permitted. |

**[MODEL-4]** The four actions are a closed set in v1. An implementation MUST NOT invent
additional actions (`lock`, `dim`, `reboot`, …) without a config-format major version bump
(§11.9).

### 2.2 The one composition rule

> The configuration is a set of `[while.<condition>]` blocks. Each gives a timeout **per action**.
> For each action **independently**, take the **longest** deadline among the currently-true blocks
> **that set a key for that action**. `never` is infinity and always wins. A block that says
> nothing about an action is not a block that says `never` about it. The maximum over the empty set
> is `never`.

**[MODEL-5]** Block order MUST NOT affect the result. There MUST be no priority list, no
first-match, no last-wins. MAX and MIN over instants are commutative and associative; an
implementation whose result depends on iteration order is non-conforming.

**[MODEL-6]** The default MUST be to not act. With no configuration loaded — that is, with the
**empty set of blocks** — every action's resolved instant is `+∞`.

This is a statement about the empty set of *blocks*, and MUST NOT be read as a statement about the
empty set of *keys within a block*. A block that is true and sets no key for action `a` contributes
nothing to `a`; it does not contribute `+∞`. That is [COMP-2b], and §3.2 makes it mechanical via
`participates(b, a)`.

**[MODEL-7]** The engine MUST NOT have a built-in notion of "hard veto" versus "soft veto". That
distinction is expressed entirely in data, as `never` versus a long finite timeout, per action.
The same applies to ceilings: v1 has no soft/hard ceiling split either ([CEIL-3]).
*(Rationale: the shipped system this was extracted from grew a hard/soft split in code, and then
needed a third category when a game's GPU memory had to be soft while a training job's stayed
hard. Expressed as timeouts, the same behaviour needs no special cases at all — see §12.4.)*

---

## 3. Composition (normative core)

This section is the heart of the specification. If anything else here is wrong the project is
buggy; if this section is wrong the project is dangerous.

### 3.1 Composition happens over instants, never over durations

**[COMP-1]** Composition MUST be performed over **absolute deadline instants**, never over
durations.

Rationale: different blocks are measured on different clocks. `max(30m, 5m)` is meaningless when
the two are counted from different origins. *(Measured: composing durations reproduced a real
incident verbatim — a machine woke at 08:35:45 and logged "SUSPENDING" at 08:35:46. See
§12.2 and [TEST-2].)*

### 3.2 The formula

For each action `a` independently:

```
    participates(b, a)  ⟺  block b sets a key for action a. A block that does not set a
                           key for a does not participate in a, whatever its condition state.

    deadline(b, a)      =  +∞                                  if timeout_b(a) = "never"
                        =  origin(clock_b) + timeout_b(a)      otherwise

    floor(a)            =  MAX over deadline(b, a) for every participating, enabled,
                           currently-TRUE [while] block b,
                           together with one +∞ term for each implicit floor in force
                           (the doubt floor of [FACT-4] and the configuration-fault floor
                           of [CFG-16]; neither applies to screen_off).
                           MAX over the empty set = +∞.

    ceiling(a)          =  MIN over deadline(b, a) for every participating, enabled,
                           currently-TRUE [when] block b, plus the ephemeral ceiling
                           installed by --force (§9.3).
                           MIN over the empty set = +∞.

    resolved(a)         =  MIN(floor(a), ceiling(a))

    a is permitted      ⟺  now ≥ resolved(a)
```

**[COMP-2]** An implementation MUST compute `resolved(a)` exactly as above.

**[COMP-2b] — the omission rule.** A block that does not set a key for action `a` contributes
NOTHING to `floor(a)` and nothing to `ceiling(a)`, whatever its condition's state. **Silence is not
`never`.** This is what makes per-action policy work: `[while.steam_downloading]` vetoes `suspend`
while having no opinion about the panel, and `[while.human_active]` holds the machine awake without
pinning a lit image on it. An implementation that treats an unset action key as `never` is
non-conforming.

*(Rationale, and it is not a tie. The alternative reading — "a true block that grants no permission
for `a` contributes `+∞`" — makes a download, or a dead idle agent, pin the panel lit forever: the
exact OLED failure that §5.6, [FACT-40] and [HUM-6] exist to prevent, and the reading under which
Appendix A's own comments are incoherent. [MODEL-6]'s "the default MUST be to not act" is about the
empty set of blocks, not the empty set of keys within a block. Mandatory vector: [TEST-21].)*

Three consequences of the shape of this formula are deliberate and MUST be preserved:

- **A ceiling can only pull an action earlier.** `MIN` over instants cannot produce a later instant
  than either operand, so a `[when]` block is never a way to keep a machine awake ([CEIL-3],
  [TEST-6b]).
- **A ceiling defeats everything a floor can raise**, including `never` floors and the two implicit
  floors, because both are `+∞` terms inside `floor(a)` and `MIN(+∞, x) = x` ([CEIL-4]). There is
  exactly one override channel in the system and this is it.
- **The implicit floors are not blocks.** They are not satisfiable by `rest --now` ([REQ-3]), they
  are not disabled by `enabled = false`, and they never touch `screen_off` ([FACT-5], [FACT-40]).

**[COMP-3]** `+∞` MUST be representable and MUST NOT be approximated by a large finite value. An
implementation using, for example, `u64::MAX` nanoseconds MUST guarantee that no arithmetic path
can wrap it into the past.

**[COMP-4]** Deadlines MUST be computed on `CLOCK_BOOTTIME` — a monotonic clock that continues to
advance while the system is suspended.

`CLOCK_MONOTONIC` MUST NOT be used: it does not advance during suspend, so time spent asleep would
not count towards idle. A machine that slept for nine hours would wake believing no time had
passed, which contradicts both the observed behaviour of real idle counters and [COMP-9].

`CLOCK_REALTIME` MUST NOT be used: NTP steps, manual clock changes and timezone or DST transitions
move it arbitrarily, including backwards, which would move every armed deadline with it.

A consequence of `CLOCK_BOOTTIME` is that a timer armed before a sleep is already expired on
resume. That is expected and is handled by [COMP-8], not by choosing a different clock.

**[COMP-5]** A deadline in the past is not an error and MUST NOT be clamped. `MAX` naturally
discards it. *(Rationale: `[while.after_resume]` stays true for the whole boot; its deadline is in
the past for almost all of that time and must simply lose the MAX. See §4.4.)*

**[COMP-6]** Evaluation MUST be re-run, from scratch, at each of the following moments — not on a
fixed poll alone:

1. any fact transition (including transitions to and from `INDETERMINATE`),
2. return from any system sleep state,
3. configuration reload,
4. arrival or expiry of a request (§10),
5. the earliest pending `resolved(a)` across all actions, armed as a timer.

**[COMP-7]** An implementation MUST NOT rely on a fixed polling interval as its only trigger.
*(Measured: a 5-minute poll in the prior system produced two distinct defects — a wake-up
evaluation landing in the same second as the resume, and up to 5 minutes of latency between a
condition clearing and the machine acting on it.)*

**[COMP-8]** A timer that expired while the system was asleep MUST NOT be executed on resume. The
implementation MUST discard it and recompute from current origins. *(Measured: `systemd` runs the
tick that was missed during sleep, so the naive design evaluates in the same second as the wake.
The recomputation is what allows the `after_resume` floor to move the deadline forward; see
[TEST-2].)*

### 3.3 Idle counters do not reset across suspend

**[COMP-9]** An implementation MUST NOT reset the human-input clock, or any other clock, on
resume.

*(Measured: this is the tempting fix and it is wrong. Resetting the human-input origin on resume
makes idle time zero immediately after a wake, so a machine woken by a remote relay to do a job
refuses to go back to sleep when the job finishes — it now looks freshly used. The correct fix is
the `after_resume` floor (§4.4), which delays the action without falsifying the observation.)*

### 3.4 Ordering property

**[COMP-10]** For any set of blocks and any permutation of that set, `resolved(a)` MUST be
identical. Implementations SHOULD include a property test that shuffles the block set and asserts
bit-identical results.

---

## 4. Clocks

### 4.1 Catalogue

Every block is measured on exactly one clock. All clocks are instants on `CLOCK_BOOTTIME`.

| Clock | Origin | Unknown when | Behaviour when Unknown |
|---|---|---|---|
| `human_input` | Instant of the most recent human input event observed on any seat. | No input has been observed since boot. | Origin falls back to **boot** (§4.3). |
| `resume` | Instant of the most recent return from a system sleep state. | No sleep/resume cycle has occurred since boot. | There is **no** fallback to boot. A TRUE block on this clock contributes `+∞` for `suspend`/`hibernate`/`poweroff` and its boot-origin deadline for `screen_off` — [CLK-11]. |
| `condition` | Instant of the most recent FALSE→TRUE edge of the block's own condition. | The condition has been true since before the daemon started. | Origin falls back to **daemon start**, and `doctor` MUST report the substitution (§4.5). |
| `boot` | `CLOCK_BOOTTIME` zero. | Never. | — |

**[CLK-1]** These four clocks are the complete set for v1. An implementation MUST NOT add clocks
without a config-format major version bump.

**[CLK-2]** Each block MUST have exactly one clock, given by its `clock` key. The default when
`clock` is absent is `human_input`.

**[CLK-3]** `explain` MUST display, for every block, which clock it uses and that clock's current
origin as an absolute instant *and* as an age. *(Rationale: choosing `condition` when you meant
`human_input` is the single most likely authoring error, and it is invisible without this. See
[CLK-6].)*

### 4.2 Reading the human-input clock

**[CLK-4]** The human-input clock MUST be fed by an idle-notification protocol that reports **real
input**, not by a heartbeat that a compositor emits regardless of input. On Wayland this is
`ext-idle-notify-v1`. On X11 it is the XScreenSaver extension's idle counter.

**[CLK-5]** The session agent feeding the clock MUST publish a liveness heartbeat, and the daemon
MUST treat a missing or stale heartbeat as `INDETERMINATE` for `human_active` — see [HUM-4].
*(Measured: the human-activity signal is only trustworthy in the negative direction. While
somebody is holding a controller, an idle-notifier emits nothing at all; a naive
"last activity" file therefore goes stale during exactly the activity it is supposed to detect.
The reliable construction is the one that *disappears* on input.)*

**[CLK-6]** A block whose timeout is intended to mean "this long without a human" MUST use
`human_input`, not `condition`. *(Measured: "I have been playing for five hours" and "I fell
asleep with the game running" are indistinguishable in process state and identical on the
`condition` clock; only human input separates them. A 2 h timeout on the `condition` clock
suspends the machine in the middle of a 2 h session.)*

**[CLK-13]** An agent MUST measure idle time on a clock that keeps counting while the machine is
suspended. *(Rationale: the daemon anchors the origin at `now - idle`, both on `CLOCK_BOOTTIME`. An
agent whose stopwatch is `CLOCK_MONOTONIC` puts the two halves of that subtraction on two different
timelines, and the error is exactly the time spent asleep. Measured on a machine up for 7 h 24 m of
which 2 h 16 m were suspended: `CLOCK_BOOTTIME` 26649 s against `CLOCK_MONOTONIC` 18471 s, a
discrepancy of 8178 s. The daemon then recorded a seat nobody had touched in seventy minutes as
"input in session 1 3m ago", because the stopwatch had been frozen for the whole sleep. This is
also what makes the `after_resume` settle floor of [CLK-8] meaningful: it exists to defeat a large
accumulated idle time, which presupposes that idle time accumulates across a sleep.)*

**[CLK-14]** An agent whose protocol reports only **transitions** MUST NOT report an idle time of
zero before it has observed one. Until a transition arrives, or an instant is carried over under
[CLK-15], it MUST report the idle time as unknown.

*(Rationale: `ext-idle-notify-v1` sends `idled` and `resumed` and offers no request for the current
idle time, so an agent that has just started has observed nothing. Reporting zero asserts that
input just happened, which is an observation nobody made, and it is the direction that keeps a
machine awake. Unknown is the state [CLK-5] and [HUM-4] already define, and it resolves within one
notification timeout. Measured: KWin under Wayland answers
`org.freedesktop.ScreenSaver.GetSessionIdleTime` with `not supported on this platform`, so the
information cannot be recovered by asking either. X11 needs none of this, because
`MIT-SCREEN-SAVER` exposes the counter itself.)*

**[CLK-15]** An agent SHOULD carry its last-input instant across its own restarts, in a runtime
location that survives suspend and is cleared by a cold boot ([CLK-9]). It MUST NOT adopt a carried
instant when no agent was watching the seat for longer than a bounded gap, and MUST discard one the
moment the compositor reports real input.

*(Rationale: without this, restarting the agent restarts every countdown measured from the
human-input clock — the outcome [CLK-7] forbids for the daemon, arriving by way of the agent
instead. A package upgrade is enough to cause it. Measured with nobody in the room: restarting the
agent moved the daemon's `human_input` origin from +25823 s to +26299 s, exactly the uptime at the
restart, while an independent `swayidle` timestamp watching the same seat did not move at all. The
bound is required because input during the gap is unobservable by construction: nothing was
watching. At the sizes involved — a sub-second restart from an upgrade, against timeouts of tens of
minutes — the residual error is at most the gap. Mandatory vector: [TEST-25].)*

### 4.3 Human-input clock never touched this boot

**[CLK-7]** When no human input has been observed since boot **and the input path is otherwise
healthy**, `origin(human_input)` MUST be the boot instant, and MUST NOT be `+∞` or "now". This
fallback applies only to the never-touched case; it MUST NOT be applied when the clock is
unreadable ([CLK-11]).

Rationale: a machine woken by a remote relay to run a job has never been touched. Its base
schedule must still be able to expire, so the machine can go back to sleep as soon as the job
finishes. Setting the origin to `now` on first evaluation would restart the countdown on every
daemon restart; setting it to `+∞` would keep such a machine awake forever. This is the **remote
relay fast path** and it MUST be preserved. Note that this is a statement about the *clock*; the
`human_active` *fact* is separately FALSE in this state (§7).

### 4.4 The `resume` clock and the `after_resume` condition

**[CLK-8]** `after_resume` MUST be TRUE from the first resume of a boot until the machine next
cold-boots, and MUST NOT expire after the settle window.

This single condition carries two behaviours, and an implementation MUST provide both:

1. **A settle floor.** Measured on the `resume` clock with a finite timeout (default 5 min), it
   delays autonomous action after a wake. Because it is a floor and composition uses MAX, it
   defeats an arbitrarily large accumulated idle time — which is the entire point ([TEST-2]).
2. **A whole-boot marker.** Because it stays TRUE for the rest of the boot, it is the signal that
   this boot contains a session somebody left suspended. `explain` and `doctor` MUST be able to say
   so, and it is one of the exactly two conditions that `rest --now` satisfies ([REQ-3]).
   *(Measured: deliberately conservative. If you suspend, wake, use the machine and leave it idle,
   a later rest request re-suspends rather than powering off — it returns the machine to the state
   you left it in, and never closes a session on your behalf. In v1 that outcome is delivered by
   [REQ-17] — `rest` without `--action` is always `suspend` — rather than by a configurable
   substitution; see [REQ-18], WITHDRAWN.)*

**[CLK-9]** The state backing `after_resume` MUST live in a runtime location that **survives
suspend/resume and is cleared by a cold boot** (on Linux: a `tmpfs` runtime directory). It MUST
NOT be persisted to disk. *(Measured: this is precisely the discriminator that an external
observer cannot compute — from outside the machine, S3 and S5 are indistinguishable by ARP, by
ping and by port probing, and the same wake packet wakes it from both.)*

**[CLK-10]** The resume hook MUST be a system unit ordered against the sleep targets. A
sleep-hook script directory MUST NOT be the only mechanism. *(Measured: on systemd 261 a hook
placed in the sleep-hook directory did not execute during a real suspend/resume cycle — it ran
correctly when invoked by hand, and left no trace in the journal during the actual cycle. A unit
ordered `After=`/`WantedBy=` the sleep targets does work, and is what the graphics-driver vendor's
own units use.)*

### 4.5 Unreadable and unknown origins

**[CLK-11]** If a block is TRUE and its clock's origin is either **unreadable** (the detector
feeding it is erroring, stale or absent) or Unknown with no fallback defined by this
specification, the block's deadline for `suspend`, `hibernate` and `poweroff` MUST be `+∞`, and
for `screen_off` MUST be the block's deadline computed from the `boot` origin. The `screen_off`
carve-out is [FACT-40] applied to clocks: an unreadable clock must not be able to leave a static
image on a panel indefinitely.

The `resume` clock with **no resume recorded this boot** is exactly such an origin, and this rule
is the whole of its behaviour: an implementation MUST NOT silently substitute the `boot` origin for
`resume`. *(Rationale: the substitution is sometimes defended on the grounds that a block on the
`resume` clock is "in practice" also conditioned on `after_resume`. Nothing enforces that.
`[while.always] clock = "resume"` is a legal configuration, and under a boot fallback it fires
immediately on a machine that has never slept — a settle window that acts as an accelerator. Under
[CLK-11] it contributes `+∞` until the machine has actually resumed once, which is what the block's
author meant. Mandatory vector: [TEST-23].)*

**[CLK-12]** An origin that is Unknown-by-definition (no resume has happened yet) MUST be
distinguished in `explain` and `doctor` output from an origin that is **unreadable** (the detector
feeding it is erroring or stale). The first is normal; the second is a fault and MUST be reported
as such. *(Rationale: "fail loudly, never silently" — see [OBS-6].)*

---

## 5. Facts

### 5.1 The four states

| State | Meaning |
|---|---|
| `TRUE` | The detector ran and observed the condition to hold. |
| `FALSE` | The detector ran and observed the condition not to hold. |
| `INDETERMINATE` | The detector should be able to answer and could not: unreadable file, timed out call, parse failure, dead helper, stale sample. **This is doubt.** |
| `UNAVAILABLE` | The capability this fact observes is absent on this machine: the software is not installed, the protocol is not offered, the device does not exist. **This is knowledge, not doubt.** |

**[FACT-1]** Every fact MUST be reported in exactly one of these four states. A fact MUST NOT be
reported as a bare boolean anywhere in the daemon, the CLI, the D-Bus surface or the logs.

**[FACT-2]** The distinction between `INDETERMINATE` and `UNAVAILABLE` MUST be preserved end to
end. Collapsing them is a conformance failure: they compose in opposite directions.

**[FACT-43]** A fact whose **capability is absent** MUST report `UNAVAILABLE`, never
`INDETERMINATE`. No Steam installation, no session bus, no GPU query interface, no session manager:
each of those is a thing the machine is known not to have, and knowledge is not doubt. `INDETERMINATE`
is reserved for a detector that should have been able to answer and did not. *(Rationale: [FACT-4]
is block-independent, so a capability misreported as doubt would wedge every machine that lacks the
capability. This requirement is one of the three disciplines that make the block-independent veto
safe to ship; the others are [FACT-44] and [CFG-12].)*

### 5.2 How each state composes

| State of fact `f` | Effect on `[while.f]` / `[when.f]` | Additional systemic effect |
|---|---|---|
| `TRUE` | Block is true; its deadline enters the MAX (floor) or MIN (ceiling). | — |
| `FALSE` | Block is false; contributes nothing. | — |
| `UNAVAILABLE` | Block is **false**; contributes nothing. | Block MUST be listed as *inert* by `doctor`. |
| `INDETERMINATE` | Block is **false** as a selector, in `[while]` and `[when]` alike ([FACT-4b]). | The **fact set**, not the block, adds an implicit `+∞` floor to `suspend`, `hibernate` and `poweroff` — see [FACT-4]. |

**[FACT-3]** A block whose condition is `UNAVAILABLE` MUST degrade to "condition never true" and
MUST NOT veto anything. *(Rationale: the Steam detectors ship enabled and first-class. On a machine
with no Steam they must be silently inert, not a permanent veto.)*

**[FACT-4] — the doubt veto.** At each evaluation, if **any enabled fact** is `INDETERMINATE`, the
engine MUST add one implicit floor of `+∞` to `suspend`, `hibernate` and `poweroff`. This applies
whether or not any configured block references that fact: the rule is about the machine no longer
being able to see something it normally sees, and whether the operator happened to write a block
for it is irrelevant to that.

The implicit floor is **not a block**. It is not satisfiable by `rest --now` ([REQ-3]), it is
defeated only by a ceiling ([CEIL-4]), and `explain` and `doctor` MUST name each `INDETERMINATE`
fact as a reason for the floor. *(Measured: nine separate veto checks in the prior system, and the
ninth was literally "any error while evaluating".)*

**[FACT-4b]** A block whose condition is `INDETERMINATE` is **FALSE as a selector** and contributes
nothing, in `[while]` and in `[when]` alike. **Doubt may prevent an action; it may never cause
one.** An implementation MUST NOT deliver the doubt veto as a per-block contribution: doing so
couples the veto to whichever actions some block happened to name, and it lets an administrator's
`[when.<fact>]` ceiling fire on a machine whose detector for `<fact>` is broken.

**[FACT-5]** The implicit `+∞` floor from [FACT-4] MUST NOT apply to `screen_off`, and no other
implicit floor may either ([FACT-40], [CFG-16].3). One unreadable detector must never be able to
leave a static image on a panel. See §5.6.

**[FACT-6]** A fact that is disabled by configuration (`[facts.<name>] enabled = false`) MUST
report `UNAVAILABLE`, MUST NOT run its detector, and MUST NOT contribute a doubt veto. `doctor`
MUST list every disabled fact. This is the supported, auditable way to escape a permanently
`INDETERMINATE` detector; silently demoting `INDETERMINATE` to `FALSE` in the detector is NOT.

**[FACT-7]** Every detector MUST have a bounded execution budget. Exceeding it MUST yield
`INDETERMINATE` for that fact and MUST NOT block the evaluation of other facts, and MUST NOT stall
the reactor.

### 5.3 Fact catalogue

The eleven names below are the **complete and frozen** fact vocabulary of v1, in the order an
implementation SHOULD enumerate them:

```
human_active   after_resume   remote_session   lease_held      inhibitor_block   media_playing
steam_game_running   steam_downloading   gpu_busy_game   gpu_busy_other   local_service_busy
```

**[FACT-46]** These eleven names, plus the single built-in condition `always`, are the complete set
of legal condition names in v1. An implementation MUST NOT add, rename or alias one without a
config-format major version bump ([CFG-22].1), and every unknown condition name MUST be a fatal
configuration error ([CFG-20]).

Every fact below is REQUIRED in v1. The two mandatory facts — `human_active` and `after_resume` —
are always available. The other nine are **optional capabilities**: each has an `UNAVAILABLE`
condition given with it, and on a machine lacking the capability it reads `UNAVAILABLE`
([FACT-43]) and its blocks are inert ([FACT-3]). In every case a detector error, timeout or
unparseable reading yields `INDETERMINATE`.

#### 5.3.1 `human_active`

Derived, not raw. TRUE when a human has touched an input device within `[general] min_idle`
(default 5 min). Fully specified in §7, including its FALSE case ([HUM-3]) and its `INDETERMINATE`
case ([HUM-4]).

#### 5.3.2 `after_resume`

**[FACT-45]** `after_resume` is a fact like any other — not a built-in condition. It is TRUE from
the first resume of a boot until the next cold boot ([CLK-8]), FALSE before the first resume, and
never `UNAVAILABLE`: it is backed by a runtime marker the implementation writes itself ([CLK-9]),
not by an external capability. Fully specified in §4.4. Paired with `clock = "resume"` it is the
settle window.

#### 5.3.3 `remote_session`

**[FACT-8]** TRUE iff the session manager reports at least one open session whose class is a user
session and whose type or service indicates a remote login. UNAVAILABLE if no session manager is
present.

**[FACT-9]** A caller MAY request that its **own** session be excluded from this fact, but the
exclusion MUST be opt-in per request and MUST NOT be the default. *(Measured: a relay that opens
a session to ask the machine to rest would otherwise veto its own request. Every other caller —
the internal timer, or a human at a terminal — must count their own session, so the machine never
sleeps out from underneath the person driving it.)*

**[FACT-10]** `doctor` SHOULD report the age of each remote session. *(Measured: a process
detached with `setsid`/`nohup` from a remote shell stays inside the session scope, so the session
never closes and this fact becomes a permanent veto for the rest of the boot. This is a legitimate
diagnostic technique and an illegitimate way to leave something running; the lease (§5.3.4) is the
correct mechanism for the latter.)*

#### 5.3.4 `lease_held`

**[FACT-13]** TRUE iff at least one unexpired lease exists. A lease is a record carrying an owner
identifier, a human-readable reason, and an absolute expiry (TTL).

**[FACT-14]** Leases MUST live in the same runtime location class as [CLK-9]: surviving
suspend/resume, cleared by a cold boot.

**[FACT-15]** An expired lease MUST be treated as absent and SHOULD be reaped. A lease MUST NOT be
renewable-by-default; a holder that wants a longer window MUST take a longer TTL or re-acquire
explicitly.

**[FACT-16]** Lease state is written by unprivileged callers and read by a privileged daemon. The
daemon MUST parse lease records with an explicit parser and MUST NOT evaluate them as code
(no shell `source`, no `eval`, no config-language execution). A malformed lease record MUST yield
`INDETERMINATE`, not a crash and not silence.

#### 5.3.5 `inhibitor_block`

**[FACT-11]** TRUE iff at least one `logind` inhibitor lock with mode `block` covers `sleep` or
`shutdown`. Locks with mode `delay` MUST NOT make this fact TRUE. UNAVAILABLE if `logind` is not
present. *(Measured: `delay` locks only postpone a transition so their holder can tidy up; treating
them as vetoes makes this fact permanently true on any machine running a network manager.)*

**[FACT-12]** Inhibitor locks MUST be read as a **signal only**. The daemon MUST NOT rely on
inhibitors to prevent anything, and MUST NOT assume that other software will respect its own
decisions because an inhibitor exists. See §13.2 for the four measurements behind this.

#### 5.3.6 `media_playing`

**[FACT-17]** TRUE iff at least one MPRIS player on a user session bus reports
`PlaybackStatus == "Playing"`. UNAVAILABLE if no session bus is reachable.

**[FACT-18]** Desktop-environment "power management inhibition" APIs MUST NOT be used as the
source for this fact. *(Measured: with a real desktop power-inhibit held, three separate
inhibition query interfaces reported `false` and two empty lists. A veto built on them would have
been dead letter. MPRIS `PlaybackStatus` was verified to change in both directions.)*

#### 5.3.7 `steam_game_running`

**[FACT-19]** TRUE iff a Steam **application** — not merely the Steam client — is running,
detected either by the launcher wrapper signature or by a microcompositor-based game session.
UNAVAILABLE if Steam is not installed.

**[FACT-20]** The launcher signature MUST default to the wrapper pattern Steam actually uses in its
command lines, **verified against a real installation, not inferred**. The Steam installation root
is configurable as `[facts.steam_game_running] steam_root`. *(Measured: the working pattern was
found by reading a real launch's process command lines. Guessing at it produces a detector that has
never once been true.)*

#### 5.3.8 `steam_downloading`

**[FACT-21]** TRUE iff some file under the Steam staging tree has been modified within the download
window (`[facts.steam_downloading] window`, default 5 min). UNAVAILABLE if the staging directory
does not exist.

**[FACT-22]** This fact MUST NOT require a TTL or any explicit release, because it self-extinguishes
from both ends:

| Situation | Staging mtime | Result |
|---|---|---|
| Downloading | refreshed every 1–5 s | TRUE |
| Download paused | ages out | FALSE after the window |
| Download finished | platform empties the directory | FALSE |
| Debris from an aborted download | stale | FALSE |

**[FACT-23]** The implementation MUST walk the staging tree and take the **maximum** mtime; it
MUST NOT stop at the first file inside the window. *(Measured: stopping early halves the cost and
gives the correct decision, but reports whichever file directory order happened to yield —
producing log lines like "written 297 s ago" against a 300 s window while a download was running
at full speed. The decision was right and the number invited the opposite conclusion. The full
walk cost 20 ms over ~6900 files, once per evaluation. A log that misleads on the one output
anybody reads while debugging is not worth 10 ms.)*

**[FACT-24]** This fact MUST NOT be implemented as a network-throughput threshold. *(Measured and
rejected: throughput is more general — it would also cover package upgrades, torrents and backups —
but it cannot distinguish downloading from playing. 4K video playback is roughly 3 MiB/s, so a
throughput rule would make media playback a hard veto, contradicting the deliberate decision to
make it a soft one. The distinction between the two is exactly what the system needs.)*

#### 5.3.9 / 5.3.10 `gpu_busy_game` and `gpu_busy_other`

GPU load is **two** facts, not one. The split is what lets a game keep a soft, finite floor while
unattributed GPU load keeps a policy of its own.

**[FACT-25]** The GPU memory threshold is the **compiled-in constant 512 MiB**. It is not
configurable in v1: there is no `gpu_min_mem` key, and none may be added without a config-format
major version bump ([CFG-23].1 does not apply to a key that changes an existing meaning).
UNAVAILABLE, for both facts, if no GPU query interface is present.

**[FACT-26]** The compositor MUST be excluded **by name**, not only by the threshold. *(Measured:
the desktop compositor appears in compute-process listings holding roughly 20 MiB. It is below any
sane threshold, but it must be named explicitly so the threshold is not the only thing standing
between the user and a permanent veto.)*

**[FACT-27] — game attribution, promoted to a fact.** `gpu_busy_game` is TRUE iff at least one
process holds ≥ 512 MiB of GPU memory **and** is in the process ancestry of a running game.
`gpu_busy_other` is TRUE iff at least one process holds ≥ 512 MiB of GPU memory and is **not**
attributable to a game. A single reading of the GPU therefore produces two independent facts, and
neither is computed by subtracting inside the other. *(Measured: a lightweight 2D title holds no
compute memory at all, which led to the false conclusion that games never trigger this fact. A
GPU-heavy Proton title holds up to ~10 GiB. With one undifferentiated `gpu_busy` as a `never`
floor, the deliberate soft treatment of a running game could never take effect for exactly the
games it was written for — which is why the shipped `[while.gpu_busy_game]` carries the same
finite `suspend` as `[while.steam_game_running]`, and `[while.gpu_busy_other]` does not.)*

**[FACT-28]** Attribution MUST use process ancestry, not an executable-name allowlist. *(Rejected
alternative: maintaining a list of game executables is unbounded and fragile; the
"a game is running" signal is already computed next door.)*

**[FACT-29]** A process belonging to a tracked service (§5.3.11) MUST NEVER be attributed to a
game, even if ancestry would allow it; such a process counts towards `gpu_busy_other`. *(Measured:
the first implementation attributed a model server's 11 GiB to a running game. It opened no hole —
the service's own fact vetoed independently — but it wrote a false line into the log, and a log
that lies costs somebody an afternoon six months later.)*

**[FACT-47] — the generic source is read by the session agent, not by the privileged daemon.** The
generic, driver-independent reading of GPU memory is DRM `fdinfo` under `/proc`. A process's
`fdinfo` directory is mode `0555` and is nevertheless gated by `ptrace_may_access`, so a privileged
daemon needs `CAP_SYS_PTRACE` — the ability to read any process's memory — to read **another user's**.
The session agent MUST therefore report the holders it can see in its own session, as `(pid, process
name, bytes)` de-duplicated per DRM client id, and the daemon MUST apply [FACT-25], [FACT-26],
[FACT-27], [FACT-28] and [FACT-29] to them itself. The reported list MUST be raw: an unprivileged
process MUST NOT be the one deciding what counts as a game.

Where no agent is reporting, this source MUST contribute nothing, exactly as if no DRM device
published memory accounting, and MUST NOT yield `INDETERMINATE`. *(Measured: with
`CapabilityBoundingSet=CAP_DAC_READ_SEARCH` — the daemon's one read-only capability — reading
another user's `fdinfo` is denied, and with `CAP_SYS_PTRACE` the same read succeeds. Granting the
component that can power a machine off the ability to read any process's memory, so that video RAM
can be attributed, is the wrong trade; the agent needs no capability at all for the same data.
Doubt on an absent agent would be a second veto for a cause [HUM-4] already reports once, and would
freeze every headless machine awake — where `nvidia-smi`, which needs no session, still answers.)*

#### 5.3.11 `local_service_busy`

A long-running local service that is *up* is not a long-running local service that is *in use*.

**[FACT-30]** This fact MUST be derived from **monotonically increasing cumulative counters**
sampled between evaluations, read from `[facts.local_service_busy] counters_url`. TRUE iff a
counter advanced since the previous sample, or the service reports work currently in flight; FALSE
iff no counter has advanced for longer than `[facts.local_service_busy] idle_window`.

**[FACT-31]** Instantaneous state MUST NOT be used as the source. *(Measured: a request that
begins and ends between two samples is invisible to instantaneous state. Cumulative counters are
monotonic, so any request between samples leaves a permanent trace.)*

**[FACT-32]** "The service unit is active" MUST NOT be used as the source. *(Measured: a model
server left running "just in case" turned a start-up decision into a permanent block — the machine
never slept again all night, and roughly 12 GiB of VRAM stayed pinned, which is exactly the memory
a game needs.)*

**[FACT-33]** A health endpoint that returns OK whenever the process is alive MUST NOT be used as
the source.

**[FACT-34]** If the counters cannot be read, the fact MUST be `INDETERMINATE`. It MUST NOT be
`FALSE`. *(Measured: the counter endpoint of the service in question is disabled by default. A
permissive default would silently disable this veto for everybody who had not changed the unit —
which is everybody, on day one.)*

**[FACT-45]** A **refused connection** to `counters_url` MUST be read as FALSE, not
`INDETERMINATE`: nothing is listening on that port, so no work is in flight anywhere on it. Every
other failure — a timeout, a reply that cannot be parsed, a non-200 status — remains
`INDETERMINATE` under [FACT-34].

*(Rationale: [FACT-34] is about a service that is **up** while its counter endpoint is off, where
the daemon genuinely cannot tell whether work is in flight. A refused connection answers the
question outright. Without the distinction the fact is unusable for the very service it was written
for: a model server started on demand and stopped when idle is absent most of the time, so every
evaluation would read `INDETERMINATE` and veto every sleep action for as long as the machine is up.
The only safe configuration would then be not to configure it — and the veto it exists to provide
would never run either. Measured: with `counters_url` pointing at a stopped server, the machine held
`suspend` at `+infinity` indefinitely. Prior art in the system this was extracted from, which asked
whether the unit was active before consulting its counters, and raised no veto at all when it was
not.)*

**[FACT-35]** The first sample after daemon start MUST be treated as "in use". *(Rationale: at
first sample nothing is known about the past. The idle countdown starts then, rather than assuming
the service has been idle forever.)*

**[FACT-36]** The sample state MUST live in the runtime location class of [CLK-9]. *(Rationale:
suspending is not using the service, so the count must survive a sleep; a cold boot has no
history, so it must be cleared.)*

**[FACT-44]** This fact MUST ship **disabled**. Until `[facts.local_service_busy] counters_url` is
set, it reports `UNAVAILABLE`, its detector MUST NOT run, and it contributes no doubt veto. Setting
`counters_url` is what enables it.

*(Rationale: [FACT-34] read in the other direction. The counter endpoint of the service this fact
was written for is off by default, so a detector that shipped enabled would be unreadable — and
therefore `INDETERMINATE` — on every machine on the day it was installed. Combined with the
block-independent veto of [FACT-4], that would freeze every installation on day one. Shipping it
disabled costs one line of configuration to the small number of operators who actually run such a
service, and costs everybody else nothing.)*

### 5.4 Detector isolation

**[FACT-37]** A detector fault MUST NOT terminate the daemon. It MUST yield `INDETERMINATE` for
that fact, be logged once per transition into the faulted state (not once per evaluation), and be
surfaced by `doctor`.

**[FACT-38]** No detector may hold a lock across a blocking call that another detector needs.

### 5.5 Fact edges

**[FACT-39]** The engine MUST record the instant of each fact's most recent FALSE→TRUE edge, for
the `condition` clock (§4.1). Transitions through `INDETERMINATE` MUST NOT be treated as edges:
`TRUE → INDETERMINATE → TRUE` MUST NOT reset the `condition` origin. *(Rationale: otherwise a
flaky detector silently extends every deadline measured on that clock.)*

Only a definite `FALSE` — or `UNAVAILABLE` — clears a recorded edge.

**[FACT-39b]** Recorded edges MUST survive suspend and resume. A resume is not a FALSE→TRUE edge of
anything, and [COMP-9] forbids resetting the human-input clock "or any other clock" on resume; the
`condition` clock is such a clock. *(Rationale, and it is the same bug class as §13.3: with
`[while.gpu_busy_other] clock = "condition"` and a finite `suspend`, clearing edges on resume
re-arms a fresh countdown after every wake, so a periodically-woken machine with steady background
GPU load never reaches that deadline at all. That is an unbounded, undocumented and invisible
re-arm. Mandatory vector: [TEST-16], which MUST also be exercised across a simulated resume.)*

### 5.6 The `screen_off` exception

**[FACT-40]** Doubt MUST NOT veto `screen_off`. Specifically, neither the implicit floor of
[FACT-4] nor the configuration-error floor of [CFG-16] may raise `screen_off` to `+∞`.

**[FACT-41]** The shipped `[while.human_active]` block MUST NOT set `screen_off`. See [HUM-6].

**[FACT-42]** An *explicit* `screen_off = "never"` written by an operator in any block remains
valid and MUST be honoured. The prohibition is on **implicit, doubt-derived** infinities only.

Rationale for the whole of §5.6: the invariant of [MODEL-1] does not hold for the panel. A
suspend that happens wrongly costs an interrupted session, which is bad but bounded and
recoverable. A static image left on an OLED panel for hours causes cumulative, permanent damage,
and `screen_off` is undone by any keypress. One unreadable detector must not be able to burn a
display.

---

## 6. `[when]` ceilings

A `[while]` block answers "how long must the machine wait before it may do this". A `[when]` block
answers "how long may the machine refuse to do this".

**[CEIL-1]** `[when]` blocks MUST compose by MIN over deadlines, per action, independently.

**[CEIL-2]** A `never` timeout in a `[when]` block yields `+∞`, which is a no-op under MIN. It
MUST NOT be an error.

**[CEIL-3]** There is **one ceiling class** in v1. Every `[when]` block is absolute. There is no
`hard` key: `hard` is not part of the configuration format, MUST NOT be accepted by a parser, and
MUST NOT appear in any block table.

`resolved(a) ≤ floor(a)` for every action and every configuration: a ceiling can only ever pull an
action **earlier** and MUST NOT be able to delay one, because MIN over instants cannot produce a
later instant than either operand. A `[when]` block is never a way to keep a machine awake, and
[TEST-6b] MUST hold.

*(Rationale for one class rather than two. Under a two-class model the default — soft — ceiling
silently does nothing against a `never` floor, so the man page's own `[when.lease_held] suspend =
"12h"` example is a dead letter that reports success. A silent no-op is exactly what [OBS-6]
forbids. One loud absolute class is cheaper, and it keeps the only construct that can act against a
floor impossible to acquire by accident.)*

**[CEIL-4]** A ceiling defeats **everything a floor can raise**: `never` floors, including
`[while.human_active]`; the implicit doubt floor of [FACT-4]; and the configuration-fault floor of
[CFG-16]. There is exactly one override channel in the system and this is it. `--force` (§9.3) is
this requirement in imperative form.

**[CEIL-5]** WITHDRAWN — `hard` no longer exists, so there is nothing for a per-block-versus-per-action
rule to govern. See [CEIL-3].

**[CEIL-6]** Every `[when]` block present in the effective configuration MUST be logged at warning
level when the configuration is loaded, naming the file it came from.

**[CEIL-7]** `doctor` MUST list every `[when]` block as a **standing hazard**, in a section that is
present whenever any such block exists and absent otherwise.

**[CEIL-8]** When a ceiling causes an action to fire, the log record MUST name the ceiling block,
its file of origin, and the complete list of floors it defeated, including the reason each floor
was raised — implicit floors included, named as such.

Rationale for [CEIL-6] through [CEIL-8]: a ceiling is the only construct in the system that can
suspend a machine with somebody typing on it. A capability of that magnitude that operates silently
is indistinguishable from a bug, and it is the exact shape of the failure this project was built to
eliminate. It must be impossible to have one and not know.

**[CEIL-10]** A ceiling MAY be set on any of the four actions. An implementation MUST NOT restrict
ceilings to `screen_off`, and MUST NOT reject a ceiling on `suspend`, `hibernate` or `poweroff` as
a configuration error. *(Rationale: such a restriction would make `--force` — which is an ephemeral
`suspend`/`hibernate`/`poweroff` ceiling — inexpressible in the model it is defined by, and it
would delete §12.6 and the man page's own worked example. The protection against a careless ceiling
is [CEIL-6]–[CEIL-8], not a prohibition.)*

**[CEIL-9]** Ceilings MUST NOT be used to implement the base schedule. The base schedule is a
floor (`[while.always]`), because a floor with no request behind it means "the machine may do this
on its own", while a ceiling means "the machine must do this whatever anyone says". *(Rationale:
inverting these makes every safety floor advisory.)*

---

## 7. The `human_active` floor

### 7.1 Why it is a first-class fact

**[HUM-1]** `human_active` MUST exist as a first-class fact and MUST NOT be modelled only as the
clock that other timeouts are measured on.

Rationale: measuring the base schedule on the human-input clock keeps the machine awake while
somebody is typing **only for the base schedule**. It provides no protection at all against a
request or an override that bypasses the base schedule — and §10 defines exactly such a request.
Without this floor, a remote relay could suspend a machine mid-keystroke. The floor is load-bearing
precisely in the case where the schedule has been collapsed.

### 7.2 Definition

**[HUM-2]** `human_active` is TRUE iff `human_idle < min_idle`, where `human_idle = now −
origin(human_input)` and `min_idle` is `[general] min_idle`, default 5 minutes.

**[HUM-3]** When the human-input clock has **never been touched this boot**, `human_active` MUST
be FALSE. *(Rationale: preserves the remote relay fast path of [CLK-7]. A machine nobody has ever
touched has no human on it, and must be allowed to finish its job and go back to sleep.)*

**[HUM-4]** When the human-input clock is **unreadable** — the session agent is not running, its
heartbeat is stale, or the idle protocol errors — `human_active` MUST be `INDETERMINATE`.

It MUST NOT be reported as TRUE: the detector did not observe a human, and reporting an observation
that was not made collapses doubt into knowledge, which [FACT-1] and [FACT-2] forbid end to end.
The veto is delivered instead by [FACT-4], which is block-independent and therefore raises
`suspend`, `hibernate` and `poweroff` to `+∞` with exactly the same force as a `never` floor.

*(Rationale, four reasons cutting the same way. (1) Because the doubt veto no longer depends on a
block existing, `INDETERMINATE` fails closed just as hard as TRUE would, so the safety argument for
TRUE evaporates. (2) TRUE would let doubt **cause** an action: an administrator's
`[when.human_active] suspend = "1h"` would fire on a machine whose agent is dead, violating
[FACT-4b]. Under `INDETERMINATE` that ceiling contributes nothing. (3) [OBS-4] already requires
`doctor` to exit non-zero when any fact is `INDETERMINATE`; under TRUE a dead agent produces no
`INDETERMINATE` fact anywhere and both the exit status and the fault line would need bespoke rules.
(4) It makes [HUM-5]'s three cases three distinct fact **states** rather than two flavours of TRUE,
so "this machine is awake forever because its agent died" is diagnosable from the fact table
alone.)*

**[HUM-5]** The three cases below MUST be distinguishable in `explain` and `doctor` output. A
machine that is permanently awake because its idle agent died MUST be diagnosable in one command.

| Case | Situation | `human_active` |
|---|---|---|
| 1 | A human touched an input device within `min_idle` ([HUM-2]) | `TRUE` |
| 2 | Never touched this boot; input path healthy ([HUM-3], origin = boot per [CLK-7]) | `FALSE` |
| 3 | Idle clock unreadable: agent absent, heartbeat stale, protocol erroring ([HUM-4]) | `INDETERMINATE` |

*(Rationale: [OBS-6] — fail loudly, never silently. Case 3 is indistinguishable from case 1 to the
user, and lasts forever.)*

### 7.3 The shipped block

**[HUM-6]** The shipped defaults MUST include:

```toml
[while.human_active]
suspend   = "never"
hibernate = "never"
poweroff  = "never"
```

and MUST NOT set `screen_off` in this block.

Rationale for the omission: `screen_off` needs no help from this block. It is already measured on
`human_input` in the base schedule, so real input pushes it back continuously, while a dead agent
lets it fire. Setting `screen_off = "never"` here would buy nothing and would cost something real —
a lit panel with nothing counting down against it, which is the OLED burn of [FACT-40] arriving by
another route. Under [COMP-2b] the omission is exactly the right instrument: silence about
`screen_off` is not `never` about `screen_off`.

**[HUM-7]** WITHDRAWN — `collapsible` does not exist in v1 (see [REQ-3]), so there is no way for an
operator to make the human-presence floor satisfiable by a request. The set of blocks `rest --now`
satisfies is fixed by condition name and cannot be enlarged by configuration. The hazard this
requirement guarded against is now unreachable rather than merely reported.

---

## 8. Actions and execution

### 8.1 Deciding is not doing

**[ACT-1]** The daemon MUST make the decision itself and MUST NOT delegate any part of it to a
tool that re-decides. In particular it MUST NOT invoke a shutdown helper in a mode that consults
inhibitors or logged-in sessions, and MUST NOT interpret such a helper's exit status as policy.

*(Measured, both directions: a plain shutdown invocation without a controlling terminal returns
success and ignores inhibitors and sessions entirely — so it is no safety net at all. The variant
that does check refuses whenever any user is logged in, which on a console with autologin is
always — so it would never act. There is no usable middle setting. The decision has to be made
here, and only the mechanical transition delegated.)*

### 8.2 Single transition in flight

**[ACT-2]** The daemon MUST hold a single-transition guard. At most one power transition may be
in flight at a time.

**[ACT-3]** A refusal from the system's sleep mechanism (for example, "an operation of this type
is already in progress") MUST be logged at warning level. It MUST NOT be silently ignored, and it
MUST NOT be retried immediately.

*(Measured: the incident of [TEST-2] did not actually suspend the machine — but only because the
previous suspend was still finishing and the request was refused. The user noticed nothing. One
tick two seconds later and the machine would have slept in their face. An intermittent fault masked
by luck is worse than a reproducible one, because nobody fixes it.)*

### 8.3 Re-evaluation before acting

**[ACT-4]** Between deciding that `now ≥ resolved(a)` and issuing the transition, the daemon MUST
re-read all facts and recompute `resolved(a)`. If the action is no longer permitted, it MUST NOT
be issued.

**[ACT-5]** After every resume, the daemon MUST recompute all origins before any action may fire
([COMP-8]).

### 8.4 Action independence and ordering

**[ACT-6]** The four actions MUST be evaluated independently. `screen_off` firing MUST NOT imply
anything about `suspend`, and `suspend` being vetoed MUST NOT veto `screen_off`.

**[ACT-7]** `screen_off` is not a change of the system's power state. It MAY be applied in the same
evaluation as a sleep action, and its being applied MUST NOT suppress any other action ([ACT-6]).

Among `suspend`, `hibernate` and `poweroff`, at most **one** MAY be applied per evaluation, and it
MUST be the **shallowest** one that is due: `suspend` before `hibernate` before `poweroff`.

A deeper action is never a safe substitute for a shallower one. Suspend loses nothing; poweroff ends
every session on the machine and, on the hardware this policy was extracted from, hung roughly one
time in four, leaving a box that only a physical trip could revive. *(This is [MODEL-1] applied to
the choice between two permitted actions.)*

An action already in effect MUST NOT be re-issued until its own deadline has been re-armed by a new
origin. A machine whose panel is already blank does not re-fire `screen_off`, and therefore never
starves the sleep actions.

**[ACT-7b] — idlectl does not escalate.** This is a consequence of [ACT-7] and MUST be documented
rather than discovered. With `suspend = "30m"` and `poweroff = "8h"` both eventually due, the
machine suspends and never powers itself off. An operator who wants escalation has three supported
routes: set `suspend = "never"` and let the deeper action own the schedule; delegate to `logind`'s
own suspend-then-hibernate, which is mechanism rather than policy (§1.1) and which idlectl does not
fight; or ask for the deep action explicitly with `idlectl rest --action poweroff`. Mandatory
vector: [TEST-24].

**[ACT-7c] — a requested action is never traded for a shallower one.** When an action has been
asked for **by name**, that action is the only one that may be performed in that evaluation. If it
is held, nothing is performed. The shallowest-wins rule of [ACT-7] governs the schedule acting on
its own, not a request.

*(Rationale: `--now` makes the base schedule contribute `now` for every action it names, so a
shallower action is due at the very same instant as the deeper one that was asked for. Under
shallowest-wins, `idlectl rest --action poweroff` therefore suspends the machine and reports the
request unsatisfied — the caller is told "no" while the machine goes to sleep, a relay records it as
powered off, and nothing anywhere says otherwise. [ACT-7b] names this command as a supported route
to a deeper action than the schedule would ever pick, and this rule is what makes that true.
Refusing rather than downgrading is the same principle read backwards: if the action somebody named
is held, the answer is "no", not a different action they did not ask for. Mandatory vector:
[TEST-26].)*

### 8.5 The unit must not be conditionally skipped

**[ACT-8]** The service unit shipping `idlepolicyd` MUST NOT carry `Condition*` or `Assert*`
directives that gate on runtime policy state.

*(Measured: a unit in the prior system carried a `ConditionPathExists` on a pending-request file.
When autonomous rest was added, the new code path was correct, installed, visible in status
output — and unreachable, because the unit was skipped whenever no request was pending. The
journal contained no error at all, only "skipped, unmet condition check". A perfectly written
feature was dead code and nothing said so. General lesson: when adding a capability, audit the
guards **above** the new code, not only the new code.)*

**[ACT-9]** `doctor` MUST report the timestamp of the last completed evaluation. *(Rationale: this
is the check that catches [ACT-8]-class failures from the outside.)*

### 8.6 How `screen_off` is performed

`logind` has no screen-off mechanism, and on a Wayland session only the compositor — or whoever
holds DRM master — can blank an output. The action on which the entire OLED carve-out hangs
therefore needs a named mechanism of its own.

**[ACT-12]** In v1, `screen_off` MUST be performed by the **session agent**. The agent exposes
`Blank` and `Unblank` on the system bus, callable **only by uid 0**, and the daemon invokes them.
The agent is the only process that can address the compositor or hold DRM master, so the daemon
cannot perform this action itself.

This is the one and only exception to the rule that the agent reports facts and commands nothing:
the agent commands nothing but the screen, only inside its own session, and only when the caller is
the daemon. Documentation that states the general rule MUST state this exception alongside it.

**[ACT-13]** Where no agent is present, or the session type offers no blanking mechanism,
`screen_off` MUST report `UNAVAILABLE` **as an action**. The daemon MUST NOT propose it, every
block setting a `screen_off` key MUST be listed by `doctor` as inert, and `doctor` MUST say which
blocks those are ([OBS-3].12). An action reported as unavailable is knowledge, not doubt: it MUST
NOT raise a doubt veto on anything, by [FACT-43] applied to actions.

### 8.7 Privilege and state

**[ACT-10]** The daemon runs privileged and reads state written by unprivileged callers (leases,
agent heartbeats, the GPU memory holders of [FACT-47]). All such state MUST be parsed with an
explicit parser, MUST be range- and type-checked, MUST be bounded in length, and MUST NOT be
executed, sourced or interpolated into a shell.

None of it can make the machine sleep. A holder an agent reports only ever adds a reason to stay
awake, so the worst a hostile agent achieves with it is a machine that stays on — the cheap error of
[MODEL-1] — and the length bound is what stops it also being an unbounded allocation inside the
daemon.

**[ACT-11]** Runtime state directories MUST be created with explicit ownership and mode by a
declarative mechanism (on systemd: `tmpfiles.d`), not lazily by whichever process gets there first.

---

## 9. Requests

A **request** is an external ask that the machine rest now, distinct from the machine's own
schedule.

### 9.1 The architectural statement

> The base schedule is what the machine does **on its own**.
> A request is what somebody else may **ask for**.
> The safety floors apply to **both**.

**[REQ-1]** An implementation MUST preserve this separation. Removing it — by letting a request
bypass the floors, or by making the schedule unrequestable — collapses the design.

### 9.2 `rest --now`

**[REQ-2]** `idlectl rest --now` MUST NOT collapse condition blocks. It MUST satisfy **only** the
base schedule and the resume settle window; every other block MUST be evaluated normally on its
own clock.

**[REQ-3]** Concretely: a `rest --now` request for action `a` is resolved by evaluating the normal
formula of §3.2 with exactly **two** `[while]` blocks treated as satisfied — the block whose
condition is `always` and the block whose condition is `after_resume` — meaning each of those two
contributes `now` for `a` instead of its configured deadline, including when its configured value
is `never`. Every other block, every ceiling and every implicit floor is evaluated unchanged.

The set is fixed **by condition name**. There is no per-block key, and an administrator cannot
enlarge it.

**[REQ-4]** WITHDRAWN — superseded by the name-based rule of [REQ-3]. There is no `collapsible`
key in v1: it is not part of the configuration format, MUST NOT be accepted by a parser, and MUST
NOT appear in any block table.

*(Rationale for deleting it: a name-based rule is strictly cheaper — no config surface, nothing to
get wrong, nothing to report — and it removes the hazard that an operator could make the
human-presence floor satisfiable by a request by typing one word. See [HUM-7], WITHDRAWN.)*

Rationale for [REQ-2] and [REQ-3]: without this, a remote relay's request to rest suspends a
running game. That is a regression against the shipped system this design was extracted from, and
it is the exact class of failure that motivated the project. It is also what makes `poweroff =
"never"` in the base schedule the right default: the machine never powers itself off, but an
authenticated request may ask it to, and the safety floors still hold.

**[REQ-5]** A request MUST carry a requester identity and MUST be logged with it.

**[REQ-6]** A request that cannot fire immediately MAY be held pending, with a TTL. While pending,
it MUST be re-evaluated on the schedule of [COMP-6].

*(Implemented. A held request is opt-in and additive: the bare `rest` still asks exactly once, and
`rest --now` is still the same command as `rest`. What holds a request is a separate flag carrying a
TTL, and it changes **how long the machine keeps trying**, never **which floors are satisfied** —
the one drift this section cannot tolerate. Every retry evaluates the identical two satisfied blocks
the original did, so a veto that arrives after the request refuses it exactly as one already present
would have. What is remembered is the asking, never the answer.*

*The state is held in memory rather than in a file, and that is a decision rather than an omission.
The system this was extracted from kept its pending request in `/run` because it was not a daemon at
all: a timer ran a script every five minutes, so all of its state had to outlive the process. A
daemon has somewhere better to put it, and the alternative — a file that survives a restart — also
survives a change of mind. What is lost is a held request across an upgrade of the daemon, and the
direction of that loss is the safe one: the machine stays awake and somebody has to ask again.)*

**[REQ-7]** A pending request MUST be discarded if the human-input clock advances after the
request was made. *(Rationale: somebody walked in while the machine was working; the machine now
belongs to whoever is in front of it. The discard MUST be logged, so that a cancelled request is
always evidence that a real human arrived.)*

**[REQ-8]** A resume MUST NOT count as human input for the purposes of [REQ-7]. *(Measured: with
a resume marker four seconds old, the human-input clock still read 461 s and the idle state
survived — the idle protocol only reports real input. A machine woken by a relay therefore does
not cancel its own rest request.)*

### 9.3 `--force`

**[REQ-9]** `--force` MUST be a separate, explicit flag. It MUST NOT be implied by `--now`, by any
verbosity or non-interactive flag, or by any configuration setting.

**[REQ-10]** `--force` installs an **ephemeral `[when]` ceiling** for the requested action, with
deadline = `now`, for the lifetime of that one request. Because ceilings compose by MIN and are
absolute ([CEIL-4]), `resolved(action) = now`, and `--force` therefore defeats — mechanically and
without special cases:

- every `[while]` floor, including `never` and including `[while.human_active]`;
- the implicit doubt floor of [FACT-4];
- the configuration-fault floor of [CFG-16];
- the base schedule.

It is [CEIL-4] in imperative form, expressed in exactly the vocabulary of [REQ-3]: `--now`
satisfies two floors, `--force` defeats all of them.

*(Rationale for stating it as one mechanism rather than as an enumeration of blocks: the single
most likely real use of `--force` is a machine wedged awake by a dead detector or a bad config
value, and neither the doubt floor nor the configuration-fault floor is a block. A rule phrased as
"collapses all `[while]` blocks" leaves both standing and cannot do the one thing `--force` is
reached for. Mandatory vector: [TEST-22].)*

**[REQ-11]** A forced action MUST be logged at warning level, naming the requester identity and
enumerating every floor it defeated with that floor's reason — implicit floors included, named as
such.

**[REQ-12]** `doctor` MUST report the count and timestamps of forced actions since boot.

**[REQ-13]** `--force` defeats **policy** only, never **mechanism**. [ACT-2] (single transition in
flight), [ACT-3] (a refusal from the sleep mechanism is logged and not retried immediately) and
[ACT-4] (re-read facts and recompute before issuing) all still apply. [ACT-4]'s recomputation
carries the same ephemeral ceiling, so a forced action does not cancel itself.

### 9.4 Authorization separation

**[REQ-14]** Requesting a rest and forcing one MUST be **separate authorization actions**. An
implementation MUST expose at minimum:

| Polkit action id | Grants |
|---|---|
| `io.github.ericcanas.Idlectl1.rest` | Submit a rest request; floors apply. |
| `io.github.ericcanas.Idlectl1.rest-forced` | Submit a forced action; every floor is defeated ([REQ-10]). |
| `io.github.ericcanas.Idlectl1.lease` | Acquire, renew or release a lease ([FACT-13]). |
| `io.github.ericcanas.Idlectl1.reload` | Reload the effective configuration ([CFG-18]). |

These four ids are the complete set for v1. They MUST appear verbatim, and identically, in the
polkit policy file, in the D-Bus introspection annotations and here, so that a cross-file
identifier check can be automated. The `rest-forced` spelling is frozen: it pairs visibly with
`.rest` in a rules file.

**[REQ-15]** A credential that can request MUST NOT thereby be able to force.

Rationale: this is the difference between an automation credential and root. *(Measured: in the
prior system a remote relay held a command allowlist that included a raw power-off. Replacing it
with a request-only entry meant that stealing the automation credential could no longer end a game
in progress. The separation is worth more than the code it costs.)*

**[REQ-16]** The daemon MUST own the bus name `io.github.ericcanas.Idlectl1` and MUST expose its
manager interface under that namespace. The absence of a hyphen and the lower case are deliberate
and permanent: the D-Bus specification restricts interface name elements to `[A-Za-z0-9_]`, so the
project's forge handle cannot appear verbatim. This name appears in the bus policy filename, the
polkit action ids, the introspection XML and every client; it MUST NOT be changed.

### 9.5 Which action `rest` resolves to

**[REQ-17]** `idlectl rest` with no `--action` MUST resolve to `suspend`, **always**. This is a
fixed constant in v1: there is no `[rest]` table and no configurable default rest action.

**[REQ-18]** WITHDRAWN — the after-resume substitution has no configuration to substitute for.

*(Measured, and the measurement is preserved by [REQ-17] rather than discarded: a boot containing
a session somebody left suspended must not have that session closed on their behalf. Powering off
closes it; re-suspending returns the machine to exactly the state it was left in, preserving both
the session and the energy saving. With `suspend` as the only implicit rest action and `poweroff`
requiring an explicit `--action`, no request can ever close a session unless its sender typed the
word. That serves the measured intent completely, with one fewer key and one fewer conditional.)*

---

## 10. Configuration

### 10.1 Format

**[CFG-1]** Configuration MUST be TOML.

**[CFG-2]** The daemon only **reads** configuration. Comment-preserving round-trip is NOT a
requirement of the daemon. Any future configuration *editor* is a separate component and its needs
MUST NOT be used to justify constraints on the daemon.

**[CFG-27] — the frozen table set.** The top-level tables of v1 are exactly:

| Table | Contents |
|---|---|
| `version` | Optional scalar, default `1` ([CFG-21]). |
| `[general]` | `min_idle` only. |
| `[facts.<name>]` | Per-fact settings. `<name>` MUST be one of the eleven facts of §5.3. |
| `[while.<condition>]` | A floor. Keys: `enabled`, `clock`, `screen_off`, `suspend`, `hibernate`, `poweroff`. |
| `[when.<condition>]` | A ceiling. Same key set. |

Nothing else is legal at the top level. In particular v1 has **no** `[rest]` table ([REQ-17]),
**no** `[facts] min_idle` (it is `[general] min_idle`) and **no** `gpu_min_mem` anywhere (it is the
compiled-in constant of [FACT-25]).

`[facts.<name>]` carries `enabled` for every fact, plus a small per-fact key set **validated by
name**:

| Fact | Additional keys |
|---|---|
| `local_service_busy` | `counters_url`, `idle_window` |
| `steam_game_running`, `steam_downloading` | `steam_root` |
| `steam_downloading` | `window` |

A key that is not defined for that particular fact MUST be a configuration error under [CFG-16],
and so MUST an unknown fact name ([CFG-20]). *(Rationale: `[facts.media_playing] counters_url` is
either a typo or a misunderstanding; in neither case is it a policy the operator should be allowed
to believe is in force — [CFG-19].)*

### 10.2 File locations and precedence

**[CFG-3]** The effective configuration is assembled from the following layers, in increasing
precedence. Every layer is optional except the first, which the package always installs. **One
filename throughout: `idlectl.toml`.**

| Order | Path | Owner |
|---|---|---|
| 1 | `/usr/lib/idlectl/idlectl.toml` | the package — vendor defaults |
| 2 | `/etc/idlectl/idlectl.toml` | the administrator |
| 3 | `/etc/idlectl/conf.d/*.toml` | the administrator, applied in ascending byte-wise order of basename, each later file overriding earlier ones |

**[CFG-4]** Drop-in files SHOULD be named `NN-description.toml` and are compared by **basename**,
byte-wise. Drop-ins are applied **after** `/etc/idlectl/idlectl.toml`, so a drop-in wins over it —
the systemd drop-in contract, which administrators already know.

**[CFG-5]** WITHDRAWN — v1 has no `/usr/lib/idlectl/conf.d`, so there is no vendor drop-in to
shadow.

**[CFG-6]** WITHDRAWN — with no vendor drop-in directory there is nothing for a zero-byte file to
neutralise. A vendor **block** is switched off with `enabled = false` ([CFG-11]) and a vendor
**fact** with `[facts.<name>] enabled = false` ([CFG-12]); `doctor` reports both.

**[CFG-7]** `/usr/lib/idlectl/` MUST be treated as package-owned. The implementation MUST NOT
write there and documentation MUST NOT instruct anyone to edit it.

**[CFG-26]** The vendor file is **layer 1 of the resolution chain**, not an example: it is read on
every start, and a machine with an empty `/etc` therefore has a complete, safe policy. A commented
example configuration MAY additionally be shipped for humans to copy (conventionally
`/usr/share/idlectl/idlectl.example.toml`). If one is shipped it MUST NOT be part of the resolution
chain of [CFG-3], and the daemon MUST NOT read it.

*(Rationale: an example file that is also the source of default behaviour makes "did you copy the
example yet?" a load-bearing question, and the answer on day one is always no. A machine with no
policy is a machine with no owner of its power state, which is the condition this project exists to
remove.)*

### 10.3 Merge semantics

**[CFG-8]** Merging is **per key within a block table**. A later file setting `suspend` in
`[while.always]` replaces only that key; the block's other keys survive.

**[CFG-9]** There is at most one `[while.<condition>]` table and at most one
`[when.<condition>]` table per condition in the effective configuration. Occurrences across files
merge per [CFG-8].

**[CFG-10]** A file that fails to parse MUST be rejected **as a whole**. Partial application of a
file MUST NOT occur.

### 10.4 Disabling things

**[CFG-11]** A block is disabled by `enabled = false` within the block. A disabled block MUST
contribute nothing and MUST be listed as disabled by `doctor`.

**[CFG-12]** A fact is disabled by `[facts.<name>] enabled = false`, with the consequences of
[FACT-6]: it reports `UNAVAILABLE`, its detector does not run, it contributes no doubt veto, and
`doctor` lists it. This is the supported, auditable escape hatch from a permanently `INDETERMINATE`
detector, and it is the third of the three disciplines that make the block-independent veto of
[FACT-4] safe to ship ([FACT-43], [FACT-44]).

**[CFG-13]** Deleting keys from a merged block is NOT supported. Neutralise a timeout by setting
it explicitly (`"never"` or a duration), or disable the block.

### 10.5 Duration syntax

**[CFG-14]** A timeout MUST be one of:

- the exact, **lower-case** string `"never"`, meaning `+∞`. Case-insensitive matching MUST NOT be
  used: every other name in this format is lower-case, and a case-insensitive island is a dialect
  nobody asked for;
- a duration: one or more `<integer><unit>` pairs, units `s`, `m`, `h`, `d`, e.g. `"90s"`, `"30m"`,
  `"2h"`, `"1h30m"`.

Whitespace and underscores **inside** a duration are accepted and ignored: `"1h 30m"` and
`"1h_30m"` are both `"1h30m"`. *(They are harmless, and rejecting them would contradict the man
page and the shipped parser for no gain.)*

**[CFG-15]** A **bare integer MUST be rejected** as a configuration error, `0` included: write
`"0s"`, which is valid and means "immediately". *(Measured: an unqualified number means seconds to
one implementer and minutes to another, and the failure is silent and off by 60×. Requiring a unit
removes the class; exempting `0` reintroduces "sometimes a number is fine" and with it the habit
that produces `suspend = 1800`.)*

Negative values, fractional values and unknown units MUST all be rejected.

**[CFG-15b]** A timeout given as a TOML **integer** rather than a string MUST produce the
missing-unit error of [CFG-15] — not a type error. `suspend = 30` MUST be reported as "missing
unit: write `30s` or `30m`". *(Rationale: the operator's mistake is the missing unit; a message
about types sends them to look at the wrong thing, which is [OBS-2] applied to configuration
errors.)*

**[CFG-25]** WITHDRAWN — no byte-size value remains in the configuration format. The GPU memory
threshold is the compiled-in constant of [FACT-25], so there is nothing for a byte-size grammar to
apply to.

### 10.6 Invalid configuration is a veto, not a crash

**[CFG-16]** An invalid configuration value MUST NOT terminate the daemon and MUST NOT prevent
evaluation. Loading MUST therefore be a **degraded-mode** operation, not an all-or-nothing one: it
yields the configuration that could be assembled **together with a list of faults**, never an
error in place of a configuration. The implementation MUST:

1. log the offending key, its file, and the offending text, at error level;
2. add an implicit `+∞` floor to `suspend`, `hibernate` and `poweroff` for as long as any fault
   persists;
3. **not** raise `screen_off` ([FACT-40], [FACT-5]);
4. surface the fault in `doctor` with a non-zero exit status;
5. drop the offending **file as a whole** ([CFG-10]) while keeping the remaining layers, and
   continue evaluating with what survived.

**[CFG-28]** If the **vendor layer itself** is unusable, the daemon MUST apply a compiled-in
fallback of exactly

```toml
[while.always]
clock      = "human_input"
screen_off = "15m"
```

and nothing else, and MUST report the fallback in `doctor` as a fault. *(Rationale: this is the one
case "keep the previous configuration" ([CFG-18]) cannot cover — on a **first start** after a bad
edit, or on a broken package, there is no previous configuration to keep. Every sleep action is
already `+∞` by [CFG-16].2, so the fallback's only job is to make sure a broken package cannot
leave a panel lit indefinitely.)*

*(Measured: in the prior system a single non-numeric value in the configuration file killed the
whole decision script before it evaluated a single condition or wrote a single log line — the
shell's arithmetic evaluation treated the text as a variable name under `set -u`. It ran from a
timer every five minutes, so one typo left both rest and shutdown dead in complete silence, which
is the exact opposite of what the program promised. It affected all thirteen numeric keys, not
just the newly added ones.)*

**[CFG-17]** `idlectl check-config` MUST validate the effective configuration without touching the
running system and MUST exit non-zero on any error. (The subcommand is spelled `check-config` in
the CLI, in both man pages and in the vendor file's header.)

**[CFG-18]** Configuration reload MUST be atomic: either the new configuration becomes effective
in full, or the previous one remains effective in full. A failed reload MUST keep the previous
configuration, log the failure, and mark it in `doctor`.

### 10.7 Unknown keys

**[CFG-19]** An unknown key MUST be a configuration error under [CFG-16], not a silent ignore.
*(Rationale: a silently ignored key is a policy the operator believes is in force and is not. That
is the failure mode of [ACT-8] arriving through the configuration file instead of the unit.)*

**[CFG-20]** An unknown `[while.<condition>]` or `[when.<condition>]`, where `<condition>` is
neither one of the eleven facts of §5.3 nor the built-in `always`, MUST be a configuration error,
not an inert block. The same applies to an unknown `[facts.<name>]`. `UNAVAILABLE` is for facts
that exist and are inapplicable; a typo is not that.

### 10.8 Versioning and breaking changes

**[CFG-21]** The top-level key `version` is OPTIONAL and defaults to `1`. An implementation MUST
refuse, with a clear message, a configuration declaring a major version it does not implement.

**[CFG-22]** The following are **BREAKING CHANGES** to the configuration format and MUST NOT occur
outside a major version bump:

1. renaming or removing an action, a fact, a condition, a clock or a key;
2. changing the meaning of an existing key;
3. changing the composition formula of §3.2, [CEIL-3] or [CEIL-4] in any way;
4. changing the merge order or precedence of §10.2;
5. changing the duration grammar of [CFG-14] to reject anything it previously accepted;
6. changing a shipped default such that a machine that previously did not act now **acts**, or
   acts **sooner** — including making a `never` finite, shortening any timeout, or narrowing any
   safety floor.

**[CFG-23]** The following are **NOT** breaking changes:

1. adding a new fact, a new condition, or a new key that does not alter the meaning of any
   existing configuration (adding an **action** or a **clock** is breaking — see [MODEL-4] and
   [CLK-1]);
2. adding a new shipped `[while]` block that only lengthens deadlines;
3. changing a shipped default such that the machine acts **later** or **not at all**;
4. adding a new `INDETERMINATE` source, which can only lengthen deadlines by [FACT-4] — though it
   MUST still be announced in release notes, because a machine that stops sleeping is a support
   burden even when it is safe.

**[CFG-24]** The asymmetry between [CFG-22].6 and [CFG-23].3 is deliberate and MUST be preserved:
it is [MODEL-1] applied to release engineering. Making a machine sleep sooner is a behaviour change
that can lose somebody's work; making it sleep later cannot.

---

## 11. Observability

**[OBS-1]** `idlectl explain [ACTION]` MUST print, for each action (or the named action):

- every block, `[while]` and `[when]`, including disabled and inert ones;
- each block's condition and that condition's current state
  (`TRUE`/`FALSE`/`INDETERMINATE`/`UNAVAILABLE`), with the reason for the latter two;
- each block's clock, that clock's origin as an absolute instant and as an age;
- each block's configured timeout and computed deadline, and — for a block that sets no key for the
  action being explained — that it **does not participate** in that action ([COMP-2b]);
- the resulting `floor`, `ceiling` and `resolved` instants, with every implicit floor in force
  listed by name and reason ([FACT-4], [CFG-16]);
- which block won, by name;
- the time remaining until `resolved`, or that it is `never`.

**[OBS-2]** The numbers `explain` reports MUST be the numbers the decision actually used. An
implementation MUST NOT report a cheaper approximation alongside a decision made on a different
value. *(Measured: see [FACT-23] — a correct decision reported with an approximate number sends
the reader looking in the wrong place. The 10 ms saved is not worth it.)*

**[OBS-3]** `idlectl doctor` MUST report at minimum:

1. every fact with its current state, and for `INDETERMINATE` the specific fault;
2. every `UNAVAILABLE` fact with the capability that is missing, and every block rendered inert
   by it;
3. every disabled fact and every disabled block ([CFG-11], [CFG-12]);
4. every `[when]` ceiling, as a standing hazard ([CEIL-7]);
5. WITHDRAWN — there is no `collapsible` key to report ([REQ-4], [HUM-7]);
6. the count and timestamps of forced actions since boot ([REQ-12]);
7. the timestamp of the last completed evaluation ([ACT-9]);
8. the configuration files that were read, in effective order, and any that were disabled;
9. any configuration fault currently vetoing ([CFG-16]), including a fallback to [CFG-28];
10. which of the three `human_active` cases is currently in force ([HUM-5]);
11. **the conflict scan** — any other candidate owner of the machine's power state: `logind`'s
    configured `IdleAction` and `IdleActionSec`; any process currently owning a known desktop
    power-management bus name; and any running idle-helper process it can detect (`swayidle`,
    `hypridle`, `xautolock`). Each MUST be named as a potential second owner of power. *(This is
    cheap — one config read, one bus-name list, one process scan — and single-owner-of-power is the
    conclusion §13.3 says matters more than the bug that produced it. A release protocol that gates
    on this check tests nothing unless the check is required.)*
12. whether `screen_off` is available as an action, and if not, every block whose `screen_off` key
    is therefore inert ([ACT-13]).

**[OBS-4]** `doctor` MUST exit non-zero if any configuration fault, any `INDETERMINATE` fact, or
any standing hazard is present.

**[OBS-5]** Every action that fires MUST produce exactly one log record naming: the action, the
resolved instant, the winning block, the clock and origin it was measured on, and whether the
action was autonomous, requested, or forced.

**[OBS-6] — fail loudly, never silently.** Any condition that prevents the engine from doing what
its configuration says MUST produce a log record and MUST be visible in `doctor`. A silent no-op
is a conformance failure. *(Measured: three separate incidents in the prior system — a unit skipped
by a `Condition*`, a configuration parse that killed the decider before it logged anything, and a
desktop power daemon whose suspend timer simply never fired while it kept reporting healthy —
shared exactly one property: nothing said anything was wrong.)*

**[OBS-7]** Repeated identical fault records MUST be rate-limited to one per transition into the
fault state, not one per evaluation.

---

## 12. Worked examples

Throughout, `t` is the current instant. All origins and deadlines are on `CLOCK_BOOTTIME`.

### 12.1 The two-block example

Configuration:

```toml
[while.always]
suspend = "30m"          # clock defaults to human_input

[while.after_resume]
clock   = "resume"
suspend = "5m"
```

State: the machine has been idle for 12 minutes; the last resume was 40 minutes ago.

| Block | True? | Clock | Origin | Timeout | Deadline |
|---|---|---|---|---|---|
| `while.always` | yes | `human_input` | `t − 12m` | 30m | `t + 18m` |
| `while.after_resume` | yes | `resume` | `t − 40m` | 5m | `t − 35m` |

`floor(suspend) = MAX(t + 18m, t − 35m) = t + 18m`. No ceilings. `resolved = t + 18m`.

The machine suspends 18 minutes from now if nothing changes. The resume block is true but has long
since expired, and MAX discards it — this is [COMP-5].

### 12.2 The normative test: 9 h of idle, resumed 0 seconds ago

State: the machine accumulated 9 hours of human idle overnight and resumed **0 seconds ago**.
Same configuration as §12.1.

**Correct — composition over instants:**

| Block | Clock | Origin | Timeout | Deadline |
|---|---|---|---|---|
| `while.always` | `human_input` | `t − 9h` | 30m | `t − 8h30m` |
| `while.after_resume` | `resume` | `t` | 5m | `t + 5m` |

`floor(suspend) = MAX(t − 8h30m, t + 5m) = t + 5m`. The machine stays awake for five minutes.

**Wrong — composition over durations:**

`max(30m, 5m) = 30m`; observed idle `9h ≥ 30m`; fire **now**.

This is not a hypothetical. It reproduces a measured incident verbatim: the journal recorded a
wake at 08:35:45 and a "SUSPENDING" decision at 08:35:46. The machine did not actually sleep, only
because the previous suspend was still unwinding and the system refused a second one — see
[ACT-3].

An implementation MUST pass this case, yielding `resume + 5m` and never `t + 0`. It is stated as a
mandatory conformance vector in [TEST-2].

### 12.3 `rest --now` while a game is running

Configuration (shipped defaults, abridged):

```toml
[while.always]
suspend  = "30m"
poweroff = "never"

[while.after_resume]
clock    = "resume"
suspend  = "5m"
poweroff = "5m"

[while.steam_game_running]
clock    = "human_input"   # see [CLK-6]
suspend  = "2h"
poweroff = "never"

[while.human_active]
suspend  = "never"
poweroff = "never"
```

State: a game is running, human idle is 3 minutes, last resume was 6 hours ago. A remote relay
finishes a job and issues `idlectl rest --now` for `suspend`.

| Block | True? | Deadline for `suspend` | Satisfied by `--now` (`always` / `after_resume`)? |
|---|---|---|---|
| `while.always` | yes | `t + 27m` | **yes → `t`** |
| `while.after_resume` | yes | `t − 5h55m` | **yes → `t`** |
| `while.steam_game_running` | yes | `t + 1h57m` | no → `t + 1h57m` |
| `while.human_active` | yes (idle 3m < 5m) | `+∞` | no → `+∞` |

`floor(suspend) = +∞`. **Nothing happens.** The person playing is not interrupted.

Now the same request later in the night, with the game still running and human idle now at exactly
3 hours:

| Block | True? | Deadline for `suspend` | Satisfied by `--now` (`always` / `after_resume`)? |
|---|---|---|---|
| `while.always` | yes | `t − 2h30m` | yes → `t` |
| `while.after_resume` | yes | past | yes → `t` |
| `while.steam_game_running` | yes | `t − 1h` (origin `t − 3h`, timeout 2h) | no → `t − 1h` |
| `while.human_active` | no (idle 3h ≥ 5m) | — | — |

`floor(suspend) = t`. The machine suspends, and the game is exactly where it was on the next wake.

This is the "I fell asleep with the game running" case, separated from the "I have been playing
for five hours" case by nothing but the human-input clock ([CLK-6]). Note also that `poweroff`
remains `never` from `[while.steam_game_running]`, whose condition is neither `always` nor
`after_resume` — so no request can power the machine off with a game open. Only `--force` can, and
it will say so ([REQ-11]).

### 12.4 A game running while a download runs

State: a game is running (soft, 2 h), a platform download is in flight (`never`), human idle is
4 hours, no resume this boot.

| Block | True? | `suspend` deadline | `poweroff` deadline |
|---|---|---|---|
| `while.always` | yes | `t − 3h30m` | `+∞` |
| `while.steam_game_running` | yes | `t − 2h` | `+∞` |
| `while.steam_downloading` | yes | `+∞` | `+∞` |
| `while.gpu_busy_game` | yes — the game's ~10 GiB | `t − 2h` | `+∞` |
| `while.gpu_busy_other` | **no** — see below | — | — |
| `while.human_active` | no | — | — |

`floor(suspend) = +∞`. The download finishes.

Two things are happening here that both had to be paid for:

- **The game's ~10 GiB raises `gpu_busy_game`, not `gpu_busy_other`**, because the memory is in the
  game's process ancestry ([FACT-27], [FACT-28]). That is what makes the split worth having:
  `[while.gpu_busy_game]` carries the same finite 2 h `suspend` as the game block itself, so the
  soft treatment survives; `[while.gpu_busy_other]`, which a training job would raise, keeps its own
  policy. A single undifferentiated `gpu_busy` with `suspend = "never"` would make the game's soft
  2 h timeout unreachable, and the "I fell asleep with the game running" case of §12.3 would never
  fire for any game that actually uses the GPU — which is all the games it was written for.
- **`steam_downloading` is a `never` and needs no lease and no TTL** ([FACT-22]). It
  self-extinguishes when the writes stop. Making it finite, like the game, would cut off exactly the
  long overnight downloads it exists to protect. Note that it says nothing about `screen_off`: under
  [COMP-2b] that silence contributes nothing, so a six-hour download does not hold the panel lit.

When the download completes, the staging directory is emptied, `steam_downloading` goes FALSE, and
`floor(suspend)` drops to `t − 2h` — already past — so the machine suspends at the next
evaluation ([COMP-6]), with the game intact. It suspends and stops there: `poweroff` is `+∞`, and
even if it were due, [ACT-7] takes the shallowest action, never the deepest.

### 12.5 An indeterminate detector

State: the session agent feeding the human-input clock has died. Its heartbeat is stale. Nothing
else is running; human idle would read 6 hours if it were readable.

| Fact | State | Effect |
|---|---|---|
| `human_active` | `INDETERMINATE` ([HUM-4], case 3) | `[while.human_active]` is **false as a selector** ([FACT-4b]) and contributes nothing. The fact set instead adds the implicit `+∞` floor of [FACT-4] to `suspend`, `hibernate`, `poweroff` |
| `human_input` clock | unreadable | `[while.always]` deadline for `suspend`/`hibernate`/`poweroff` = `+∞` ([CLK-11]); for `screen_off`, computed from `boot` |
| everything else | `FALSE` | — |

Results — unchanged from the earlier fail-closed-as-TRUE formulation, which is the point:

- `resolved(suspend) = resolved(hibernate) = resolved(poweroff) = +∞`. The machine stays awake.
  This is the cheap error ([MODEL-1]). The veto arrives from [FACT-4], with exactly the same force
  as a `never` floor and without the engine pretending a human was observed.
- `resolved(screen_off)` = `boot + 10m` (the shipped base schedule of Appendix A, computed from the
  `boot` origin per [CLK-11]), long past → **the screen turns off**. This is [FACT-5],
  [FACT-40] and [HUM-6] doing their job: one dead helper must not be able to leave a static image
  on an OLED panel indefinitely.
- `doctor` exits non-zero ([OBS-4], which fires on any `INDETERMINATE` fact without needing a
  bespoke rule), names the stale heartbeat as the fault, and reports `human_active` as case 3
  rather than case 1 ([HUM-5]).
- An administrator's `[when.human_active] suspend = "1h"` would contribute **nothing** here
  ([FACT-4b]). Doubt may prevent an action; it may never cause one.

To sleep this machine deliberately, `rest --now` is not enough — the implicit floor is not a block
and `--now` does not satisfy it ([REQ-3]). `rest --now --force` installs a ceiling due now, defeats
the doubt floor and logs it as defeated ([REQ-10], [TEST-22]).

The machine is now awake forever until somebody fixes the agent — and one command says so. That
combination is the whole design.

### 12.6 A ceiling

Configuration added by an administrator:

```toml
[when.always]
screen_off = "45m"
```

Effect: whatever any floor says, the panel goes dark 45 minutes after the last human input. Even
`[while.media_playing] screen_off = "never"` cannot hold it lit. There is no `hard` key to write
and no soft variant to get wrong: every `[when]` block is absolute ([CEIL-3]).

`doctor` lists this block permanently as a standing hazard ([CEIL-7]), and it is logged at warning
level at every load ([CEIL-6]). When it fires, the log names it and every floor it defeated
([CEIL-8]). If the same block also set `suspend`, it could suspend the machine with somebody typing
on it — which is precisely why a ceiling can never be arrived at by accident and can never operate
quietly.

### 12.7 A block that says nothing about an action

Configuration:

```toml
[while.always]
screen_off = "10m"
suspend    = "30m"

[while.steam_downloading]
suspend    = "never"
```

State: a download is in flight; human idle is 40 minutes.

| Block | True? | `screen_off` | `suspend` |
|---|---|---|---|
| `while.always` | yes | `origin + 10m` | `origin + 30m` |
| `while.steam_downloading` | yes | **does not participate** ([COMP-2b]) | `+∞` |

`resolved(suspend) = +∞` — the download is protected. `resolved(screen_off) =
origin(human_input) + 10m`, thirty minutes in the past — **the panel goes dark** while the download
continues.

This is the whole of [COMP-2b] in one table. Reading the download block's silence about
`screen_off` as `never` would keep a static image on the panel for the entire six-hour download,
which is the failure §5.6 exists to prevent. Stated as a mandatory vector in [TEST-21].

---

## 13. What this is not

This section documents designs that were measured not to work. **It MUST NOT be deleted.** Without
it, this project's reason to exist is invisible and somebody eventually replaces it with one of
the following.

### 13.1 It is not `logind`'s `IdleActionSec`

`systemd-logind` can perform an idle action on a timer. It is driven by the session's `IdleHint`
property.

Measured (systemd 261, Plasma 6, Wayland): `IdleHint` was **permanently `no`** on the seat. It
never became true, no matter how long the session sat untouched. `IdleActionSec` would therefore
never have fired at all.

Measured on the same machine: the desktop's session-idle D-Bus method returned *"not supported on
this platform"*. There was no working session-idle signal reaching `logind` from anywhere.

The Wayland idle protocol (`ext-idle-notify-v1`, v2) **was** offered by the compositor — the same
protocol `swayidle` uses. The information existed; nothing was carrying it to `logind`.

**Conclusion:** `IdleActionSec` is not a simpler version of this project. On a modern Wayland
desktop it is a timer wired to a signal that never arrives.

### 13.2 It is not an inhibitor-based design

`logind` inhibitor locks are the obvious mechanism. Four measurements rule them out as
*enforcement*:

1. **A 150 GB game download declared no inhibitor at all.** With the download running at
   11.7 MiB/s, the complete inhibitor list was: two `sleep` locks in mode **`delay`** (the network
   manager and the power daemon), and one `block` lock from the desktop's power daemon covering
   the power/suspend/hibernate keys and the lid switch. **Nothing blocked sleep.** An
   inhibitor-based design cannot see a download in flight. It is blind to the single most common
   reason a machine should not sleep at night.
2. **Taking a `block` shutdown lock requires interactive authorization.** An attempt from a
   non-interactive remote session failed with an authorization error. The automation that most
   needs to say "not now" is structurally unable to.
3. **A shutdown invocation without a controlling terminal returns success and ignores inhibitors
   and sessions entirely.** Whatever locks exist, that path does not consult them.
4. **The variant that does consult them refuses whenever any user is logged in.** On a console
   with autologin — the target configuration for a living-room machine — that is always. The check
   is not usable.

And separately, at the desktop layer: with a real desktop power-inhibit held, three different
inhibition query interfaces reported `false` and two empty lists. A veto built on the desktop's own
inhibition APIs would have been dead letter from the first line of code. MPRIS `PlaybackStatus`
was the signal that actually worked, verified changing in both directions.

**Conclusion:** inhibitors are read as one signal among several ([FACT-11], [FACT-12]). They are
never trusted to prevent anything, and their absence is never taken as permission.

### 13.3 It is not a desktop environment's auto-suspend timer

Measured over an afternoon on a real desktop (Plasma 6, PowerDevil 6.7.3):

| Trial | Threshold | Stayed awake | Suspended? |
|---|---|---|---|
| Idle, with a remote probe every 25 s | 1800 s | 35 min | **no** |
| Idle, no remote sessions at all | 1800 s | **53.4 min** | **no** |
| After reloading configuration | 120 s | 401 s | **no** |
| Same, prolonged | 120 s | a further 882 s | **no** |
| **After real human input** | 120 s | 561 s | **no** |

The last row is the decisive one: the timer was not merely desynchronised by a resume — **touching
the machine did not re-arm it either**.

Ruled out by measurement, not intuition: it was not the remote sessions (a dozen were open during
the one suspend that did happen, and with none open it stayed awake 53 minutes); not display
events (the power daemon wrote a single journal line in 75 minutes); not an inhibitor (its only
lock covered the power key, which does not block sleep); not lack of human input (last row).

Meanwhile the *screen-off* timer in the same daemon was exact, firing at 900 s in two independent
measurements. The component was alive and healthy and reported no error. One of its two timers
simply did not exist in practice.

Two conclusions, and the second matters more than the first:

1. **The bug is not the point.** It is somebody else's component in a rolling release, with no
   deadline and no guarantee; and even fixed, it would still be unable to see a lease, a remote
   session, a training job's GPU memory or a download in progress.
2. **The architecture is the point.** Two independent mechanisms could put the machine to sleep and
   neither knew the other existed. Whichever fires first wins, and the one that fires first is the
   one that looks at nothing. **This project exists to make power a single-owner resource.** The
   desktop keeps the screen-off timer, which works and protects the panel; everything that stops
   the whole machine belongs to one decider that can explain itself.

*(Corollary, also measured: that desktop's screen-off timer counts from the **resume**, not from
the last human input. A machine woken at 3 AM by a remote relay therefore relit its video output
for fifteen minutes with nobody in the room. `idlectl` measures `screen_off` on `human_input`, so
a headless wake never lights the panel — see the shipped defaults in Appendix A.)*

### 13.4 It is not a periodic poll

A five-minute timer is the cheapest possible implementation and it produces two defects that
cannot be tuned away:

- The tick that was missed during sleep runs at resume, so the evaluation lands in the same second
  as the wake ([COMP-8], §12.2).
- Up to a full period elapses between a condition clearing and the machine acting on it.

`idlectl` is event-driven with a timer armed at the earliest pending deadline ([COMP-6]). A
periodic sweep MAY exist as a backstop for detectors that cannot be watched, but MUST NOT be the
primary trigger ([COMP-7]).

### 13.5 It is not "reset the idle counter on resume"

Tempting, one line, and wrong. After a remote wake the idle count would read zero, so a machine
woken specifically to run a job would refuse to go back to sleep when the job finished — the
opposite of what the wake was for. The correct construction is the `after_resume` floor: delay the
action without falsifying the observation ([COMP-9], [CLK-8]).

### 13.6 It is not a CPU tuning tool

See [MODEL-2].

---

## 14. Conformance

### 14.1 Mandatory test vectors

An implementation claiming conformance MUST pass all of the following. Each is stated so it can be
transcribed directly into a test.

**[TEST-1] — order independence.** For any block set and any permutation of it, `resolved(a)` is
identical for all four actions. (§3.4)

**[TEST-2] — 9 h of idle after a resume.** `[while.always] suspend = "30m"` on `human_input` with
origin `t − 9h`, and `[while.after_resume] suspend = "5m"` on `resume` with origin `t`, yield
`resolved(suspend) = t + 5m`. Any result of `t` or earlier is a conformance failure. (§12.2)

**[TEST-3] — empty set.** With no true `[while]` block, `resolved(a) = +∞` for every action.
(§3.2, [MODEL-6])

**[TEST-4] — `never` wins the MAX.** `[while.always] suspend = "1s"` and
`[while.lease_held] suspend = "never"`, both true, yield `resolved(suspend) = +∞`. (§3.2)

**[TEST-5] — WITHDRAWN.** It asserted the behaviour of a soft ceiling against a `never` floor.
v1 has one ceiling class and every ceiling is absolute, so the case it described does not exist
([CEIL-3], [CEIL-5]).

**[TEST-6] — a ceiling beats `never`.** Adding `[when.always] suspend = "1s"` to [TEST-4] yields
`resolved(suspend) = origin(human_input) + 1s`, and produces the log record of [CEIL-8] and the
`doctor` hazard of [CEIL-7]. ([CEIL-4])

**[TEST-6b] — a ceiling never delays.** With `[while.always] suspend = "2m"` true and
`[when.always] suspend = "10m"` true, `resolved(suspend) = origin + 2m`. A `[when]` block MUST NOT
be able to push an action later. (§3.2, [CEIL-3])

**[TEST-7] — doubt vetoes three actions and not the fourth.** With any enabled fact
`INDETERMINATE`, `resolved(suspend) = resolved(hibernate) = resolved(poweroff) = +∞`, while
`resolved(screen_off)` is unchanged from the same scenario with that fact `FALSE`.
([FACT-4], [FACT-5], [FACT-40])

**[TEST-8] — `UNAVAILABLE` is inert.** With a fact `UNAVAILABLE`, blocks referencing it are false
and no action's `resolved` changes relative to the same scenario with that fact `FALSE`.
([FACT-3])

**[TEST-9] — `rest --now` does not collapse condition blocks.** In the state of §12.3 (game
running, human idle 3 min), `rest --now` for `suspend` yields `resolved(suspend) = +∞` and issues
no transition. Exactly `[while.always]` and `[while.after_resume]` are satisfied; naming any third
block is a conformance failure. ([REQ-2], [REQ-3])

**[TEST-10] — `--force` defeats every floor.** The same state with `--force` issues the transition,
emits the warning record of [REQ-11] naming both `while.steam_game_running` and
`while.human_active`, and increments the counter of [REQ-12].

**[TEST-11] — human never touched this boot.** With no input observed since boot, `human_active`
is `FALSE` and `origin(human_input)` is the boot instant. A base schedule of `suspend = "30m"`
therefore becomes permitted at `boot + 30m`. ([HUM-3], [CLK-7])

**[TEST-12] — human clock unreadable.** With the agent's heartbeat stale, `human_active` is
`INDETERMINATE` (case 3), the three sleep actions are `+∞` via the implicit floor of [FACT-4],
`screen_off` still fires, a `[when.human_active]` ceiling contributes nothing, and `doctor` exits
non-zero identifying case 3. ([HUM-4], [HUM-5], [FACT-4b], §12.5)

**[TEST-13] — bad config value vetoes without crashing.** A configuration containing
`suspend = "half an hour"` leaves the daemon running, logs the offending key and text, sets the
three sleep actions to `+∞`, leaves `screen_off` unaffected, and makes `doctor` exit non-zero.
([CFG-15], [CFG-16])

**[TEST-14] — bare integer rejected.** `suspend = 1800` is a configuration error under [CFG-15]
and is handled exactly as [TEST-13].

**[TEST-15] — WITHDRAWN.** It asserted zero-byte shadowing of a vendor drop-in. v1 has no
`/usr/lib/idlectl/conf.d`, so there is no vendor drop-in to shadow ([CFG-5], [CFG-6]). Layer
precedence is instead covered by the ordering of [CFG-3] and [CFG-4].

**[TEST-16] — indeterminate flap does not reset the condition clock.** A fact transitioning
`TRUE → INDETERMINATE → TRUE` leaves the `condition` clock origin at the original FALSE→TRUE edge.
The same MUST hold across a suspend/resume cycle: a resume does not clear a recorded edge.
([FACT-39], [FACT-39b], [COMP-9])

**[TEST-17] — game GPU attribution across the two facts.** With a game running and its process tree
holding ≥ 512 MiB, `gpu_busy_game` is `TRUE` and `gpu_busy_other` is `FALSE`; with the same memory
held by a process unrelated to any game, `gpu_busy_game` is `FALSE` and `gpu_busy_other` is `TRUE`.
([FACT-25], [FACT-27], [FACT-28])

**[TEST-18] — service in use by counters.** With `[facts.local_service_busy] counters_url` set: a
service whose counters advanced between the last two samples is `TRUE`; one whose counters have not
advanced beyond `idle_window` is `FALSE`; one whose counters cannot be read is `INDETERMINATE`.
With no `counters_url` configured the fact is `UNAVAILABLE` and its detector does not run.
([FACT-30], [FACT-34], [FACT-44])

**[TEST-19] — re-check before acting.** If a fact changes between the decision and the issue of the
transition, no transition is issued. ([ACT-4])

**[TEST-20] — expired-during-sleep timer is discarded.** A timer whose deadline passed while the
system slept does not fire on resume; the engine recomputes. ([COMP-8])

**[TEST-21] — silence is not `never`.** `[while.always] screen_off = "10m", suspend = "30m"` plus
`[while.steam_downloading] suspend = "never"` with `steam_downloading` TRUE yields
`resolved(suspend) = +∞` **and** `resolved(screen_off) = origin(human_input) + 10m`. A result of
`resolved(screen_off) = +∞` is a conformance failure. ([COMP-2b], §12.7)

**[TEST-22] — `--force` defeats a floor that is not a block.** With one enabled fact
`INDETERMINATE` and **no** configured block referencing it: `rest --now` issues no transition and
reports the doubt floor as the reason; `rest --now --force` issues the transition and logs the doubt
floor among the floors defeated. ([FACT-4], [REQ-3], [REQ-10], [REQ-11])

**[TEST-23] — the `resume` clock with no resume this boot.** A TRUE block with `clock = "resume"`
on a machine that has never slept contributes `+∞` for `suspend`, `hibernate` and `poweroff`, and
its boot-origin deadline for `screen_off`. Substituting the boot origin for the three sleep actions
is a conformance failure. ([CLK-11])

**[TEST-24] — shallowest sleep action, and no escalation.** With `[while.always] suspend = "30m",
poweroff = "8h"` and both deadlines in the past, the evaluation issues `suspend` and MUST NOT issue
`poweroff`, in that evaluation or in any later one while the machine remains awake with the same
origins. `screen_off`, if also due, MAY be applied in the same evaluation and MUST NOT suppress the
`suspend`. ([ACT-6], [ACT-7], [ACT-7b])

**[TEST-25] — restarting the agent does not restart the countdown.** With a seat that has been idle
for one hour and `[while.always] suspend = "30m"`, restarting the session agent MUST leave
`origin(human_input)` where it was, and `suspend` MUST stay due at its original deadline. An
implementation that anchors the origin at the restart fails this. Restarting the agent after a gap
longer than the adoption bound MUST instead report `human_active` as `INDETERMINATE` until the
first transition — never as active. ([CLK-14], [CLK-15])

**[TEST-26] — a requested action is not downgraded.** With `[while.always] suspend = "30m",
poweroff = "never"` and the machine idle for longer than 30 m, `rest --action poweroff` MUST either
power the machine off or do nothing at all. An implementation that suspends it fails, whatever it
returns to the caller. ([ACT-7c])

**[TEST-27] — a held request outlives a veto and is spent once.** With a request held for an action
that is currently vetoed, clearing the veto MUST cause that action on the next evaluation, and the
request MUST NOT survive being carried out. Human input arriving first MUST drop it instead, and a
resume MUST NOT. ([REQ-6], [REQ-7], [REQ-8])

### 14.2 What conformance does not cover

Detector implementation details, the exact wording of log messages, the CLI's output formatting,
and the D-Bus method signatures are outside this section. The requirement is that the numbers,
states and decisions match.

---

## Appendix A — Shipped defaults (normative)

This is the vendor default configuration, `/usr/lib/idlectl/idlectl.toml` — **layer 1 of the
resolution chain of [CFG-3]**, read on every start, not an example to copy ([CFG-26]). Values here
are normative in the sense of [CFG-22].6: they may be lengthened without a major version bump and
may not be shortened.

The listing below is the effective content of the shipped file, key for key. The file as installed
carries extensive comments; those are not reproduced here. It parses under the frozen table set of
[CFG-27] and contains no key the shipped parser rejects.

```toml
[general]
min_idle = "5m"                 # human_active threshold — [HUM-2]

# ── Base schedule: what the machine does on its own ───────────────────────────
[while.always]
clock = "human_input"
screen_off = "10m"
suspend = "30m"
hibernate = "never"             # needs swap ≥ RAM and a matching resume= — opt in
poweroff = "never"              # the machine never powers itself off; a request may ask

# ── The floor: somebody is here ───────────────────────────────────────────────
# Does not set screen_off, on purpose — [HUM-6], [FACT-40], [COMP-2b].
[while.human_active]
suspend = "never"
hibernate = "never"
poweroff = "never"

# ── Settle window after a wake ────────────────────────────────────────────────
# Measured on the resume clock. Defeats arbitrarily large accumulated idle
# because composition is MAX over instants — [TEST-2]. Covers all three sleep
# actions so the window still holds on a machine where hibernate was enabled.
# Deliberately does NOT set screen_off: a headless wake must not relight the
# panel — see the §13.3 corollary.
[while.after_resume]
clock = "resume"
suspend = "5m"
hibernate = "5m"
poweroff = "5m"

# ── Floors: work in progress ──────────────────────────────────────────────────
[while.remote_session]
suspend = "never"
hibernate = "never"
poweroff = "never"

[while.lease_held]
suspend = "never"
hibernate = "never"
poweroff = "never"

# ── Soft floors: a session that survives a suspend ────────────────────────────
# `never` for poweroff (which ends the session), a long finite timeout for
# suspend (which does not), and an explicit `screen_off = "never"` — permitted by
# [FACT-42] because it is an operator-written infinity, not a doubt-derived one:
# a running game or a playing film is exactly the state in which the panel is in
# use, and both facts self-extinguish. Measured on human_input, NOT on condition:
# a five-hour session and a fall-asleep are identical in process state and are
# separated only by human input. See [CLK-6], §12.3.
[while.media_playing]
clock = "human_input"
screen_off = "never"
suspend = "2h"
hibernate = "never"
poweroff = "never"

[while.steam_game_running]
clock = "human_input"
screen_off = "never"
suspend = "2h"
hibernate = "never"
poweroff = "never"

# Same treatment, reached by the other route: GPU memory in a game's ancestry.
[while.gpu_busy_game]
clock = "human_input"
screen_off = "never"
suspend = "2h"
hibernate = "never"
poweroff = "never"

# ── Floors that expire or self-extinguish ─────────────────────────────────────
# Self-extinguishing, no TTL — [FACT-22]. Says nothing about screen_off: a
# download has no opinion about the panel — [COMP-2b].
[while.steam_downloading]
suspend = "never"
hibernate = "never"
poweroff = "never"

# GPU load NOT attributable to a game: a finite timeout counted from when the
# load appeared, so background noise expires and a real workload announces
# itself with a lease.
[while.gpu_busy_other]
clock = "condition"
suspend = "20m"
hibernate = "never"
poweroff = "never"

# Counters, not "is the unit active" — [FACT-32]. Ships UNAVAILABLE until
# [facts.local_service_busy] counters_url is set — [FACT-44].
[while.local_service_busy]
suspend = "never"
hibernate = "never"
poweroff = "never"

# A signal we choose to honour, never relied on — [FACT-12].
[while.inhibitor_block]
suspend = "never"
hibernate = "never"
poweroff = "never"
```

**On `hibernate` and `poweroff` being `"never"` in every safety block.** The base schedule already
says `never` for both, so the restatements look redundant. They are not: the single most likely edit
anybody makes to this file — and the one its own comments anticipate — is `[while.always] poweroff =
"8h"`, or the same for `hibernate`. Without the restatements, that one line gives a machine that can
power itself off mid-film, mid-game and mid-download. With them, the safe answer survives the edit.
`[while.gpu_busy_other]` keeps its finite `suspend` and takes both `never`s like the rest.

The rule is **both keys in every block**, not "poweroff in every block". An earlier revision of this
appendix stated it as the latter and then omitted `hibernate` from three blocks —
`steam_downloading`, `gpu_busy_other` and `inhibitor_block` — which left exactly the hole the rule
exists to close, for the administrator who enables hibernation rather than poweroff. The shipped
`data/idlectl.toml` had all three; the appendix did not, and a conformance check that diffs the two
is what found it.

**On `version`.** It is optional and defaults to `1` ([CFG-21]); whether the shipped file carries
the line changes nothing about the effective policy.

**On `local_service_busy`.** The shipped file carries an explicit `[facts.local_service_busy]
enabled = false` rather than relying on the compiled-in default, so that `idlectl doctor` reports it
as *switched off in configuration* and points at the block of comments that says how to turn it on.
Beyond that, no `counters_url` is
configured, so the fact reads `UNAVAILABLE` and its detector never runs ([FACT-44]). The
`[while.local_service_busy]` block above is therefore inert on a fresh install and is listed as
inert by `doctor` ([FACT-3], [OBS-3].2). It becomes live the moment an administrator points it at a
counters endpoint, which is the only moment it can be trusted.

**On what does not ship.** No `[when]` block ships by default: ceilings are an administrator's tool
and every one of them is a standing hazard ([CEIL-6]–[CEIL-8]). The shipped posture is that the
machine's own schedule and the safety floors are the whole policy.

---

## Appendix B — Requirement index by rationale

Requirements that exist because of a specific measured failure. If any of these is ever proposed
for removal, the measurement is the thing to re-run first.

| Requirement | The failure it prevents |
|---|---|
| [COMP-1], [COMP-4], [TEST-2] | Suspending in the same second as a wake, after composing durations across different clock origins. |
| [COMP-8], [COMP-9] | The tick missed during sleep running at resume; and the wrong fix that breaks the remote-wake path. |
| [COMP-7] | Five-minute granularity in both directions. |
| [CLK-5] | An activity signal that goes stale during exactly the activity it detects. |
| [CLK-6] | A 2 h "soft" timeout on the wrong clock suspending a 2 h session. |
| [CLK-7], [HUM-3] | A never-touched machine either sleeping instantly or never sleeping. |
| [CLK-9], [CLK-10] | The suspend-vs-cold-boot distinction being uncomputable from outside; a sleep hook that silently never ran. |
| [FACT-4], [FACT-5] | Doubt letting a machine sleep; doubt burning a panel. |
| [FACT-4b], [HUM-4] | Doubt *causing* an action: a ceiling firing on a machine whose detector is dead. |
| [FACT-43], [FACT-44], [CFG-12] | A block-independent doubt veto wedging every machine on install, via a capability misread as doubt or a counter endpoint that is off by default. |
| [COMP-2b], [TEST-21] | A download, or a dead idle agent, pinning a lit image on an OLED panel by saying nothing about it. |
| [FACT-39b] | An invisible re-arm: a `condition`-clock deadline that a periodically-woken machine can never reach. |
| [CLK-11], [TEST-23] | A settle window acting as an accelerator on a machine that has never slept. |
| [ACT-7], [ACT-7b] | Powering off instead of suspending: every session closed, and a shutdown that hung one time in four. |
| [ACT-12], [ACT-13] | An OLED carve-out resting on an action with no named mechanism and no unavailable story. |
| [CFG-28] | A broken package leaving a panel lit, on a first start with no previous configuration to keep. |
| [OBS-3].11 | Two owners of power, neither aware of the other (§13.3). |
| [FACT-11] | `delay` locks read as vetoes, making the fact permanently true. |
| [FACT-12], §13.2 | An inhibitor design that cannot see a 150 GB download. |
| [FACT-18] | A veto built on desktop inhibition APIs that report empty while an inhibit is held. |
| [FACT-23], [OBS-2] | A correct decision reported with a misleading number. |
| [FACT-25], [FACT-27], [FACT-29] | A soft game veto unreachable behind an undifferentiated GPU veto; a log line attributing a service's memory to a game. |
| [FACT-47] | A GPU source that sees nothing because the daemon may not read another user's `fdinfo`, and the "fix" that hands a power daemon the right to read any process's memory. |
| [FACT-30]–[FACT-35] | "Running" mistaken for "in use", pinning ~12 GiB of VRAM all night. |
| [ACT-1] | A shutdown path that ignores everything, and its alternative that never fires. |
| [ACT-3] | An intermittent bug masked by a race that happened to save it. |
| [ACT-8] | A correct feature made unreachable by a unit condition, logging nothing. |
| [CFG-15], [CFG-16] | One non-numeric config value killing the decider before it evaluated anything. |
| [CFG-19] | A policy the operator believes is in force and is not. |
| [CFG-24] | A release that makes machines sleep sooner than the one before. |
| [CEIL-3], [CEIL-6]–[CEIL-8] | A second silent owner of power; and a "soft" ceiling that reports success while doing nothing. |
| [HUM-1] | A request suspending a machine somebody is typing on. |
| [OBS-6] | Three separate failures that shared only one property: silence. |
| [REQ-2], [REQ-3] | A remote relay suspending a running game. |
| [REQ-10], [TEST-22] | An override that cannot free a machine wedged awake by a dead detector — the one thing it is reached for. |
| [REQ-15] | A stolen automation credential ending a game in progress. |
