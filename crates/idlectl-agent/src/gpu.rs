//! DRM `fdinfo`: which of this session's processes hold GPU memory.
//!
//! # Why this is in the agent and not in the daemon
//!
//! Because `/proc/<pid>/fdinfo` is not gated by its mode bits. The directory is mode 0555
//! and looks world-readable; the kernel checks `ptrace_may_access` on every open anyway,
//! so reading **another user's** fdinfo needs `CAP_SYS_PTRACE`. Measured on a real
//! machine: with `CapabilityBoundingSet=CAP_DAC_READ_SEARCH` -- the one read-only
//! capability idlepolicyd holds -- the read is denied, and with `CAP_SYS_PTRACE` the same
//! read succeeds.
//!
//! `CAP_SYS_PTRACE` is not a capability a daemon that can power a machine off is going to
//! be given: it is the ability to read any process's memory, asked for so that video RAM
//! can be attributed. So the walk lives here, where the processes are the agent's own and
//! no capability is needed at all -- the agent's unit sets `CapabilityBoundingSet=` empty
//! and this still works.
//!
//! This is the same trade [`crate::mpris`] already makes for `media_playing`, and for the
//! same shape of reason: the privileged half cannot reach the data, the unprivileged half
//! can, so the unprivileged half reads it and reports it. It is a privilege reduction
//! rather than a workaround.
//!
//! # What this deliberately does NOT do
//!
//! Attribution. Whether a holder belongs to a game is decided in the daemon, from the
//! process tree and the command lines -- which are world-readable and which the daemon
//! reads today with no capability at all. What comes out of here is raw: pid, process
//! name, bytes. An unprivileged process is the wrong place to decide what counts as a
//! game, and the daemon does not have to take its word for what is running on the machine.
//!
//! # Processes this agent cannot read
//!
//! Skipped, in silence. Another user's processes -- and root's -- are the normal case
//! rather than a fault: this is the very permission wall the daemon hit, seen from the
//! other side of it. On a single-seat machine the processes holding the GPU are the
//! session's own, which is why moving the walk here loses nothing that was ever readable.

use std::collections::HashMap;
use std::path::Path;

/// One process holding GPU memory, in the shape `ReportSession` carries it: `(pid,
/// process name, bytes)`, which is `a(ust)` in the interface definition.
///
/// A tuple rather than a struct because it is a wire type and nothing here interprets it.
/// The daemon is what turns these into facts.
pub type Holder = (u32, String, u64);

/// Every DRM memory holder this agent can see.
///
/// An empty list is the honest answer to two different questions -- "no process holds any
/// GPU memory" and "no DRM device on this machine publishes memory accounting at all" --
/// and collapsing them is deliberate. Both mean this source contributed nothing, which is
/// exactly what the daemon does with them; neither is doubt. An agent that is not running
/// at all is already reported once through `human_active`, and raising a second veto for
/// the same cause would keep every headless machine awake forever.
#[must_use]
pub fn read() -> Vec<Holder> {
    walk().unwrap_or_default()
}

/// Walks `/proc` and reports every readable process that holds DRM memory.
///
/// Returns [`None`] when nothing published a DRM **memory** key, which is how the
/// proprietary NVIDIA driver presents: it exposes `drm-driver` and a client id and no
/// memory accounting anywhere. Keying the answer on `drm-driver` alone would report every
/// process as holding zero bytes and call that a successful reading of the GPU -- the
/// daemon would conclude the generic source works and never ask `nvidia-smi`, which is the
/// one failure its two-source merge exists to prevent.
fn walk() -> Option<Vec<Holder>> {
    let mut saw_memory_key = false;
    let mut out = Vec::new();

    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let Some(pid) = entry
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<u32>().ok())
        else {
            continue;
        };
        let dir = entry.path();
        let Ok(fds) = std::fs::read_dir(dir.join("fdinfo")) else {
            // Another user's process, or one that exited between the two reads. Both are
            // normal here and neither is a fault -- see the module note.
            continue;
        };

        let texts = fds
            .flatten()
            .filter_map(|fd| std::fs::read_to_string(fd.path()).ok());
        let (bytes, saw) = fold(texts);
        saw_memory_key |= saw;
        if bytes > 0 {
            out.push((pid, name_of(&dir), bytes));
        }
    }

    saw_memory_key.then_some(out)
}

