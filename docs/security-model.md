<!-- Moved out of README.md: the full security model. -->

# Security model

*[← back to the README](../README.md)*

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
