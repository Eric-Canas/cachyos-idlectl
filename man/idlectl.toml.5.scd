idlectl.toml(5) "idlectl" "File Formats Manual"

# NAME

idlectl.toml - idle and power policy for idlepolicyd

# SYNOPSIS

/usr/lib/idlectl/idlectl.toml++
/etc/idlectl/idlectl.toml++
/etc/idlectl/conf.d/\*.toml

# DESCRIPTION

*idlectl.toml* tells *idlepolicyd*(8) when this machine may blank its screen,
suspend, hibernate or power off.

The file is a set of blocks. Each block names a condition and gives a timeout per
action. Read one aloud and it says what it does:

	while a game is running, do not suspend for two hours

The three locations above are layers, in increasing precedence, and later layers
override earlier ones:

```
1  /usr/lib/idlectl/idlectl.toml   the package
2  /etc/idlectl/idlectl.toml       the administrator
3  /etc/idlectl/conf.d/*.toml      the administrator, in ascending
                                   byte-wise basename order
```

Layer 1 is the vendor default and belongs to the package; it is replaced on every
upgrade and must not be edited. It is *not* an example to copy: it is read on every
start, so a machine with an empty /etc already has a complete and safe policy.

Layers 2 and 3 belong to the administrator, and *the package owns nothing under
/etc*: it never creates, modifies or removes those files, so an upgrade cannot revert
a local decision and an uninstall cannot leave a stale one behind.

Drop-ins are applied *after* /etc/idlectl/idlectl.toml, so a drop-in overrides it.
This is the systemd drop-in contract, which administrators already know. Name them
_NN-description_.toml; they are compared by basename, byte-wise.

Validate a change without restarting anything:

	idlectl check-config

# COMPOSITION

This section is normative. Everything else in this page follows from it.

Blocks are measured on different clocks (see *CLOCKS* below), so comparing their
timeouts as durations is meaningless: 30 minutes counted from one origin and 5
minutes counted from another cannot be ordered without knowing the origins. Each
block's timeout is therefore converted to an absolute instant on *CLOCK_BOOTTIME*
first, and the instants are composed.

For each action _a_ *independently*:

```
deadline(b,a) = origin(clock of b) + timeout(b,a)      never = +infinity

floor(a)      = MAX over deadline(b,a) for every enabled, currently-true
                [while] block b THAT SETS A KEY FOR a, together with one
                +infinity term for each implicit floor in force
ceiling(a)    = MIN over deadline(b,a) for every enabled, currently-true
                [when] block b THAT SETS A KEY FOR a
resolved(a)   = MIN(floor(a), ceiling(a))

act when now >= resolved(a)
```

MAX and MIN over the empty set are both *never*. Order can never matter: maximum and
minimum over instants are commutative and associative. If nothing is true, nothing
happens: the default is to not act.

The last rule is the one people are surprised by, and it is deliberate. A machine
that stays awake wrongly is a cheap error. A machine that sleeps wrongly is not.

## SILENCE IS NOT never

A block that does not set a key for an action contributes *nothing* to that action,
to its floor and to its ceiling alike, whatever the state of its condition. This is
what makes per-action policy work: *[while.steam_downloading]* vetoes suspend while
having no opinion about the panel, and *[while.human_active]* holds a machine awake
without pinning a lit image on it. Treating an unset key as *never* would leave a
display lit for the length of a six-hour download.

## A [when] BLOCK CAN ONLY PULL AN ACTION EARLIER

Because floor and ceiling are combined with MIN, *resolved(a) is never later than
floor(a)*, in any configuration. A *[when]* block cannot be used to keep a machine
awake, and it is the only construct that can defeat a *never* -- including
*[while.human_active]*, the implicit floor raised by doubt and the implicit floor
raised by a rejected configuration file. See *[when] BLOCKS*.

## THE CASE THIS EXISTS FOR

A machine has been idle for nine hours and then wakes from suspend. Idle counters do
not reset across a suspend, so "time since last human input" still reads nine hours.
Two blocks are true:

```
[while.always]        suspend = "30m"    clock = "human_input"
[while.after_resume]  suspend = "5m"     clock = "resume"
```

Composing *durations*: max(30m, 5m) = 30m, counted from nine hours ago, which is
eight and a half hours in the past. The machine suspends about one second after
waking. This is a measured incident, not a hypothetical: wake at 08:35:45,
"SUSPENDING" at 08:35:46, repeatedly.

