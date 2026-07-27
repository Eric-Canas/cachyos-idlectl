//! The session-agent registry: where `human_active` comes from, and how the screen is
//! blanked.
//!
//! # Why a root daemon cannot do either of these itself
//!
//! Human input and screen blanking are both properties of a *session*, and a session is
//! owned by a compositor. On Wayland, only the compositor — or whoever holds DRM master —
//! can turn an output off, and a root daemon has no business being either ([ACT-12]). The
//! idle signal has the same shape: [CLK-4] requires it to come from an idle-notification
//! protocol that reports **real input**, which on Wayland is `ext-idle-notify-v1` and on
//! X11 is the XScreenSaver idle counter. Both are session-scoped.
//!
//! So the daemon decides and the agent performs. The agent commands nothing but the
//! screen, only inside its own session, and only when the caller is the daemon.
//!
//! # Why the heartbeat exists
//!
//! [CLK-5], and it is the least obvious requirement in the specification. The human
//! activity signal is only trustworthy in the negative direction: **while somebody is
//! holding a controller, an idle notifier emits nothing at all.** A naive "last activity"
//! timestamp therefore goes stale during exactly the activity it is supposed to detect,
//! and a daemon reading it concludes the machine is idle while a game is being played. The
//! reliable construction is the one that *disappears* on failure — an agent that must keep
//! saying "I am here and the session has been idle for N seconds", so that silence is
//! read as doubt rather than as idleness.

use std::collections::HashMap;
use std::time::Duration;

use idlectl_policy::{BootInstant, ClockOrigin};

use crate::facts::{Reading, ago};

/// How long an agent may go quiet before its reports are treated as stale.
///
/// The agent sends at least one report every [`HEARTBEAT_INTERVAL`]; this is three of
/// those, so a single missed report on a loaded machine does not raise a false fault while
/// a genuinely dead agent is noticed in well under two minutes.
pub const HEARTBEAT_MAX_AGE: Duration = Duration::from_secs(90);

/// The interval the agent is told to report at. Published so the agent and the daemon
/// cannot drift apart by being configured separately.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);

/// `ReportIdle` uses this to say "I am alive and my idle protocol is not answering".
///
/// Distinct from a large idle time on purpose: "idle for a very long time" would permit
/// sleep, and an agent whose protocol broke has observed nothing at all.
pub const IDLE_UNKNOWN: u64 = u64::MAX;

/// One registered agent.
#[derive(Clone, Debug)]
pub struct Agent {
    /// The session the agent lives in, as logind names it.
    pub session_id: String,
    /// The uid it runs as, taken from the bus rather than from the caller's claim.
    pub uid: u32,
    /// Whether this session offers a blanking mechanism at all ([ACT-13]).
    pub can_blank: bool,
    /// How long the session had been idle at [`Agent::last_report`], or [`None`] when the
    /// agent said its idle protocol is not answering.
    pub idle: Option<Duration>,
    /// What the agent last said about MPRIS playback in its session, in the four-name
    /// state vocabulary. Empty before the first report.
    pub media: String,
    /// When the last report arrived. Staleness is measured from here.
    pub last_report: BootInstant,
}

impl Agent {
    fn stale_at(&self, now: BootInstant) -> bool {
        now.since(self.last_report) > HEARTBEAT_MAX_AGE
    }
}

/// Every agent currently registered, keyed by its unique bus connection name.
///
/// Keyed by the *unique* name, not by a well-known one, and that is a security property
/// rather than a detail: an agent owns no well-known name, so an ordinary user cannot
/// squat the interface by claiming one. The daemon calls back on the connection that
/// registered, and only uid 0 may call the agent's methods.
#[derive(Debug, Default)]
pub struct Registry {
    agents: HashMap<String, Agent>,
    /// Whether the daemon believes the screen is currently blank.
    ///
    /// [ACT-7]: an action already in effect must not be re-issued until its deadline has
    /// been re-armed by a new origin. Without this, a blank panel re-fires `screen_off`
    /// every evaluation and, because a due action is handled before the sleep actions are
    /// considered, starves them forever.
    blanked: bool,
}

impl Registry {
    /// Adds or replaces an agent.
    pub fn register(&mut self, unique_name: String, agent: Agent) {
        self.agents.insert(unique_name, agent);
    }

    /// Removes an agent, on disconnect or on an explicit unregister.
    pub fn remove(&mut self, unique_name: &str) -> bool {
        self.agents.remove(unique_name).is_some()
    }

    /// Records a session report. Returns `false` if the sender is not registered.
    pub fn report(
        &mut self,
        unique_name: &str,
        idle_usec: u64,
        media: &str,
        now: BootInstant,
    ) -> bool {
        let Some(agent) = self.agents.get_mut(unique_name) else {
            return false;
        };
        agent.idle = (idle_usec != IDLE_UNKNOWN).then(|| Duration::from_micros(idle_usec));
        agent.media = media.to_owned();
        agent.last_report = now;
        true
    }