/// Folds one process's `fdinfo` files into the GPU memory it holds.
///
/// Returns the bytes, and whether any DRM **memory** key was seen at all -- the second
/// half is what [`walk`] uses to tell "this process holds nothing" from "this driver does
/// not do memory accounting".
///
/// # De-duplication
///
/// A process holding several file descriptors on the same DRM client reports the same
/// memory once per descriptor. Summing naively multiplies a game's memory by its
/// descriptor count and turns a 400 MiB title into a 4 GiB one. `drm-client-id` is the
/// identity that must be de-duplicated on.
fn fold(files: impl IntoIterator<Item = String>) -> (u64, bool) {
    let mut saw_memory_key = false;
    // client id -> bytes, so several descriptors on one client count once.
    let mut clients: HashMap<u64, u64> = HashMap::new();

    for text in files {
        if !text.contains("drm-driver") {
            continue;
        }

        let mut client_id = None;
        let mut vram = 0u64;
        let mut gtt = 0u64;
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match key.trim() {
                "drm-client-id" => client_id = value.parse::<u64>().ok(),
                // `resident` is what is actually in device memory; `memory` is the older
                // spelling of the same idea. Whichever the driver publishes.
                "drm-resident-vram" | "drm-memory-vram" => {
                    saw_memory_key = true;
                    vram = vram.max(parse_size(value));
                }
                "drm-resident-gtt" | "drm-memory-gtt" => {
                    saw_memory_key = true;
                    gtt = gtt.max(parse_size(value));
                }
                _ => {}
            }
        }
        // On an integrated GPU there is no dedicated video memory at all, so fall back to
        // the shared aperture rather than reporting zero for every process.
        let bytes = if vram > 0 { vram } else { gtt };
        if let Some(id) = client_id {
            let slot = clients.entry(id).or_default();
            *slot = (*slot).max(bytes);
        }
    }

    (clients.values().sum(), saw_memory_key)
}

/// Parses a `fdinfo` size, which is written as a number and a unit (`"16384 KiB"`).
fn parse_size(value: &str) -> u64 {
    let mut parts = value.split_whitespace();
    let Some(number) = parts.next().and_then(|n| n.parse::<u64>().ok()) else {
        return 0;
    };
    let multiplier = match parts.next() {
        Some("KiB") => 1024,
        Some("MiB") => 1024 * 1024,
        Some("GiB") => 1024 * 1024 * 1024,
        // The kernel documents KiB as the unit; a bare number is assumed to follow it
        // rather than to be bytes, because reading 16384 as bytes would silently put every
        // process under the daemon's threshold.
        _ => 1024,
    };
    number.saturating_mul(multiplier)
}

