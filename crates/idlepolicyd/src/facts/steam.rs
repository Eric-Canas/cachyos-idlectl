//! `steam_game_running` and `steam_downloading`.
//!
//! Both facts are about the same installation and neither is about the Steam *client*. A
//! client sitting in the tray is not a reason to keep a machine awake; a title running or
//! a download in flight is.

use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use idlectl_policy::FactId;

use super::{Context, Reading, ago};
use crate::proc;

/// Where Steam keeps itself, relative to a user's home.
const DEFAULT_ROOT_SUFFIX: &str = ".local/share/Steam";

/// The wrapper Steam puts in front of every title it launches.
///
/// **Measured against a real launch, not inferred** ([FACT-20]). It spans two argv
/// entries, which is why [`proc`] joins the command line before matching. Guessing at
/// this pattern produces a detector that has never once been true and says nothing about
/// it.
const LAUNCH_SIGNATURE: &str = "SteamLaunch AppId=";

/// The shader precompiler, matched on the command line rather than by name.
///
/// Measured 2026-07-27, on a title initialising: sixteen processes of 63–75 MiB each,
/// about 1000 MiB in total, and **not one of them reaches the 512 MiB per-process GPU
/// threshold** of [FACT-25] — so the GPU facts cannot see this at all. Suspending during
/// precompilation throws all of that work away and the platform repeats it on the next
/// launch, which is exactly the kind of silent loss this project exists to prevent.
///
/// Sixteen characters long, so `comm` holds `fossilize_repla` and a name match finds
/// nothing. See the module note in [`crate::proc`].
const SHADER_PRECOMPILE: &str = "fossilize_replay";

/// The microcompositor a game session runs under, when one is used.
const MICROCOMPOSITOR: &str = "gamescope";

/// Resolves the Steam root: the configured value, or a search of the machine's home
/// directories.
///
/// Searching rather than hardcoding one user matters on exactly the machine this is for:
/// a console-shaped box logs in one user automatically, but nothing says which, and a
/// package cannot know. `steam_root` in the configuration is the escape hatch for an
/// install somewhere unusual — a second library on another disk, or a flatpak layout.
fn root(ctx: &Context<'_>, fact: FactId) -> Option<PathBuf> {
    if let Some(configured) = ctx.policy.fact_settings(fact).steam_root {
        let path = PathBuf::from(configured);
        return path.is_dir().then_some(path);
    }

    // /home is the overwhelmingly common case; /root is included because a
    // single-purpose console is sometimes exactly that badly set up, and finding it is
    // better than reporting "Steam is not installed" on a machine where it plainly is.
    let candidates = std::fs::read_dir("/home")
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .chain(std::iter::once(PathBuf::from("/root")));

    candidates
        .map(|home| home.join(DEFAULT_ROOT_SUFFIX))
        .find(|p| p.is_dir())
}

/// [FACT-19]: true iff a Steam **application** is running — not merely the client.
pub fn game_running(ctx: &Context<'_>) -> Reading {
    let installed = root(ctx, FactId::SteamGameRunning).is_some();

    let procs = proc::snapshot();
    if procs.is_empty() {
        // `/proc` unreadable is doubt, and it is doubt even when Steam is not installed:
        // the daemon has lost the ability to see something it normally sees.
        return Reading::doubt("/proc could not be read");
    }

    if !installed {
        return Reading::absent("Steam is not installed");
    }

    let mut evidence = Vec::new();
    for (label, needle) in [
        ("a game", LAUNCH_SIGNATURE),
        ("the microcompositor", MICROCOMPOSITOR),
        ("shader precompilation", SHADER_PRECOMPILE),
    ] {
        let hits = proc::matching(&procs, needle);
        if let Some(first) = hits.first() {
            evidence.push(format!(
                "{label} ({} process(es), pid {})",
                hits.len(),
                first.pid
            ));
        }
    }

    if evidence.is_empty() {
        Reading::no("no Steam application running")
    } else {
        Reading::yes(evidence.join(", "))
    }
}

/// The pids a game's GPU memory may be attributed to ([FACT-27], [FACT-28]).
///
/// Returned separately from the fact so that the GPU detector can use process ancestry
/// without re-deriving what a game is.
#[must_use]
pub fn game_pids(procs: &[proc::Process]) -> Vec<u32> {
    let mut pids = Vec::new();
    for needle in [LAUNCH_SIGNATURE, MICROCOMPOSITOR, SHADER_PRECOMPILE] {
        pids.extend(proc::matching(procs, needle).iter().map(|p| p.pid));
    }
    pids
}