    /// The `media_playing` fact, folded across every live agent.
    ///
    /// TRUE wins over everything: a film playing on one seat is a film playing. After that
    /// doubt wins over knowledge, in keeping with [FACT-1] — an agent that could not read
    /// its own session bus has not established that nothing is playing.
    ///
    /// No live agent at all is UNAVAILABLE and never doubt. [FACT-17] says the fact is
    /// unavailable when no session bus is reachable, and from the daemon's side that is
    /// exactly the situation: it cannot reach one itself, by construction. The dead-agent
    /// case is already reported once, by `human_active`, and vetoing twice for one cause
    /// would just make `doctor` harder to read.
    #[must_use]
    pub fn media(&self, now: BootInstant) -> Reading {
        let live: Vec<&Agent> = self.agents.values().filter(|a| !a.stale_at(now)).collect();
        if live.is_empty() {
            return Reading::absent("no session agent, so no session bus is reachable");
        }
        if let Some(agent) = live.iter().find(|a| a.media == "true") {
            return Reading::yes(format!("playing in session {}", agent.session_id));
        }
        if let Some(agent) = live.iter().find(|a| a.media == "indeterminate") {
            return Reading::doubt(format!(
                "the agent in session {} could not read MPRIS",
                agent.session_id
            ));
        }
        if live
            .iter()
            .all(|a| a.media == "unavailable" || a.media.is_empty())
        {
            return Reading::absent("no session bus reachable in any session");
        }
        Reading::no("no MPRIS player is playing")
    }

    /// Drops every agent that has stopped reporting.
    ///
    /// Returns the ones dropped, so the caller can log each exactly once. They are removed
    /// rather than merely ignored: a stale agent that comes back registers again, and
    /// keeping the corpse would mean a session with two entries.
    pub fn reap(&mut self, now: BootInstant) -> Vec<Agent> {
        let stale: Vec<String> = self
            .agents
            .iter()
            .filter(|(_, a)| a.stale_at(now))
            .map(|(name, _)| name.clone())
            .collect();
        stale
            .into_iter()
            .filter_map(|name| self.agents.remove(&name))
            .collect()
    }

    pub fn iter(&self) -> impl Iterator<Item = (&String, &Agent)> {
        self.agents.iter()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.agents.is_empty()
    }

    /// Whether `screen_off` is available as an action at all ([ACT-13]).
    ///
    /// An action reported as unavailable is knowledge, not doubt: it raises no veto on
    /// anything. It only means the daemon must not propose it, and that `doctor` must
    /// list every block whose `screen_off` key is therefore inert.
    #[must_use]
    pub fn can_blank(&self) -> bool {
        self.agents.values().any(|a| a.can_blank)
    }

    #[must_use]
    pub const fn blanked(&self) -> bool {
        self.blanked
    }

    pub fn set_blanked(&mut self, blanked: bool) {
        self.blanked = blanked;
    }

