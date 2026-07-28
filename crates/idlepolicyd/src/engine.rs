//! The decision loop: what the daemon knows, and what it does about it.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Duration;

use idlectl_config::{ConfigError, Source};
use idlectl_policy::{
    Action, BlockEdges, BootInstant, ClockSnapshot, Deadline, Decision, FactId, FactState, Policy,
    Request,
};
use tracing::{error, info, warn};

use crate::agents::Registry;
use crate::clock::{self, ResumeTracker};
use crate::facts::{self, Context, Detectors, Reading};
use crate::leases::Table;

/// The [COMP-7] backstop interval.
///
/// A periodic sweep MAY exist as a backstop for detectors that cannot be watched, and MUST
/// NOT be the primary trigger ([COMP-7], §13.4). This one is armed **only when something
/// that can only be polled is currently holding the machine awake** — see
/// [`Engine::sweep_needed`]. On an idle machine with no download, no GPU load and no
/// tracked service, the daemon arms one timer at the next real deadline and does not wake
/// up again until then. That is the difference between a daemon that costs nothing and a
/// five-minute tick, and it is why "does not consume resources" is a property of the
/// design rather than a claim about the code.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Where the configuration is read from. Kept so `Reload()` re-reads the same layers.
#[derive(Clone, Debug)]
pub struct Sources {
    pub vendor: PathBuf,
    pub admin: PathBuf,
    pub dropin_dir: PathBuf,
}

impl Sources {
    /// Builds the layer list: vendor file, admin file, then `*.toml` drop-ins in filename
    /// order, skipping the ones that are absent.
    ///
    /// A missing vendor file is an error — the package installs it, so its absence means a
    /// broken installation and guessing a default would hide that. A missing admin file
    /// and a missing drop-in directory are both normal.
    pub fn read(&self) -> anyhow::Result<Vec<Source>> {
        use anyhow::Context as _;

        let mut sources = vec![read_one(&self.vendor).with_context(|| {
            format!(
                "cannot read the vendor configuration at {}",
                self.vendor.display()
            )
        })?];

        if self.admin.exists() {
            sources.push(read_one(&self.admin)?);
        }

        if self.dropin_dir.is_dir() {
            let mut dropins: Vec<PathBuf> = std::fs::read_dir(&self.dropin_dir)
                .with_context(|| format!("cannot list {}", self.dropin_dir.display()))?
                .filter_map(Result::ok)
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|ext| ext == "toml"))
                .collect();
            // Lexicographic, like systemd's own drop-in ordering, so `10-` really does
            // come before `20-` regardless of what the filesystem returns.
            dropins.sort();
            for path in dropins {
                sources.push(read_one(&path)?);
            }
        }

        Ok(sources)
    }
}

fn read_one(path: &Path) -> anyhow::Result<Source> {
    use anyhow::Context as _;
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(Source::new(path.display().to_string(), text))
}

/// A record of an action that was forced past its floors ([REQ-12]).
#[derive(Clone, Debug)]
pub struct Forced {
    pub at: BootInstant,
    pub action: Action,
    pub why: String,
    pub uid: u32,
}

/// Everything the daemon knows.
pub struct Engine {
    pub sources: Sources,
    pub policy: Policy,
    pub layers: Vec<String>,
    pub warnings: Vec<String>,
    pub faults: Vec<ConfigError>,

    pub bus: zbus::Connection,
    pub detectors: Detectors,
    pub readings: BTreeMap<FactId, Reading>,
    pub edges: BlockEdges,
    pub agents: Registry,
    pub leases: Table,
    pub resume: ResumeTracker,

    /// The most recent decision and the clocks it was computed against, so that `explain`
    /// and `doctor` report **the numbers the decision actually used** ([OBS-2]) rather
    /// than recomputing a cheaper approximation next to it.
    pub decision: Option<Decision>,
    pub clocks: ClockSnapshot,
    pub last_eval: Option<BootInstant>,
    pub screen_off_available: bool,

    /// A request accepted but not yet carried out, retried on every evaluation until it
    /// fires, expires, or a human arrives -- [REQ-6]. In memory on purpose; see `pending`.
    pub pending: Option<crate::pending::Pending>,

    pub forced: Vec<Forced>,
    /// [ACT-2]: at most one power transition in flight at a time.
    transition_in_flight: bool,
    pub dry_run: bool,

    /// Poked by anything that should cause a re-evaluation before the timer would.
    pub wake: async_channel::Sender<&'static str>,
}

