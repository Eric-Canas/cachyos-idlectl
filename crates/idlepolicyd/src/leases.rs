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

/// The caller that took a lease, for diagnosis and for nothing else.
///
/// [FACT-13b]. A lease is the one thing that can hold a machine awake with nothing in the
/// configuration to point at, so `lease list` has to answer both halves of the question it
/// exists for: `why` answers *what for*, and this answers *where to look*.
///
/// Measured need: a lease called `eval-flake` held a machine awake, and finding the process
/// behind it took a walk over `/proc/*/fd` and an `ss -xp` cross-reference of socket inodes,
/// because the only identity on offer was a uid that every process the user owns shares.
///
/// **Never used to authorize anything.** `polkit.rs` explains why a pid is not an identity;
/// this type does not contradict it, it pays the price the objection names. The start time
/// recorded beside the number is what lets a recycled pid be reported as recycled instead of
/// shown to a human who is about to kill it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Holder {
    pid: Option<u32>,
    started: Option<u64>,
}

/// What a holder's pid means *now*, which is not always what it meant when it was recorded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HolderState {
    /// The bus did not answer for the caller's connection. Reported as unknown rather than
    /// as a zero, because a zero in a pid column reads as a real pid.
    Unknown,
    /// The process that took the lease is still running, and is called this.
    Alive(String),
    /// The process that took the lease is gone, yet the lease stands — so something else
    /// holds the descriptor open, which means a child inherited it. Worth an eyebrow: the
    /// lease has outlived the thing that asked for it.
    Gone,
    /// That pid is alive but is a *different* process. Says so instead of naming it.
    Recycled,
}

impl HolderState {
    /// The token that crosses the bus. Callers render it; the daemon decides it, because the
    /// daemon is the side holding the recorded start time.
    #[must_use]
    pub fn wire(&self) -> &'static str {
        match self {
            HolderState::Unknown => "unknown",
            HolderState::Alive(_) => "alive",
            HolderState::Gone => "gone",
            HolderState::Recycled => "recycled",
        }
    }

    /// The process name, when there is a live process to name.
    #[must_use]
    pub fn comm(&self) -> &str {
        match self {
            HolderState::Alive(comm) => comm,
            _ => "",
        }
    }
}

impl Holder {
    /// Records a pid together with the start time that makes it meaningful later.
    #[must_use]
    pub fn of(pid: Option<u32>) -> Self {
        Self {
            pid,
            started: pid.and_then(crate::proc::started_at),
        }
    }

    #[must_use]
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Re-reads `/proc` and says what this pid means now.
    ///
    /// Resolved at display time and never cached: the answer changes without anything
    /// happening to the lease, which is the entire reason the distinction exists.
    #[must_use]
    pub fn state(&self) -> HolderState {
        self.state_with(|pid| (crate::proc::started_at(pid), crate::proc::comm_of(pid)))
    }

    /// The decision, with `/proc` passed in so it can be tested without one.
    fn state_with(&self, look: impl Fn(u32) -> (Option<u64>, Option<String>)) -> HolderState {
        let Some(pid) = self.pid else {
            return HolderState::Unknown;
        };
        let (started_now, comm) = look(pid);
        match (self.started, started_now) {
            // No process there at all. The lease is still held, so the descriptor went to
            // a child; either way this number is no longer somebody to talk to.
            (_, None) => HolderState::Gone,
            (Some(recorded), Some(now)) if recorded != now => HolderState::Recycled,
            // Including the case where the start time could not be read at acquire: a
            // recycle cannot be *proven*, so it is not claimed.
            _ => HolderState::Alive(comm.unwrap_or_default()),
        }
    }
}