    /// The `human_active` fact and the origin of the human-input clock.
    ///
    /// The three cases of [HUM-5] are three distinct fact **states**, which is what makes
    /// "this machine has been awake for a week because its idle agent died" diagnosable
    /// from the fact table alone instead of from a week of journal.
    ///
    /// `has_graphical_session` is what separates a *fault* from a *machine shape*. An
    /// agentless machine that has a compositor running is case 3: something that should be
    /// there is missing. An agentless machine with no graphical session at all is not
    /// broken — it is a headless box, and [CLK-7]'s remote-relay fast path requires it to
    /// be able to finish its job and sleep. Treating those two alike would either freeze
    /// every server awake or hide every dead agent, depending on which way it was
    /// resolved.
    #[must_use]
    pub fn human(
        &self,
        now: BootInstant,
        min_idle: Duration,
        has_graphical_session: bool,
    ) -> (Reading, ClockOrigin) {
        let live: Vec<&Agent> = self.agents.values().filter(|a| !a.stale_at(now)).collect();

        if live.is_empty() {
            return if has_graphical_session {
                (
                    Reading::doubt(
                        "no session agent is reporting, but a graphical session exists -- \
                         idlectl-agent is not running or has stopped answering",
                    ),
                    ClockOrigin::Unreadable,
                )
            } else {
                (
                    Reading::no("no graphical session on this machine; nothing has been touched"),
                    ClockOrigin::NotYet,
                )
            };
        }

        // An agent that is alive but whose idle protocol is not answering is doubt, even
        // if another session is reporting cleanly: the daemon has lost sight of one place
        // a human could be, and [FACT-1] forbids reporting an observation that was not made.
        if let Some(broken) = live.iter().find(|a| a.idle.is_none()) {
            return (
                Reading::doubt(format!(
                    "the agent in session {} is alive but its idle protocol is not answering",
                    broken.session_id
                )),
                ClockOrigin::Unreadable,
            );
        }

        // The most recent input across all sessions wins: a human at any seat is a human.
        let (idle, session) = live
            .iter()
            .filter_map(|a| a.idle.map(|d| (d, a)))
            // The report describes the state at `last_report`; time has passed since.
            .map(|(d, a)| (d + now.since(a.last_report), a))
            .min_by_key(|(d, _)| *d)
            .expect("the broken case returned above");

        let origin = ClockOrigin::At(now.saturating_sub(idle));
        if idle < min_idle {
            (
                Reading::yes(format!(
                    "input in session {} {}",
                    session.session_id,
                    ago(idle)
                )),
                origin,
            )
        } else {
            (
                Reading::no(format!(
                    "last input {} (threshold {}s)",
                    ago(idle),
                    min_idle.as_secs()
                )),
                origin,
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn agent(session: &str, idle: Option<Duration>, last_report: u64) -> Agent {
        Agent {
            session_id: session.to_owned(),
            uid: 1000,
            can_blank: true,
            idle,
            media: "false".to_owned(),
            last_report: BootInstant::from_secs(last_report),
        }
    }

    const MIN_IDLE: Duration = Duration::from_secs(300);

    /// [HUM-5] case 1.
    #[test]
    fn recent_input_is_true() {
        let mut reg = Registry::default();
        reg.register(
            ":1.5".into(),
            agent("c2", Some(Duration::from_secs(10)), 1000),
        );
        let (reading, origin) = reg.human(BootInstant::from_secs(1000), MIN_IDLE, true);
        assert_eq!(reading.state, idlectl_policy::FactState::True);
        assert_eq!(origin, ClockOrigin::At(BootInstant::from_secs(990)));
    }

    /// [HUM-5] case 2 -- and the reason a headless machine can still go back to sleep.
    #[test]
    fn no_agent_and_no_graphical_session_is_false_not_doubt() {
        let reg = Registry::default();
        let (reading, origin) = reg.human(BootInstant::from_secs(1000), MIN_IDLE, false);
        assert_eq!(reading.state, idlectl_policy::FactState::False);
        assert_eq!(origin, ClockOrigin::NotYet);
    }

    /// [HUM-5] case 3. The distinction from the test above is the whole of [HUM-4]:
    /// a compositor with no agent in it is a fault, and a machine with no compositor
    /// is not.
    #[test]
    fn a_graphical_session_with_no_agent_is_doubt() {
        let reg = Registry::default();
        let (reading, origin) = reg.human(BootInstant::from_secs(1000), MIN_IDLE, true);
        assert_eq!(reading.state, idlectl_policy::FactState::Indeterminate);
        assert_eq!(origin, ClockOrigin::Unreadable);
    }

    /// A stale heartbeat is a dead agent, whatever it last claimed. This is [CLK-5]: the
    /// signal has to disappear on failure, because while somebody is holding a controller
    /// an idle notifier emits nothing and a last-activity timestamp would look identical
    /// to a crashed agent.
    #[test]
    fn a_stale_agent_stops_counting() {
        let mut reg = Registry::default();
        reg.register(":1.5".into(), agent("c2", Some(Duration::ZERO), 0));
        let now = BootInstant::from_secs(HEARTBEAT_MAX_AGE.as_secs() + 1);
        let (reading, _) = reg.human(now, MIN_IDLE, true);
        assert_eq!(reading.state, idlectl_policy::FactState::Indeterminate);
        assert_eq!(reg.reap(now).len(), 1);
        assert!(reg.is_empty());
    }

    /// An agent alive with a broken protocol must not be rescued by a healthy sibling.
    #[test]
    fn one_broken_agent_makes_the_whole_reading_doubt() {
        let mut reg = Registry::default();
        reg.register(
            ":1.5".into(),
            agent("c2", Some(Duration::from_secs(1)), 1000),
        );
        reg.register(":1.6".into(), agent("c3", None, 1000));
        let (reading, origin) = reg.human(BootInstant::from_secs(1000), MIN_IDLE, true);
        assert_eq!(reading.state, idlectl_policy::FactState::Indeterminate);
        assert_eq!(origin, ClockOrigin::Unreadable);
    }

    /// The report is a snapshot taken at `last_report`; the daemon must add the time that
    /// has passed since. Without this a machine whose agent reports every 30 s reads as
    /// "idle 0 s" for the whole interval and `human_active` never goes false.
    #[test]
    fn idle_accrues_between_reports() {
        let mut reg = Registry::default();
        reg.register(
            ":1.5".into(),
            agent("c2", Some(Duration::from_secs(280)), 1000),
        );
        let (reading, _) = reg.human(BootInstant::from_secs(1030), MIN_IDLE, true);
        assert_eq!(
            reading.state,
            idlectl_policy::FactState::False,
            "280 + 30 > 300"
        );
    }

    #[test]
    fn the_most_recent_input_across_sessions_wins() {
        let mut reg = Registry::default();
        reg.register(
            ":1.5".into(),
            agent("c2", Some(Duration::from_secs(9000)), 1000),
        );
        reg.register(
            ":1.6".into(),
            agent("c3", Some(Duration::from_secs(3)), 1000),
        );
        let (reading, _) = reg.human(BootInstant::from_secs(1000), MIN_IDLE, true);
        assert_eq!(reading.state, idlectl_policy::FactState::True);
    }
}
