# Contributing

Thanks for looking. Before anything else, read [README.md](README.md) for what this project is, and
[`docs/spec.md`](docs/spec.md) for how the engine is defined. The spec is normative; the code
implements it.

---

## Rule zero: this repository is written in English

**Everything in this repository is in English. No exceptions.** Code, identifiers, comments,
documentation, config file comments, man pages, log and error message strings, commit messages,
branch names, pull request titles and descriptions, issue titles and bodies, and review comments.

This is not a preference about which language is nicer. It is what keeps a single-maintainer project
readable by contributors who will never share a first language with the maintainer, and what keeps a
grep for an error string useful to everyone. A contribution written in another language will be
asked to be translated before it is reviewed, however good it is.

If English is not your first language, write it anyway and do not apologise for it. Plain, blunt,
imperfect English is welcome here; a perfect sentence in any other language is not.

---

## The one process rule: spec first

**A change in behaviour lands in [`docs/spec.md`](docs/spec.md) before it lands in code.**

This is not ceremony. The composition rule is the entire product, it is subtle in exactly one place
(deadlines are instants, not durations), and a machine that sleeps at the wrong moment costs a
person their work. If the behaviour is not written down first, nobody can tell whether the code is
wrong or the expectation is.

So:

- **Bug fix, behaviour unchanged** — PR straight at the code, referencing the spec clause it
  restores.
- **Behaviour changes, or a new fact, action, or config key** — spec change first, in its own PR or
  as the first commit of the branch, and it must be reviewable on its own. A PR that changes what
  the daemon does without a matching spec change will be asked to split.
- **The spec disagrees with the code** — that is a bug report, and which side is wrong is a decision
  for the discussion, not for whoever pushes first.

Design decisions that are already closed are listed in the README and the spec. Reopening one needs
an argument that engages with the reason it was closed.

---

## Build

Prerequisites (Arch names; equivalents elsewhere):

| package  | why                                                     |
|----------|---------------------------------------------------------|
| `rust`   | `rustc` + `cargo`                                        |
| `scdoc`  | builds the man pages in `man/`                           |
| `dbus`   | `dbus-daemon`, used by the integration tests on a private bus |

`rustup` is not required and not assumed. **MSRV tracks the `rust` package in the Arch
repositories**, because that is what the package is built against; CI pins that version. Needing a
newer toolchain feature means raising the MSRV first, in its own PR, with the reason.

```sh
cargo build
cargo test
cargo build --release
```

Run the daemon against a scratch config without letting it act. Copy the vendor file out of the tree
first and edit the copy — `data/idlectl.toml` is what gets installed as layer 1, so keep it clean:

```sh
cp data/idlectl.toml /tmp/scratch-idlectl.toml
sudo ./target/debug/idlepolicyd --config /tmp/scratch-idlectl.toml --dry-run
```

`--dry-run` evaluates and logs every decision but executes none of them. Use it for anything
touching the decision path; you will iterate faster and you will not lose a session to a typo.

---

## Style and checks

CI runs all of these; run them before you push.

```sh
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo deny check
```

Rustfmt defaults, unmodified. Clippy warnings are errors; if a lint is genuinely wrong for a case,
`#[allow]` it at the narrowest scope with a comment saying why — never crate-wide.

Beyond the lints, three rules specific to this codebase:

1. **No panics in the daemon.** No `unwrap()`, no `expect()`, no indexing that can go out of range,
   no arithmetic that can overflow unchecked in the decision path. A panicking power decider is a
   machine whose power is now owned by nobody. A detector that cannot read its source returns
   `indeterminate`; it does not propagate a fatal error and it does not take the process down.
2. **Invalid input is a veto, not a crash, and not a dead daemon.** Config values are validated when
   they are loaded. A file that fails to parse or carries a bad value is dropped *as a whole file*,
   the remaining layers stay in effect, evaluation continues, the three sleep actions are held off
   until the fault is fixed, `screen_off` is not, and the fault is reported by `doctor` with the
   key, the file and the offending text. Loading must never return "no policy": there is no previous
   configuration to fall back on during a first start after a bad edit, which is precisely when this
   matters. An earlier decider was once killed outright by a single non-numeric value in a shell
   config under `set -u`, before one rule had been evaluated — every rule after it silently stopped
   existing.
3. **Never fail silently.** Anything that does not happen — a detector that could not run, a config
   key that has no effect, a block that could not be evaluated — logs at a level a user will see. A
   `systemd` unit carrying a `Condition*` once turned a whole feature into dead code that logged
   nothing at all. Prefer a noisy log to a quiet no-op, always.

---

## Dependencies

The dependency list is deliberately short, and adding to it needs a sentence of justification in the
PR description.

**Hard rule: no second async runtime.** The reactor is `async-io`. `zbus` cannot compile without
`async-io` or `tokio`, and this project picked `async-io` — it also drives inotify, udev and
timerfd, and one scheduler is enough. A dependency that drags in `tokio`, transitively or otherwise,
is not merged.

```sh
cargo tree -i tokio     # must report that nothing depends on it
```

CI enforces this. If a crate you want has a tokio-free feature set, use it and pin the features; if
it does not, the answer is a different crate.

`cargo deny check` gates licences and advisories — see `deny.toml`.

---

## Layout

```
crates/          the workspace: the daemon, the CLI, the session agent, the engine
docs/spec.md     normative specification
data/            what the package installs: vendor idlectl.toml, units, D-Bus and polkit files
man/             scdoc sources for the man pages
scripts/         publication and hygiene gates, run by CI and before a release
```

`data/idlectl.toml` is not an example. It is installed at `/usr/lib/idlectl/idlectl.toml` and is
layer 1 of the config chain on every machine, so a change to it changes the day-one behaviour of
every install and is reviewed as a behaviour change, spec first.

