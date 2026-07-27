idlepolicyd(8) "idlectl" "System Manager's Manual"

# NAME

idlepolicyd - idle and power policy daemon

# SYNOPSIS

*idlepolicyd* [--vendor-config _path_] [--config _path_] [--config-dir _dir_] [--check] [--dry-run]

# DESCRIPTION

*idlepolicyd* decides when a machine may blank its screen, suspend, hibernate or
power off, and asks *systemd-logind* to do it.

It replaces the *policy* half of a desktop's power management: the timers, the
conditions and the arbitration. It does not replace the mechanism. Suspending,
hibernating and powering off remain logind's job, which is why this daemon runs with
an empty capability set.

It is not a CPU tuning tool. Frequency scaling and power profiles belong to *tlp*(8),
*tuned*(8) or *powerprofilesctl*(1), and *idlepolicyd* does not touch them.

The policy itself is documented in *idlectl.toml*(5), which is the page to read
first. This page covers running the daemon.

## THE NAME

The daemon is *idlepolicyd* and the client is *idlectl*(1). They differ because
*/usr/bin/idled* is already taken by cyrus-imapd. The same split exists between
*powerprofilesctl*(1) and *power-profiles-daemon*.

# OPTIONS

*--vendor-config* _path_
	Vendor default configuration. Default */usr/lib/idlectl/idlectl.toml*. This is
	layer 1 of the policy the daemon actually reads, not an example: a machine with
	an empty /etc is fully configured by it.

	Its absence, or a failure to parse it, is logged at ERROR level naming the
	file. The daemon does not invent a general default and does not carry on as if
	the file had been empty; see *A CONFIGURATION FAULT IS A VETO, NOT AN EXIT*.

*--config* _path_
	Administrator configuration. Default */etc/idlectl/idlectl.toml*. Optional.

*--config-dir* _dir_
	Drop-in directory. Default */etc/idlectl/conf.d*. Files matching \*.toml are
	applied in ascending byte-wise basename order, after the administrator file, so
	a drop-in overrides it. Optional.

*--check*
	Load and validate the configuration, print the resolved policy and exit
	non-zero if anything was rejected. Takes no privileges and does not contact the
	bus. *idlectl check-config* does the same thing from the client.

*--dry-run*
	Evaluate normally and log every decision, but never act on the machine.

# OPERATION

The daemon runs one event loop on one reactor, waiting on the D-Bus system bus, an
inotify watch on the configuration, a udev monitor for input devices, and a timer
armed at the next deadline. On every wakeup it refreshes its facts, takes one clock
snapshot, resolves the policy and performs what has come due: *screen_off* if it is
due, which is not a power-state change and suppresses nothing, and at most one sleep
action -- the *shallowest* one that is due, suspend before hibernate before
poweroff. It does not escalate from a shallow action to a deeper one; see
*idlectl.toml*(5).

One reactor is a deliberate constraint, not an implementation accident. Every source
above is a file descriptor and one reactor drives all of them; a second runtime would
mean a task blocked in one scheduler could stall the loop that decides whether the
machine may sleep.

## STARTUP

The daemon loads and validates its configuration *before* claiming its bus name, and
logs every layer it applied. A file it had to reject is named in the journal together
with the key and the offending text, and the daemon still starts; see the next
section for what it does instead.

## A CONFIGURATION FAULT IS A VETO, NOT AN EXIT

A file that fails to parse, or that carries a value the format does not accept, is
dropped *as a whole* -- per file, not per key, because a half-applied file is a
policy nobody wrote. The remaining layers are kept and evaluation continues.

While any such fault is present:

- *suspend*, *hibernate* and *poweroff* are held at *never*, exactly as doubt about
	a fact holds them;
- *screen_off* is *not* held, so a typo in a drop-in cannot leave a panel lit all
	night;
- *idlectl doctor* exits non-zero, naming the file, the key and the text it could
	not read.

This is deliberately not "refuse to start". The case that decides it is the first
start after a bad edit: there is no previous policy to keep, so a daemon that exited
would leave the machine with no owner of its power state at all, which is the
condition this project exists to remove. A machine that will not sleep and says
loudly why is the cheap failure.

