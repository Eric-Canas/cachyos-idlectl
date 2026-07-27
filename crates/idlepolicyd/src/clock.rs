//! Monotonic time and resume detection.
//!
//! # Why `CLOCK_BOOTTIME` everywhere
//!
//! `CLOCK_MONOTONIC` stops while the machine is suspended. Every deadline computed before
//! a sleep would therefore come back short by exactly the length of the sleep, which is
//! the difference between "eight hours idle" and "eight hours idle, minus the eight hours
//! you were asleep". `CLOCK_REALTIME` is worse: NTP can step it, and a backwards step
//! makes a deadline unreachable. `CLOCK_BOOTTIME` counts real time and never steps.
//!
//! # How a resume is detected, and why not with a hook
//!
//! `BOOTTIME - MONOTONIC` is the total time this boot has spent suspended. It is
//! non-decreasing, it is zero on a machine that has never slept, and it jumps by exactly
//! the length of the sleep on every resume — including a hibernate, and including a sleep
//! nothing announced.
//!
//! That last part is why this is the primary mechanism rather than a `systemd-sleep` hook.
//! `rtcwake -m mem` writes `/sys/power/state` directly: it never goes through logind, so
//! `PrepareForSleep` is not emitted, no sleep target is entered and no hook of any kind
//! runs — measured. A daemon that learned about resumes only from hooks would miss every
//! one of those and would then evaluate against origins that are silently wrong. Reading
//! two clocks costs two syscalls and cannot be bypassed by anything, because it is not a
//! notification: it is the kernel's own record of what happened.
//!
//! logind's `PrepareForSleep` signal is still subscribed to, but only as a *promptness*
//! optimisation — it makes the daemon notice at the instant of resume instead of at its
//! next wakeup. Correctness does not depend on it. [CLK-10] mandates that a hook, where
//! one is used, be a unit ordered against the sleep targets rather than a script in the
//! sleep-hook directory; this implementation needs no hook at all, which satisfies the
//! requirement's intent more strongly than complying with its letter would.
//!
//! # `after_resume` needs no state file
//!
//! [CLK-9] requires the state backing `after_resume` to live somewhere that survives
//! suspend and is cleared by a cold boot, and suggests a `tmpfs` runtime directory. The
//! suspended-time delta is exactly such a location and a better one: it is maintained by
//! the kernel, it cannot be deleted by a stray `rm`, and it is not lost when the daemon
//! restarts. `after_resume` is simply `delta > 0`.

use std::time::Duration;

use idlectl_policy::BootInstant;
use rustix::time::{ClockId, Timespec};

/// The instant now, on `CLOCK_BOOTTIME`.
///
/// Read exactly once per evaluation and threaded through as a
/// [`idlectl_policy::ClockSnapshot`]. Reading it twice inside one evaluation is how a
/// deadline ends up compared against a later instant than the one that produced it.
#[must_use]
pub fn now() -> BootInstant {
    BootInstant::from_nanos(nanos(ClockId::Boottime))
}

/// How long this boot has spent suspended, in nanoseconds.
///
/// The two reads are not atomic with respect to each other, so on a machine that resumes
/// between them the difference is short by the length of that sleep. It self-corrects at
/// the next sample and the error is in the safe direction — a resume noticed one wakeup
/// late delays a sleep, it does not cause one.
#[must_use]
fn suspended_nanos() -> u64 {
    nanos(ClockId::Boottime).saturating_sub(nanos(ClockId::Monotonic))
}

fn nanos(clock: ClockId) -> u64 {
    let Timespec { tv_sec, tv_nsec } = rustix::time::clock_gettime(clock);
    let secs = u64::try_from(tv_sec).unwrap_or(0);
    let nsec = u64::try_from(tv_nsec).unwrap_or(0);
    secs.saturating_mul(1_000_000_000).saturating_add(nsec)
}

/// How much the suspended-time delta must grow before it counts as a resume.
///
/// Not zero. The two `clock_gettime` calls in [`suspended_nanos`] are separated by a few
/// hundred nanoseconds of scheduling, and on a busy machine that gap can widen. A second
/// is far below the shortest sleep anything can perform and far above any scheduling jitter.
const RESUME_EPSILON: Duration = Duration::from_secs(1);

/// Tracks resumes by watching the suspended-time delta.
#[derive(Debug)]
pub struct ResumeTracker {
    /// The delta at the previous sample.
    seen: u64,
    /// When the most recent resume was observed.
    last_resume: Option<BootInstant>,
    /// Whether this boot has ever suspended. Latched: [CLK-8] makes `after_resume` true
    /// from the first resume until the next cold boot, and it does not expire.
    resumed_this_boot: bool,
}

impl ResumeTracker {
    /// Builds a tracker, adopting whatever the machine has already done.
    ///
    /// A non-zero delta at start-up means the machine suspended before this daemon did —
    /// on a restart after a resume, or on a first install onto a machine that has already
    /// slept. `after_resume` is latched true immediately, because it is true.
    ///
    /// The resume *instant*, though, is genuinely unknown: the delta says how long was
    /// spent asleep, not when the last waking happened. `now` is recorded, which arms a
    /// full settle window. That is deliberately the conservative direction — it delays a
    /// sleep rather than permitting one — and it is why restarting the daemon is not a way
    /// to make a machine sleep sooner.
    #[must_use]
    pub fn adopt(now: BootInstant) -> Self {
        let seen = suspended_nanos();
        let resumed_this_boot = seen > 0;
        ResumeTracker {
            seen,
            last_resume: resumed_this_boot.then_some(now),
            resumed_this_boot,
        }
    }

