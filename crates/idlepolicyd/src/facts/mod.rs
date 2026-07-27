//! The eleven detectors.
//!
//! # The one thing every detector must get right
//!
//! Three states are not enough and four are not decoration. A detector answers with
//! [`FactState::True`], [`FactState::False`], [`FactState::Indeterminate`] — "I exist and
//! I could not tell" — or [`FactState::Unavailable`] — "this machine has no such
//! capability". The last two look alike and compose in opposite directions: absence is
//! knowledge and behaves as false, while doubt vetoes every sleep action on the machine
//! whether or not any block mentions the fact ([FACT-4]).
//!
//! The consequence for anyone adding a detector: **a detector that cannot read its source
//! returns `Indeterminate`, never `False`**. Returning false there is how a veto silently
//! stops existing. The inverse mistake — returning indeterminate where the capability is
//! merely absent — freezes the machine awake forever, which is why every detector below
//! begins by deciding whether the capability is present at all, and answers `Unavailable`
//! before it tries to read anything.
//!
//! # Detector isolation
//!
//! [FACT-37]: a detector fault must not terminate the daemon. Each detector returns a
//! [`Reading`] instead of an error, so there is no `?` in this module that could propagate
//! out of one detector into another, and [FACT-38] follows from the same shape: nothing
//! here holds a lock at all.

use std::collections::BTreeMap;
use std::time::Duration;

use idlectl_policy::{FactId, FactSet, FactState, Policy};

pub mod counters;
pub mod gpu;
pub mod sessions;
pub mod steam;

/// One detector's answer, with the evidence for it.
///
/// The `detail` is not cosmetic. [OBS-3].1 requires `doctor` to name the specific fault
/// behind every indeterminate reading, and [OBS-3].2 the missing capability behind every
/// unavailable one; a reading that carried only a state would make the two requirements
/// unimplementable. For a `True` reading it carries the evidence — which process, which
/// file, how long ago — because "why is this machine awake?" is answered by that string.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Reading {
    pub state: FactState,
    pub detail: String,
}

impl Reading {
    #[must_use]
    pub fn new(state: FactState, detail: impl Into<String>) -> Self {
        Reading {
            state,
            detail: detail.into(),
        }
    }

    #[must_use]
    pub fn yes(detail: impl Into<String>) -> Self {
        Reading::new(FactState::True, detail)
    }

    #[must_use]
    pub fn no(detail: impl Into<String>) -> Self {
        Reading::new(FactState::False, detail)
    }

    /// The detector exists here and could not answer. Vetoes every sleep action.
    #[must_use]
    pub fn doubt(detail: impl Into<String>) -> Self {
        Reading::new(FactState::Indeterminate, detail)
    }

    /// This machine has no such capability, or the fact was switched off by name.
    #[must_use]
    pub fn absent(detail: impl Into<String>) -> Self {
        Reading::new(FactState::Unavailable, detail)
    }
}

/// Everything the detectors need that they cannot read for themselves.
///
/// Passed in rather than reached for, so that each detector is a function of its inputs
/// and can be tested without a machine underneath it.
pub struct Context<'a> {
    pub policy: &'a Policy,
    /// The system bus, already connected.
    pub bus: &'a zbus::Connection,
    /// Whether at least one unexpired lease is held, and by whom.
    pub leases: Option<String>,
    /// The `human_active` reading, derived by the agent registry from idle notifications.
    pub human_active: Reading,
    /// The `media_playing` reading, reported by the session agents.
    ///
    /// Passed in rather than read here, because a root daemon **cannot connect to a user's
    /// session bus**: measured, the bus authenticates by uid and closes the connection.
    /// The agent reads MPRIS where the session bus is simply its own.
    pub media_playing: Reading,
    /// The DRM `fdinfo` GPU memory holders, reported by the session agents and folded
    /// across the live ones.
    ///
    /// Passed in for the same class of reason as `media_playing`, this time a permission
    /// wall rather than a bus one: `/proc/<pid>/fdinfo` is gated by `ptrace_may_access`,
    /// so reading another user's needs `CAP_SYS_PTRACE` -- measured -- and this daemon holds
    /// only `CAP_DAC_READ_SEARCH`. Raw holders; [`gpu::read`] is what attributes them.
    ///
    /// Empty when no agent is reporting, which is the absence of a reading rather than a
    /// broken one and must never become doubt.
    pub gpu_holders: Vec<gpu::Holder>,
    /// Whether this boot has resumed at least once ([CLK-8]).
    pub after_resume: bool,
    /// Session ids whose own presence a caller asked to be discounted ([FACT-9]).
    pub excluded_sessions: &'a [String],
}

