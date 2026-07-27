//! A rest request that could not fire yet, held until it can -- [REQ-6].
//!
//! # What it is for
//!
//! A relay that has finished with a machine wants to say "you can go to sleep now" once and
//! be done. Without this, a request that lands while a game is running is refused, and the
//! caller has to poll -- which means the caller needs its own timer, its own idea of how
//! long to keep trying, and its own copy of "is it worth asking again". Three things the
//! machine already knows.
//!
//! With it, the request is remembered and re-evaluated on the ordinary schedule ([COMP-6]).
//! The machine sleeps the moment the last veto clears, and the relay hears about it or does
//! not, having already hung up.
//!
//! # It weakens nothing
//!
//! A pending request is re-evaluated exactly as the original was: it satisfies the same two
//! blocks `--now` satisfies and no others. A game, a download, a held lease, an open remote
//! session and any doubtful detector all still refuse it, every time it is retried. What is
//! remembered is the *asking*, never the answer.
//!
//! # Why it is not written to a file
//!
//! `idlepolicyd.service` states that the daemon writes no runtime state anywhere, and that
//! property is worth more than surviving its own restart. The system this was extracted
//! from kept its pending request in `/run` because it was not a daemon at all: a timer ran
//! a script every five minutes, so *all* of its state had to outlive the process. A daemon
//! holding it in memory is the same mechanism without the file.
//!
//! What is lost is a pending request across a package upgrade, and the direction of that
//! loss is the safe one: the machine stays awake and somebody has to ask again. The
//! alternative -- a file that survives a restart -- can also survive a change of mind.

use idlectl_policy::{Action, BootInstant, ClockOrigin};
use tracing::info;

/// A request that has been accepted but not yet carried out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pending {
    /// The action asked for, by name. Never substituted -- see
    /// [`Decision::action_to_perform`](idlectl_policy::Decision::action_to_perform).
    pub action: Action,
    /// When this request stops being retried.
    pub expires_at: BootInstant,
    /// Where the human-input clock stood when the request was made, for [REQ-7].
    pub human_origin: ClockOrigin,
    /// Who asked, for the journal.
    pub uid: u32,
}

/// Why a pending request stopped being pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dropped {
    /// Its TTL ran out.
    Expired,
    /// Somebody touched the machine after it was made -- [REQ-7].
    HumanArrived,
}

impl Dropped {
    pub const fn reason(self) -> &'static str {
        match self {
            Dropped::Expired => "its time to live ran out before every veto cleared",
            Dropped::HumanArrived => "somebody used the machine after it was asked to rest",
        }
    }
}

impl Pending {
    /// Whether this request should be dropped, and why.
    ///
    /// [REQ-7]: a request is discarded once the human-input clock advances past where it
    /// stood when the request was made. Somebody walked in; the machine now belongs to
    /// whoever is in front of it, and a relay's opinion from ten minutes ago does not
    /// outrank them.
    ///
    /// [REQ-8]: a resume is not human input, and nothing here has to arrange that. The
    /// human-input clock is fed by an idle protocol that reports real input only, so a
    /// machine woken by a relay to run a job does not cancel the very request that will let
    /// it sleep again afterwards.
    ///
    /// The comparison is deliberately one-sided: it drops the request only when input can
    /// be *proven* to have happened, meaning both origins are known and the clock has moved
    /// forward. An origin that merely became readable or unreadable proves nothing -- an
    /// agent that restarted carries its instant over, so "unknown, then known" is not an
    /// arrival. Being wrong in that direction costs nothing, because a human who really is
    /// there raises `human_active`, and that floor refuses the action on its own.
    #[must_use]
    pub fn dropped_at(&self, now: BootInstant, human: ClockOrigin) -> Option<Dropped> {
        if now >= self.expires_at {
            return Some(Dropped::Expired);
        }
        if let (Some(was), Some(is)) = (self.human_origin.instant(), human.instant())
            && is > was
        {
            return Some(Dropped::HumanArrived);
        }
        None
    }

    /// Log line for a request that has just been dropped, so that a cancelled request is
    /// always evidence rather than a silence ([REQ-7], [OBS-6]).
    pub fn log_dropped(&self, why: Dropped) {
        info!(
            action = self.action.name(),
            uid = self.uid,
            reason = why.reason(),
            "pending rest request dropped"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::{Dropped, Pending};
    use idlectl_policy::{Action, BootInstant, ClockOrigin};

    fn at(secs: u64) -> BootInstant {
        BootInstant::from_secs(secs)
    }

    fn request(expires: u64, human: ClockOrigin) -> Pending {
        Pending {
            action: Action::PowerOff,
            expires_at: at(expires),
            human_origin: human,
            uid: 1000,
        }
    }

    #[test]
    fn survives_while_the_machine_is_merely_busy() {
        let p = request(9000, ClockOrigin::At(at(100)));
        assert_eq!(p.dropped_at(at(5000), ClockOrigin::At(at(100))), None);
    }

    #[test]
    fn expires_on_its_ttl() {
        let p = request(9000, ClockOrigin::At(at(100)));
        assert_eq!(
            p.dropped_at(at(9000), ClockOrigin::At(at(100))),
            Some(Dropped::Expired)
        );
    }

    /// [REQ-7].
    #[test]
    fn a_human_arriving_cancels_it() {
        let p = request(9000, ClockOrigin::At(at(100)));
        assert_eq!(
            p.dropped_at(at(5000), ClockOrigin::At(at(4000))),
            Some(Dropped::HumanArrived)
        );
    }

    /// [REQ-8]. The clock does not move on a resume, so neither does this. Modelled as the
    /// origin staying exactly where it was across a wake, which is what the agent reports.
    #[test]
    fn a_resume_does_not_cancel_it() {
        let p = request(9000, ClockOrigin::At(at(100)));
        assert_eq!(p.dropped_at(at(8000), ClockOrigin::At(at(100))), None);
    }

    /// An agent that restarted may report the clock as unreadable for a few seconds. That
    /// is not somebody arriving, and a request that survived a game must not be lost to it.
    #[test]
    fn an_unreadable_clock_is_not_an_arrival() {
        let p = request(9000, ClockOrigin::At(at(100)));
        assert_eq!(p.dropped_at(at(5000), ClockOrigin::Unreadable), None);
        let q = request(9000, ClockOrigin::Unreadable);
        assert_eq!(q.dropped_at(at(5000), ClockOrigin::At(at(100))), None);
    }

    /// Expiry is checked before arrival: an expired request is expired whatever else
    /// happened, and reporting the reason the operator can act on matters more.
    #[test]
    fn expiry_wins_over_arrival() {
        let p = request(9000, ClockOrigin::At(at(100)));
        assert_eq!(
            p.dropped_at(at(9500), ClockOrigin::At(at(9400))),
            Some(Dropped::Expired)
        );
    }
}