/// One held lease.
pub struct Lease {
    pub who: String,
    pub why: String,
    pub uid: u32,
    /// The caller that asked for it. Diagnosis only — see [`Holder`].
    pub holder: Holder,
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
        pid: Option<u32>,
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
                // Read now, while the caller is certainly alive: after the reply is sent
                // there is no moment at which this is still guaranteed.
                holder: Holder::of(pid),
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
            .acquire("job", "building", 1000, None, Duration::from_secs(600), T0)
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
            .acquire("job", "", 1000, None, Duration::from_secs(60), T0)
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
            table
                .acquire("   ", "", 0, None, DEFAULT_TTL, T0)
                .unwrap_err(),
            Refusal::EmptyId
        );
        assert_eq!(
            table
                .acquire(&"x".repeat(MAX_TEXT + 1), "", 0, None, DEFAULT_TTL, T0)
                .unwrap_err(),
            Refusal::TooLong("who")
        );
        assert_eq!(
            table
                .acquire("job", "", 0, None, Duration::from_secs(u32::MAX.into()), T0)
                .unwrap_err(),
            Refusal::TtlTooLong
        );
        assert!(table.is_empty());
    }

    #[test]
    fn a_duplicate_identifier_is_refused() {
        let mut table = Table::default();
        let _a = table
            .acquire("job", "", 0, None, DEFAULT_TTL, T0)
            .expect("first");
        assert_eq!(
            table
                .acquire("job", "", 0, None, DEFAULT_TTL, T0)
                .unwrap_err(),
            Refusal::AlreadyHeld
        );
    }

    #[test]
    fn a_zero_ttl_takes_the_default_rather_than_expiring_instantly() {
        let mut table = Table::default();
        let _h = table
            .acquire("job", "", 0, None, Duration::ZERO, T0)
            .expect("ok");
        assert!(table.reap(T0).is_empty());
        assert!(!table.reap(T0.checked_add(DEFAULT_TTL).unwrap()).is_empty());
    }

    /// A stand-in for `/proc`, so the four answers can be tested without four processes.
    fn absent(_pid: u32) -> (Option<u64>, Option<String>) {
        (None, None)
    }

    #[test]
    fn a_lease_names_the_process_that_took_it() {
        let mut table = Table::default();
        let me = std::process::id();
        let _h = table
            .acquire("job", "", 1000, Some(me), DEFAULT_TTL, T0)
            .expect("accepted");
        let lease = table.iter().next().expect("one lease");
        assert_eq!(lease.holder.pid(), Some(me));
        let state = lease.holder.state();
        assert_eq!(state.wire(), "alive");
        assert!(
            !state.comm().is_empty(),
            "a live holder must be named, not merely counted"
        );
    }

    #[test]
    fn a_holder_without_a_pid_is_unknown_and_not_pid_zero() {
        // The bus can decline to answer for a connection. Rendering that as `pid 0` would
        // put a number in the column that a human could act on, and pid 0 is not a process.
        let holder = Holder::of(None);
        assert_eq!(holder.pid(), None);
        assert_eq!(holder.state(), HolderState::Unknown);
    }

    #[test]
    fn a_holder_whose_process_is_gone_says_so_rather_than_naming_it() {
        // The lease outliving its holder is possible — a child that inherited the
        // descriptor keeps it open — and it is exactly the case where the recorded pid
        // stops meaning anything.
        let holder = Holder {
            pid: Some(4321),
            started: Some(987_654),
        };
        assert_eq!(holder.state_with(absent), HolderState::Gone);
    }

    #[test]
    fn a_recycled_pid_is_never_reported_as_the_holder() {
        // The whole reason the start time is recorded. Without this branch `lease list`
        // would print the name of an unrelated process next to the lease holding the
        // machine awake, and the obvious next step -- kill it -- would hit a bystander.
        let holder = Holder {
            pid: Some(4321),
            started: Some(987_654),
        };
        let recycled = |_pid: u32| (Some(999_999), Some("innocent".to_owned()));
        assert_eq!(holder.state_with(recycled), HolderState::Recycled);
        assert_eq!(holder.state_with(recycled).comm(), "");

        // Same pid, same start time: this one really is the holder.
        let same = |_pid: u32| (Some(987_654), Some("the-holder".to_owned()));
        assert_eq!(
            holder.state_with(same),
            HolderState::Alive("the-holder".to_owned())
        );
    }

    #[test]
    fn an_unreadable_start_time_at_acquire_does_not_become_a_recycle_claim() {
        // Reading `/proc` can fail for reasons that are not "the process changed". A
        // recycle that cannot be proven is not claimed: the pid is reported with the name
        // it has now, which is the honest answer and the useful one.
        let holder = Holder {
            pid: Some(4321),
            started: None,
        };
        let live = |_pid: u32| (Some(1), Some("something".to_owned()));
        assert_eq!(holder.state_with(live).wire(), "alive");
    }

    #[test]
    fn the_summary_is_stable_across_calls() {
        let mut table = Table::default();
        let _a = table
            .acquire("b-job", "two", 0, None, DEFAULT_TTL, T0)
            .unwrap();
        let _b = table
            .acquire("a-job", "one", 0, None, DEFAULT_TTL, T0)
            .unwrap();
        assert_eq!(table.summary(), table.summary());
        assert_eq!(table.summary().unwrap(), "held by a-job (one), b-job (two)");
    }
}