Composing *instants*: max(input+30m, resume+5m) = resume+5m, five minutes in the
future. Correct.

The engine implements the second, and ships the first paragraph of this section as a
unit test so that it cannot be "simplified" back into the first.

# CLOCKS

Every block counts its timeouts from one origin, selected with *clock*. All clocks
are on *CLOCK_BOOTTIME*, which keeps counting across a suspend; *CLOCK_MONOTONIC*
stops during one and *CLOCK_REALTIME* can be stepped backwards by NTP, which would
make a deadline unreachable.

*human_input*
	The last key, click, touch or gamepad input. The classic idle timer, and the
	default when a block does not say otherwise. If nothing has been touched since
	boot, the origin is boot.

*resume*
	The last wake from suspend or hibernate.

	If the machine has *not* slept this boot there is no such instant, and one is
	not invented: a block on this clock contributes *never* to *suspend*,
	*hibernate* and *poweroff*, and the boot origin to *screen_off*. Falling back
	to boot for the sleep actions would make *[while.always] clock = "resume"*
	fire immediately on a machine that has never slept.

*condition*
	The moment this block's own condition last became false-to-true. "Twenty
	minutes after the download started", rather than "twenty minutes after the last
	keypress".

	A single failed detector reading does not restart it; only a definite false
	does. Neither does a suspend: the origin survives sleep, so a machine that is
	woken periodically does not hand every *condition* block a fresh timeout on
	every wake. A wake is not a moment at which a condition became true.

*boot*
	Boot. Rarely what anyone wants.

# ACTIONS

*screen_off*
	Blank the display. Performed by *idlectl-agent* inside the session that owns
	the compositor, on the daemon's instruction; see *idlepolicyd*(8).

	Where no agent is running, or the session offers no blanking mechanism, this
	action is *unavailable*: the daemon never proposes it, every block setting a
	*screen_off* key is inert, and *idlectl doctor* says so and names them.

*suspend*
	Suspend to RAM (S3).

*hibernate*
	Hibernate to disk (S4).

*poweroff*
	Power the machine off.

*screen_off* is not a change of the power state. It may be applied in the same
evaluation as a sleep action and never suppresses one.

Among *suspend*, *hibernate* and *poweroff*, at most one is performed per
evaluation, and it is the *shallowest* one that is due: suspend before hibernate
before poweroff. A deeper action is never a safe substitute for a shallower one --
suspend loses nothing, poweroff ends every session on the machine and, on the
hardware this policy was extracted from, hung roughly one time in four.

An action already in effect is not re-issued until its own deadline has been
re-armed by a new origin, so a machine whose panel is already blank does not re-fire
*screen_off* and never starves the sleep actions.

## idlectl DOES NOT ESCALATE

This follows from the rule above and is worth stating rather than discovering. With
*suspend = "30m"* and *poweroff = "8h"* both eventually due, the machine suspends and
never powers itself off. To get escalation, either set *suspend = "never"* and let
the deeper action own the schedule, delegate to logind's own *suspend-then-hibernate*
(which idlectl does not fight), or ask for the deep action by name:

	idlectl rest --action poweroff

# FACT STATES

Every fact has four states, and the last two are not interchangeable.

*true*, *false*
	As expected.

*unavailable*
	The machine has no such capability: no media player has ever appeared, Steam is
	not installed, there is no discrete GPU. Also the state of a fact switched off
	with *[facts.NAME] enabled = false*. Behaves exactly like *false*. A missing
	capability must never wedge a machine awake, or every machine without a sound
	card would refuse to sleep.

	A capability that is *absent* always reads *unavailable*, never
	*indeterminate*. Absence is knowledge; only a detector that exists and could
	not answer produces doubt.

*indeterminate*
	The detector exists and could not answer: a read failed, a helper timed out, a
	parse failed.

	While *any enabled fact* is indeterminate, *suspend*, *hibernate* and
	*poweroff* are held at *never* -- whether or not any block in the
	configuration mentions that fact. The rule is about the machine no longer being
	able to see something it normally sees; whether somebody happened to write a
	block for it is irrelevant to that. Any doubt is a veto.

	It does *not* hold *screen_off*. Screen-off is cheap to get wrong in both
	directions, and if one unreadable detector could pin the display on, a single
	flaky sensor would burn an OLED panel overnight.