/// The detectors, plus the little state the pollable ones must carry between samples.
#[derive(Debug, Default)]
pub struct Detectors {
    pub counters: counters::Sampler,
    /// The last reading of each fact, so that [OBS-7] can rate-limit fault records to one
    /// per transition into the fault state rather than one per evaluation.
    last: BTreeMap<FactId, FactState>,
}

/// The facts that no event announces, and which therefore need the [COMP-7] backstop
/// sweep to be noticed changing.
///
/// The rest arrive as events: `human_active` from the agent, `after_resume` from a clock
/// read, `remote_session` and `inhibitor_block` from logind's own signals, `lease_held`
/// from this daemon's own bookkeeping. Keeping this list short is what lets an idle
/// machine with nothing running sleep with **no** periodic wakeup at all — see
/// [`crate::engine`].
pub const POLLED: [FactId; 6] = [
    FactId::MediaPlaying,
    FactId::SteamGameRunning,
    FactId::SteamDownloading,
    FactId::GpuBusyGame,
    FactId::GpuBusyOther,
    FactId::LocalServiceBusy,
];

impl Detectors {
    /// Reads every enabled fact once.
    ///
    /// A disabled fact reports [`FactState::Unavailable`] and **its detector does not
    /// run** ([FACT-6], [CFG-12]). That is not an optimisation: a disabled fact is the
    /// supported escape hatch from a permanently-broken detector, and an escape hatch that
    /// still ran the broken detector would still pay its cost and still log its fault.
    pub async fn sample(&mut self, ctx: &Context<'_>) -> BTreeMap<FactId, Reading> {
        let mut out = BTreeMap::new();

        // The GPU is read once and answers two facts ([FACT-27]). Attribution needs to
        // know whether a game is running, so Steam is read first; when it is disabled or
        // unreadable, nothing can be attributed to a game and all load counts as
        // `gpu_busy_other`, which is the conservative direction.
        let game = if ctx.policy.fact_enabled(FactId::SteamGameRunning) {
            steam::game_running(ctx)
        } else {
            Reading::absent("disabled by configuration")
        };

        // `local_service_busy` is read before the GPU for the reason in [FACT-29]: a
        // process belonging to a tracked service must never be attributed to a game, even
        // when process ancestry would allow it. The first implementation of that rule
        // attributed a model server's 11 GiB to a running game -- it opened no hole,
        // because the service's own fact vetoed independently, but it wrote a false line
        // into the log, and a log that lies costs somebody an afternoon six months later.
        let service = if ctx.policy.fact_enabled(FactId::LocalServiceBusy) {
            self.counters.sample(ctx).await
        } else {
            Reading::absent("no counters_url is configured")
        };

        let gpu_enabled = ctx.policy.fact_enabled(FactId::GpuBusyGame)
            || ctx.policy.fact_enabled(FactId::GpuBusyOther);
        let gpu = if gpu_enabled {
            gpu::read(&ctx.gpu_holders, game.state == FactState::True, &service).await
        } else {
            gpu::Split {
                game: Reading::absent("disabled by configuration"),
                other: Reading::absent("disabled by configuration"),
            }
        };

        out.insert(FactId::HumanActive, ctx.human_active.clone());
        out.insert(
            FactId::AfterResume,
            if ctx.after_resume {
                Reading::yes("this boot has resumed from sleep at least once")
            } else {
                Reading::no("this boot has not slept")
            },
        );
        out.insert(FactId::RemoteSession, sessions::remote(ctx).await);
        out.insert(
            FactId::LeaseHeld,
            match &ctx.leases {
                Some(who) => Reading::yes(who.clone()),
                None => Reading::no("no lease held"),
            },
        );
        out.insert(FactId::InhibitorBlock, sessions::inhibitor(ctx).await);
        out.insert(FactId::MediaPlaying, ctx.media_playing.clone());
        out.insert(FactId::SteamGameRunning, game);
        out.insert(FactId::SteamDownloading, steam::downloading(ctx));
        out.insert(FactId::GpuBusyGame, gpu.game);
        out.insert(FactId::GpuBusyOther, gpu.other);
        out.insert(FactId::LocalServiceBusy, service);

        // Apply the disable switch uniformly, after the fact, so that no detector has to
        // remember to check it and none can be added later that forgets.
        for (id, reading) in &mut out {
            if !ctx.policy.fact_enabled(*id) {
                *reading = Reading::absent("disabled by configuration");
            }
        }

        out
    }