Two things that are *not* here on purpose:

- **The PKGBUILD.** It lives in the AUR package repository. `makepkg` needs `source` to point at a
  published tarball plus a checksum, so a PKGBUILD in the source tree is either self-referential or
  permanently stale.
- **Personal tooling paths.** Keep editor state, local scratch directories and anything specific to
  your own setup out of the committed `.gitignore`; put them in your own `.git/info/exclude`. The
  committed ignore file describes the *project's* build output, not any contributor's desk.

New top-level directories need a named precedent in a comparable Arch-packaged project, and the PR
should say which one.

---

## Adding a fact

A new fact is the most common contribution and the easiest one to get subtly wrong. It needs all of
this:

- [ ] A precise definition of when it is `true`, `false`, `indeterminate` and `unavailable` — all
      four, in the spec, in words that do not require reading the code.
- [ ] A stated default clock (`human_input`, `resume`, `condition`, `boot`) and a reason for it.
- [ ] Clean degradation: on a machine without the capability it is `unavailable`, which means the
      condition is simply never true. Not an error, not a warning on every cycle, not a veto.
      **`unavailable` is knowledge; `indeterminate` is doubt.** An absent capability must never
      report `indeterminate`, because doubt raises a machine-wide floor on all three sleep actions
      whether or not anybody wrote a block about the fact. A new detector that can be indeterminate
      on a perfectly healthy machine is a machine that never sleeps, shipped to everyone.
- [ ] An entry in `[facts.<name>]` with at least `enabled`, and a decision — argued in the PR —
      about whether it ships enabled. A detector whose source is off by default on the software it
      reads ships **disabled**, like `local_service_busy`, so that installing the package cannot
      wedge a machine awake on day one.
- [ ] A fakeable source. The detector sits behind a trait with a fake implementation, so the engine
      can be tested without the hardware. If it cannot be faked, it cannot be tested, and that is a
      review objection.
- [ ] Parser tests against captured real-world fixtures.
- [ ] It appears in `idlectl status` and in `idlectl explain`, with its source named.
- [ ] A bounded cost: a detector that polls says how often and how expensive, and a detector that
      can block says what its timeout is. A timeout expiring produces `indeterminate`, not a hang.

Ask yourself the question that keeps producing bugs in this domain: **is this measuring state, or
measuring use?** "The process exists" and "the service is active" are state. A machine is kept awake
by *use*, which is a counter that moved.

## Adding an action

Rarer, and heavier. Beyond the above:

- [ ] Say what `indeterminate` does to it. Sleeping actions are vetoed by doubt; `screen_off`
      deliberately is not, because one unreadable detector must not hold an OLED panel lit all
      night. A new action must argue its side of that line explicitly.
- [ ] Say where it sits in the depth order. `screen_off` composes freely with everything; among the
      sleep actions only the shallowest due one is issued, and a deeper action is never a safe
      substitute for a shallower one. A new action has to say which side of that it is on and why.
- [ ] Say how the mechanism is invoked and what happens when it is absent. `screen_off` is performed
      by the session agent's uid-0-only `Blank`/`Unblank` method — the one thing the agent commands
      — and reports `unavailable` as an *action* where no agent or no blanking mechanism exists, in
      which case blocks setting it are inert and `doctor` says so.
- [ ] Ceilings target every action, so say what a `[when]` block on the new action means. Ceilings
      are absolute: whatever you write here can be overridden to fire immediately.
- [ ] Say what happens if the action fails — the machine must end in a known state, and a failed
      action must not silently re-fire in a loop.

---

## Tests

See [TESTING.md](TESTING.md). Short version:

- Engine and config changes need unit tests with the injected clock. The composition properties
  (order-independence, `never` absorbing, empty-set defaults, and the normative resume case) are
  regression tests — do not weaken them to make a change pass.
- Detector changes need fixture tests.
- **Anything touching the decision path needs the manual protocol run on real hardware, with the
  results pasted into the PR.** CI cannot enter S3, cannot talk to a compositor, cannot see a GPU
  and cannot run a Steam download. A green CI on this project means the algebra is right, not that
  the machine wakes up.

---

## Commits and pull requests

- English, per rule zero — subject, body, PR description, review comments, all of it.
- Imperative subject, one logical change per commit. Explain *why* in the body when the reason is
  not obvious from the diff; the what is already in the diff.
- Reference the spec clause when a change implements or restores one.
- Keep PRs small enough to review in one sitting. A spec change plus its implementation may share a
  branch, but they should be separate commits.
- No force-pushes over an in-progress review; append fixups and let the merge squash them.

## Out of scope

Please don't open these; they will be declined, and it's better you know before writing them:

- CPU frequency, EPP, platform profiles or any power *tuning*. That is TLP, tuned, auto-cpufreq and
  `power-profiles-daemon`, and conflating the two is the single most common misreading of this
  project.
- Laptop policy in v1 — battery rules, lid handling, AC transitions. Battery *facts* may be
  displayed; no policy is built on them.
- Any policy decision made in the session agent. The agent reports facts; the daemon decides. That
  split is the security model. The single exception is mechanical, not a decision: the agent exposes
  `Blank`/`Unblank` to uid 0 so the daemon can turn the panel off, because no root process can blank
  a Wayland output. The agent still decides nothing.
- A priority list, a first-match-wins ordering, or anything else that makes block order matter.
- A PKGBUILD in this repository.

## Security

Do not open a public issue or PR for an undisclosed vulnerability. Use GitHub's private
vulnerability reporting (*Security* → *Report a vulnerability*); the threat model is in the README's
[security model](README.md#security-model) section.