One case is left over: if the *vendor* layer itself is unusable, there is no policy
to degrade from. The daemon then applies a compiled-in fallback of exactly one block,
and nothing else, so that a broken package still cannot leave a panel lit
indefinitely:

```
[while.always]
```
	clock = "human_input"
	screen_off = "15m"

It says so at ERROR level when it does, naming the vendor file it could not read.
Nothing suspends in that state, because there is no sleep timeout to reach.

## RELOAD

	systemctl reload idlepolicyd.service

Rejected files are handled exactly as at startup: they are dropped, named in the
journal, and become a veto on the three sleep actions until they are fixed. Layers
that still parse take effect. The daemon never ends up with no policy at all.

## HOW screen_off IS PERFORMED

Sleep, hibernation and poweroff are logind's job. Blanking a display is not: logind
has no screen-off mechanism, and on a Wayland session only the compositor, or
whatever holds DRM master, can turn an output off.

So *idlectl-agent* -- and only it -- exposes *Blank* and *Unblank* on the system bus,
callable by uid 0 alone, and the daemon invokes them. This is the one and only thing
the session agent does that is not reporting a fact: it commands nothing but the
screen, only inside its own session, and only when the caller is the daemon. It
cannot suspend, hibernate or power off a machine, and no configuration can give it
those.

Where no agent is running in a graphical session, or the session type offers no
blanking mechanism, *screen_off* is *unavailable as an action*: the daemon never
proposes it, every block that sets a *screen_off* key is inert, and *idlectl doctor*
says so and names them. It is not silently treated as done.

## NO CONDITION DIRECTIVES

The shipped unit carries no *Condition\*=* or *Assert\*=* directives, deliberately.

A unit whose condition does not hold is skipped *silently*: systemd marks it inactive
with result "success" and logs nothing worth finding. Something as ordinary as
*ConditionVirtualization=!container* would mean a user in a container gets no policy,
no error and no clue. If this daemon cannot usefully run it starts, says so at ERROR
level with the actual reason, and exits non-zero.

## WHY NOT systemctl

The daemon never shells out to *systemctl poweroff*. Invoked without a controlling
terminal that command returns 0 and ignores inhibitors and open sessions entirely, so
it cannot be used by anything claiming to arbitrate. Power transitions go to logind
over D-Bus.

# SECURITY

The shipped unit is sandboxed. Three directives are relaxed from their strictest
value, each for a stated reason, and each is commented in the unit itself:

*ProtectHome=read-only*
	Relaxed from *yes*. Attributing a process to a game means resolving
	*/proc/PID/exe*, and a Steam library lives under a user's home directory.
	Hidden paths would make the detector read *indeterminate*, which vetoes, and the
	machine would never suspend for a reason nobody could see.

*ProtectProc=default*
	Relaxed from *invisible*, which hides other users' processes even from root.
	That is precisely what the game and service detectors inspect, and it would fail
	quietly by reporting false.

*PrivateDevices=no*
	Relaxed from *yes*. GPU attribution on NVIDIA goes through NVML, which needs the
	real */dev/nvidia\** nodes; *PrivateDevices=yes* replaces */dev* with pseudo
	devices only and no *DeviceAllow=* can put a node back. A closed *DevicePolicy*
	with a read-only allowlist is used instead.

Note one interaction that is easy to miss: *ProtectClock=yes* also removes
*CAP_SYS_TIME* and *CAP_WAKE_ALARM* from the bounding set. That is acceptable because
escalation from suspend to hibernate is delegated to logind's *suspend-then-hibernate*
rather than reimplemented with a wake alarm. A future version that arms a
*CLOCK_BOOTTIME_ALARM* timer must relax *ProtectClock=* and add *CAP_WAKE_ALARM*
together.

The daemon opens no network socket. *RestrictAddressFamilies=* permits only AF_UNIX
for D-Bus and AF_NETLINK for the udev monitor.

