//! Carrying the last-input instant across a restart of this agent.
//!
//! # The hole this fills
//!
//! `ext-idle-notify-v1` reports **transitions**, never a counter. There is no request that
//! answers "how long has this seat been idle right now", and the compositor-specific escape
//! hatch does not exist either: KWin answers `org.freedesktop.ScreenSaver.GetSessionIdleTime`
//! with `GetSessionIdleTime is not supported on this platform` -- measured on Plasma 6 under
//! Wayland. X11 has no such hole, because `MIT-SCREEN-SAVER` exposes exactly that counter,
//! which is why [`crate::x11`] needs none of this.
//!
//! So an agent that has just started knows nothing, and the only honest first answer is
//! [`Idle::Unknown`](crate::backend::Idle::Unknown). The trouble is what comes next: ten
//! seconds later the compositor says `idled`, the agent concludes "idle for ten seconds",
//! and the daemon anchors the human-input clock at the moment the agent started. Every
//! deadline measured from that clock restarts from zero. On a machine that upgrades weekly
//! that is a package upgrade silently granting a fresh thirty-minute countdown, and the
//! specification calls the outcome out by name: [CLK-7]'s rationale exists precisely to
//! forbid an origin that "would restart the countdown on every daemon restart".
//!
//! Measured before the fix, restarting the agent with nobody in the room: the daemon's
//! `human_input` origin moved from +25823 s to +26299 s -- exactly the uptime at which the
//! restart happened -- while a `swayidle` timestamp written to disk by the machine's other
//! idle watcher did not move at all.
//!
//! # What is stored, and why a runtime directory
//!
//! Two instants on `CLOCK_BOOTTIME`: the last observed input, and when this file was last
//! written. `$XDG_RUNTIME_DIR` is a `tmpfs`, so the file survives suspend and a restart of
//! this agent, and a cold boot removes it. That is the property [CLK-9] already requires of
//! the state behind `after_resume`, applied to the same problem one layer down. Nothing here
//! is written to disk: an idle instant that outlived a boot would be a lie about a machine
//! that has not been touched since it was switched on.
//!
//! # Why the gap is bounded
//!
//! While no agent is running, nobody is watching the seat, so input during that window is
//! unobservable by construction. Adoption is therefore refused unless the previous instance
//! was writing until very recently ([`ADOPT_MAX_GAP`]). The residual error is bounded by the
//! gap: if somebody did touch the machine during it and then stopped, the adopted instant is
//! older than the truth by at most the gap, and the machine may sleep that much early. A
//! restart from a package upgrade is under a second and a restart after a crash is
//! `RestartSec=5s`, against policy timeouts measured in tens of minutes.
//!
//! Refusing to adopt is not a fallback to zero. It is [`Idle::Unknown`], which the daemon
//! turns into `human_active = INDETERMINATE` and which vetoes every sleep action until the
//! first real transition arrives -- at most [`NOTIFY_TIMEOUT`](crate::wayland::NOTIFY_TIMEOUT)
//! later.

use std::path::{Path, PathBuf};
use std::time::Duration;

use idlectl_policy::BootInstant;
use tracing::debug;

/// How stale the previous instance's last write may be for its instant to still be adopted.
///
/// Three heartbeats, matching the daemon's own staleness rule for an agent that has stopped
/// reporting: the two are the same judgement about the same silence, and they should not be
/// able to disagree.
pub const ADOPT_MAX_GAP: Duration = Duration::from_secs(90);

/// `$XDG_RUNTIME_DIR/idlectl-agent/last-input`, or [`None`] where there is no runtime
/// directory to put it in.
///
/// A session without `XDG_RUNTIME_DIR` is not an error worth failing on. It costs the
/// carry-over across restarts and nothing else, so the agent says so once and runs.
fn path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    let mut path = PathBuf::from(dir);
    path.push("idlectl-agent");
    path.push("last-input");
    Some(path)
}

/// Reads the instant the previous instance last observed input, if it is still credible.
///
/// [`None`] means "start from unknown", which is the honest answer and the safe one.
#[must_use]
pub fn load(now: BootInstant) -> Option<BootInstant> {
    load_from(&path()?, now)
}

/// [`load`], against a given file.
///
/// The split exists so the tests can drive this without touching `XDG_RUNTIME_DIR`:
/// `std::env::set_var` is `unsafe` in this edition and the crate forbids `unsafe` outright,
/// which is the correct trade -- a process-wide variable is not worth an exception, and
/// passing the path in is clearer besides.
#[must_use]
fn load_from(path: &Path, now: BootInstant) -> Option<BootInstant> {
    let text = std::fs::read_to_string(path).ok()?;

    let mut fields = text.split_whitespace();
    let last_input = BootInstant::from_nanos(fields.next()?.parse().ok()?);
    let written_at = BootInstant::from_nanos(fields.next()?.parse().ok()?);

    // A `tmpfs` file cannot outlive its boot, so neither of these should ever be in the
    // future. Checking anyway costs two comparisons and turns a corrupt file into a fresh
    // start rather than into an origin from a boot that no longer exists.
    if written_at > now || last_input > written_at {
        debug!("the stored last-input file is not from this boot; starting from unknown");
        return None;
    }

    let gap = now.since(written_at);
    if gap > ADOPT_MAX_GAP {
        debug!(
            gap_s = gap.as_secs(),
            "no agent was watching this seat for too long to carry the last input over"
        );
        return None;
    }
    Some(last_input)
}

