//! `gpu_busy_game` and `gpu_busy_other`: one reading of the GPU, two independent facts.
//!
//! # Why two facts and not one
//!
//! [FACT-27]. A lightweight 2D title holds no compute memory at all, which once led to the
//! confident and wrong conclusion that games never trigger a GPU fact. A GPU-heavy title
//! under a translation layer holds up to ~10 GiB. With a single undifferentiated
//! `gpu_busy` used as a `never` floor, the deliberate *soft* treatment of a running game
//! could never take effect for exactly the games it was written for — the "I fell asleep
//! with the game on" case would never rest. Splitting the reading by attribution is what
//! lets `[while.gpu_busy_game]` carry the same finite timeout as `[while.
//! steam_game_running]` while `[while.gpu_busy_other]` keeps a policy of its own.
//!
//! Neither fact is computed by subtracting inside the other. One walk of the processes
//! produces two answers.
//!
//! # Two sources, MERGED — not a fallback chain
//!
//! DRM `fdinfo` is generic (amdgpu, i915, xe, nouveau), is read from `/proc` with no fork
//! -- by the session agent rather than by this daemon, for the permission reason below --
//! and is what the graphical process monitors use. The proprietary NVIDIA driver publishes
//! `drm-driver` and a client id and **no memory accounting at all**, so it can only be
//! read through `nvidia-smi` — a fork and roughly 100 ms, which is why it is awaited
//! rather than blocking the reactor that owns the machine's power decision.
//!
//! The obvious structure is "try fdinfo, fall back to nvidia-smi". It is wrong, and
//! measurably so on exactly the hardware this project targets. On a hybrid machine — an
//! AMD integrated GPU and a discrete NVIDIA card, which is most gaming laptops and plenty
//! of desktops — the integrated GPU publishes fdinfo keys for trivia like a browser helper
//! holding 24 KiB, the fallback chain concludes the generic source works, and the card the
//! games actually run on is never asked. Both facts then read false forever, with a game
//! running. A detector that is never true is worse than no detector, because it looks like
//! one.
//!
//! So both are read and merged per process, taking the larger reading rather than the sum:
//! a process can legitimately appear on two GPUs, and the threshold asks "does this process
//! hold half a gigabyte somewhere", not "how much in total".
//!
//! Absent both, the facts are `UNAVAILABLE`, which is knowledge and behaves as false.
//! Present but failing is `INDETERMINATE`, which vetoes. `nvidia-smi` reporting *no
//! devices* is `UNAVAILABLE`, not a failure: a machine with the driver package installed
//! and no card in it must still be allowed to sleep.
//!
//! # Why only one of those two sources is read here
//!
//! The `fdinfo` half is **reported by the session agents** and arrives in the
//! [`Context`](super::Context); only `nvidia-smi` is run from this process. A process's
//! `fdinfo` directory is mode 0555 and looks world-readable, but every open is gated by
//! `ptrace_may_access`, so reading **another user's** needs `CAP_SYS_PTRACE`. Measured:
//! with this daemon's `CapabilityBoundingSet=CAP_DAC_READ_SEARCH` the read is denied, and
//! with `CAP_SYS_PTRACE` the same read succeeds. That capability is the ability to read
//! any process's memory, and a daemon that can power a machine off is not getting one in
//! order to attribute video RAM. The agent runs as the session user, reads its own
//! processes with no capability at all, and reports pid, name and bytes.
//!
//! Attribution stays here, and that is the whole division: deciding whether a holder
//! belongs to a game needs the process tree and the command lines, which are
//! world-readable and cost no capability, and an unprivileged process must not be the one
//! that decides what counts as a game. The same split as `media_playing` -- the agent
//! observes what only it can observe, the daemon decides what it means.
//!
//! With no agent running, the `fdinfo` source contributes nothing, exactly as if no DRM
//! device published memory accounting. Never `INDETERMINATE`: an absent agent is already
//! reported once through `human_active`, and vetoing twice for one cause would hold every
//! sleep action on a headless machine forever -- where `nvidia-smi`, which needs no
//! session, still answers.

use idlectl_policy::FactState;

use super::Reading;
use crate::facts::steam;
use crate::proc;

