//! The D-Bus surface: `io.github.ericcanas.Idlectl1.Manager`.
//!
//! This is the whole external contract. `idlectl` is a client of it and has no privileged
//! path of its own, which is deliberate — anything the command-line tool can do, a script,
//! a desktop applet or a remote relay can do too, through the same authorization checks.
//!
//! # No double hyphen in the `///` comments of the interface below
//!
//! zbus copies those **verbatim** into the introspection XML it serves at runtime, and XML
//! forbids a double hyphen inside a comment, with no escape for it. One of them therefore
//! produces a document that every client introspecting before it calls — dbus-python,
//! gdbus, anything generating bindings — cannot parse. Measured: one `RestPending` comment
//! had a dash pair before `[REQ-6]`, and dbus-python answered `not well-formed (invalid
//! token): line 58, column 23` for the whole object.
//!
//! What let it survive is that the call still worked: the parse error goes to the client's
//! stderr, and nothing in this daemon ever mentions it. `scripts/preflight-publish.sh` now
//! refuses to publish one, here or in any other file that declares an interface.
//!
//! # Time on the wire
//!
//! Microseconds on `CLOCK_BOOTTIME`, matching logind's own convention, with `UINT64_MAX`
//! meaning "never". `0` is a real instant (boot), so it cannot be the sentinel: a client
//! that forgets to special-case `UINT64_MAX` gets a deadline absurdly far in the future —
//! a machine that stays awake, which is the cheap error — instead of one already in the past.

use std::sync::Arc;

use async_lock::Mutex;
use idlectl_policy::{Action, FactId, FactState, Request};
use tracing::{info, warn};
use zbus::object_server::SignalEmitter;
use zbus::zvariant::OwnedFd;

use crate::agents::{Agent, HEARTBEAT_INTERVAL};
use crate::engine::Engine;
use crate::facts::gpu::Holder;
use crate::polkit;

pub const BUS_NAME: &str = "io.github.ericcanas.Idlectl1";
pub const OBJECT_PATH: &str = "/io/github/ericcanas/Idlectl1";

/// How many GPU memory holders one agent may report, and how long each name may be.
///
/// [ACT-10]: state written by an unprivileged caller is parsed explicitly and bounded.
/// Nothing an agent reports here can make the machine sleep (a holder only ever adds a
/// reason to stay awake), but an agent reporting a million of them, or one whose "process
/// name" is a megabyte of text, would grow this daemon's memory and its log lines without
/// limit. Both bounds are far above any real machine: a session has a handful of processes
/// holding GPU memory, and a process name is a filename.
const MAX_HOLDERS: usize = 256;
const MAX_HOLDER_NAME: usize = 64;

pub struct Manager {
    pub engine: Arc<Mutex<Engine>>,
    pub bus: zbus::Connection,
}

#[zbus::interface(name = "io.github.ericcanas.Idlectl1.Manager")]
impl Manager {
    // ------------------------------------------------------------------ properties

    #[zbus(property)]
    async fn version(&self) -> String {
        env!("CARGO_PKG_VERSION").to_owned()
    }

    #[zbus(property)]
    async fn config_layers(&self) -> Vec<String> {
        self.engine.lock().await.layers.clone()
    }

    /// Every fact and its state, as `(name, state)`.
    ///
    /// The four state strings are not interchangeable and a client must not collapse them.
    /// `unavailable` means this machine has no such capability and behaves as false;
    /// `indeterminate` means the detector exists and could not answer, which vetoes
    /// suspend, hibernate and poweroff whether or not any block names that fact.
    ///
    /// This property is the flag surface an external caller reads. A relay that woke this
    /// machine and wants to know whether it may put it back to sleep should generally call
    /// `Rest()` and let the daemon decide — but when it needs the raw signals,
    /// `steam_downloading`, `local_service_busy` and the rest are here by name.
    #[zbus(property)]
    async fn facts(&self) -> Vec<(String, String)> {
        let engine = self.engine.lock().await;
        FactId::ALL
            .into_iter()
            .map(|id| {
                let state = engine
                    .readings
                    .get(&id)
                    .map_or(FactState::Unavailable, |r| r.state);
                (id.name().to_owned(), state.to_string())
            })
            .collect()
    }