Doubt may prevent an action; it may never cause one. A block whose condition is
indeterminate counts as false *as a selector* and contributes nothing, in *[while]*
and *[when]* alike.

The veto raised by doubt is not a block. *idlectl rest --now* does not satisfy it;
only *idlectl rest --force*, or a *[when]* block, defeats it. The supported way out
of a detector that is permanently indeterminate on your hardware is to switch it off
by name, in the *[facts]* table below, where *idlectl doctor* will report it.

Use *idlectl doctor* to see which facts are *unavailable* on this machine and which
are *indeterminate*, rather than assuming.

# THE [general] TABLE

*min_idle* = _duration_
	How recently a human must have touched an input device for the *human_active*
	fact to hold. A presence threshold, not a timeout. Default *5m*. *never* is
	rejected here.

# THE [facts] TABLES

A table named *[facts.NAME]* configures one detector, where _name_ is one of the
facts in *FACTS* below. An unknown fact name is a fatal configuration error, like an
unknown condition.

*enabled* = _true_ | _false_
	Default *true*, except for *local_service_busy*, which ships disabled. A
	disabled fact reports *unavailable*, its detector never runs, it can never
	raise the doubt veto, and *idlectl doctor* lists it as disabled.

	This is the supported answer to a detector that is broken on your hardware and
	is holding the machine awake: an auditable switch rather than a mystery.

Each fact also accepts a small set of keys of its own, and a key that is not defined
for that particular fact is a configuration error:

*counters_url* = _url_, *idle_window* = _duration_
	*local_service_busy* only. Where to read the service's cumulative request
	counters, and how recently they must have moved.

*steam_root* = _path_
	*steam_game_running* and *steam_downloading* only. The Steam installation to
	verify against.

*window* = _duration_
	*steam_downloading* only. How recently a file under the staging tree must have
	been written. Default *5m*.

# BLOCKS

A block is a table named *[while.CONDITION]* or *[when.CONDITION]*, where
_condition_ is *always* or one of the facts in *FACTS* below. *always* is the only
built-in condition; everything else, *after_resume* included, is an ordinary fact.

A name that is neither is a *fatal configuration error*, not a block that quietly
never fires. A misspelled *[while.game_runing]* would otherwise produce a file that
looks like it protects a running game and does not.

Keys:

*enabled* = _true_ | _false_
	Default *true*. Setting it to *false* in a drop-in is how a vendor block is
	switched off; there is no syntax for deleting one, because a silent deletion is
	indistinguishable from a typo.

*clock* = _name_
	One of the clocks above. Default *human_input*.

*screen_off*, *suspend*, *hibernate*, *poweroff* = _timeout_
	A timeout for that action. Omit a key to say nothing about that action; see
	*SILENCE IS NOT never*.

There are no other keys. In particular there is no per-block flag that makes a
ceiling weaker or a floor collapsible: a *[when]* block is always absolute, and what
*idlectl rest --now* satisfies is fixed by condition name and cannot be enlarged from
a configuration file.

## [while] BLOCKS

While the condition holds, do not act before the deadline. These are the blocks
almost everyone writes, and they compose by maximum: the latest deadline wins.

## [when] BLOCKS

When the condition holds, act by the deadline at the latest. These compose by
minimum, and are therefore *the only thing that can override a never* -- including
the *human_active* floor, the veto raised by doubt and the veto raised by a rejected
configuration file. None are enabled by default.

Adding one should feel like a decision, and it is treated as one. Every *[when]*
block in the effective configuration:

- is logged at *warning* level when the configuration is loaded, naming the file it
	came from;
- is listed by *idlectl doctor* as a standing hazard;
- names itself, its file and every floor it defeated, with each floor's reason, in
	the log record written when it fires.

A ceiling that silently did nothing and reported success would be worse than no
ceiling at all.

*idlectl rest --force* installs one for a single action, due immediately, for the
lifetime of that one request; writing one into a configuration file makes it
permanent. Both are logged.

# TIMEOUTS

A timeout is the exact string *never*, or a number followed by a unit, repeated. The
units are *s*, *m*, *h* and *d*. Underscores and spaces inside the value are ignored.

	suspend = "30m"
	suspend = "1h30m"
	suspend = "0s"
	suspend = "never"