/// The per-process GPU memory threshold, in bytes.
///
/// [FACT-25]: **compiled in, not configurable**, and no `gpu_min_mem` key may be added
/// without a config-format major version bump. A threshold an operator can lower to zero
/// is a permanent veto waiting to be typed.
const THRESHOLD_BYTES: u64 = 512 * 1024 * 1024;

/// Compositors and desktop shells, excluded **by name** as well as by the threshold.
///
/// [FACT-26]: measured, the desktop compositor appears in compute-process listings holding
/// roughly 20 MiB. That is below any sane threshold — and it is named here anyway, so that
/// the threshold is not the only thing standing between a user and a permanent veto. A
/// future compositor that holds 600 MiB would otherwise wedge every machine that runs it.
///
/// `gamescope` is deliberately **absent** from this list: it is the microcompositor a game
/// session runs under, so its memory is a game's memory and it must be attributed, not
/// discarded.
const DESKTOP_PROCESSES: [&str; 14] = [
    "kwin_wayland",
    "kwin_x11",
    "plasmashell",
    "kded6",
    "ksmserver",
    "Xwayland",
    "Xorg",
    "gnome-shell",
    "mutter",
    "sway",
    "Hyprland",
    "wayfire",
    "labwc",
    "cosmic-comp",
];

/// The two facts one GPU reading produces.
pub struct Split {
    pub game: Reading,
    pub other: Reading,
}

/// One process holding GPU memory.
///
/// Public because the session agents report these and the registry stores them until an
/// evaluation asks for them. The bytes are raw: the 512 MiB threshold and the desktop
/// exclusion are applied here, in [`read`], and never by whoever reported the holder.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Holder {
    pub pid: u32,
    pub name: String,
    pub bytes: u64,
}

/// Reads the GPU and splits the result by attribution.
///
/// `reported`, `game_running` and `service` are inputs rather than things this function
/// derives: the DRM `fdinfo` holders can only be read inside a session (see the module
/// note), and attribution needs to know what a game is ([FACT-28]) and what a tracked
/// service is ([FACT-29]) -- all three are already computed next door.
///
/// There is deliberately no `Context` parameter even though `reported` comes from one:
/// [FACT-25] pins the threshold as a compiled-in constant, so these two facts have nothing
/// an operator can configure and nothing to read a setting from.
pub async fn read(reported: &[Holder], game_running: bool, service: &Reading) -> Split {
    let procs = proc::snapshot();
    if procs.is_empty() {
        let doubt = Reading::doubt("/proc could not be read");
        return Split {
            game: doubt.clone(),
            other: doubt,
        };
    }

    // BOTH sources, merged. Not "fdinfo, falling back to nvidia-smi" -- that was the
    // first implementation and it is wrong on exactly the machines this project targets.
    //
    // Measured on a hybrid box: an AMD integrated GPU publishes DRM fdinfo (a browser
    // helper holding 24 KiB of VRAM on it), while the discrete NVIDIA card publishes
    // none and answers only through nvidia-smi. A fallback chain sees the amdgpu keys,
    // concludes the generic source works, and never asks the card that the games actually
    // run on. Both GPU facts then read false forever, on a machine with a game running.
    //
    // A detector that is never true is worse than no detector, because it looks like one.
    let mut holders = Vec::new();
    let mut sources = Vec::new();

    // Empty means the source saw nothing: no agent is running, or the DRM devices publish
    // no memory accounting. Both are the absence of a reading and neither is doubt, so the
    // source is simply not listed -- exactly what happened before when the walk lived here
    // and returned `None`.
    if !reported.is_empty() {
        sources.push("DRM fdinfo via the session agent");
        holders.extend(reported.iter().cloned());
    }

    match read_nvidia_smi().await {
        NvidiaResult::Holders(found) => {
            sources.push("nvidia-smi");
            merge(&mut holders, found);
        }
        // No such tool, or a tool that reports no devices. Both are knowledge, and
        // neither is doubt: a machine with the driver package installed and no card in it
        // must still be allowed to sleep.
        NvidiaResult::Absent => {}
        NvidiaResult::Failed(why) => {
            // Present, has devices, and not answering. Doubt, never false: a GPU query
            // that used to work and has stopped is precisely the state in which the
            // daemon must not decide that nothing is running.
            let doubt = Reading::doubt(format!("nvidia-smi did not answer: {why}"));
            return Split {
                game: doubt.clone(),
                other: doubt,
            };
        }
    }

    if sources.is_empty() {
        let absent = Reading::absent("no GPU query interface (no DRM fdinfo, no nvidia-smi)");
        return Split {
            game: absent.clone(),
            other: absent,
        };
    }
    let source = sources.join(" + ");

    let game_pids = steam::game_pids(&procs);
    let service_running =
        service.state == FactState::True || service.state == FactState::Indeterminate;

    let mut game_evidence = Vec::new();
    let mut other_evidence = Vec::new();

    for holder in holders {
        if holder.bytes < THRESHOLD_BYTES {
            continue;
        }
        if DESKTOP_PROCESSES.contains(&holder.name.as_str()) {
            continue;
        }

        let mib = holder.bytes / (1024 * 1024);
        let line = format!("{} (pid {}, {mib} MiB)", holder.name, holder.pid);

        // [FACT-29]: a process belonging to a tracked service is NEVER attributed to a
        // game, even where ancestry would allow it. Implemented as "lives in the system
        // slice", which is generic and true by construction for anything systemd started.
        // The first implementation of this rule attributed a model server's 11 GiB to a
        // running game: it opened no hole, because the service's own fact vetoed
        // independently, but it wrote a false line into the log.
        let is_service = service_running && in_system_slice(holder.pid);

        if !is_service && game_running && proc::descends_from(&procs, holder.pid, &game_pids) {
            game_evidence.push(line);
        } else {
            other_evidence.push(line);
        }
    }

    Split {
        game: verdict(game_evidence, &source, "attributable to a running game"),
        other: verdict(other_evidence, &source, "not attributable to a game"),
    }
}

