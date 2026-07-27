idlectl(1) "idlectl" "General Commands Manual"

# NAME

idlectl - inspect and control this machine's idle and power policy

# SYNOPSIS

*idlectl* status [--json]++
*idlectl* explain [_action_] [--json]++
*idlectl* doctor [--json]++
*idlectl* rest [_action_] [--now] [--force --why _text_]++
*idlectl* lease acquire _id_ [--ttl _duration_] [--why _text_]++
*idlectl* lease release _id_++
*idlectl* lease list [--json]++
*idlectl* reload++
*idlectl* check-config [_file_...] [--json]

# DESCRIPTION

*idlectl* is the client for *idlepolicyd*(8), the daemon that decides when this
machine may blank its screen, suspend, hibernate or power off.

It is a *policy* tool. For CPU frequency scaling and power profiles, which are a
different problem, see *tlp*(8), *tuned*(8) or *powerprofilesctl*(1).

An _action_ is one of *screen_off*, *suspend*, *hibernate* or *poweroff*.

# COMMANDS

*status*
	What the daemon currently believes, and what it will do next.

*explain* [_action_]
	Why the machine is or is not allowed to perform an action. Prints the composed
	deadline, the clock each contributing block was measured on, and every block
	holding the action back. With no argument, explains all four actions.

	It also names the reasons that are *not* blocks: every fact currently reading
	*indeterminate*, and every configuration file that had to be rejected. Those
	hold the three sleep actions at *never* on their own, and a machine wedged awake
	by one of them has nothing in its configuration to point at.

	This is the command for "why did my machine not sleep last night". It needs no
	authentication, deliberately: a diagnostic that prompts is a diagnostic nobody
	runs.

*doctor*
	Which detectors work on this machine, which capabilities are absent, and who
	else is trying to own the power state.

	The distinction it exists to show is *unavailable* against *indeterminate*. A
	fact whose capability is missing, or that has been switched off with
	*[facts.NAME] enabled = false*, reads *unavailable* and behaves as false; a
	fact whose detector is broken reads *indeterminate* and vetoes suspend,
	hibernate and poweroff whether or not anything in the configuration mentions it.
	A machine that will not sleep and gives no reason usually has one of the second
	kind. See *idlectl.toml*(5).

	It also reports, as potential second owners of the power state: logind's
	configured *IdleAction* and *IdleActionSec*, any process owning a known desktop
	power-management bus name, and any running idle helper it can detect
	(*swayidle*, *hypridle*, *xautolock*). Two owners of the power state is the bug
	this project exists to remove, and the first step is being able to see them.

	Every *[when]* block in the effective configuration is listed as a standing
	hazard, because a ceiling can make the machine sleep against every other rule
	in the file.

	Exits non-zero when any enabled fact is *indeterminate*, when any configuration
	file was rejected, or when an action the configuration relies on is
	unavailable.

*rest* [_action_]
	Ask the machine to rest now. With no _action_ this is always *suspend* -- the
	shallowest resting state, the one that loses nothing. *hibernate* and
	*poweroff* end or risk ending a session, so they are never chosen on somebody's
	behalf and must be named:

		idlectl rest --action poweroff

	Without *--now*, this is a request evaluated against the policy exactly as the
	daemon's own timer would evaluate it.

*lease acquire* _id_
	Take a lease: the machine will not sleep while it is held. This is the honest
	way to say "I am working" before starting a long job, rather than hoping a
	detector notices.

	The lease is bounded twice: it is released when the handle is closed, so a job
	that crashes cannot pin the machine awake forever, and it expires at its
	*--ttl* (default *1h*) if the holder survives but forgets.

*lease release* _id_
	Release a lease early.

*lease list*
	The leases currently held, with who took them, why, and when they expire.

*reload*
	Re-read the configuration without restarting the daemon.

	A file that cannot be parsed is dropped as a whole and named in the error, with
	the key and the text that could not be read; the layers that still parse take
	effect, and the three sleep actions are held at *never* until the fault is
	fixed. The daemon never ends up with no policy at all: that would leave the
	machine with no owner of its power state, which is the condition this project
	exists to remove. See *idlepolicyd*(8).

*check-config* [_file_...]
	Parse, merge and validate configuration files, and exit non-zero if anything
	was rejected. Needs no daemon and no privileges, so it can be run before
	installing anything and it never touches the running system.

	With no arguments it checks the layers the daemon would read, in precedence
	order: the vendor default, /etc/idlectl/idlectl.toml if present, and every
	drop-in in ascending byte-wise basename order. With arguments it checks exactly
	those files, in the order given.

# OPTIONS

