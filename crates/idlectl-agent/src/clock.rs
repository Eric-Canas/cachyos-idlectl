//! The one clock this agent is allowed to read.
//!
//! `CLOCK_BOOTTIME`, for the reason [`idlectl_policy::BootInstant`] already gives: it counts
//! real time and keeps counting while the machine is suspended. The agent reports an idle
//! *duration* and the daemon turns it into an origin on this clock, so measuring it on any
//! other clock puts the two halves of one computation on two different timelines.
//!
//! Measured, on a machine that had been up for 7 h 24 m of which 2 h 16 m were spent
//! suspended: `CLOCK_BOOTTIME` read 26649 s and `CLOCK_MONOTONIC` read 18471 s. An agent
//! keeping its stopwatch on the second one had lost 8178 s -- and reported a session
//! nobody had touched in seventy minutes as "input three minutes ago", because the
//! stopwatch had been frozen for the whole sleep.
//!
//! # Why this is not in `idlectl-policy` next to the type
//!
//! That crate is deliberately clock-free: it is plain data so a whole scenario can be built
//! in a unit test without a syscall. Two lines duplicated here is a smaller price than
//! making the policy engine depend on the machine it is reasoning about.

use idlectl_policy::BootInstant;
use rustix::time::{ClockId, Timespec};

/// The instant now, on `CLOCK_BOOTTIME`.
#[must_use]
pub fn now() -> BootInstant {
    let Timespec { tv_sec, tv_nsec } = rustix::time::clock_gettime(ClockId::Boottime);
    let secs = u64::try_from(tv_sec).unwrap_or(0);
    let nsec = u64::try_from(tv_nsec).unwrap_or(0);
    BootInstant::from_nanos(secs.saturating_mul(1_000_000_000).saturating_add(nsec))
}

#[cfg(test)]
mod tests {
    use super::now;

    /// The clock advances and does not run backwards. Weak on purpose: anything stronger
    /// would be a test of the kernel rather than of this function.
    #[test]
    fn advances() {
        let first = now();
        let second = now();
        assert!(second >= first);
    }

    /// A machine that has been up for zero nanoseconds does not exist, so a reading of
    /// exactly BOOT means the conversion above silently failed.
    #[test]
    fn is_not_boot() {
        assert_ne!(now(), idlectl_policy::BootInstant::BOOT);
    }
}