/// Folds a second source into the first, keeping the larger reading per process.
///
/// Max rather than sum: on a machine with two GPUs a process can legitimately appear in
/// both listings, and adding the two would report memory it does not hold. The threshold
/// is per process and per [FACT-25], so the question is "does this process hold half a
/// gigabyte somewhere", not "how much in total".
fn merge(into: &mut Vec<Holder>, extra: Vec<Holder>) {
    for holder in extra {
        match into.iter_mut().find(|h| h.pid == holder.pid) {
            Some(existing) => existing.bytes = existing.bytes.max(holder.bytes),
            None => into.push(holder),
        }
    }
}

fn verdict(evidence: Vec<String>, source: &str, what: &str) -> Reading {
    if evidence.is_empty() {
        Reading::no(format!("no process over 512 MiB {what} ({source})"))
    } else {
        Reading::yes(format!("{} [{source}]", evidence.join("; ")))
    }
}

/// Whether a pid lives in a systemd **system** service.
///
/// Best effort by design: an unreadable cgroup file means the process is gone or is not
/// ours to inspect, and answering "not a service" there sends the memory to the
/// attribution path, which is the conservative direction — `gpu_busy_other` keeps the
/// stricter policy of the two.
fn in_system_slice(pid: u32) -> bool {
    std::fs::read_to_string(format!("/proc/{pid}/cgroup"))
        .map(|s| s.contains("/system.slice/"))
        .unwrap_or(false)
}

enum NvidiaResult {
    Holders(Vec<Holder>),
    /// No such tool on this machine.
    Absent,
    /// The tool exists and did not answer.
    Failed(String),
}

