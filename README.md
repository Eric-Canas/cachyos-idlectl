# idlectl

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/logo/logo-dark.svg">
  <img alt="idlectl" title="idlectl" src="docs/logo/logo.svg" width="17%" align="left">
</picture>

**idlectl** decides **when a Linux machine may turn its screen off, suspend, hibernate or power
off** — from what is actually happening on it: who is logged in, what is playing, what is
downloading, what the GPU is holding. One root daemon, one config file, and a CLI that will
**explain every decision before it happens**.

It replaces the *policy* half of your desktop's power settings and of logind's `IdleActionSec`,
which between them cannot see a download, a game or a model loaded into VRAM.

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

**Three rules, and that is the entire model.** Every block that is true *right now* proposes a
deadline per action. For each action the **longest** proposal wins, and `never` beats everything. A
block that does not set a key contributes **nothing** to that action, and zero true blocks means do
nothing.

**Order never matters, and there is no priority list to get wrong.**

> **Status: `0.4.6`, and working.** The daemon, the CLI and the session agent are implemented, and
> everything below has been exercised on a real CachyOS machine: the eleven facts, the decision
> loop, leases, held requests, polkit authorization, resume detection across a genuine `deep`
> suspend, and blanking on KWin — verified by watching the panel go dark, not by trusting the call
> to return. The wlroots blanking backend is implemented and compiles but has not been run against
> a wlroots compositor.
>
> It has spent days running in `--dry-run` beside the hand-written system it was extracted from, on
> the same machine, with both sets of decisions compared in the journal. That is where every bug
> fixed since `0.1.0` came from, and **none of them were visible to the test suite**: they only
> appear once the shipped units are actually started, and the last one was only visible to a human
> looking at a television. The [CHANGELOG](CHANGELOG.md) records what each one was and how it was
> measured.

---

## Contents