    /// Returns the facts whose state changed since the previous sample.
    ///
    /// [OBS-7]: fault records are rate-limited to one per transition into the fault state.
    /// The daemon logs from this list, not from the full reading set, which is the
    /// difference between one line when a detector breaks and one line every evaluation
    /// for as long as it stays broken.
    pub fn changed(&mut self, readings: &BTreeMap<FactId, Reading>) -> Vec<FactId> {
        let mut changed = Vec::new();
        for (id, reading) in readings {
            if self.last.insert(*id, reading.state) != Some(reading.state) {
                changed.push(*id);
            }
        }
        changed
    }
}

/// Collapses a reading set into the [`FactSet`] the pure engine consumes.
#[must_use]
pub fn to_fact_set(readings: &BTreeMap<FactId, Reading>) -> FactSet {
    let mut facts = FactSet::new();
    for (id, reading) in readings {
        facts.set(*id, reading.state);
    }
    facts
}

/// Renders a duration the way every detail string in this daemon does.
///
/// Deliberately whole seconds and no thousands separator: these strings end up in the
/// journal next to timestamps, and a "1.7 s" that is really 1.699 s invites somebody to
/// believe the daemon is measuring more precisely than it is.
#[must_use]
pub fn ago(d: Duration) -> String {
    let secs = d.as_secs();
    if secs < 90 {
        return format!("{secs}s ago");
    }
    let mins = secs / 60;
    if mins < 90 {
        return format!("{mins}m ago");
    }
    format!("{}h{:02}m ago", mins / 60, mins % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reading_that_cannot_answer_is_doubt_and_not_false() {
        // The single most consequential distinction in this module, asserted so that a
        // refactor that collapses them fails here rather than in somebody's bedroom.
        assert_eq!(Reading::doubt("x").state, FactState::Indeterminate);
        assert_ne!(Reading::doubt("x").state, FactState::False);
        assert_eq!(Reading::absent("x").state, FactState::Unavailable);
    }

    #[test]
    fn changed_reports_only_transitions() {
        let mut det = Detectors::default();
        let mut readings = BTreeMap::new();
        readings.insert(FactId::HumanActive, Reading::yes("keypress"));

        assert_eq!(det.changed(&readings), vec![FactId::HumanActive]);
        // Same state, different evidence: not a transition, and must not produce a second
        // log record. [OBS-7] is about the state, not the string.
        readings.insert(FactId::HumanActive, Reading::yes("mouse"));
        assert!(det.changed(&readings).is_empty());
        readings.insert(FactId::HumanActive, Reading::no("idle"));
        assert_eq!(det.changed(&readings), vec![FactId::HumanActive]);
    }

    #[test]
    fn ago_never_pretends_to_sub_second_precision() {
        assert_eq!(ago(Duration::from_millis(1699)), "1s ago");
        assert_eq!(ago(Duration::from_secs(300)), "5m ago");
        assert_eq!(ago(Duration::from_secs(9 * 3600 + 60)), "9h01m ago");
    }
}