    /// Per-action resolution, as `(action, deadline_usec, due)`.
    ///
    /// More than one sleep action may be due at once. A client MUST NOT read that as "all
    /// of these will happen": the daemon performs `screen_off` if it is due, plus the
    /// **shallowest** due sleep action and never a deeper one.
    #[zbus(property)]
    async fn deadlines(&self) -> Vec<(String, u64, bool)> {
        let engine = self.engine.lock().await;
        Action::ALL
            .into_iter()
            .map(|action| {
                let resolution = engine.decision.as_ref().map(|d| d.get(action));
                (
                    action.name().to_owned(),
                    resolution.map_or(u64::MAX, |r| crate::report::deadline_usec(r.deadline)),
                    resolution.is_some_and(|r| r.due),
                )
            })
            .collect()
    }

    #[zbus(property)]
    async fn next_deadline_usec(&self) -> u64 {
        let engine = self.engine.lock().await;
        engine.decision.as_ref().map_or(u64::MAX, |d| {
            crate::report::deadline_usec(d.next_deadline())
        })
    }

    #[zbus(property)]
    async fn dry_run(&self) -> bool {
        self.engine.lock().await.dry_run
    }

    /// The request being held, if there is one: `(action, expires_usec, uid)`.
    ///
    /// A list of zero or one rather than a tuple with a sentinel, because "no request" and
    /// "a request for the action whose name happens to be empty" must not be the same wire
    /// value. There is at most one held request by design ([REQ-6]).
    ///
    /// It exists so `status` can show it. A held request and a lease are the only two
    /// things that keep a machine awake **without appearing anywhere in the
    /// configuration**, so leaving them out of the first screen a person looks at is how
    /// "why will this not sleep?" turns into a long afternoon.
    #[zbus(property)]
    async fn pending(&self) -> Vec<(String, u64, u32)> {
        let engine = self.engine.lock().await;
        engine
            .pending
            .map(|p| vec![(p.action.name().to_owned(), p.expires_at.as_micros(), p.uid)])
            .unwrap_or_default()
    }

    // --------------------------------------------------------------------- methods

    /// Human-readable explanation. Deliberately not polkit-checked: a diagnostic that
    /// needs authentication is a diagnostic nobody runs.
    async fn explain(&self, action: &str) -> zbus::fdo::Result<String> {
        let only = parse_optional_action(action)?;
        let engine = self.engine.lock().await;
        Ok(crate::report::explain(&engine, only))
    }

    /// Which detectors work here and which capabilities are absent.
    ///
    /// Returns the text and whether everything is in order, so the client can set its exit
    /// status without parsing prose. [OBS-4] fixes the rule: non-zero if **any**
    /// configuration fault, **any** indeterminate fact, or **any** standing hazard is
    /// present — three conditions, not one.
    async fn doctor(&self) -> zbus::fdo::Result<(String, bool)> {
        let engine = self.engine.lock().await;
        Ok(crate::report::doctor(&engine).await)
    }

    /// The same information as `Explain`, as a JSON document. See `schema` for the
    /// versioning contract.
    async fn report(&self) -> zbus::fdo::Result<String> {
        let engine = self.engine.lock().await;
        Ok(crate::report::report_json(&engine).to_string())
    }

