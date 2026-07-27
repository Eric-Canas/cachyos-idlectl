# fish completion for idlectl.
#
# Hand-written rather than generated: generating would mean running the freshly built
# binary during packaging, which cross-building forbids.

set -l commands status explain doctor rest lease reload check-config

# No file completion anywhere except check-config, which is the only command that takes
# a path. Offering the whole filesystem after `idlectl rest` is noise.
complete -c idlectl -f

complete -c idlectl -n "not __fish_seen_subcommand_from $commands" -a status \
	-d "What the daemon believes and what it will do next"
complete -c idlectl -n "not __fish_seen_subcommand_from $commands" -a explain \
	-d "Why an action is or is not allowed, in full"
complete -c idlectl -n "not __fish_seen_subcommand_from $commands" -a doctor \
	-d "Which detectors work here, and what else owns this power state"
complete -c idlectl -n "not __fish_seen_subcommand_from $commands" -a rest \
	-d "Ask the machine to rest now"
complete -c idlectl -n "not __fish_seen_subcommand_from $commands" -a lease \
	-d "Hold or inspect leases: 'I am working, do not sleep'"
complete -c idlectl -n "not __fish_seen_subcommand_from $commands" -a reload \
	-d "Re-read the configuration without restarting"
complete -c idlectl -n "not __fish_seen_subcommand_from $commands" -a check-config \
	-d "Validate configuration files without contacting the daemon"

complete -c idlectl -n "__fish_seen_subcommand_from status doctor" -l json \
	-d "Emit machine-readable JSON"

complete -c idlectl -n "__fish_seen_subcommand_from explain" -l json \
	-d "Emit machine-readable JSON"
complete -c idlectl -n "__fish_seen_subcommand_from explain" \
	-a "screen_off suspend hibernate poweroff" -d "Action"

# screen_off is deliberately absent from rest: "rest" means change the power state.
complete -c idlectl -n "__fish_seen_subcommand_from rest" -l action -x \
	-a "suspend hibernate poweroff" -d "Which action to request (default: suspend)"
complete -c idlectl -n "__fish_seen_subcommand_from rest" -l force \
	-d "Override every block, including human presence -- requires --why"
complete -c idlectl -n "__fish_seen_subcommand_from rest" -l why -x \
	-d "Reason recorded in the journal"

complete -c idlectl -n "__fish_seen_subcommand_from check-config" -l json \
	-d "Emit machine-readable JSON"
complete -c idlectl -n "__fish_seen_subcommand_from check-config" -F -a "*.toml"

complete -c idlectl -n "__fish_seen_subcommand_from lease; and not __fish_seen_subcommand_from acquire release list" \
	-a acquire -d "Take a lease and hold it"
complete -c idlectl -n "__fish_seen_subcommand_from lease; and not __fish_seen_subcommand_from acquire release list" \
	-a release -d "Release a lease early"
complete -c idlectl -n "__fish_seen_subcommand_from lease; and not __fish_seen_subcommand_from acquire release list" \
	-a list -d "List the leases currently held"

complete -c idlectl -n "__fish_seen_subcommand_from acquire" -l ttl -x \
	-d "Time to live, e.g. 30m or 6h"
complete -c idlectl -n "__fish_seen_subcommand_from acquire" -l why -x \
	-d "Reason, shown by lease list"

# The ids actually held, from the daemon. Silent when it is not running, which is the
# correct answer: no daemon means no leases.
complete -c idlectl -n "__fish_seen_subcommand_from release" -x -a \
	"(idlectl lease list --json 2>/dev/null | string match -r '\"who\"\s*:\s*\"([^\"]*)\"' | string replace -r '.*\"([^\"]*)\"\$' '\$1')"

complete -c idlectl -n "__fish_seen_subcommand_from list" -l json \
	-d "Emit machine-readable JSON"
