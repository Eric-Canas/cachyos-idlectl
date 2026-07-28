<!-- Moved out of README.md: how each of the eleven facts is actually measured, and what
     each one refuses to guess. The README keeps the one-line table. -->

# Facts

*[← back to the README](../README.md)*

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