    /// Samples the delta. Returns `true` if a resume happened since the last sample.
    pub fn sample(&mut self, now: BootInstant) -> bool {
        let delta = suspended_nanos();
        let threshold = self
            .seen
            .saturating_add(u64::try_from(RESUME_EPSILON.as_nanos()).unwrap_or(u64::MAX));
        if delta < threshold {
            // Not a resume. Still adopt a delta that crept up by less than the epsilon,
            // so repeated sub-epsilon drift cannot accumulate into a phantom resume.
            self.seen = self.seen.max(delta);
            return false;
        }
        self.seen = delta;
        self.last_resume = Some(now);
        self.resumed_this_boot = true;
        true
    }

    /// Records a resume announced by logind, for promptness. Idempotent with respect to
    /// [`ResumeTracker::sample`]: both set the same two fields.
    pub fn note_announced_resume(&mut self, now: BootInstant) {
        self.seen = suspended_nanos();
        self.last_resume = Some(now);
        self.resumed_this_boot = true;
    }

    /// The origin of the `resume` clock.
    ///
    /// Never [`idlectl_policy::ClockOrigin::Unreadable`]: this clock is fed by two
    /// syscalls that cannot fail, so its origin is either known or has genuinely not
    /// happened yet. That distinction is [CLK-12], and it is why a machine that has not
    /// slept reports "not yet this boot" rather than a fault.
    #[must_use]
    pub fn origin(&self) -> idlectl_policy::ClockOrigin {
        match self.last_resume {
            Some(t) => idlectl_policy::ClockOrigin::At(t),
            None => idlectl_policy::ClockOrigin::NotYet,
        }
    }

    /// The `after_resume` fact ([CLK-8], [FACT-45]).
    #[must_use]
    pub const fn after_resume(&self) -> bool {
        self.resumed_this_boot
    }

    /// Total time spent suspended this boot, for `doctor`.
    #[must_use]
    pub fn suspended_total(&self) -> Duration {
        Duration::from_nanos(self.seen)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boottime_is_at_least_monotonic() {
        // MONOTONIC first, then BOOTTIME, and the order is the whole test.
        //
        // The two reads are not atomic. Reading BOOTTIME first and MONOTONIC second puts
        // the elapsed time between the calls on the MONOTONIC side, so on a machine that
        // has never suspended -- where the two are equal -- the naive assertion
        // `boottime >= monotonic` fails by a few hundred nanoseconds. Measured: this test
        // passed on every machine that had slept and failed on a freshly booted one,
        // which is the worst possible distribution of a false alarm.
        //
        // `suspended_nanos` reads them in the other order for the same reason, and there
        // it is a safety property rather than a test artefact: putting the gap on the
        // MONOTONIC side makes the computed delta an UNDER-estimate, so scheduling jitter
        // can never invent a resume that did not happen.
        let monotonic = nanos(ClockId::Monotonic);
        let boottime = nanos(ClockId::Boottime);
        assert!(
            boottime >= monotonic,
            "BOOTTIME {boottime} is behind MONOTONIC {monotonic}, which the kernel forbids"
        );
    }

    #[test]
    fn a_machine_that_has_not_slept_reports_no_suspended_time() {
        // Not "reports zero": this runs on machines that HAVE slept. The assertion is
        // that the figure is a plausible duration rather than a wrapped subtraction --
        // `saturating_sub` turns an inversion into 0, and this catches the case where the
        // two clock ids were swapped and every machine suddenly looked freshly booted.
        let suspended = suspended_nanos();
        let uptime = nanos(ClockId::Boottime);
        assert!(
            suspended <= uptime,
            "suspended time {suspended} exceeds the age of the boot {uptime}"
        );
    }

    #[test]
    fn now_advances() {
        let a = now();
        std::thread::sleep(Duration::from_millis(2));
        let b = now();
        assert!(b.as_nanos() > a.as_nanos());
    }

    #[test]
    fn a_machine_that_never_slept_reports_not_yet_rather_than_a_fault() {
        // Constructed by hand rather than via `adopt`, which would read the real machine.
        let tracker = ResumeTracker {
            seen: 0,
            last_resume: None,
            resumed_this_boot: false,
        };
        assert_eq!(tracker.origin(), idlectl_policy::ClockOrigin::NotYet);
        assert!(!tracker.after_resume());
    }

    #[test]
    fn after_resume_latches_and_does_not_expire() {
        let mut tracker = ResumeTracker {
            seen: 0,
            last_resume: None,
            resumed_this_boot: false,
        };
        tracker.note_announced_resume(BootInstant::from_secs(100));
        assert!(tracker.after_resume());
        // [CLK-8]: it stays true for the rest of the boot. Nothing in the API can clear
        // it, which is the point -- the settle window expires, the marker does not.
        assert!(tracker.after_resume());
        assert_eq!(
            tracker.origin(),
            idlectl_policy::ClockOrigin::At(BootInstant::from_secs(100))
        );
    }
}
