# Design

*[← back to the README](../README.md)*

Why this exists in the shape it does. Moved out of README.md, which now answers
"what is it and how do I use it" and links here for "why".

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