/// What a human calls the process at `/proc/<pid>`.
///
/// The basename of `argv[0]`, falling back to `comm`. That order and not the other one:
/// the kernel stores a task name in a 16-byte field, so `fossilize_replay` appears in
/// `comm` as `fossilize_repla`. The daemon matches this string against its list of desktop
/// shells by name and prints it as the evidence for a veto, so a name truncated at fifteen
/// bytes would silently stop matching and would read wrong in the log.
fn name_of(dir: &Path) -> String {
    let raw = std::fs::read(dir.join("cmdline")).unwrap_or_default();
    // NUL-separated, and empty for a kernel thread.
    let cmdline = String::from_utf8_lossy(&raw);
    let argv0 = cmdline.split('\0').next().unwrap_or_default();
    if !argv0.is_empty() {
        return argv0.rsplit('/').next().unwrap_or(argv0).to_owned();
    }
    std::fs::read_to_string(dir.join("comm"))
        .map(|comm| comm.trim_end().to_owned())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One descriptor of a process holding 600 MiB on an AMD card.
    const VRAM: &str = "\
pos:    0
drm-driver:     amdgpu
drm-client-id:  42
drm-resident-vram:      614400 KiB
";

    /// A game opens the same DRM client several times. Counting its memory once per file
    /// descriptor is what turns a 400 MiB title into a 4 GiB one -- above the daemon's
    /// 512 MiB threshold, so a machine would refuse to sleep because a browser tab had a
    /// few descriptors open on the GPU.
    #[test]
    fn memory_is_counted_once_per_drm_client_not_once_per_descriptor() {
        let (bytes, saw) = fold([VRAM.to_owned(), VRAM.to_owned(), VRAM.to_owned()]);
        assert!(saw);
        assert_eq!(bytes, 614_400 * 1024, "one client, counted once");
    }

    /// Two clients in one process are two allocations and do add up.
    #[test]
    fn separate_drm_clients_in_one_process_add_up() {
        let second = VRAM.replace("drm-client-id:  42", "drm-client-id:  43");
        let (bytes, _) = fold([VRAM.to_owned(), second]);
        assert_eq!(bytes, 2 * 614_400 * 1024);
    }

    /// The NVIDIA proprietary driver publishes `drm-driver` and a client id and no memory
    /// accounting at all. Reporting that as "read the GPU successfully, holds zero bytes"
    /// is precisely how the daemon ends up believing the generic source works and never
    /// asking `nvidia-smi` -- both GPU facts then read false forever with a game running.
    #[test]
    fn a_client_id_with_no_memory_key_is_not_a_reading() {
        let text = "\
pos:    0
drm-driver:     nvidia-drm
drm-client-id:  7
";
        let (bytes, saw) = fold([text.to_owned()]);
        assert_eq!(bytes, 0);
        assert!(
            !saw,
            "no memory key was published, so this source saw nothing"
        );
    }

    /// An integrated GPU has no dedicated video memory. Reporting zero for every process
    /// there would disable this source on exactly the machines where it is the only one.
    #[test]
    fn an_integrated_gpu_falls_back_to_the_shared_aperture() {
        let text = "\
drm-driver:     i915
drm-client-id:  3
drm-resident-vram:      0 KiB
drm-resident-gtt:       2 MiB
";
        let (bytes, saw) = fold([text.to_owned()]);
        assert!(saw);
        assert_eq!(bytes, 2 * 1024 * 1024);
    }

    #[test]
    fn a_file_that_is_not_a_drm_client_is_ignored() {
        let (bytes, saw) = fold(["pos:\t0\nflags:\t02\nmnt_id:\t24\n".to_owned()]);
        assert_eq!(bytes, 0);
        assert!(!saw);
    }

    #[test]
    fn fdinfo_sizes_default_to_kib() {
        assert_eq!(parse_size("16384 KiB"), 16 * 1024 * 1024);
        assert_eq!(parse_size("2 MiB"), 2 * 1024 * 1024);
        // The kernel documents KiB; reading a bare number as bytes would put every process
        // under the daemon's threshold and silently disable both GPU facts.
        assert_eq!(parse_size("1048576"), 1024 * 1024 * 1024);
        assert_eq!(parse_size("nonsense"), 0);
    }

    /// The daemon merges these with `nvidia-smi`'s holders **by pid**, so a pid appearing
    /// twice would be resolved arbitrarily. It cannot happen -- one entry is pushed per
    /// `/proc` directory -- and this asserts it against a future refactor of the walk.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_walk_reports_each_pid_at_most_once() {
        let mut pids: Vec<u32> = read().into_iter().map(|(pid, _, _)| pid).collect();
        let before = pids.len();
        pids.sort_unstable();
        pids.dedup();
        assert_eq!(pids.len(), before, "one entry per process");
    }
}
