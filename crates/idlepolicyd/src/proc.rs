//! Reading `/proc`, with the traps that cost measurements to find.
//!
//! # `comm` is truncated to 15 bytes
//!
//! The kernel stores a task's name in a 16-byte field, so `fossilize_replay` — the shader
//! precompiler a game platform runs immediately before launching a title — appears in
//! `/proc/<pid>/comm` as `fossilize_repla`. Matching on the full name against `comm` finds
//! nothing, ever, and does so silently. Measured: a name-based match for that process
//! returned zero hits while sixteen of them were running. Anything longer than fifteen
//! characters must be matched against `cmdline`.
//!
//! # `cmdline` is NUL-separated and can be empty
//!
//! Kernel threads have an empty `cmdline`. Joining the arguments with spaces before
//! matching is what lets a pattern like `SteamLaunch AppId=` — which spans two argv
//! entries — match at all.
//!
//! # Every read here races process exit
//!
//! A pid can disappear between `read_dir` and the read. That is expected, not a fault, and
//! every function in this module treats a failed read as "not a match" rather than as an
//! error worth propagating. A detector that returned doubt every time a process exited
//! during its scan would return doubt constantly on a busy machine.

use std::collections::HashMap;
use std::fs;

/// A process, as much of it as this daemon ever needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Process {
    pub pid: u32,
    pub ppid: u32,
    /// `/proc/<pid>/comm`, truncated by the kernel to 15 bytes.
    pub comm: String,
    /// `/proc/<pid>/cmdline`, NUL bytes replaced by spaces.
    pub cmdline: String,
}

impl Process {
    /// The basename of `argv[0]`, which is what a human calls the process.
    #[must_use]
    pub fn name(&self) -> &str {
        self.cmdline
            .split(' ')
            .next()
            .filter(|s| !s.is_empty())
            .and_then(|argv0| argv0.rsplit('/').next())
            .unwrap_or(&self.comm)
    }
}

/// Every process on the machine.
///
/// One pass, because the alternative is four separate walks of `/proc` per evaluation for
/// four different questions. Returns an empty vector if `/proc` cannot be read at all,
/// which callers must treat as doubt rather than as "no processes".
#[must_use]
pub fn snapshot() -> Vec<Process> {
    let Ok(entries) = fs::read_dir("/proc") else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(pid) = name.to_str().and_then(|s| s.parse::<u32>().ok()) else {
            continue;
        };
        let base = entry.path();

        let cmdline = fs::read(base.join("cmdline"))
            .map(|raw| {
                String::from_utf8_lossy(&raw)
                    .replace('\0', " ")
                    .trim_end()
                    .to_owned()
            })
            .unwrap_or_default();
        let comm = fs::read_to_string(base.join("comm"))
            .map(|s| s.trim_end().to_owned())
            .unwrap_or_default();
        let ppid = fs::read_to_string(base.join("stat"))
            .ok()
            .and_then(|stat| parse_ppid(&stat))
            .unwrap_or(0);

        out.push(Process {
            pid,
            ppid,
            comm,
            cmdline,
        });
    }
    out
}

/// Extracts the parent pid from `/proc/<pid>/stat`.
///
/// Field 2 is the executable name **in parentheses, unescaped**, so a process named
/// `foo) 1 2 3 (bar` would defeat a naive whitespace split. The last `)` is the reliable
/// delimiter, because the kernel writes the rest of the line itself and it contains none.
#[must_use]
fn parse_ppid(stat: &str) -> Option<u32> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(1)?.parse().ok()
}

/// The moment `pid` started, in clock ticks since boot, from `/proc/<pid>/stat`.
///
/// # Why this is recorded next to a pid, and not the pid alone
///
/// A pid is not an identity. The kernel hands them out monotonically and wraps at
/// `pid_max`, so a number observed once can belong to a different process later — which is
/// exactly why `polkit.rs` authorizes against a bus name and never against a pid. Field 22
/// closes the gap for the cases where a pid is *reported* rather than trusted: it never
/// changes while the process lives, so the pair `(pid, starttime)` names one process for
/// the life of the boot. Anything that shows a human a pid they might act on has to carry
/// it, so "the process that took this lease" and "whatever holds that number now" can be
/// told apart before somebody kills the wrong one.
#[must_use]
pub fn started_at(pid: u32) -> Option<u64> {
    parse_starttime(&fs::read_to_string(format!("/proc/{pid}/stat")).ok()?)
}