*--now*
	For *rest*. Treats exactly two blocks as satisfied -- the one whose condition is
	*always* and the one whose condition is *after_resume* -- as if their timeouts
	had elapsed, including where their value is *never*. The base schedule and the
	post-resume settle window, and nothing else.

	The set is fixed by condition name. There is no key in *idlectl.toml*(5) that
	adds a block to it, so an administrator cannot enlarge it by accident.

	*It does not collapse condition blocks.* A running game, an active download, a
	held lease and an open remote session are all still evaluated normally, each on
	its own clock, and any of them can still refuse. Neither does it satisfy the
	vetoes that are not blocks: an *indeterminate* fact and a rejected configuration
	file both still hold. This is what a remote relay sends when it has finished
	with a machine; it must never be able to suspend one that is in the middle of
	something.

	A refusal is a normal outcome, not an error. Run *idlectl explain* to see what
	refused.

*--force*
	For *rest*. Installs a ceiling due immediately -- a *[when]* block for that one
	action, for the lifetime of that one request. Ceilings are absolute, so this
	defeats, without special cases: every *never*, the *human_active* floor, the
	veto raised by a fact reading *indeterminate*, and the veto raised by a
	configuration file that had to be rejected.

	Those last two are what it is usually reached for. A machine wedged awake by a
	dead session agent or a typo in a drop-in has nothing in its configuration to
	switch off, and *--now* cannot help it; this can.

	It overrides *policy*, never *mechanism*: one power transition at a time still
	holds, a refusal from logind is still logged rather than retried blindly, and
	the facts are still re-read immediately before acting.

	Deliberately separate from *--now*, deliberately named differently, gated by its
	own polkit action (*io.github.ericcanas.Idlectl1.rest-forced*), and always
	recorded in the journal at warning level, naming the requester and every floor
	it defeated with that floor's reason. *--why* is mandatory. This is the only way
	to suspend a machine somebody is actively typing on.

*--ttl* _duration_
	For *lease acquire*. How long the lease lives if nothing releases it. Default
	*1h*.

*--why* _text_
	Reason, recorded in the journal and shown by *lease list*.

*--json*
	Machine-readable output.

# ENVIRONMENT

*IDLECTL_LOG*
	Log filter for *idlepolicyd*(8) and *idlectl-agent*, in the usual
	directive syntax (*info*, *debug*, *idlepolicyd=trace*). Set it in a systemd
	drop-in rather than globally.

# FILES

/usr/lib/idlectl/idlectl.toml
	Vendor default, and layer 1 of the policy the daemon reads: a machine with an
	empty /etc is already configured. Belongs to the package; replaced on upgrade.
	Do not edit.

/etc/idlectl/idlectl.toml
	Administrator configuration, layer 2. Never created, modified or removed by the
	package.

/etc/idlectl/conf.d/\*.toml
	Drop-ins, layer 3: applied in ascending byte-wise basename order, after
	/etc/idlectl/idlectl.toml, so a drop-in overrides it.

/run/idlectl
	Runtime state, including held leases. Readable by unprivileged clients.

# EXIT STATUS

*0*
	Success. For *rest*, this means the request was accepted; it does not by itself
	mean the machine acted, because a condition block may still hold it back.

*1*
	Failure: the daemon is unreachable, authorisation was refused, or a
	configuration file was rejected. The reason is printed on standard error.

	Also the exit status of *doctor* when it found something wrong -- an enabled
	fact reading *indeterminate*, a rejected configuration file, or an action the
	configuration relies on that this machine cannot perform. The report is still
	printed; the status is what makes it usable from a script.

# EXAMPLES

Find out why a machine will not sleep:

	idlectl explain suspend

Hold a machine awake for a long job, and let it rest again afterwards:

	idlectl lease acquire nightly-build --ttl 6h --why "training run"
	# ... work ...
	idlectl lease release nightly-build
	idlectl rest --now

Check a configuration change before applying it:

	idlectl check-config /etc/idlectl/conf.d/50-local.toml

Suspend a machine somebody is using, on purpose and on the record:

	idlectl rest --action suspend --force --why "maintenance window"

# NOTES

Neither *idlepolicyd*(8) nor *idlectl-agent* is enabled by the package. Installing a
program that can suspend a machine is not the same act as permitting it to:

	systemctl enable --now idlepolicyd.service
	systemctl --user enable --now idlectl-agent.service

Before enabling, turn off your desktop environment's own automatic suspend. Two
owners of the power state is the bug this project exists to remove, and it is not
this project's place to disable another one behind your back.

# SEE ALSO

*idlepolicyd*(8), *idlectl.toml*(5), *systemd-logind.service*(8), *loginctl*(1)