impl Engine {
    pub fn new(
        sources: Sources,
        bus: zbus::Connection,
        dry_run: bool,
        wake: async_channel::Sender<&'static str>,
    ) -> anyhow::Result<Self> {
        let now = clock::now();
        let raw = sources.read()?;
        let (loaded, faults) = idlectl_config::load(&raw);

        Ok(Engine {
            sources,
            policy: loaded.policy,
            layers: loaded.layers,
            warnings: loaded.warnings.iter().map(ToString::to_string).collect(),
            faults,
            bus,
            detectors: Detectors::default(),
            readings: BTreeMap::new(),
            edges: BlockEdges::new(),
            agents: Registry::default(),
            leases: Table::default(),
            resume: ResumeTracker::adopt(now),
            decision: None,
            clocks: ClockSnapshot::at(now),
            last_eval: None,
            screen_off_available: false,
            pending: None,
            forced: Vec::new(),
            transition_in_flight: false,
            dry_run,
            wake,
        })
    }

    /// Re-reads the configuration. Returns the layers applied.
    ///
    /// A file that cannot be parsed, or that carries a value the format does not accept,
    /// is dropped **as a whole** — per file, never per key, because a half-applied file is
    /// a policy nobody wrote. The layers that still parse take effect, and the fault
    /// floors the three sleep actions at `+infinity` until it is fixed ([CFG-16]).
    ///
    /// The daemon therefore never ends up with no policy at all, including on a first
    /// start after a bad edit, when there is no previous policy to fall back to. A machine
    /// with no owner of its power state is the condition this project exists to remove.
    pub fn reload(&mut self) -> anyhow::Result<Vec<String>> {
        let raw = self.sources.read()?;
        let (loaded, faults) = idlectl_config::load(&raw);
        self.policy = loaded.policy;
        self.layers = loaded.layers.clone();
        self.warnings = loaded.warnings.iter().map(ToString::to_string).collect();
        for fault in &faults {
            error!("{fault}");
        }
        self.faults = faults;
        Ok(loaded.layers)
    }

    /// Reads every fact once and resolves the policy against it.
    ///
    /// Pure with respect to the machine: it changes the daemon's own bookkeeping (fact
    /// edges, counter samples, reaped leases) but performs no action. Every caller that
    /// wants an action calls [`Engine::tick`].
    pub async fn evaluate(&mut self, request: &Request) -> Decision {
        let now = clock::now();

        if self.resume.sample(now) {
            // [ACT-5] and [COMP-8]: after a resume every origin is recomputed before any
            // action may fire. That happens by construction here -- the snapshot below is
            // built after this point -- and the log line exists because a resume nothing
            // announced is otherwise invisible.
            info!(
                suspended_total_s = self.resume.suspended_total().as_secs(),
                "resume detected from the suspended-time delta"
            );
        }

        for agent in self.agents.reap(now) {
            warn!(
                session = agent.session_id,
                "session agent stopped reporting; human presence is now unknown in that session"
            );
        }
        for (who, why) in self.leases.reap(now) {
            info!(lease = who, reason = why, "lease released");
        }

        let has_graphical = !facts::sessions::graphical_uids(&self.bus)
            .await
            .unwrap_or_default()
            .is_empty();
        let (human, human_origin) = self.agents.human(now, self.policy.min_idle, has_graphical);

        let ctx = Context {
            policy: &self.policy,
            bus: &self.bus,
            leases: self.leases.summary(),
            human_active: human,
            media_playing: self.agents.media(now),
            gpu_holders: self.agents.gpu_holders(now),
            after_resume: self.resume.after_resume(),
            excluded_sessions: &[],
        };
        let readings = self.detectors.sample(&ctx).await;

        for id in self.detectors.changed(&readings) {
            let reading = &readings[&id];
            // [OBS-7]: one record per transition into the state, not one per evaluation.
            // [OBS-6]: and never silence -- a detector that broke must say so once.
            match reading.state {
                FactState::Indeterminate => {
                    warn!(
                        fact = id.name(),
                        detail = reading.detail,
                        "fact is indeterminate: every sleep action is held"
                    );
                }
                _ => {
                    info!(fact = id.name(), state = %reading.state, detail = reading.detail, "fact changed")
                }
            }
        }

        self.clocks = ClockSnapshot {
            now,
            human_input: human_origin,
            resume: self.resume.origin(),
        };
        let fact_set = facts::to_fact_set(&readings);
        self.edges.observe(&self.policy, &fact_set, now);

        let decision = idlectl_policy::resolve_request(
            &self.policy,
            &fact_set,
            &self.clocks,
            &self.edges,
            request,
        );

        self.screen_off_available = self.agents.can_blank();
        self.readings = readings;
        self.last_eval = Some(now);
        self.decision = Some(decision.clone());
        decision
    }