async fn read_nvidia_smi() -> NvidiaResult {
    let output = async_process::Command::new("nvidia-smi")
        .args([
            "--query-compute-apps=pid,process_name,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .await;

    let output = match output {
        Ok(o) => o,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return NvidiaResult::Absent,
        Err(err) => return NvidiaResult::Failed(err.to_string()),
    };
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        // The driver package installed with no card in the machine, or a card the driver
        // cannot talk to. That is knowledge -- there is no NVIDIA GPU to be busy -- and
        // treating it as doubt would freeze every such machine awake permanently, which
        // includes every machine where somebody installed the drivers speculatively.
        if stderr.contains("No devices were found")
            || stdout.contains("No devices were found")
            || stderr.contains("NVIDIA-SMI has failed")
        {
            return NvidiaResult::Absent;
        }
        return NvidiaResult::Failed(format!("exit {}: {}", output.status, stderr.trim()));
    }

    match parse_nvidia_csv(&String::from_utf8_lossy(&output.stdout)) {
        Ok(holders) => NvidiaResult::Holders(holders),
        Err(line) => NvidiaResult::Failed(format!("unreadable output line: {line}")),
    }
}

/// Parses `pid, process_name, used_memory` with `--nounits`, so the memory is bare MiB.
///
/// A line that does not parse is an error rather than a skip: the whole reading is what
/// decides whether the machine may sleep, and quietly dropping the one line that happened
/// to be malformed would drop the process that was using the GPU.
fn parse_nvidia_csv(stdout: &str) -> Result<Vec<Holder>, String> {
    let mut out = Vec::new();
    for line in stdout.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split(',').map(str::trim).collect();
        let [pid, name, mib] = fields.as_slice() else {
            return Err(line.to_owned());
        };
        let (Ok(pid), Ok(mib)) = (pid.parse::<u32>(), mib.parse::<u64>()) else {
            return Err(line.to_owned());
        };
        out.push(Holder {
            pid,
            name: name.rsplit('/').next().unwrap_or(name).to_owned(),
            bytes: mib.saturating_mul(1024 * 1024),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nvidia_csv_is_parsed_in_mib() {
        let holders = parse_nvidia_csv("1234, /usr/bin/game, 10240\n5678, /usr/lib/other, 300\n")
            .expect("well-formed");
        assert_eq!(holders.len(), 2);
        assert_eq!(holders[0].name, "game");
        assert_eq!(holders[0].bytes, 10240 * 1024 * 1024);
        assert!(holders[0].bytes > THRESHOLD_BYTES);
        assert!(holders[1].bytes < THRESHOLD_BYTES);
    }

    /// A malformed line must fail the whole reading rather than be skipped: the skipped
    /// line is exactly the one that might have been holding the GPU.
    #[test]
    fn a_malformed_nvidia_line_is_a_fault_not_a_skip() {
        assert!(parse_nvidia_csv("1234, /usr/bin/game, not-a-number\n").is_err());
        assert!(parse_nvidia_csv("only-one-field\n").is_err());
    }

    /// The two sources are merged per process by taking the larger reading, never the sum.
    /// A process can legitimately appear on two GPUs, and adding the two would report
    /// memory it does not hold -- [FACT-25] asks "does this process hold half a gigabyte
    /// somewhere", not "how much in total".
    #[test]
    fn a_process_seen_by_both_sources_is_not_counted_twice() {
        let mut holders = vec![Holder {
            pid: 42,
            name: "game".into(),
            bytes: 600 * 1024 * 1024,
        }];
        merge(
            &mut holders,
            vec![
                Holder {
                    pid: 42,
                    name: "game".into(),
                    bytes: 700 * 1024 * 1024,
                },
                Holder {
                    pid: 43,
                    name: "other".into(),
                    bytes: 100 * 1024 * 1024,
                },
            ],
        );
        assert_eq!(holders.len(), 2);
        assert_eq!(
            holders[0].bytes,
            700 * 1024 * 1024,
            "the larger, not the sum"
        );
        assert_eq!(holders[1].pid, 43);
    }

    /// [FACT-26], asserted rather than assumed: the microcompositor is a game's process
    /// and must never be filtered out with the desktop shells.
    #[test]
    fn the_microcompositor_is_not_treated_as_desktop_furniture() {
        assert!(!DESKTOP_PROCESSES.contains(&"gamescope"));
        assert!(DESKTOP_PROCESSES.contains(&"kwin_wayland"));
    }

    #[test]
    fn the_threshold_is_512_mib() {
        // [FACT-25] pins this value in the specification. If it moves, the specification
        // moves with it, and that is a config-format major version bump.
        assert_eq!(THRESHOLD_BYTES, 536_870_912);
    }
}