/// What a human calls the process at `pid`, or `None` if it is not there any more.
///
/// `comm` and not `cmdline`: this is used to label a pid in one column of a table, where
/// the 15-byte truncation described above is a feature rather than the trap it is when
/// matching.
#[must_use]
pub fn comm_of(pid: u32) -> Option<String> {
    fs::read_to_string(format!("/proc/{pid}/comm"))
        .ok()
        .map(|s| s.trim_end().to_owned())
}

/// Extracts `starttime` — field 22 — from `/proc/<pid>/stat`.
///
/// Indexed from the last `)` for the same reason as [`parse_ppid`]: field 2 is the
/// executable name, unescaped, and may contain spaces *and* parentheses. After that
/// delimiter, field *n* sits at index *n - 3*, so field 22 is index 19.
#[must_use]
fn parse_starttime(stat: &str) -> Option<u64> {
    let rest = &stat[stat.rfind(')')? + 1..];
    rest.split_whitespace().nth(19)?.parse().ok()
}

/// Whether any process has exactly this name.
///
/// Matches `comm` **and** the basename of `argv[0]`, so it works for names of any length —
/// see the module note about the 15-byte truncation.
#[must_use]
pub fn any_process_named(name: &str) -> bool {
    snapshot()
        .iter()
        .any(|p| p.comm == name || p.name() == name)
}

/// Every process whose command line contains `needle`.
#[must_use]
pub fn matching<'a>(procs: &'a [Process], needle: &str) -> Vec<&'a Process> {
    procs
        .iter()
        .filter(|p| p.cmdline.contains(needle))
        .collect()
}

/// Walks parent pids from `pid` upwards, returning the chain excluding `pid` itself.
///
/// Bounded rather than "until pid 0": a `/proc` snapshot taken while processes are being
/// reparented can contain a cycle, and an unbounded walk over one never returns. The
/// bound is far above any real process tree.
#[must_use]
pub fn ancestry(procs: &[Process], pid: u32) -> Vec<u32> {
    let by_pid: HashMap<u32, u32> = procs.iter().map(|p| (p.pid, p.ppid)).collect();
    let mut chain = Vec::new();
    let mut current = pid;
    for _ in 0..64 {
        let Some(&parent) = by_pid.get(&current) else {
            break;
        };
        if parent == 0 || chain.contains(&parent) {
            break;
        }
        chain.push(parent);
        current = parent;
    }
    chain
}