/// [FACT-21]: true iff something under the staging tree was modified within the window.
///
/// # Why the whole tree is walked ([FACT-23])
///
/// Stopping at the first file inside the window halves the cost and gives the same
/// verdict. It was rejected anyway, because it reports whichever file directory order
/// happened to yield: measured, that produced log lines reading "written 297 s ago"
/// against a 300 s window **while a download was running at full speed**. The decision was
/// right and the number invited the opposite conclusion. The full walk cost 20 ms over
/// ~6900 files, once per evaluation. A log that misleads on the one output anybody reads
/// while debugging is not worth 10 ms.
///
/// # Why not network throughput ([FACT-24])
///
/// More general — it would also cover package upgrades, torrents and backups — and
/// unusable, because it cannot distinguish downloading from playing. 4K video playback is
/// roughly 3 MiB/s, so a throughput rule makes media playback a hard veto and contradicts
/// the deliberate decision to make it a soft one. Telling those two apart is the whole job.
pub fn downloading(ctx: &Context<'_>) -> Reading {
    let settings = ctx.policy.fact_settings(FactId::SteamDownloading);
    let window = settings.window.unwrap_or(DEFAULT_WINDOW);

    let Some(root) = root(ctx, FactId::SteamDownloading) else {
        return Reading::absent("Steam is not installed");
    };
    let staging = root.join("steamapps/downloading");
    if !staging.is_dir() {
        // Absent, not false: the platform creates this directory when a download starts
        // and removes it when the queue empties. Its absence is the normal idle state.
        return Reading::absent("no staging directory (nothing has been queued)");
    }

    let (newest, count, errors) = newest_mtime(&staging);

    if errors > 0 && newest.is_none() {
        return Reading::doubt(format!(
            "staging tree at {} could not be read",
            staging.display()
        ));
    }

    let Some((path, mtime)) = newest else {
        return Reading::no("staging directory is empty");
    };

    let age = SystemTime::now()
        .duration_since(mtime)
        // A file with an mtime in the future -- a clock step, or a copy that preserved
        // timestamps -- is treated as brand new. That errs towards not sleeping.
        .unwrap_or(Duration::ZERO);

    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    if age < window {
        Reading::yes(format!(
            "{name} written {} ({count} file(s) in the staging tree)",
            ago(age)
        ))
    } else {
        // [FACT-22]: self-extinguishing from both ends, which is why this fact needs no
        // TTL and no explicit release. Paused, the mtime ages out; finished, the platform
        // empties the directory; debris from an aborted download is stale on arrival.
        Reading::no(format!(
            "newest staging file {name} is {} (window {}s)",
            ago(age),
            window.as_secs()
        ))
    }
}

/// The default download window: five minutes.
///
/// The platform touches staging files every 1–5 seconds while a download runs, so the
/// window is two orders of magnitude larger than it needs to be for the true case. It is
/// sized for the *false* case instead: how long after a pause the machine should keep
/// waiting before deciding the download is not coming back.
const DEFAULT_WINDOW: Duration = Duration::from_secs(300);

/// Walks a tree and returns the newest mtime found, the number of files seen, and how
/// many entries could not be read.
///
/// Iterative rather than recursive: a staging tree is shallow in practice, but a symlink
/// loop planted in one must not blow the daemon's stack. Symlinks are not followed for
/// exactly that reason — `symlink_metadata` reads the link itself.
fn newest_mtime(root: &Path) -> (Option<(PathBuf, SystemTime)>, usize, usize) {
    let mut stack = vec![root.to_path_buf()];
    let mut newest: Option<(PathBuf, SystemTime)> = None;
    let mut count = 0usize;
    let mut errors = 0usize;

    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            errors += 1;
            continue;
        };
        for entry in entries {
            let Ok(entry) = entry else {
                errors += 1;
                continue;
            };
            let path = entry.path();
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                errors += 1;
                continue;
            };
            if meta.is_dir() {
                stack.push(path);
                continue;
            }
            count += 1;
            let Ok(mtime) = meta.modified() else {
                errors += 1;
                continue;
            };
            if newest.as_ref().is_none_or(|(_, best)| mtime > *best) {
                newest = Some((path, mtime));
            }
        }
    }

    (newest, count, errors)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// The whole point of [FACT-23]: the reported file must be the newest one, not
    /// whichever the filesystem happened to hand over first.
    #[test]
    fn newest_mtime_reports_the_newest_and_not_the_first() {
        let dir = std::env::temp_dir().join(format!("idlectl-steam-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("sub")).unwrap();

        fs::write(dir.join("old.bin"), b"x").unwrap();
        fs::write(dir.join("sub/new.bin"), b"x").unwrap();
        // Push the first file well into the past so the ordering cannot be a coincidence
        // of both being written in the same millisecond.
        let past = SystemTime::now() - Duration::from_secs(3600);
        fs::File::open(dir.join("old.bin"))
            .unwrap()
            .set_modified(past)
            .unwrap();

        let (newest, count, errors) = newest_mtime(&dir);
        let (path, _) = newest.expect("a file was written");
        assert_eq!(path.file_name().unwrap(), "new.bin");
        assert_eq!(count, 2, "the walk must descend into subdirectories");
        assert_eq!(errors, 0);

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn an_empty_tree_has_no_newest() {
        let dir = std::env::temp_dir().join(format!("idlectl-steam-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let (newest, count, errors) = newest_mtime(&dir);
        assert!(newest.is_none());
        assert_eq!((count, errors), (0, 0));
        fs::remove_dir_all(&dir).unwrap();
    }

    /// The three signatures must stay matchable against a joined command line. A refactor
    /// that split on argv boundaries would break the launch signature silently, which is
    /// the failure [FACT-20] was written about.
    #[test]
    fn the_launch_signature_spans_two_argv_entries() {
        assert!(LAUNCH_SIGNATURE.contains(' '));
        let joined = "/usr/bin/reaper SteamLaunch AppId=1551360 -- /usr/bin/proton run";
        assert!(joined.contains(LAUNCH_SIGNATURE));
    }

    /// If this ever fits in `comm`, the note about matching it on the command line stops
    /// being load-bearing and somebody will "simplify" it. Fifteen is the kernel's limit.
    #[test]
    fn the_shader_precompiler_name_does_not_fit_in_comm() {
        assert!(SHADER_PRECOMPILE.len() > 15);
    }
}