- [Install](#install) — one line, plus the two units to enable
- [Usage](#usage) — `status`, `explain`, `doctor`, `lease`, with real output
- [Configuration](#configuration) — blocks, floors and ceilings, and the clock that trips everyone up
- [Facts](#facts) — the eleven things it can know
- [Explaining a decision](#explaining-a-decision)
- [Conflicts](#conflicts--read-this-before-installing) — **read this**: two owners of power is the bug this removes
- [What it is not](#what-it-is-not)
- [Security model](#security-model)
- [Documentation](#documentation) — the spec, and everything moved out of this page
- [License](#license)

---

## Install

```sh
paru -S idlectl          # or: yay -S idlectl
```

[![AUR](https://img.shields.io/aur/version/idlectl?label=AUR&color=1793d1)](https://aur.archlinux.org/packages/idlectl)

`pacman` cannot fetch this by itself, and that is not an oversight on anyone's part: the AUR holds
**recipes**, not binaries, so something has to clone the recipe, build it, and hand the result to
`pacman`. An AUR helper is that something. By hand it is two lines, and `pacman` still does the
installing:

```sh
git clone https://aur.archlinux.org/idlectl.git
cd idlectl && makepkg -si          # -s pulls build deps, -i calls pacman
```

Then enable the two units. The package installs them **disabled**, on purpose — nothing that can
power a machine off should start doing it because you installed a package:

```sh
sudo systemctl enable --now idlepolicyd.service       # the daemon, one per machine
systemctl --user enable --now idlectl-agent.service   # the agent, one per graphical session
```

Without the agent, `human_active` is *indeterminate* and the daemon **will not let the machine
sleep** — deliberately: there is a compositor running and nobody can say whether somebody is
sitting at it.

There is nothing to copy to get started. The package installs the vendor policy at
`/usr/lib/idlectl/idlectl.toml`, which is layer 1 of the config chain and is in effect immediately:

```sh
idlectl doctor                         # what works on this machine, and what does not
sudoedit /etc/idlectl/idlectl.toml     # layer 2, overrides the vendor file
idlectl check-config                   # parses and merges without contacting the daemon
```

<details>
<summary><b>Without an AUR helper, or on a machine that is not Arch-based</b></summary>

<br>

[`packaging/install.sh`](packaging/install.sh) builds from source and lays down the same file layout
under `/usr/local`:

```sh
curl -fsSL https://raw.githubusercontent.com/Eric-Canas/cachyos-idlectl/main/packaging/install.sh | bash
```

It is the second-choice route on purpose, and it says so: it refuses to run when an AUR helper is
present, refuses to install over a `pacman`-owned path, never writes to `/etc` and never enables a
unit. [`packaging/uninstall.sh`](packaging/uninstall.sh) removes exactly what it added.

A packaged install is owned by `pacman`, upgrades with the system and can be removed completely.
This one is not, which is fine deliberately and bad by accident.

To build from a checkout instead, see [CONTRIBUTING.md](CONTRIBUTING.md).

</details>

<details>
<summary><b>Why the PKGBUILD is not in this repository</b></summary>

<br>

`makepkg` needs `source` to point at a published tarball plus a checksum. A PKGBUILD kept inside
the tree it builds is therefore either self-referential or permanently out of date by one release.
It lives in [Eric-Canas/idlectl-aur](https://github.com/Eric-Canas/idlectl-aur), which mirrors the
AUR repository. Every comparable project surveyed does the same.

</details>

---

## Usage

Four commands. All of them take `--json`, and none of them need root.

### `idlectl status` — what it believes, and what it will do

```console
$ idlectl status
idlepolicyd 0.4.6
layer      /usr/lib/idlectl/idlectl.toml
layer      /etc/idlectl/idlectl.toml

facts
  human_active         false
  after_resume         false
  remote_session       false
  lease_held           true
  inhibitor_block      false
  media_playing        false
  steam_game_running   false
  steam_downloading    true
  gpu_busy_game        false
  gpu_busy_other       false
  local_service_busy   false

actions
  screen_off           DUE
  suspend              in 12m40s  (at +18160s since boot)
  hibernate            never
  poweroff             never

holding this machine awake
  lease    backup                 uid 1000   in 1h59m  — nightly backup
  request  poweroff               uid 1000   in 5h59m  — will happen when every veto clears

Run `idlectl explain` for the whole computation, or `idlectl doctor` for what is broken.
```

The last section only appears when there is something in it. A lease and a held request are the two
things that can keep a machine awake **without appearing anywhere in your configuration**, so they
are on the first screen rather than behind a second command.

### `idlectl explain` — why, block by block

The question this answers is *"why is my machine not asleep?"*, and it answers it with the numbers
the decision actually used — not a recomputation ([OBS-2]):

```console
$ idlectl explain suspend
evaluated 3s ago
min_idle  300s

── suspend ──
  while.always                 true           30m       clock=human_input origin=+16360s (28m ago) -> boottime+18160s
  while.steam_downloading      true           never     clock=human_input origin=+16360s (28m ago) -> never
  while.steam_game_running     false          2h        clock=human_input origin=+16360s (28m ago) -> contributes nothing
  while.media_playing          false          2h        clock=human_input origin=+16360s (28m ago) -> contributes nothing
  while.human_active           false          never     clock=human_input origin=+16360s (28m ago) -> contributes nothing
  while.after_resume           false          5m        clock=resume     origin=+120s (4h32m ago) -> contributes nothing
  ---
  floor    never
  ceiling  never
  resolved never   never
  held by  while.steam_downloading
```

`held by` is the whole answer: a 150 GB download is running, so `suspend` is `never` until it
finishes. Note what `explain` does *not* do — there is no "priority" column, because there are no
priorities. The longest proposal won, and `never` is the longest there is.

### `idlectl doctor` — what is broken, and who else thinks they own power

```console
$ idlectl doctor
configuration
  layer     /usr/lib/idlectl/idlectl.toml
  layer     /etc/idlectl/idlectl.toml
  warning   logind IdleAction=suspend — a second owner of power (see Conflicts)

last evaluation  3s ago
suspended total  4h12m this boot

facts
  human_active         false          last input 28m10s ago (threshold 300s)
  media_playing        false          no MPRIS player is playing
  gpu_busy_game        false          0 MiB held by a game
  local_service_busy   unavailable    counters_url is not set

session agents
  agent :1.418 in session 2 (uid 1000), can_blank=true, 0 GPU holder(s), last report 4s ago

screen_off       available
```

`doctor` reporting the *other* candidate owners of power is a required output ([OBS-3].11), not a
best-effort extra: two components each believing they own the suspend decision is the bug this
project exists to remove, and the symptoms are silent and intermittent.

### `idlectl lease` — "I am working, do not sleep"

```console
$ idlectl lease acquire backup --ttl 2h --why "nightly backup" -- ./backup.sh
```

The lease lives exactly as long as the command does. It is a **file descriptor**, not a registry
entry, so a job that crashes cannot pin a machine awake — the kernel closes the handle. The TTL
(default `1h`, hard maximum `24h`) is the second bound, for a job that survives but hangs.

```console
$ idlectl lease list
backup                   uid 1000   expires in 1h59m       nightly backup
```

---

## Configuration

One file, three layers, each overriding the one before it:

| order | path                            | owner                                                    |
|-------|---------------------------------|----------------------------------------------------------|
| 1     | `/usr/lib/idlectl/idlectl.toml` | the package — vendor defaults, always installed          |
| 2     | `/etc/idlectl/idlectl.toml`     | you                                                      |
| 3     | `/etc/idlectl/conf.d/*.toml`    | you, ascending by basename, last wins                    |

**A block is `[while.<fact>]` or `[when.<fact>]`**, and it sets a timeout per action:

```toml
[while.steam_downloading]     # a floor: while this is true, do not do these things yet
suspend  = "never"
poweroff = "never"

[when.battery_critical]       # a ceiling: when this becomes true, do this NOW
hibernate = "0s"
```

- **`while` blocks propose floors** and the longest one wins. This is the one you want.
- **`when` blocks are ceilings** and they defeat every floor, including a human at the keyboard.
  There is exactly one honest use for them — a battery about to die — and `doctor` reports every
  ceiling you have configured as a standing hazard, whether or not it is true right now.
- **`clock = "human_input" | "resume" | "condition"`** picks *what the timeout is counted from*.
  Getting this wrong is the classic idle-daemon bug: a 2 h game timeout counted from the last
  keypress suspends a 2 h film.

**Full reference: [`docs/configuration.md`](docs/configuration.md)** — every key, the clocks and
why they exist, `min_idle`, per-fact tuning, and the worked composition examples.

---

## Facts

Eleven things the daemon can know about a machine. Each one is `true`, `false`, `indeterminate` or
`unavailable`, and **the last two are not the same**: a detector that cannot answer vetoes sleep,
while a capability this machine does not have raises no veto at all.

| fact | true when |
|---|---|
| `human_active` | somebody touched the seat within `min_idle` — from the compositor, never a heartbeat |
| `after_resume` | the machine came back from sleep recently, counted on the **resume** clock |
| `remote_session` | an `ssh` or other non-seat session is open |
| `lease_held` | something took a lease: *"I am working, do not sleep"* |
| `inhibitor_block` | a logind **block** inhibitor is held — `delay` inhibitors are ignored on purpose |
| `media_playing` | an MPRIS player reports `Playing`, in any session |
| `steam_game_running` | a Steam game process is up |
| `steam_downloading` | Steam is moving bytes — measured from its own download state, because Steam declares no inhibitor |
| `gpu_busy_game` | a **game** is holding GPU memory |
| `gpu_busy_other` | something that is not a game is holding GPU memory — a model, a render, a compile |
| `local_service_busy` | a local HTTP service's counters moved inside a window — *started* is not *in use* |

The two GPU facts are separate for a reason: a game holding VRAM should stop a `poweroff` but must
not stop a `suspend` forever, and a 13 GB model loaded for inference is a different decision from a
game. One undifferentiated "GPU busy" veto cannot express either.

**How each is measured, and what each refuses to guess: [`docs/facts.md`](docs/facts.md).**

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

## What it is not

`idlectl` is **not** a CPU or power tuning tool. It never touches governors, EPP, platform profiles,
turbo, or PCIe/USB runtime power management. That layer belongs to TLP, tuned, auto-cpufreq or
`power-profiles-daemon`, and `idlectl` is designed to run alongside any of them without overlap.

`idlectl` decides *when the machine stops being used*. Nothing else.

---

## Security model

The daemon runs as root, so the short version is what it refuses to do:

- Every state-changing D-Bus method is behind a **polkit action**, not a uid check written in the
  code. Asking the machine to rest is separate from **forcing** it, and forcing is a different
  action that a remote caller cannot reach without authenticating.
- All unprivileged input — leases, agent reports, GPU holders — is parsed with an explicit,
  length-bounded parser, and **none of it can make the machine sleep**. It can only ever add a
  reason to stay awake.
- The agent runs as you, in your session, and reports facts. The one thing it can command is your
  own screen, and only when the caller is the daemon.
- No shell, ever. Nothing read from any of the above is sourced, interpolated or executed.

**Full model, including the threat it does and does not defend against:
[`docs/security-model.md`](docs/security-model.md).**

---

## Documentation

| document | what is in it |
|---|---|
| [`docs/configuration.md`](docs/configuration.md) | every configuration key, the clocks, worked composition examples |
| [`docs/facts.md`](docs/facts.md) | how each of the eleven facts is measured, and what it refuses to guess |
| [`docs/design.md`](docs/design.md) | why not `IdleActionSec`, why not an inhibitor, why Rust, the design principles, and the scope |
| [`docs/security-model.md`](docs/security-model.md) | the full security model |
| [`docs/spec.md`](docs/spec.md) | **the normative specification.** Every rule is numbered, and most of them name the measurement that produced them |
| [CHANGELOG.md](CHANGELOG.md) | every release, what the bug was, and how it was measured |
| [CONTRIBUTING.md](CONTRIBUTING.md) · [TESTING.md](TESTING.md) | building from a checkout, and how the tests are organised |

`docs/spec.md` is the one to read if you want to argue with a decision: the rules are numbered so
that a disagreement can be about `[CLK-9]` rather than about a vibe.

---

## License

MIT. See [LICENSE](LICENSE).