    /// Evaluates and then performs whatever is due.
    ///
    /// Returns the action performed, if any. `screen_off` is not returned: it is not a
    /// power transition, and reporting it as one would let a caller conclude the machine
    /// went to sleep because the panel went dark.
    /// Folds any held request into this evaluation, dropping it first if it should no
    /// longer be retried.
    ///
    /// Returns the request to evaluate: the caller's own if nothing is held, otherwise one
    /// that satisfies the same two blocks `--now` satisfies, for the action that was asked
    /// for by name.
    ///
    /// An explicit request in flight wins over a held one. Somebody asking right now has
    /// better information than a relay that hung up ten minutes ago.
    fn resolve_pending(&mut self, request: &Request) -> Request {
        let Some(held) = self.pending else {
            return *request;
        };
        if let Some(why) = held.dropped_at(self.clocks.now, self.clocks.human_input) {
            held.log_dropped(why);
            self.pending = None;
            return *request;
        }
        if request.requested.is_some() || request.force.is_some() {
            return *request;
        }
        Request {
            now: true,
            force: None,
            requested: Some(held.action),
        }
    }

    pub async fn tick(&mut self, request: &Request) -> Option<Action> {
        // A held request rides on the ordinary evaluation ([REQ-6]): same schedule, same
        // vetoes, same two satisfied blocks as when it was first made. Only the asking is
        // remembered, never the answer -- so a game that started since then still refuses
        // it, exactly as it would have refused the original.
        let request = &self.resolve_pending(request);

        let decision = self.evaluate(request).await;

        // [ACT-6]: the four actions are independent. `screen_off` firing implies nothing
        // about `suspend`, and `suspend` being vetoed does not veto `screen_off`. The two
        // halves below therefore do not talk to each other.
        self.apply_screen(&decision).await;

        let action = decision.action_to_perform()?;

        if self.transition_in_flight {
            // [ACT-2]. Not an error and not a retry: the previous transition is still
            // being carried out and the machine will be re-evaluated when it comes back.
            warn!(
                ?action,
                "a power transition is already in flight; not issuing another"
            );
            return None;
        }

        // [ACT-4]: between deciding that the deadline has passed and issuing the
        // transition, re-read every fact and recompute. Cheap insurance against the window
        // in which somebody touched a key, and the only thing standing between "the
        // machine suspended while I was using it" and a correct refusal.
        let confirm = self.evaluate(request).await;
        let Some(confirmed) = confirm.action_to_perform() else {
            info!(?action, "no longer permitted on re-evaluation; not issued");
            return None;
        };
        if confirmed != action {
            info!(?action, ?confirmed, "re-evaluation changed the action");
        }

        let resolution = confirm.get(confirmed);
        let reason = crate::report::why_due(&confirm, confirmed, &self.policy);

        if resolution.ceiling_defeated_floor() {
            // [CEIL-6], [CEIL-8]: a ceiling is the only construct that can act against a
            // floor, so it is never allowed to fire quietly.
            warn!(
                action = confirmed.name(),
                reason, "a ceiling defeated a floor"
            );
        }

        if self.dry_run {
            info!(
                action = confirmed.name(),
                reason, "dry run: would act, and did not"
            );
            return None;
        }

        self.transition_in_flight = true;
        let outcome = crate::actions::perform(&self.bus, confirmed).await;
        self.transition_in_flight = false;

        match outcome {
            Ok(()) => {
                // The request has been honoured; there is nothing left to retry.
                if self.pending.is_some_and(|p| p.action == confirmed) {
                    self.pending = None;
                }
                // [OBS-5]: exactly one record naming the action, the resolved instant, the
                // winning block, the clock and origin it was measured on, and whether the
                // action was autonomous, requested or forced.
                info!(
                    action = confirmed.name(),
                    deadline = %resolution.deadline,
                    reason,
                    kind = kind_of(request, confirmed),
                    "action performed"
                );
                Some(confirmed)
            }
            Err(err) => {
                // [ACT-3]: logged at warning level, never silently ignored, never retried
                // immediately. The next evaluation will decide again from scratch.
                warn!(
                    action = confirmed.name(),
                    error = err,
                    "the sleep mechanism refused"
                );
                None
            }
        }
    }

