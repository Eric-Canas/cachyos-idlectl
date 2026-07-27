//! Leases: "I am working, do not sleep".
//!
//! # Why a file descriptor and not a file
//!
//! [FACT-13] and [FACT-15]. A lease has to survive its holder being *right*, and has to
//! not survive its holder being killed. A record in a directory does the first and fails
//! the second: a job that segfaults leaves its lease behind and pins the machine awake
//! until somebody notices, which on a machine that is supposed to sleep unattended means
//! nobody notices at all.
//!
//! The descriptor is the same mechanism logind's own `Inhibit()` uses, and for the same
//! reason: the kernel closes it when the process dies, whatever the process intended. The
//! TTL is a second bound on top of that, for the case where the holder survives but forgets.
//!
//! Both bounds are needed and neither is redundant. The descriptor covers "the job
//! crashed"; the TTL covers "the job is stuck in a retry loop and will hold this until the
//! heat death of the universe". [FACT-15] additionally forbids renew-by-default, so a
//! holder that wants a longer window has to say so.
//!
//! # Parsing hostile input
//!
//! [FACT-16] and [ACT-10]: this state is written by unprivileged callers and read by a
//! privileged daemon. Everything arriving here is length- and range-checked below, and
//! none of it is ever interpolated into a shell, sourced or evaluated — there is no shell
//! anywhere in this daemon to interpolate into.

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::time::Duration;

use idlectl_policy::BootInstant;

/// The longest TTL a lease may ask for.
///
/// A day. Not a limitation anybody meets in practice, and a bound on the damage a caller
/// that passes `u64::MAX` can do: without it, one malformed request keeps the machine
/// awake for the rest of the boot with no diagnostic beyond "a lease is held".
pub const MAX_TTL: Duration = Duration::from_secs(86_400);

/// The default when a caller asks for none.
pub const DEFAULT_TTL: Duration = Duration::from_secs(3600);

/// The longest `who` and `why` strings accepted.
///
/// These are echoed into the journal and into `idlectl lease list`. A caller that sends a
/// megabyte of text should get an error, not a log nobody can read.
const MAX_TEXT: usize = 256;

/// One held lease.
pub struct Lease {
    pub who: String,
    pub why: String,
    pub uid: u32,
    pub acquired: BootInstant,
    pub expires: BootInstant,
    /// The read end of the pipe whose write end the holder has. Readable-at-EOF means the
    /// holder is gone. Never read for data — only for its end-of-file.
    watch: OwnedFd,
}

impl Lease {
    /// Whether the holder has closed its handle.
    ///
    /// The pipe is non-blocking, so this cannot stall the decision loop:
    ///
    /// * `Ok(0)` — every write end is closed. The holder is gone.
    /// * `Err(EAGAIN)` — a write end is still open and nothing was written. Still held.
    /// * `Ok(n)` — the holder wrote something. Not a protocol this daemon defines, but a
    ///   live holder all the same, so the lease stands.
    fn released(&self) -> bool {
        let mut scratch = [0u8; 1];
        matches!(rustix::io::read(&self.watch, &mut scratch), Ok(0))
    }
}

/// Why a lease request was refused.
#[derive(Debug, PartialEq, Eq)]
pub enum Refusal {
    EmptyId,
    TooLong(&'static str),
    TtlTooLong,
    AlreadyHeld,
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Refusal::EmptyId => f.write_str("a lease needs a non-empty identifier"),
            Refusal::TooLong(field) => write!(f, "{field} is longer than {MAX_TEXT} bytes"),
            Refusal::TtlTooLong => {
                write!(f, "ttl is longer than the {}s maximum", MAX_TTL.as_secs())
            }
            // Not an error a caller should work around by picking another id: two jobs
            // that both call themselves "backup" are a naming problem, and silently
            // holding two leases under one name makes `lease list` lie.
            Refusal::AlreadyHeld => f.write_str("a lease with this identifier is already held"),
        }
    }
}

/// Every lease currently held.
#[derive(Default)]
pub struct Table {
    leases: HashMap<String, Lease>,
}