    /// Ask the machine to rest now.
    ///
    /// Treats exactly two blocks as satisfied — the one whose condition is `always` and
    /// the one whose condition is `after_resume` — as if their timeouts had elapsed,
    /// including where their configured value is `never`. The base schedule and the
    /// post-resume settle window, and nothing else. The set is fixed by condition name and
    /// no configuration key can enlarge it.
    ///
    /// **It does not collapse condition blocks.** A running game, an active download, a
    /// held lease and an open remote session are all still evaluated normally, each on its
    /// own clock, and any of them can still refuse. Neither does it satisfy the vetoes that
    /// are not blocks: an indeterminate fact and a rejected configuration file both still
    /// hold. This is what a remote relay calls when it has finished with a machine; it must
    /// never be able to suspend one that is in the middle of something.
    ///
    /// `false` is a normal answer, not an error: something else is holding the machine
    /// awake, and `Explain()` will say what.
    async fn rest(
        &self,
        action: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<bool> {
        let sender = sender_of(&header)?;
        polkit::check(&self.bus, &sender, polkit::ACTION_REST).await?;

        // [REQ-17]: no action named means suspend, always. A fixed constant in v1 -- with
        // poweroff requiring the word to be typed, no request can close somebody's session
        // unless its sender said so.
        let action = parse_action(if action.is_empty() { "suspend" } else { action })?;

        let mut engine = self.engine.lock().await;
        let performed = engine
            .tick(&Request {
                now: true,
                force: None,
                // [ACT-7b]: the caller named an action, so that is the one evaluated. It
                // is never traded for a shallower one that happens to be due at the same
                // instant, which `--now` guarantees there will be.
                requested: Some(action),
            })
            .await;
        let accepted = performed == Some(action);
        if !accepted {
            info!(
                requested = action.name(),
                performed = performed.map(Action::name),
                "rest request not satisfied"
            );
        }
        Ok(accepted)
    }

    /// Ask the machine to rest, and keep the request for up to `ttl_usec` if it cannot be
    /// carried out yet: [REQ-6].
    ///
    /// Returns `true` only if the action was performed immediately. `false` means it is
    /// being held, not that it was refused: the machine will carry it out the moment the
    /// last veto clears, and give up at the TTL.
    ///
    /// A held request is not a weaker one. It is re-evaluated on the ordinary schedule with
    /// exactly the floors the original had, so a game that starts after the request was
    /// made refuses it just as surely as one already running would have. What is remembered
    /// is the asking, never the answer.
    ///
    /// It is dropped early if somebody uses the machine ([REQ-7]); waking from sleep is not
    /// somebody using the machine ([REQ-8]).
    ///
    /// This is what a relay wants. Without it the caller has to poll, which means owning a
    /// timer, a retry budget and a copy of "is it still worth asking": three things this
    /// daemon already has.
    async fn rest_pending(
        &self,
        action: &str,
        ttl_usec: u64,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<bool> {
        let sender = sender_of(&header)?;
        polkit::check(&self.bus, &sender, polkit::ACTION_REST).await?;
        let uid = polkit::caller_uid(&self.bus, &sender).await.unwrap_or(0);

        let action = parse_action(if action.is_empty() { "suspend" } else { action })?;
        if ttl_usec == 0 {
            return Err(zbus::fdo::Error::InvalidArgs(
                "a pending request needs a time to live; use Rest() to ask once".into(),
            ));
        }

        let mut engine = self.engine.lock().await;
        let performed = engine
            .tick(&Request {
                now: true,
                force: None,
                requested: Some(action),
            })
            .await;
        if performed == Some(action) {
            return Ok(true);
        }

        // Recorded against the clocks the evaluation just used, not a fresh reading: [REQ-7]
        // asks whether input arrived *after the request was made*, and the instant the
        // decision was computed for is when that was.
        let now = engine.clocks.now;
        let held = crate::pending::Pending {
            action,
            expires_at: now
                .checked_add(std::time::Duration::from_micros(ttl_usec))
                .unwrap_or(now),
            human_origin: engine.clocks.human_input,
            uid,
        };
        info!(
            action = action.name(),
            uid,
            ttl_s = ttl_usec / 1_000_000,
            "rest request held: it will be carried out when every veto clears"
        );
        engine.pending = Some(held);
        Ok(false)
    }

    /// Forget a held request. Idempotent; `false` means there was nothing to forget.
    async fn cancel_pending(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<bool> {
        let sender = sender_of(&header)?;
        polkit::check(&self.bus, &sender, polkit::ACTION_REST).await?;

        let mut engine = self.engine.lock().await;
        let had = engine.pending.take();
        if let Some(held) = had {
            info!(
                action = held.action.name(),
                "pending rest request cancelled by request"
            );
        }
        Ok(had.is_some())
    }

    /// Install a ceiling due immediately for this one action and this one request, and
    /// perform it.
    ///
    /// Ceilings are absolute, so this defeats every floor there is, mechanically and with
    /// no special cases: every `never`, the human-presence floor, the veto raised by an
    /// indeterminate fact, and the veto raised by a configuration file that had to be
    /// rejected. The last two are what it is usually reached for — a machine wedged awake
    /// by something broken rather than by something happening, which `Rest()` by
    /// construction cannot help with.
    ///
    /// It overrides POLICY, never MECHANISM. One transition at a time still holds, a
    /// refusal from logind is still logged rather than retried blindly, and the facts are
    /// still re-read and the policy recomputed immediately before acting.
    ///
    /// Deliberately a separate method rather than a flag on `Rest()`: the caller has to
    /// have chosen it by name, the polkit action is separate, and `why` is mandatory and
    /// recorded.
    async fn rest_forced(
        &self,
        action: &str,
        why: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = sender_of(&header)?;
        polkit::check(&self.bus, &sender, polkit::ACTION_REST_FORCED).await?;

        if why.trim().is_empty() {
            return Err(zbus::fdo::Error::InvalidArgs(
                "a reason is required: a forced action is recorded in the journal with it".into(),
            ));
        }
        let action = parse_action(if action.is_empty() { "suspend" } else { action })?;
        let uid = polkit::caller_uid(&self.bus, &sender)
            .await
            .unwrap_or(u32::MAX);

        let mut engine = self.engine.lock().await;
        engine.record_forced(action, why.to_owned(), uid);
        warn!(
            action = action.name(),
            uid, why, "forced action requested: every floor for this action is being defeated"
        );
        engine
            .tick(&Request {
                now: false,
                force: Some(action),
                // Same reason as in `rest`, and it matters more here: the ceiling makes the
                // named action due, but on an already idle machine a shallower one is due
                // too, and a forced poweroff that quietly suspended would be the worst
                // possible answer to a deliberate, authenticated, logged request.
                requested: Some(action),
            })
            .await;
        Ok(())
    }

    /// Take a lease. The machine will not sleep while it is held.
    ///
    /// Returns a file descriptor, the way logind's `Inhibit()` does and for the same
    /// reason: the lease is released when the descriptor is closed, so a job that crashes
    /// cannot pin a machine awake forever. `ttl_usec` is a second bound on top of that, for
    /// the case where the holder survives but forgets.
    async fn acquire_lease(
        &self,
        who: &str,
        why: &str,
        ttl_usec: u64,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<OwnedFd> {
        let sender = sender_of(&header)?;
        polkit::check(&self.bus, &sender, polkit::ACTION_LEASE).await?;
        let uid = polkit::caller_uid(&self.bus, &sender)
            .await
            .unwrap_or(u32::MAX);
        // Asked for while the caller is still on the bus. Diagnosis only: see
        // `polkit::caller_pid` for why this is not, and must not become, an identity.
        let pid = polkit::caller_pid(&self.bus, &sender).await;

        let ttl = std::time::Duration::from_micros(ttl_usec);
        let now = crate::clock::now();

        let mut engine = self.engine.lock().await;
        let (fd, watch) = engine
            .leases
            .acquire(who, why, uid, pid, ttl, now)
            .map_err(|refusal| zbus::fdo::Error::InvalidArgs(refusal.to_string()))?;
        info!(
            lease = who,
            uid,
            // Not `pid`: the journal already attaches `_PID`, which is this daemon's, and
            // two fields called the same thing meaning different processes is how a log
            // stops being evidence.
            holder_pid = pid.unwrap_or(0),
            why,
            ttl_s = ttl.as_secs(),
            "lease acquired"
        );

        // The loop has to be told when the holder goes away, and nothing else will tell
        // it. A held lease contributes `never`, so no timer is armed; a released lease
        // that nobody notices therefore holds the machine awake for the rest of the boot.
        // Measured: the holder exited, `idlectl lease list` kept showing the lease, and
        // nothing was ever going to change its mind.
        let wake = engine.wake.clone();
        let name = who.to_owned();
        if let Err(err) = std::thread::Builder::new()
            .name(format!("idlectl-lease-{name}"))
            .spawn(move || {
                crate::leases::Table::watch_until_released(watch);
                let _ = wake.try_send("a lease holder closed its handle");
            })
        {
            // Refuse rather than hand out a lease nothing is watching. A lease that can
            // only be released by its TTL is not the lease that was asked for, and
            // silently downgrading it is how a machine ends up awake until morning.
            engine.leases.release(who);
            return Err(zbus::fdo::Error::Failed(format!(
                "cannot start the watcher thread for this lease: {err}"
            )));
        }

        let _ = engine.wake.try_send("lease acquired");
        Ok(OwnedFd::from(fd))
    }

    /// Release a lease early. Not polkit-checked beyond the lease action itself: releasing
    /// only ever permits the machine to sleep, which the schedule was going to do anyway.
    async fn release_lease(
        &self,
        who: &str,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<bool> {
        let sender = sender_of(&header)?;
        polkit::check(&self.bus, &sender, polkit::ACTION_LEASE).await?;
        let mut engine = self.engine.lock().await;
        let released = engine.leases.release(who);
        if released {
            info!(lease = who, "lease released by request");
            let _ = engine.wake.try_send("lease released");
        }
        Ok(released)
    }

    /// `(who, why, acquired_usec, expires_usec, uid, holder_pid, holder_state, holder_comm)`.
    /// Not polkit-checked: seeing what is holding a machine awake is diagnosis.
    ///
    /// `holder_pid` is 0 when unknown, and `holder_state` is `alive`, `gone`, `recycled` or
    /// `unknown` — resolved here, on every call, because the daemon is the only side holding
    /// the start time the answer depends on ([FACT-13b]). A client MUST NOT treat the pid as
    /// meaningful without the state beside it.
    async fn list_leases(&self) -> Vec<(String, String, u64, u64, u32, u32, String, String)> {
        let engine = self.engine.lock().await;
        engine
            .leases
            .iter()
            .map(|l| {
                let state = l.holder.state();
                (
                    l.who.clone(),
                    l.why.clone(),
                    l.acquired.as_micros(),
                    l.expires.as_micros(),
                    l.uid,
                    l.holder.pid().unwrap_or(0),
                    state.wire().to_owned(),
                    state.comm().to_owned(),
                )
            })
            .collect()
    }

    /// Re-read the configuration. Returns the layers applied.
    async fn reload(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
        #[zbus(signal_emitter)] emitter: SignalEmitter<'_>,
    ) -> zbus::fdo::Result<Vec<String>> {
        let sender = sender_of(&header)?;
        polkit::check(&self.bus, &sender, polkit::ACTION_RELOAD).await?;

        let mut engine = self.engine.lock().await;
        let layers = engine
            .reload()
            .map_err(|err| zbus::fdo::Error::Failed(format!("{err:#}")))?;
        info!(layers = layers.len(), "configuration reloaded");
        Manager::policy_reloaded(&emitter, layers.clone()).await?;
        let _ = engine.wake.try_send("configuration reloaded");
        Ok(layers)
    }

    // ------------------------------------------------------- the session agent's side

    /// Registers the calling connection as the agent for one session.
    ///
    /// Returns the interval, in microseconds, at which the agent must call `ReportIdle`.
    /// Returned rather than compiled into the agent so the two cannot be configured
    /// separately and drift: a daemon expecting a report every 30 s and an agent sending
    /// one every 120 s produces a permanently-indeterminate `human_active` and a machine
    /// that never sleeps, with nothing obviously wrong on either side.
    async fn register_agent(
        &self,
        session_id: &str,
        can_blank: bool,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<u64> {
        let sender = sender_of(&header)?;
        let uid = polkit::caller_uid(&self.bus, &sender)
            .await
            .ok_or_else(|| zbus::fdo::Error::AccessDenied("cannot identify the caller".into()))?;

        // No polkit check: registering only ever *adds* a reason the machine might stay
        // awake, and refusing to register is what a hostile caller would want. What an
        // agent claims is bounded instead -- it can report idleness for its own session
        // and can be told to blank its own screen, and nothing else.
        let mut engine = self.engine.lock().await;
        engine.agents.register(
            sender.clone(),
            Agent {
                session_id: session_id.to_owned(),
                uid,
                can_blank,
                // Not `Some(ZERO)`: the agent has not reported yet, and "idle for zero
                // seconds" would claim a human was observed. Doubt until it speaks.
                idle: None,
                // Empty, not "false": the same rule, for the same reason. Nothing has been
                // observed about this session's media yet.
                media: String::new(),
                // Empty is correct here rather than a claim: an empty holder list adds no
                // reason to stay awake and raises no veto, so an agent that has not spoken
                // yet contributes nothing.
                gpu_holders: Vec::new(),
                last_report: crate::clock::now(),
            },
        );
        info!(
            agent = sender,
            session = session_id,
            uid,
            can_blank,
            "session agent registered"
        );
        let _ = engine.wake.try_send("agent registered");
        Ok(u64::try_from(HEARTBEAT_INTERVAL.as_micros()).unwrap_or(u64::MAX))
    }

    /// Reports what the calling agent can see in its own session.
    ///
    /// `UINT64_MAX` means "I am alive and my idle protocol is not answering", which is
    /// deliberately distinct from a very large idle time: a large idle time permits sleep,
    /// and an agent whose protocol broke has observed nothing at all.
    ///
    /// `media_playing` is one of `true`, `false`, `indeterminate` or `unavailable` — the
    /// same four-name vocabulary the `Facts` property publishes. A root daemon cannot read
    /// MPRIS for itself: a session bus authenticates by uid and closes a root connection,
    /// measured, so this fact can only come from inside the session.
    ///
    /// `gpu_holders` is `(pid, process name, bytes)` for every process of the agent's own
    /// uid that holds DRM memory, already de-duplicated per DRM client id. It is reported
    /// for a permission reason rather than a bus one: `/proc/<pid>/fdinfo` is mode 0555 and
    /// still gated by `ptrace_may_access`, so reading another user's needs `CAP_SYS_PTRACE`
    /// (measured), which is the ability to read any process's memory and not something a
    /// daemon that can power the machine off is given in order to attribute video RAM. The
    /// list is **raw**: the threshold, the desktop exclusion and the attribution to a game
    /// all happen in this daemon, in [`crate::facts::gpu`].
    ///
    /// This doubles as the liveness heartbeat of [CLK-5]. It must be called on every
    /// idle/active edge **and** at the interval `RegisterAgent` returned, because the
    /// signal has to disappear on failure: while somebody is holding a controller an idle
    /// notifier emits nothing, so silence must read as doubt rather than as idleness.
    async fn report_session(
        &self,
        idle_usec: u64,
        media_playing: &str,
        gpu_holders: Vec<(u32, String, u64)>,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = sender_of(&header)?;
        let holders = bounded_holders(gpu_holders);
        let mut engine = self.engine.lock().await;
        if !engine.agents.report(
            &sender,
            idle_usec,
            media_playing,
            holders,
            crate::clock::now(),
        ) {
            return Err(zbus::fdo::Error::AccessDenied(
                "this connection has not registered as an agent".into(),
            ));
        }
        let _ = engine.wake.try_send("session report");
        Ok(())
    }

    /// Unregisters the calling agent. Called on clean shutdown so the daemon does not have
    /// to wait out a heartbeat timeout to notice.
    async fn unregister_agent(
        &self,
        #[zbus(header)] header: zbus::message::Header<'_>,
    ) -> zbus::fdo::Result<()> {
        let sender = sender_of(&header)?;
        let mut engine = self.engine.lock().await;
        if engine.agents.remove(&sender) {
            info!(agent = sender, "session agent unregistered");
            let _ = engine.wake.try_send("agent gone");
        }
        Ok(())
    }

    // --------------------------------------------------------------------- signals

    /// An action was performed. `reason` names what made it due.
    #[zbus(signal)]
    pub async fn action_taken(
        emitter: &SignalEmitter<'_>,
        action: String,
        reason: String,
    ) -> zbus::Result<()>;

    /// An action came due and was NOT performed, with the reasons that held it back.
    ///
    /// Emitted because "nothing happened" is otherwise invisible, and "why did my machine
    /// not sleep last night?" needs an answer that is not a guess.
    #[zbus(signal)]
    pub async fn action_held(
        emitter: &SignalEmitter<'_>,
        action: String,
        blocks: Vec<String>,
    ) -> zbus::Result<()>;

    /// One or more facts changed state.
    ///
    /// Named `fact_states_changed` in Rust and `FactsChanged` on the wire: the `Facts`
    /// property already generates a `facts_changed` helper for the standard
    /// `PropertiesChanged` signal, and the two would collide. The wire name is the
    /// contract and does not move.
    #[zbus(signal, name = "FactsChanged")]
    pub async fn fact_states_changed(
        emitter: &SignalEmitter<'_>,
        facts: Vec<(String, String)>,
    ) -> zbus::Result<()>;

    #[zbus(signal)]
    pub async fn policy_reloaded(
        emitter: &SignalEmitter<'_>,
        layers: Vec<String>,
    ) -> zbus::Result<()>;
}

/// Turns one agent's raw report into holders, bounded per [ACT-10].
///
/// Truncation rather than refusal: a report that is too long is still mostly evidence, and
/// dropping the whole thing would blind the GPU facts because of one bad entry. The bounds
/// are far enough above a real machine that a truncated report means a broken or hostile
/// agent, not a busy one.
fn bounded_holders(raw: Vec<(u32, String, u64)>) -> Vec<Holder> {
    raw.into_iter()
        .take(MAX_HOLDERS)
        .map(|(pid, name, bytes)| Holder {
            pid,
            // By characters, not bytes: this string came from another process's command
            // line, and slicing UTF-8 on a byte index that is not a boundary panics --
            // inside the daemon that owns the machine's power decision.
            name: name.chars().take(MAX_HOLDER_NAME).collect(),
            bytes,
        })
        .collect()
}

fn sender_of(header: &zbus::message::Header<'_>) -> zbus::fdo::Result<String> {
    header
        .sender()
        .map(ToString::to_string)
        .ok_or_else(|| zbus::fdo::Error::AccessDenied("the message carries no sender".into()))
}

fn parse_action(name: &str) -> zbus::fdo::Result<Action> {
    Action::from_name(name).ok_or_else(|| {
        zbus::fdo::Error::InvalidArgs(format!(
            "unknown action {name:?}: expected screen_off, suspend, hibernate or poweroff"
        ))
    })
}

fn parse_optional_action(name: &str) -> zbus::fdo::Result<Option<Action>> {
    if name.is_empty() {
        return Ok(None);
    }
    parse_action(name).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// [ACT-10]: what an unprivileged agent sends is bounded before it is stored. A
    /// hostile agent cannot make the machine sleep with this, but it could otherwise make
    /// the daemon hold an unbounded list and write unbounded log lines.
    #[test]
    fn an_agents_report_is_bounded_in_both_directions() {
        let raw: Vec<(u32, String, u64)> = (0..MAX_HOLDERS as u32 + 10)
            .map(|pid| (pid, "x".repeat(MAX_HOLDER_NAME + 10), 1))
            .collect();
        let bounded = bounded_holders(raw);
        assert_eq!(bounded.len(), MAX_HOLDERS);
        assert_eq!(bounded[0].name.chars().count(), MAX_HOLDER_NAME);
    }

    /// The name is truncated by characters. Slicing a multi-byte string on a byte index
    /// that is not a character boundary panics, and this string arrives from another
    /// process's command line: a filename with an accent in it is enough.
    #[test]
    fn truncating_a_name_cannot_panic_on_a_multibyte_character() {
        // Written as an escape rather than as the literal character so this file stays
        // pure ASCII: the repository's publication audit treats an accented Latin letter
        // in a source file as a possible fragment of the non-English notes this project
        // was extracted from, and fails on it.
        let name = "\u{e9}".repeat(MAX_HOLDER_NAME + 10);
        let bounded = bounded_holders(vec![(1, name, 1)]);
        assert_eq!(bounded[0].name.chars().count(), MAX_HOLDER_NAME);
    }
}