/// Whether `pid` is `root`, or descends from it.
///
/// [FACT-28]: attribution uses process ancestry, never an executable-name allowlist. A
/// list of game executables is unbounded and fragile, and the "a game is running" signal
/// is already computed next door.
#[must_use]
pub fn descends_from(procs: &[Process], pid: u32, roots: &[u32]) -> bool {
    if roots.contains(&pid) {
        return true;
    }
    ancestry(procs, pid).iter().any(|a| roots.contains(a))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ppid_survives_a_parenthesis_in_the_process_name() {
        // Not hypothetical: any program may name itself this, and a whitespace split
        // would read the ppid out of the middle of the name.
        let stat = "42 (evil) name) S 7 42 42 0 -1 4194304 0 0";
        assert_eq!(parse_ppid(stat), Some(7));
    }

    #[test]
    fn ppid_of_a_normal_line() {
        assert_eq!(parse_ppid("1234 (bash) S 1 1234 1234 34816"), Some(1));
    }

    #[test]
    fn starttime_is_field_22_counted_from_the_last_parenthesis() {
        // Nineteen fields sit between the name and `starttime`, and miscounting them by
        // one yields `itrealvalue` — which is 0 on every modern kernel, so the mistake
        // would look like "this process has no start time" instead of like a wrong index.
        let stat = "3878 (odd) name) S 3877 3878 3877 34816 3878 4194304 \
                    100 0 200 0 5 6 0 0 20 0 3 0 987654 123456 789";
        assert_eq!(parse_starttime(stat), Some(987_654));
        // Same line, so the two parsers are known to agree about where the fields start.
        assert_eq!(parse_ppid(stat), Some(3877));
    }

    #[test]
    fn the_start_time_of_a_live_process_is_readable_and_does_not_move() {
        let me = std::process::id();
        let first = started_at(me).expect("/proc/self/stat is readable on any Linux");
        assert_eq!(
            started_at(me),
            Some(first),
            "a live process must never change its start time; the whole point of pairing \
             it with a pid is that it cannot"
        );
        assert!(comm_of(me).is_some());
    }

    #[test]
    fn a_pid_that_cannot_exist_reads_as_absent_rather_than_as_zero() {
        // pid 0 is not a process on Linux and `/proc/0` never exists, so this is the one
        // deterministic "gone" case available to a test — no spawning, no reaping, no
        // window in which the answer could change.
        assert_eq!(started_at(0), None);
        assert_eq!(comm_of(0), None);
    }

    #[test]
    fn ancestry_terminates_on_a_cycle() {
        // A `/proc` snapshot taken mid-reparent can contain one. Without the bound and
        // the seen-check this loops forever, inside the daemon that decides whether a
        // machine may sleep, and no timeout anywhere would rescue it.
        //
        // The chain is [2, 1] and not [2]: walking up from 1 reaches 2, then 2's parent
        // is 1, which has not been RECORDED yet even though it is where the walk started.
        // It stops one step later, on reaching 2 a second time. That is correct, and it
        // is why `descends_from` checks `roots.contains(&pid)` before walking rather than
        // relying on the chain to be free of the starting pid.
        let procs = vec![
            Process {
                pid: 1,
                ppid: 2,
                comm: "a".into(),
                cmdline: String::new(),
            },
            Process {
                pid: 2,
                ppid: 1,
                comm: "b".into(),
                cmdline: String::new(),
            },
        ];
        let chain = ancestry(&procs, 1);
        assert_eq!(chain, vec![2, 1]);
        assert!(
            chain.len() < 64,
            "the walk must be bounded, not merely finite"
        );
    }

    #[test]
    fn ancestry_terminates_on_a_self_parent() {
        // pid 1's parent is 0, which is the normal terminator. A process whose parent is
        // itself is not normal and has been seen in crash dumps; it must not spin.
        let procs = vec![Process {
            pid: 5,
            ppid: 5,
            comm: "x".into(),
            cmdline: String::new(),
        }];
        assert_eq!(ancestry(&procs, 5), vec![5]);
    }

    #[test]
    fn name_prefers_argv0_over_truncated_comm() {
        // The measured case: the kernel reports `fossilize_repla` and the real name is
        // one character longer.
        let p = Process {
            pid: 1,
            ppid: 0,
            comm: "fossilize_repla".into(),
            cmdline: "/usr/bin/fossilize_replay --num-threads 4".into(),
        };
        assert_eq!(p.name(), "fossilize_replay");
    }

    #[test]
    fn name_falls_back_to_comm_for_a_kernel_thread() {
        let p = Process {
            pid: 2,
            ppid: 0,
            comm: "kthreadd".into(),
            cmdline: String::new(),
        };
        assert_eq!(p.name(), "kthreadd");
    }

    #[test]
    fn descends_from_includes_the_root_itself() {
        let procs = vec![
            Process {
                pid: 10,
                ppid: 1,
                comm: "game".into(),
                cmdline: String::new(),
            },
            Process {
                pid: 11,
                ppid: 10,
                comm: "child".into(),
                cmdline: String::new(),
            },
        ];
        assert!(descends_from(&procs, 10, &[10]));
        assert!(descends_from(&procs, 11, &[10]));
        assert!(!descends_from(&procs, 1, &[10]));
    }
}