    /// Applies or releases `screen_off`.
    ///
    /// [ACT-7]: an action already in effect is not re-issued until its deadline has been
    /// re-armed by a new origin. A machine whose panel is already blank does not re-fire
    /// `screen_off` and therefore never starves the sleep actions.
    async fn apply_screen(&mut self, decision: &Decision) {
        // [ACT-13]: where no agent offers a mechanism, `screen_off` is UNAVAILABLE as an
        // action. The daemon does not propose it, and it raises no doubt on anything --
        // an action reported as unavailable is knowledge, not doubt.
        if !self.screen_off_available {
            return;
        }

        let due = decision.screen_off.due;
        if due == self.agents.blanked() {
            return;
        }

        if self.dry_run {
            info!(blank = due, "dry run: would change the screen, and did not");
            return;
        }

        let (done, failed) = crate::actions::set_blank(&self.bus, &self.agents, due).await;
        if !failed.is_empty() {
            // A panel that stayed lit because a compositor refused is exactly the failure
            // [OBS-6] is about: it is invisible from inside the daemon and expensive on an
            // OLED. It is never reported as a success.
            warn!(
                blank = due,
                failed = failed.join("; "),
                "a session refused to change its screen"
            );
        }
        if !done.is_empty() {
            info!(
                blank = due,
                sessions = done.join(", "),
                reason = crate::report::why_due(decision, Action::ScreenOff, &self.policy),
                "screen state changed"
            );
            self.agents.set_blanked(due);
        }
    }

    /// Records a forced action for [REQ-12] and for `doctor`.
    pub fn record_forced(&mut self, action: Action, why: String, uid: u32) {
        self.forced.push(Forced {
            at: clock::now(),
            action,
            why,
            uid,
        });
        // Bounded: this list is reported in full by `doctor` and lives for the whole
        // boot. A caller stuck in a loop would otherwise grow it without limit inside the
        // daemon that has to keep deciding.
        if self.forced.len() > 64 {
            self.forced.remove(0);
        }
    }

    /// Whether anything that can only be polled is currently holding the machine, and the
    /// backstop sweep therefore has work to do.
    ///
    /// Three cases, and each is a way for a polled fact to matter:
    ///
    /// * it is TRUE, so it is holding something and the daemon must notice it stopping;
    /// * it is INDETERMINATE, which holds every sleep action and must not be permanent
    ///   without anybody looking again;
    /// * a `[when]` ceiling conditions on it, so it becoming true would *cause* an action
    ///   and no event would announce it.
    ///
    /// When none of the three holds, no timer is armed beyond the real deadline.
    #[must_use]
    pub fn sweep_needed(&self) -> bool {
        let holding = facts::POLLED.iter().any(|id| {
            self.policy.fact_enabled(*id)
                && facts::sessions::is_holding(
                    self.readings
                        .get(id)
                        .map_or(FactState::Unavailable, |r| r.state),
                )
        });
        if holding {
            return true;
        }
        self.policy
            .blocks_of(idlectl_policy::BlockKind::When)
            .any(|block| match block.id.condition {
                idlectl_policy::Condition::Fact(id) => facts::POLLED.contains(&id),
                // `Always` cannot change, and a condition this build does not know about
                // is treated as unwatchable: arming the sweep is the direction that keeps
                // working rather than the one that silently stops noticing.
                idlectl_policy::Condition::Always => false,
                _ => true,
            })
    }