*never* is lower-case. Every other name in this format is lower-case, and a
case-insensitive island in it would be a dialect nobody asked for.

A bare number is rejected, and that includes *0*: write *"0s"*. A unitless value
could mean seconds or minutes, and guessing wrong by a factor of sixty is the
difference between a nuisance and a machine that sleeps mid-game. A TOML integer
(*suspend = 30*, without quotes) is rejected the same way and for the same reason.

An unparseable value is a configuration error naming the file and the key. It is
never treated as zero and never crashes the daemon: a non-numeric value in a shell
config once killed an entire decider under *set -u* before a single rule had been
evaluated. See *idlepolicyd*(8) for what the daemon does with a file it had to
reject.

# FACTS

*human_active*
	A human has touched an input device within *min_idle*. The presence floor.

	It is a first-class fact rather than merely the clock timeouts are counted on,
	because if human presence existed only as a clock, nothing would stop a remote
	request or an override from acting while somebody is typing.

	Never touched since boot reads *false*, not *indeterminate*: a machine woken by
	a magic packet that has never seen a keypress demonstrably has nobody at it, and
	this keeps the wake-use-rest cycle working for a remote relay.

	An unreadable idle source -- no agent running, a stale heartbeat, an idle
	protocol that errors -- reads *indeterminate*, never *true*. The detector did
	not observe a human and must not claim it did. The machine still stays awake,
	because doubt vetoes the sleep actions on its own; the difference is that
	*idlectl doctor* can then say "the agent is dead" instead of "somebody is at
	the machine", and exits non-zero saying it.

*after_resume*
	At least one resume from sleep has happened this boot. True from the first
	resume until the next cold boot: it does not expire when the settle window
	does. Pair it with *clock = "resume"* to build that window; see *COMPOSITION*.

*remote_session*
	A remote login session is open. *unavailable* where there is no session
	manager.

*lease_held*
	At least one unexpired lease is held. See *idlectl*(1).

*inhibitor_block*
	A logind inhibitor lock of mode *block* is held against sleep or shutdown.

	Read as a signal we choose to honour, never as enforcement. Two measured
	reasons. *systemctl poweroff* invoked without a controlling terminal returns 0
	and ignores inhibitors and open sessions entirely, so no guarantee can rest on
	them. And Steam declares no inhibitor at all while downloading: with a 150 GB
	download in flight, the only locks present were NetworkManager's and UPower's,
	both mode *delay*, plus a power-key handler. An inhibitor-based design cannot
	see a download at all.

	Mode *delay* locks are deliberately not reported: NetworkManager and UPower hold
	them permanently on an ordinary desktop.

*media_playing*
	An MPRIS player on a session bus reports *PlaybackStatus* of *Playing*.
	Desktop power-inhibition APIs are deliberately not a source for this fact.
	*unavailable* where no session bus is reachable.

*steam_game_running*
	A Steam application is running -- not merely the Steam client. Detected from
	the launcher wrapper signature, verified against a real installation, or from a
	microcompositor game session. *unavailable* where Steam is not installed.

*steam_downloading*
	Steam is downloading or verifying content: something under the staging tree has
	been written within the download window. Separate from the above because the two
	want different answers per action.

	It is self-extinguishing and carries no timeout of its own: it stops being true
	shortly after the writes stop. It is *not* a network-throughput threshold.

*gpu_busy_game*
	At least 512 MiB of GPU memory is held by a process in the ancestry of a
	running game. Attribution is by process ancestry, never by an allowlist of
	executable names, and a tracked service process is never attributed to a game.

	The threshold is a compiled-in constant and is deliberately not configurable:
	there is no byte-size value anywhere in this format, and one number that is the
	same on every machine is one number that cannot be got wrong on yours.

*gpu_busy_other*
	The same amount of GPU memory held by a process that is *not* attributable to a
	game. The compositor is excluded by name, not merely by the threshold.

	Separate from *gpu_busy_game* because a compositor at eight per cent is not a
	reason to keep a machine awake and a game at ninety per cent is: the split is
	what lets a game keep a finite floor while unattributed load keeps its own.

	Both read *unavailable* where there is no GPU query interface.