/// Records the last observed input, so the next instance can carry it over.
///
/// Errors are logged and dropped. Failing to write this costs the carry-over across the
/// next restart; it does not make the running agent any less correct, and an agent that
/// exited because a `tmpfs` write failed would wedge the machine awake for good.
pub fn store(last_input: BootInstant, now: BootInstant) {
    let Some(path) = path() else { return };
    store_to(&path, last_input, now);
}

/// [`store`], against a given file. See [`load_from`] for why the split exists.
fn store_to(path: &Path, last_input: BootInstant, now: BootInstant) {
    let Some(dir) = path.parent() else { return };
    if let Err(err) = std::fs::create_dir_all(dir) {
        debug!(error = %err, "could not create the runtime directory for the last-input file");
        return;
    }

    // Written whole and renamed into place: a torn write would parse as a different pair of
    // numbers rather than as a failure, and the numbers are the whole point.
    let temporary = path.with_extension("new");
    let body = format!("{} {}\n", last_input.as_nanos(), now.as_nanos());
    if let Err(err) = std::fs::write(&temporary, body) {
        debug!(error = %err, "could not write the last-input file");
        return;
    }
    if let Err(err) = std::fs::rename(&temporary, path) {
        debug!(error = %err, "could not replace the last-input file");
        let _ = std::fs::remove_file(&temporary);
    }
}

#[cfg(test)]
mod tests {
    use super::{ADOPT_MAX_GAP, load_from, store_to};
    use idlectl_policy::BootInstant;
    use std::path::PathBuf;
    use std::time::Duration;

    /// A file of its own per test, so nothing here shares state and the whole module can
    /// run in parallel. Named after the case rather than randomised: a leftover file from a
    /// crashed run should collide loudly, not be silently worked around.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("idlectl-agent-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir.join("last-input")
    }

    #[test]
    fn carries_the_instant_over_a_short_gap() {
        let path = scratch("short-gap");
        let input = BootInstant::from_secs(1000);
        store_to(&path, input, BootInstant::from_secs(1500));
        // One second later a new instance starts, as after a package upgrade.
        assert_eq!(load_from(&path, BootInstant::from_secs(1501)), Some(input));
    }

    /// The point of the whole module: what is carried over is the *input* instant, not the
    /// moment the file was written, so the countdown does not restart.
    #[test]
    fn carries_the_input_instant_not_the_write() {
        let path = scratch("input-not-write");
        store_to(
            &path,
            BootInstant::from_secs(1000),
            BootInstant::from_secs(9000),
        );
        let carried = load_from(&path, BootInstant::from_secs(9001)).expect("adopted");
        assert_eq!(
            BootInstant::from_secs(9001).since(carried),
            Duration::from_secs(8001)
        );
    }

    #[test]
    fn refuses_a_gap_nobody_was_watching() {
        let path = scratch("long-gap");
        store_to(
            &path,
            BootInstant::from_secs(1000),
            BootInstant::from_secs(1500),
        );
        let late = BootInstant::from_secs(1500)
            .checked_add(ADOPT_MAX_GAP + Duration::from_secs(1))
            .expect("representable");
        assert_eq!(load_from(&path, late), None);
    }

    /// Exactly at the limit still counts: the boundary belongs to the side that keeps
    /// working, because the alternative is a needless INDETERMINATE.
    #[test]
    fn adopts_at_exactly_the_limit() {
        let path = scratch("limit");
        store_to(
            &path,
            BootInstant::from_secs(1000),
            BootInstant::from_secs(1500),
        );
        let edge = BootInstant::from_secs(1500)
            .checked_add(ADOPT_MAX_GAP)
            .expect("representable");
        assert_eq!(load_from(&path, edge), Some(BootInstant::from_secs(1000)));
    }

    /// A file claiming a longer boot than the running one cannot have been written this
    /// boot, whatever it says.
    #[test]
    fn refuses_a_file_from_a_longer_boot() {
        let path = scratch("future");
        store_to(
            &path,
            BootInstant::from_secs(5000),
            BootInstant::from_secs(5000),
        );
        assert_eq!(load_from(&path, BootInstant::from_secs(100)), None);
    }

    #[test]
    fn refuses_a_file_whose_input_is_after_its_write() {
        let path = scratch("inverted");
        std::fs::write(&path, "9000000000000 1000000000\n").expect("write");
        assert_eq!(load_from(&path, BootInstant::from_secs(9001)), None);
    }

    #[test]
    fn refuses_a_corrupt_file() {
        let path = scratch("corrupt");
        std::fs::write(&path, "not a number at all\n").expect("write");
        assert_eq!(load_from(&path, BootInstant::from_secs(100)), None);
    }

    #[test]
    fn refuses_a_truncated_file() {
        let path = scratch("truncated");
        std::fs::write(&path, "1000000000\n").expect("write");
        assert_eq!(load_from(&path, BootInstant::from_secs(100)), None);
    }

    #[test]
    fn refuses_when_there_is_nothing_stored() {
        let path = scratch("empty");
        assert_eq!(load_from(&path, BootInstant::from_secs(100)), None);
    }

    /// Storing creates the directory it needs, because the agent may be the first thing in
    /// the session to want it.
    #[test]
    fn creates_its_directory() {
        let dir = std::env::temp_dir().join("idlectl-agent-test-mkdir");
        let _ = std::fs::remove_dir_all(&dir);
        let path = dir.join("idlectl-agent").join("last-input");
        store_to(
            &path,
            BootInstant::from_secs(1000),
            BootInstant::from_secs(1000),
        );
        assert_eq!(
            load_from(&path, BootInstant::from_secs(1001)),
            Some(BootInstant::from_secs(1000))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
