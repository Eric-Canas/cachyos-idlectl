# bash completion for idlectl
#
# Checked into the tree by hand rather than generated at build time, the way paru does
# it: generating them would mean running the freshly built binary during packaging,
# which cross-building forbids.
#
# The action names here are the frozen four of the specification, and the fact names the
# frozen eleven. Neither list may grow without a config-format major version bump, which
# is what makes hard-coding them safe rather than a maintenance trap.

# SC2207 is the whole idiom of a bash completion: COMPREPLY is an array and compgen
# emits one candidate per line. `mapfile` would be shellcheck-clean and would also break
# on bash 3, which is still what macOS ships and what plenty of people test with.
# shellcheck disable=SC2207
_idlectl() {
	local cur prev words cword
	_init_completion 2>/dev/null || {
		cur="${COMP_WORDS[COMP_CWORD]}"
		prev="${COMP_WORDS[COMP_CWORD-1]}"
		words=("${COMP_WORDS[@]}")
		cword=$COMP_CWORD
	}

	local commands='status explain doctor rest lease reload check-config help'
	local actions='screen_off suspend hibernate poweroff'

	# The subcommand is the first word that is not an option.
	local subcommand='' i
	for (( i = 1; i < cword; i++ )); do
		case "${words[i]}" in
			-*) continue ;;
			*) subcommand="${words[i]}"; break ;;
		esac
	done

	case "$prev" in
		--action)
			# rest never offers screen_off: asking the machine to "rest" by blanking a
			# panel is not a thing anybody means, and the daemon refuses it anyway.
			COMPREPLY=($(compgen -W 'suspend hibernate poweroff' -- "$cur"))
			return
			;;
		--why|--ttl)
			return
			;;
	esac

	if [[ -z $subcommand ]]; then
		COMPREPLY=($(compgen -W "$commands --help --version" -- "$cur"))
		return
	fi

	case "$subcommand" in
		status)  COMPREPLY=($(compgen -W '--json --help' -- "$cur")) ;;
		doctor)  COMPREPLY=($(compgen -W '--json --help' -- "$cur")) ;;
		explain) COMPREPLY=($(compgen -W "$actions --json --help" -- "$cur")) ;;
		rest)    COMPREPLY=($(compgen -W '--action --force --why --help' -- "$cur")) ;;
		reload)  COMPREPLY=($(compgen -W '--help' -- "$cur")) ;;
		check-config)
			if [[ $cur == -* ]]; then
				COMPREPLY=($(compgen -W '--json --help' -- "$cur"))
			else
				COMPREPLY=($(compgen -f -X '!*.toml' -- "$cur"))
			fi
			;;
		lease)
			local leasecmd=''
			for (( i = 1; i < cword; i++ )); do
				case "${words[i]}" in
					acquire|release|list) leasecmd="${words[i]}"; break ;;
				esac
			done
			case "$leasecmd" in
				acquire) COMPREPLY=($(compgen -W '--ttl --why --help' -- "$cur")) ;;
				# The ids of leases actually held, straight from the daemon. Falls back
				# to nothing when it is not running, which is the correct answer: no
				# daemon means no leases.
				release) COMPREPLY=($(compgen -W "$(idlectl lease list --json 2>/dev/null |
					sed -n 's/.*"who"[[:space:]]*:[[:space:]]*"\([^"]*\)".*/\1/p')" -- "$cur")) ;;
				list)    COMPREPLY=($(compgen -W '--json --help' -- "$cur")) ;;
				*)       COMPREPLY=($(compgen -W 'acquire release list' -- "$cur")) ;;
			esac
			;;
	esac
}

complete -F _idlectl idlectl