*local_service_busy*
	A long-running local service has actually served something recently.

	Busy, not running. A local model server can sit up all night holding twelve
	gigabytes of VRAM and serving nothing. "The service is up" describes a process
	that exists, not a machine in use, and treating the two as the same means the
	machine never sleeps again. This fact is driven by cumulative counters sampled
	between evaluations, and is true only if they moved. Whether the unit is active,
	what its instantaneous state is and whether it answers a liveness probe are all
	deliberately not sources.

	*This fact ships disabled*, and is the only one that does. It has nothing to
	read until it is given a *counters_url*, and a detector that shipped enabled
	with nothing to read would report *indeterminate* -- which vetoes -- on every
	machine from the moment the package was installed. Configured counters that
	cannot be read do read *indeterminate*; the first sample after the daemon starts
	counts as in use.

# DROP-INS

Files matching \*.toml in /etc/idlectl/conf.d are applied in ascending byte-wise
basename order after /etc/idlectl/idlectl.toml, the way systemd orders its own
drop-ins. A drop-in therefore overrides the administrator file as well as the vendor
one.

Merging happens per key, not per block. Given a vendor block of

```
[while.always]
```
	screen_off = "10m"
	suspend = "30m"
	poweroff = "never"

a drop-in of

```
[while.always]
```
	suspend = "45m"

changes the suspend timeout and leaves *screen_off* and *poweroff* alone.

There is no vendor drop-in directory. Everything the package ships is in the single
file at layer 1, so there is nothing to shadow with an empty file: switch a vendor
block off with *enabled = false*, or a vendor detector off with *[facts.NAME]
enabled = false*.

# EXAMPLES

Wait an hour before suspending, and never power off:

	# /etc/idlectl/idlectl.toml
```
[while.always]
```
	suspend = "1h"
	poweroff = "never"

Keep a machine awake while a self-hosted service is being used, but not merely
because it is running -- the detector has to be turned on and told where to read:

```
[facts.local_service_busy]
```
	enabled = true
	counters_url = "http://localhost:8080/metrics"

```
[while.local_service_busy]
```
	suspend = "never"

Give a shared machine a hard cap: no lease may hold it awake for more than half a
day. Note that this is a *[when]* block, so it is absolute -- it will suspend the
machine even if somebody is typing, and it is logged every time it is loaded and
every time it fires.

```
[when.lease_held]
```
	clock = "condition"
	suspend = "12h"

Switch off a vendor block:

```
[while.media_playing]
```
	enabled = false

Switch off a detector that is broken on this machine, so that it stops vetoing:

```
[facts.gpu_busy_other]
```
	enabled = false

# NOTES

## WHY NOT logind's IdleActionSec

*logind.conf*(5) can suspend an idle machine on its own. Its timer is driven by the
session's *IdleHint* property, and that property was measured permanently *no* on a
Plasma 6 Wayland session: no desktop component ever set it. An idle action keyed off
it would never have fired once.

This refutation is written down so that nobody later replaces this project with a
setting that does not work.

## WHY NOT THE DESKTOP'S OWN AUTO-SUSPEND

A desktop environment's auto-suspend timer was measured not to re-arm after a resume:
the machine woke up and nothing was counting any more.

More fundamentally, two owners of the power state is the architectural bug this
project exists to remove. Let the desktop own the things it should own (what the
power button does, the screen lock) and let one thing decide when the machine sleeps.

## POWEROFF DEFAULTS TO never

On the machine this policy was extracted from, an unattended poweroff hung roughly
one time in four: shutdown began, something never finished, and the machine sat
drawing power with no display and no network until somebody held the button down.
Suspend, on the same hardware over the same period, resumed reliably in seconds.

A machine you must revive by hand is worse than one that stayed awake, and remote
wake, the whole reason to power off, is exactly what a hung shutdown takes away. If
you enable it, be sure you can reach the machine physically -- and remember that
idlectl does not escalate, so enabling it does not give you "suspend first, then
power off later".

# SEE ALSO

*idlectl*(1), *idlepolicyd*(8), *systemd-logind.service*(8), *logind.conf*(5),
*org.freedesktop.login1*(5)

idlectl is a *policy* engine. For CPU frequency scaling and power profiles, which are
a different problem entirely, see *tlp*(8), *tuned*(8) or *powerprofilesctl*(1).