*SystemCallErrorNumber=* is left at its default so that a filtered call kills the
process. Returning *EPERM* instead would turn a sandbox violation into a detector
quietly failing forever with nothing in the journal to explain it.

# D-BUS

	bus name     io.github.ericcanas.Idlectl1
	object path  /io/github/ericcanas/Idlectl1
	interface    io.github.ericcanas.Idlectl1.Manager

The interface definition is installed at
*/usr/share/dbus-1/interfaces/io.github.ericcanas.Idlectl1.xml* so that other
projects can generate bindings without depending on this one.

The namespace is lower-case and hyphen-free permanently: the D-Bus specification
restricts interface name elements to *[A-Za-z0-9_]*, so a hyphenated forge handle
cannot appear literally.

State-changing methods are authorised with polkit against the caller's real session
and uid. There are four action ids, and these strings are the authorization contract:
they are what an administrator writes into a polkit rule and what an automation
credential is granted.

	io.github.ericcanas.Idlectl1.rest          Rest()
	io.github.ericcanas.Idlectl1.rest-forced   RestForced()
	io.github.ericcanas.Idlectl1.lease         AcquireLease()
	io.github.ericcanas.Idlectl1.reload        Reload()

*rest* and *rest-forced* are separate on purpose: a credential that may ask a machine
to rest must not thereby be able to force one that somebody is using. See
*/usr/share/polkit-1/actions/io.github.ericcanas.Idlectl1.policy*, which documents
each action and its default. Local overrides belong in */etc/polkit-1/rules.d/*,
which the package does not touch.

# ENVIRONMENT

*IDLECTL_LOG*
	Log filter, in the usual directive syntax. Default *info*. The shipped unit sets
	it; override it with a drop-in:

		systemctl edit idlepolicyd.service

# FILES

/usr/lib/idlectl/idlectl.toml
	Vendor default configuration, and layer 1 of the policy the daemon reads. See
	*idlectl.toml*(5). *This directory belongs to the package*: nothing writes there
	and nothing should be edited there. There is no vendor drop-in directory.

/etc/idlectl/idlectl.toml, /etc/idlectl/conf.d/\*.toml
	Administrator configuration, layers 2 and 3. Drop-ins are applied after
	/etc/idlectl/idlectl.toml, in ascending byte-wise basename order, so a drop-in
	overrides it. *The package owns nothing here*: it never creates, modifies or
	removes these, so an upgrade cannot revert a local decision and an uninstall
	cannot leave a stale one behind.

/run/idlectl
	Runtime state, including held leases. Preserved across a daemon restart, because
	a lease is a promise that a machine will not sleep during a long job and a
	restart must not silently drop it.

/usr/lib/systemd/system/idlepolicyd.service
	Not enabled by the package.

# EXIT STATUS

Non-zero on any startup failure, with the reason logged at ERROR level. The daemon
never exits silently.

# NOTES

## THIS UNIT IS NOT ENABLED BY THE PACKAGE

No preset is shipped and no install hook enables anything. Installing a program that
can suspend a machine is not the same act as permitting it to.

	systemctl enable --now idlepolicyd.service

Before enabling, turn your desktop environment's automatic suspend off. Two owners of
the power state is the bug this project exists to remove, but removing it is your
decision: leave the desktop owning what it should still own, such as what the power
button does and when the screen locks.

*idlectl doctor* will tell you who else is claiming that job. It reports logind's
configured *IdleAction* and *IdleActionSec*, any process currently owning a known
desktop power-management bus name, and any idle helper it can see running
(*swayidle*, *hypridle*, *xautolock*), each named as a potential second owner of the
power state. It does not disable any of them: that is not this daemon's decision to
make behind your back.

## LAPTOPS

Battery, lid and AC policy are out of scope. Power facts may be reported by
*idlectl doctor* so that they can be seen, but no policy is built on them and lid
events are not handled. On a laptop, leave the desktop or logind owning those.

# SEE ALSO

*idlectl*(1), *idlectl.toml*(5), *systemd-logind.service*(8), *logind.conf*(5),
*systemd.exec*(5), *polkit*(8)