    /// How long to sleep before the next evaluation, or [`None`] to wait for an event only.
    ///
    /// [COMP-6]: the timer is armed at the earliest pending deadline. The sweep, when it is
    /// needed at all, can only make that earlier — never later.
    #[must_use]
    pub fn next_wakeup(&self) -> Option<Duration> {
        let now = clock::now();
        let deadline = self
            .decision
            .as_ref()
            .map_or(Deadline::Never, Decision::next_deadline);

        let until_deadline = match deadline {
            Deadline::Never => None,
            // Already passed: ask for an immediate re-evaluation rather than zero, so a
            // deadline that cannot be acted on yet cannot spin the loop.
            Deadline::At(at) => Some(at.since(now).max(Duration::from_millis(250))),
        };

        // A lease's TTL is a deadline the policy engine cannot see: a held lease
        // contributes `never`, which arms no timer at all. Without this the TTL -- the
        // bound that exists for a holder that survives but forgets -- would only ever be
        // checked when something unrelated happened to wake the loop.
        let until_deadline = match (until_deadline, self.leases.next_expiry()) {
            (Some(d), Some(expiry)) => {
                Some(d.min(expiry.since(now).max(Duration::from_millis(250))))
            }
            (None, Some(expiry)) => Some(expiry.since(now).max(Duration::from_millis(250))),
            (d, None) => d,
        };

        // A held request's TTL is a deadline of the same kind, and invisible for the same
        // reason: while something contributes `never` the resolved deadline is `never` and
        // nothing is armed, so the request would expire only when an unrelated event
        // happened to wake the loop.
        //
        // Only the expiry needs arming here. Retrying is already covered: whatever holds
        // the request is a fact, and a fact is either polled -- in which case
        // `sweep_needed` arms the sweep -- or event-driven, in which case the event pokes
        // the loop when it changes. What no fact reports is the passage of the TTL itself.
        let until_deadline = match (until_deadline, self.pending.map(|p| p.expires_at)) {
            (Some(d), Some(expiry)) => {
                Some(d.min(expiry.since(now).max(Duration::from_millis(250))))
            }
            (None, Some(expiry)) => Some(expiry.since(now).max(Duration::from_millis(250))),
            (d, None) => d,
        };

        match (until_deadline, self.sweep_needed()) {
            (Some(d), true) => Some(d.min(SWEEP_INTERVAL)),
            (Some(d), false) => Some(d),
            (None, true) => Some(SWEEP_INTERVAL),
            (None, false) => None,
        }
    }
}

/// Whether an action was autonomous, requested or forced, for the [OBS-5] record.
fn kind_of(request: &Request, action: Action) -> &'static str {
    if request.force == Some(action) {
        "forced"
    } else if request.now {
        "requested"
    } else {
        "autonomous"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The sweep predicate is what keeps an idle machine from waking every minute. If it
    /// ever returns true unconditionally, nothing else in the daemon notices — the
    /// behaviour is still correct, just permanently more expensive, which is precisely the
    /// kind of regression that ships.
    #[test]
    fn an_idle_machine_with_nothing_polled_needs_no_sweep() {
        let policy = Policy::empty();
        let mut readings = BTreeMap::new();
        for id in facts::POLLED {
            readings.insert(id, Reading::no("nothing"));
        }
        assert!(!sweep_needed_for(&policy, &readings));
    }

    #[test]
    fn a_download_in_progress_arms_the_sweep() {
        let policy = Policy::empty();
        let mut readings = BTreeMap::new();
        readings.insert(FactId::SteamDownloading, Reading::yes("downloading"));
        assert!(sweep_needed_for(&policy, &readings));
    }

    /// Doubt about a polled fact holds every sleep action. Nothing would ever look again
    /// without the sweep, so the machine would stay awake until something unrelated
    /// happened to poke the loop.
    #[test]
    fn doubt_about_a_polled_fact_arms_the_sweep() {
        let policy = Policy::empty();
        let mut readings = BTreeMap::new();
        readings.insert(FactId::GpuBusyOther, Reading::doubt("nvidia-smi hung"));
        assert!(sweep_needed_for(&policy, &readings));
    }

    #[test]
    fn a_disabled_polled_fact_does_not_arm_the_sweep() {
        let mut policy = Policy::empty();
        if let Some(settings) = policy.facts.get_mut(&FactId::SteamDownloading) {
            settings.enabled = false;
        }
        let mut readings = BTreeMap::new();
        // Stale reading from before it was switched off. A disabled fact's detector does
        // not run, so its last value must not keep the machine awake either.
        readings.insert(FactId::SteamDownloading, Reading::yes("downloading"));
        assert!(!sweep_needed_for(&policy, &readings));
    }

    /// Mirrors [`Engine::sweep_needed`] without needing a bus connection to build an
    /// `Engine`. Kept in step by the fact that both read the same two inputs.
    fn sweep_needed_for(policy: &Policy, readings: &BTreeMap<FactId, Reading>) -> bool {
        let holding = facts::POLLED.iter().any(|id| {
            policy.fact_enabled(*id)
                && facts::sessions::is_holding(
                    readings.get(id).map_or(FactState::Unavailable, |r| r.state),
                )
        });
        holding
            || policy
                .blocks_of(idlectl_policy::BlockKind::When)
                .any(|block| match block.id.condition {
                    idlectl_policy::Condition::Fact(id) => facts::POLLED.contains(&id),
                    idlectl_policy::Condition::Always => false,
                    _ => true,
                })
    }
}
