//! Performing what was decided.
//!
//! Deciding is not doing ([ACT-1]), and this module is the "doing" half. It contains no
//! policy: every function here is called only after the engine has resolved an action, and
//! none of them may consult a fact, a block or an inhibitor. If a decision is ever taken
//! in this file, the daemon has two deciders again, which is the failure the whole project
//! exists to remove.

use idlectl_policy::Action;
use zbus::proxy;

use crate::agents::Registry;
use crate::logind::LogindManagerProxy;

/// The interface the session agent exports, on the **system** bus.
///
/// System, not session, and it matters. The agent owns no well-known name: it registers
/// with the daemon and the daemon calls back on its unique connection name, so nothing
/// here needs a per-user name-ownership rule and an ordinary user cannot squat the
/// interface by claiming a name. The bus policy restricts these methods to uid 0, so the
/// only caller that can reach them is the daemon.
#[proxy(
    interface = "io.github.ericcanas.Idlectl1.Agent",
    default_path = "/io/github/ericcanas/Idlectl1/Agent",
    gen_blocking = false
)]
pub trait Agent {
    /// Blank the outputs of this session. Idempotent.
    fn blank(&self) -> zbus::Result<()>;

    /// Unblank. Called when a new origin re-arms the schedule, and on the agent's own
    /// shutdown, so that stopping the agent can never leave a session dark with nothing
    /// left running that knows how to bring it back.
    fn unblank(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn blanked(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn can_blank(&self) -> zbus::Result<bool>;
}

/// Asks logind to perform a power transition.
///
/// `interactive` is always `false`: `true` asks polkit to prompt, and there is nobody to
/// prompt. A dialogue nobody sees is a transition that hangs rather than one that is
/// refused, and a hung transition is indistinguishable from a working daemon that decided
/// not to act.
pub async fn perform(bus: &zbus::Connection, action: Action) -> Result<(), String> {
    let manager = LogindManagerProxy::new(bus)
        .await
        .map_err(|err| format!("cannot reach logind: {err}"))?;

    let result = match action {
        Action::Suspend => manager.suspend(false).await,
        Action::Hibernate => manager.hibernate(false).await,
        Action::PowerOff => manager.power_off(false).await,
        // Not reachable through this function: `screen_off` is not a power transition and
        // logind has no mechanism for it. Returning an error rather than panicking keeps a
        // future refactor that routes it here from taking the daemon down.
        Action::ScreenOff => {
            return Err("screen_off is performed by the session agent, not by logind".to_owned());
        }
    };

    // [ACT-3]: a refusal is logged at warning level by the caller and MUST NOT be retried
    // immediately. The measurement behind that: an incident that looked like a machine
    // suspending in a user's face did not actually suspend -- the previous transition was
    // still finishing and the request was refused. One tick two seconds later and it
    // would have. An intermittent fault masked by luck is worse than a reproducible one.
    result.map_err(|err| err.to_string())
}

/// Blanks or unblanks every session that offers a mechanism.
///
/// Returns the sessions that were addressed and the failures, so the caller can log one
/// record naming both. A partial success is a real state — two monitors on two seats, one
/// compositor wedged — and reporting it as a plain success would hide a panel that stayed
/// lit all night.
pub async fn set_blank(
    bus: &zbus::Connection,
    registry: &Registry,
    blank: bool,
) -> (Vec<String>, Vec<String>) {
    let mut done = Vec::new();
    let mut failed = Vec::new();

    for (unique_name, agent) in registry.iter() {
        if !agent.can_blank {
            continue;
        }
        let proxy = match AgentProxy::builder(bus).destination(unique_name.clone()) {
            Ok(builder) => match builder.build().await {
                Ok(p) => p,
                Err(err) => {
                    failed.push(format!("{}: {err}", agent.session_id));
                    continue;
                }
            },
            Err(err) => {
                failed.push(format!("{}: {err}", agent.session_id));
                continue;
            }
        };
        let result = if blank {
            proxy.blank().await
        } else {
            proxy.unblank().await
        };
        match result {
            Ok(()) => done.push(agent.session_id.clone()),
            Err(err) => failed.push(format!("{}: {err}", agent.session_id)),
        }
    }

    (done, failed)
}

/// What the sessions say their screens are **actually** doing.
///
/// The daemon has always kept its own record of the screen state, written when it asked
/// for a change and never read back. That record is an intent, and an intent is not an
/// observation: every reason a blank might not take effect -- a compositor that ignores
/// the request, another program turning the panel back on, a compositor restart that
/// leaves the protocol object dead, an output replaced by a hotplug -- ends in a lit panel
/// that nothing reports. On an OLED that is the failure the action exists to prevent.
///
/// This reads the agent's `Blanked` property, which since agent 0.4.3 is written by the
/// display server rather than by the agent's own request ([ACT-15]). It only *reports*:
/// nothing here re-issues anything. See [OBS-8] for why observing came before correcting.
#[derive(Debug, Default)]
pub struct ScreenReport {
    /// Sessions whose display server says the outputs are dark.
    pub blanked: Vec<String>,
    /// Sessions whose display server says the outputs are lit.
    pub lit: Vec<String>,
    /// Sessions that offer a mechanism but could not be asked. Not counted as either
    /// state: an agent that cannot be reached has observed nothing, and treating silence
    /// as "lit" would raise a false divergence every time one restarts.
    pub unreachable: Vec<String>,
}

impl ScreenReport {
    /// The sessions that disagree with `want`, which is what the daemon last asked for.
    pub fn disagreeing(&self, want: bool) -> &[String] {
        if want { &self.lit } else { &self.blanked }
    }
}

pub async fn observe_blank(bus: &zbus::Connection, registry: &Registry) -> ScreenReport {
    let mut report = ScreenReport::default();

    for (unique_name, agent) in registry.iter() {
        if !agent.can_blank {
            continue;
        }
        let proxy = match AgentProxy::builder(bus).destination(unique_name.clone()) {
            Ok(builder) => match builder.build().await {
                Ok(p) => p,
                Err(err) => {
                    report
                        .unreachable
                        .push(format!("{}: {err}", agent.session_id));
                    continue;
                }
            },
            Err(err) => {
                report
                    .unreachable
                    .push(format!("{}: {err}", agent.session_id));
                continue;
            }
        };
        match proxy.blanked().await {
            Ok(true) => report.blanked.push(agent.session_id.clone()),
            Ok(false) => report.lit.push(agent.session_id.clone()),
            Err(err) => report
                .unreachable
                .push(format!("{}: {err}", agent.session_id)),
        }
    }

    report
}