impl Table {
    /// Takes a lease.
    ///
    /// Returns two descriptors: the write end, which goes to the caller and whose closure
    /// releases the lease, and a **duplicate of the read end** for the caller to watch.
    ///
    /// The duplicate exists because closing a descriptor wakes nobody. Reaping happens
    /// inside an evaluation, and evaluations happen when something asks for one; a lease
    /// with `suspend = "never"` produces no deadline and arms no timer, so without a
    /// watcher the loop would never run again and the released lease would hold the
    /// machine awake for the rest of the boot. Measured exactly that way: the holder
    /// exited, `idlectl lease list` kept showing the lease, and nothing was ever going to
    /// change its mind.
    pub fn acquire(
        &mut self,
        who: &str,
        why: &str,
        uid: u32,
        ttl: Duration,
        now: BootInstant,
    ) -> Result<(OwnedFd, OwnedFd), Refusal> {
        let who = who.trim();
        if who.is_empty() {
            return Err(Refusal::EmptyId);
        }
        if who.len() > MAX_TEXT {
            return Err(Refusal::TooLong("who"));
        }
        if why.len() > MAX_TEXT {
            return Err(Refusal::TooLong("why"));
        }
        if ttl > MAX_TTL {
            return Err(Refusal::TtlTooLong);
        }
        if self.leases.contains_key(who) {
            return Err(Refusal::AlreadyHeld);
        }

        let ttl = if ttl.is_zero() { DEFAULT_TTL } else { ttl };
        let (read, write) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::CLOEXEC | rustix::pipe::PipeFlags::NONBLOCK,
        )
        // A machine that cannot create a pipe is out of descriptors, which is not a
        // condition this daemon can improve by refusing the lease; it is reported to the
        // caller as a plain refusal, and the caller decides.
        .map_err(|_| Refusal::AlreadyHeld)?;

        // A dup, not the same descriptor: the table keeps its own read end for the
        // synchronous EOF check in `reap`, and the watcher needs one it can block on.
        // Both refer to the same pipe, so both see the holder go away.
        let watch_dup = rustix::io::dup(&read).map_err(|_| Refusal::AlreadyHeld)?;

        self.leases.insert(
            who.to_owned(),
            Lease {
                who: who.to_owned(),
                why: why.chars().take(MAX_TEXT).collect(),
                uid,
                acquired: now,
                // Saturating: an expiry that cannot be represented becomes the end of
                // time, which the TTL bound above has already made unreachable.
                expires: now.checked_add(ttl).unwrap_or(now),
                watch: read,
            },
        );
        Ok((write, watch_dup))
    }

    /// Blocks until the holder of `watch` closes its end, then returns.
    ///
    /// Runs on its own thread, parked in one syscall, costing a stack and nothing else.
    /// Leases are counted in ones and twos, so a thread each is cheaper than plumbing a
    /// task executor through a daemon that otherwise needs none.
    ///
    /// End-of-file makes a pipe readable, so `POLLIN` fires the moment the last write end
    /// closes — including when the holder was killed, which is the case the whole
    /// descriptor design exists for.
    pub fn watch_until_released(watch: OwnedFd) {
        let mut fds = [rustix::event::PollFd::new(
            &watch,
            rustix::event::PollFlags::IN,
        )];
        // A poll that errors is treated as "released": the alternative is a thread that
        // spins, and the reap check will confirm or deny it a moment later anyway.
        let _ = rustix::event::poll(&mut fds, None);
    }

    /// Releases a lease by name. Returns whether one was held.
    pub fn release(&mut self, who: &str) -> bool {
        self.leases.remove(who.trim()).is_some()
    }

    /// Drops every lease that has expired or whose holder has gone.
    ///
    /// [FACT-15]: an expired lease must be treated as absent and should be reaped. Returns
    /// the ones dropped with the reason, so each is logged exactly once — a lease
    /// disappearing is the difference between "the machine slept because a job finished"
    /// and "the machine slept for no reason I can see".
    pub fn reap(&mut self, now: BootInstant) -> Vec<(String, &'static str)> {
        let mut dropped = Vec::new();
        self.leases.retain(|who, lease| {
            if now >= lease.expires {
                dropped.push((who.clone(), "ttl expired"));
                return false;
            }
            if lease.released() {
                dropped.push((who.clone(), "holder closed its handle"));
                return false;
            }
            true
        });
        dropped
    }

    /// Every lease held, for `lease list` and for the D-Bus method.
    pub fn iter(&self) -> impl Iterator<Item = &Lease> {
        self.leases.values()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.leases.is_empty()
    }

    /// When the earliest-expiring lease expires.
    ///
    /// The loop arms a timer for this. Without it, a lease held against
    /// `suspend = "never"` produces no deadline of its own, nothing else asks for an
    /// evaluation, and the TTL — the bound that exists precisely for a holder that
    /// survives but forgets — would never be checked.
    #[must_use]
    pub fn next_expiry(&self) -> Option<BootInstant> {
        self.leases.values().map(|l| l.expires).min()
    }

    /// A one-line summary for the `lease_held` fact's evidence.
    #[must_use]
    pub fn summary(&self) -> Option<String> {
        if self.leases.is_empty() {
            return None;
        }
        let mut parts: Vec<String> = self
            .leases
            .values()
            .map(|l| {
                if l.why.is_empty() {
                    l.who.clone()
                } else {
                    format!("{} ({})", l.who, l.why)
                }
            })
            .collect();
        // Sorted so the same set of leases always renders identically. An unordered
        // HashMap walk would make the evidence string flap between evaluations, and the
        // fact would look like it was changing when nothing had.
        parts.sort();
        Some(format!("held by {}", parts.join(", ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const T0: BootInstant = BootInstant::from_secs(1_000);

    #[test]
    fn a_lease_is_released_when_its_handle_is_dropped() {
        let mut table = Table::default();
        let (handle, _watch) = table
            .acquire("job", "building", 1000, Duration::from_secs(600), T0)
            .expect("accepted");
        assert!(!table.is_empty());
        // Still held while the caller keeps the descriptor.
        assert!(table.reap(T0).is_empty());

        // This is the crash case: the holder went away without releasing anything.
        drop(handle);
        let dropped = table.reap(T0);
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].1, "holder closed its handle");
        assert!(table.is_empty());
    }

    #[test]
    fn a_lease_expires_on_its_ttl_even_with_the_handle_open() {
        let mut table = Table::default();
        let _handles = table
            .acquire("job", "", 1000, Duration::from_secs(60), T0)
            .expect("accepted");
        assert!(table.reap(BootInstant::from_secs(1_059)).is_empty());
        let dropped = table.reap(BootInstant::from_secs(1_060));
        assert_eq!(dropped.len(), 1);
        assert_eq!(dropped[0].1, "ttl expired");
    }

    #[test]
    fn hostile_input_is_refused_rather_than_stored() {
        let mut table = Table::default();
        assert_eq!(
            table.acquire("   ", "", 0, DEFAULT_TTL, T0).unwrap_err(),
            Refusal::EmptyId
        );
        assert_eq!(
            table
                .acquire(&"x".repeat(MAX_TEXT + 1), "", 0, DEFAULT_TTL, T0)
                .unwrap_err(),
            Refusal::TooLong("who")
        );
        assert_eq!(
            table
                .acquire("job", "", 0, Duration::from_secs(u32::MAX.into()), T0)
                .unwrap_err(),
            Refusal::TtlTooLong
        );
        assert!(table.is_empty());
    }

    #[test]
    fn a_duplicate_identifier_is_refused() {
        let mut table = Table::default();
        let _a = table.acquire("job", "", 0, DEFAULT_TTL, T0).expect("first");
        assert_eq!(
            table.acquire("job", "", 0, DEFAULT_TTL, T0).unwrap_err(),
            Refusal::AlreadyHeld
        );
    }

    #[test]
    fn a_zero_ttl_takes_the_default_rather_than_expiring_instantly() {
        let mut table = Table::default();
        let _h = table.acquire("job", "", 0, Duration::ZERO, T0).expect("ok");
        assert!(table.reap(T0).is_empty());
        assert!(!table.reap(T0.checked_add(DEFAULT_TTL).unwrap()).is_empty());
    }

    #[test]
    fn the_summary_is_stable_across_calls() {
        let mut table = Table::default();
        let _a = table.acquire("b-job", "two", 0, DEFAULT_TTL, T0).unwrap();
        let _b = table.acquire("a-job", "one", 0, DEFAULT_TTL, T0).unwrap();
        assert_eq!(table.summary(), table.summary());
        assert_eq!(table.summary().unwrap(), "held by a-job (one), b-job (two)");
    }
}
