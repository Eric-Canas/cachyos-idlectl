//! The Wayland backend: `ext-idle-notify-v1` for input, `wlr-output-power-management-v1`
//! for blanking.
//!
//! # Why `ext-idle-notify-v1` and not something easier
//!
//! [CLK-4] requires an idle-notification protocol that reports **real input**, not a
//! heartbeat a compositor emits regardless. This one qualifies and the desktop
//! power-management D-Bus interfaces do not: measured on a real session, with a genuine
//! power inhibit held, three separate inhibition query interfaces reported `false` and two
//! returned empty lists. Anything built on them would have been dead letter.
//!
//! # The state machine, and why it needs no polling
//!
//! One notification is created with a short timeout `T`. The compositor then sends exactly
//! two kinds of event, both on transitions:
//!
//! * `idled` — no input for `T`. The last input was therefore `T` before this arrived.
//! * `resumed` — input happened. Idle is zero again.
//!
//! While somebody is continuously using the machine, **neither event fires**, and that is
//! the correct behaviour rather than a gap: not having been idled *is* the evidence that
//! input is recent. The idle time is computed from the transition instant, so it is exact
//! rather than sampled, and the agent wakes up only when the state actually changes.
//!
//! This is the construction [CLK-5] is about. A naive "touch a file on every input" agent
//! goes stale during exactly the activity it is supposed to detect, because the compositor
//! emits nothing while a controller is being held.
//!
//! # What transitions cannot tell you, and what to do about it
//!
//! Both events describe a *change*. Neither answers "how long has this seat been idle", and
//! the protocol has no request that does. An agent that has just started has therefore
//! observed nothing, and the compositor will not tell it what it missed: KWin answers
//! `org.freedesktop.ScreenSaver.GetSessionIdleTime` with `not supported on this platform`.
//!
//! So the state here is not "when did `idled` arrive" but "where was the last input", on
//! `CLOCK_BOOTTIME`, in one of three states -- and the instant is handed to the next
//! instance of this agent through a runtime file so that restarting it does not restart
//! every countdown measured from it. [`crate::lastinput`] is that mechanism and carries the
//! measurements that made it necessary.
//!
//! # Two blanking protocols, because one is not enough
//!
//! `wlr-output-power-management-v1` is the wlroots one: sway, Hyprland, wayfire, labwc.
//! `org_kde_kwin_dpms` is KWin's. **KWin implements the first not at all**, which was
//! measured rather than assumed — a Plasma session advertises `ext_idle_notifier_v1`
//! version 2 and `org_kde_kwin_dpms_manager`, and no `zwlr_output_power_manager_v1`.
//!
//! Supporting only the wlroots protocol would therefore have left `screen_off` permanently
//! unavailable on Plasma, which is the desktop this is most likely to be installed on. The
//! daemon would have reported it honestly — `screen_off UNAVAILABLE`, every block setting
//! it named as inert — and the OLED carve-out that half this design exists for would have
//! done nothing at all on the majority target.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use idlectl_policy::BootInstant;
use tracing::info;

use wayland_client::protocol::{wl_output, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols::ext::idle_notify::v1::client::{
    ext_idle_notification_v1::{self, ExtIdleNotificationV1},
    ext_idle_notifier_v1::ExtIdleNotifierV1,
};
use wayland_protocols_plasma::dpms::client::{
    org_kde_kwin_dpms::{self, OrgKdeKwinDpms},
    org_kde_kwin_dpms_manager::OrgKdeKwinDpmsManager,
};
use wayland_protocols_wlr::output_power_management::v1::client::{
    zwlr_output_power_manager_v1::ZwlrOutputPowerManagerV1,
    zwlr_output_power_v1::{self, ZwlrOutputPowerV1},
};

use crate::backend::{Backend, Idle};
use crate::clock;

/// The idle-notification timeout.
///
/// Short, because it only sets the granularity of the *transition*, never the policy: the
/// daemon owns `min_idle` and every timeout that matters. Ten seconds means the agent
/// learns a session went idle within ten seconds of it happening and then computes the
/// exact idle time from the transition instant.
pub(crate) const NOTIFY_TIMEOUT: Duration = Duration::from_secs(10);

/// What this agent knows about the last time somebody touched the seat.
///
/// Three states rather than an `Option<BootInstant>`, because "I have not seen a transition
/// yet" and "the seat was last touched at T" are different claims and only one of them is
/// an observation. Collapsing the first into "idle for zero seconds" is what made a package
/// upgrade hand the machine a fresh countdown; see [`crate::lastinput`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Tracked {
    /// No transition seen since this agent started, and nothing credible to carry over.
    /// Reported as [`Idle::Unknown`], which vetoes every sleep action until the compositor
    /// says something -- at most [`NOTIFY_TIMEOUT`] later.
    Unknown,
    /// Carried over from the previous instance and not yet confirmed by a transition.
    /// Believed, but discarded the moment the compositor contradicts it.
    Carried(BootInstant),
    /// The instant of the most recent real input, observed by this instance.
    Observed(BootInstant),
}

/// Where the last input was, given that `idled` has just arrived at `at`.
///
/// Split out from the event handler so the rule can be tested without a compositor, because
/// the interesting case is not the arithmetic but which of two answers wins.
fn idled(tracked: Tracked, at: BootInstant) -> Tracked {
    // `idled` fires exactly NOTIFY_TIMEOUT after the last input, so that is where the input
    // was -- an offset, not an estimate.
    let observed = at.saturating_sub(NOTIFY_TIMEOUT);
    match tracked {
        // Confirmation rather than news. The seat was already quiet when this agent started
        // and it is quiet now, so the carried instant is the older and better answer;
        // taking `observed` here would restart the countdown at the moment of the restart,
        // which is the entire failure this mechanism exists to prevent. The compositor
        // cannot contradict it: `idled` means there was no input in the window that ended
        // now, so the true last input is at or before `observed`.
        Tracked::Carried(carried) => Tracked::Observed(carried.min(observed)),
        // `idled` never arrives twice in a row -- the compositor sends `resumed` in
        // between -- so in these states `observed` is the newest thing known and correct.
        Tracked::Unknown | Tracked::Observed(_) => Tracked::Observed(observed),
    }
}

/// Shared between the Wayland thread and the async main loop.
#[derive(Debug)]
struct Shared {
    /// What is known about the last input, on `CLOCK_BOOTTIME`.
    tracked: Tracked,
    /// Set when the connection dies. Everything after that is unknown, not idle.
    lost: bool,
}

/// Which protocol the compositor gave us for turning outputs off.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BlankVia {
    None,
    /// `wlr-output-power-management-v1`: sway, Hyprland, wayfire, labwc.
    WlrOutputPower,
    /// `org_kde_kwin_dpms`: KWin.
    KdeDpms,
}

impl BlankVia {
    const fn name(self) -> &'static str {
        match self {
            BlankVia::None => "no blanking protocol",
            BlankVia::WlrOutputPower => "wlr-output-power-management-v1",
            BlankVia::KdeDpms => "org_kde_kwin_dpms",
        }
    }
}

pub struct WaylandBackend {
    shared: Arc<Mutex<Shared>>,
    blank: Option<std::sync::mpsc::Sender<bool>>,
    via: BlankVia,
    compositor: String,
}

impl WaylandBackend {
    /// Connects, binds what is available, and starts the event thread.
    ///
    /// Returns an error only if there is no Wayland session to connect to. A session
    /// without the idle protocol is a hard error too — an agent that registered and then
    /// reported "unknown" forever would produce a machine that never sleeps and a fault
    /// nobody attributes to the compositor.
    pub fn connect() -> Result<Self, String> {
        let connection =
            Connection::connect_to_env().map_err(|err| format!("no Wayland session: {err}"))?;

        let display = connection.display();
        let mut queue = connection.new_event_queue();
        let handle = queue.handle();
        display.get_registry(&handle, ());

        // What the previous instance of this agent last saw, if it was still writing it
        // moments ago. Believed only until the compositor's first transition either
        // confirms it (`idled`: the seat is still quiet) or overrules it (`resumed`:
        // somebody is here now).
        let carried = match crate::lastinput::load(clock::now()) {
            Some(instant) => {
                info!("carried the last input over from the previous agent");
                Tracked::Carried(instant)
            }
            None => Tracked::Unknown,
        };

        let mut state = State {
            shared: Arc::new(Mutex::new(Shared {
                tracked: carried,
                lost: false,
            })),
            seat: None,
            notifier: None,
            power_manager: None,
            dpms_manager: None,
            outputs: Vec::new(),
            powers: Vec::new(),
            dpms: Vec::new(),
            dpms_supported: false,
        };

        // Two round trips: the first delivers the global advertisements, the second the
        // events produced by binding them. One is not enough and the failure is silent --
        // a missing seat looks exactly like a compositor without the protocol.
        queue
            .roundtrip(&mut state)
            .map_err(|err| format!("Wayland roundtrip failed: {err}"))?;
        queue
            .roundtrip(&mut state)
            .map_err(|err| format!("Wayland roundtrip failed: {err}"))?;

        let (Some(notifier), Some(seat)) = (state.notifier.clone(), state.seat.clone()) else {
            return Err(
                "this compositor does not implement ext-idle-notify-v1, so real input cannot \
                 be observed. idlectl will not guess: install a compositor that supports it, \
                 or run the daemon on a machine with no graphical session."
                    .to_owned(),
            );
        };

        notifier.get_idle_notification(
            u32::try_from(NOTIFY_TIMEOUT.as_millis()).unwrap_or(10_000),
            &seat,
            &handle,
            (),
        );

        // wlroots first where both exist: it addresses outputs individually and says
        // nothing about the rest of the session, while DPMS is a display-server-wide
        // power state with a longer history of being fought over by other software.
        let mut via = BlankVia::None;
        if let Some(manager) = state.power_manager.clone()
            && !state.outputs.is_empty()
        {
            for output in &state.outputs {
                state
                    .powers
                    .push(manager.get_output_power(output, &handle, ()));
            }
            via = BlankVia::WlrOutputPower;
        } else if let Some(manager) = state.dpms_manager.clone()
            && !state.outputs.is_empty()
        {
            for output in &state.outputs {
                state.dpms.push(manager.get(output, &handle, ()));
            }
            // One more roundtrip: `supported` is an event, and asking before it has
            // arrived would report every KWin session as unable to blank.
            let _ = queue.roundtrip(&mut state);
            if state.dpms_supported {
                via = BlankVia::KdeDpms;
            }
        }

        let shared = Arc::clone(&state.shared);
        let (blank_tx, blank_rx) = std::sync::mpsc::channel::<bool>();
        let thread_via = via;

        // A dedicated thread rather than a future. The Wayland queue is a blocking,
        // synchronous API; driving it from the same reactor that owns the D-Bus socket
        // would mean a compositor that stops reading its socket could stall the agent's
        // heartbeat, and a stalled heartbeat reads as a dead agent.
        std::thread::Builder::new()
            .name("idlectl-wayland".to_owned())
            .spawn(move || {
                loop {
                    // Blanking requests arrive from the other thread and must be issued on
                    // this one, because the protocol objects are not Send-safe to use
                    // concurrently.
                    while let Ok(blank) = blank_rx.try_recv() {
                        match thread_via {
                            BlankVia::WlrOutputPower => {
                                let mode = if blank {
                                    zwlr_output_power_v1::Mode::Off
                                } else {
                                    zwlr_output_power_v1::Mode::On
                                };
                                for power in &state.powers {
                                    power.set_mode(mode);
                                }
                            }
                            BlankVia::KdeDpms => {
                                let mode = if blank {
                                    org_kde_kwin_dpms::Mode::Off
                                } else {
                                    org_kde_kwin_dpms::Mode::On
                                };
                                for dpms in &state.dpms {
                                    // The generated request takes a bare u32: this
                                    // protocol predates the enum attribute the scanner
                                    // uses to type such arguments.
                                    dpms.set(mode.into());
                                }
                            }
                            BlankVia::None => {}
                        }
                        let _ = connection.flush();
                    }

                    if queue.blocking_dispatch(&mut state).is_err() {
                        // The compositor went away. Everything this agent knows about
                        // human presence is now unknown -- NOT idle, which would permit a
                        // sleep -- and the daemon turns that into a veto.
                        if let Ok(mut guard) = state.shared.lock() {
                            guard.lost = true;
                        }
                        return;
                    }
                }
            })
            .map_err(|err| format!("cannot start the Wayland thread: {err}"))?;

        Ok(WaylandBackend {
            shared,
            blank: (via != BlankVia::None).then_some(blank_tx),
            via,
            compositor: std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_else(|_| "wayland".into()),
        })
    }
}

impl Backend for WaylandBackend {
    fn idle(&self) -> Idle {
        let Ok(shared) = self.shared.lock() else {
            return Idle::Unknown;
        };
        if shared.lost {
            return Idle::Unknown;
        }
        match shared.tracked {
            // Not an estimate: every path that sets one of these instants derives it from a
            // transition whose offset from the real input is exactly known.
            Tracked::Carried(at) | Tracked::Observed(at) => Idle::For(clock::now().since(at)),
            // Nothing has been observed and there was nothing to carry over. Saying "idle
            // for zero" here would be an observation nobody made, and the one that keeps a
            // machine awake; see [CLK-4] and the rationale under [CLK-7].
            Tracked::Unknown => Idle::Unknown,
        }
    }

    fn set_blank(&self, blank: bool) -> Result<(), String> {
        let Some(tx) = &self.blank else {
            return Err("this session offers no blanking mechanism".to_owned());
        };
        tx.send(blank)
            .map_err(|_| "the Wayland thread has exited".to_owned())
    }

    fn can_blank(&self) -> bool {
        self.via != BlankVia::None
    }

    fn persist(&self) {
        let Ok(shared) = self.shared.lock() else {
            return;
        };
        // A lost connection is not evidence about the seat, and neither is Unknown. Writing
        // either would refresh the timestamp that decides whether the next instance may
        // carry anything over, and would turn "nobody was watching" into "watched, saw
        // nothing" -- which is the one claim this agent must never make.
        if shared.lost {
            return;
        }
        if let Tracked::Carried(at) | Tracked::Observed(at) = shared.tracked {
            crate::lastinput::store(at, clock::now());
        }
    }

    fn describe(&self) -> String {
        format!(
            "wayland ({}), ext-idle-notify-v1, {}",
            self.compositor,
            self.via.name()
        )
    }
}

struct State {
    shared: Arc<Mutex<Shared>>,
    seat: Option<wl_seat::WlSeat>,
    notifier: Option<ExtIdleNotifierV1>,
    power_manager: Option<ZwlrOutputPowerManagerV1>,
    dpms_manager: Option<OrgKdeKwinDpmsManager>,
    outputs: Vec<wl_output::WlOutput>,
    powers: Vec<ZwlrOutputPowerV1>,
    dpms: Vec<OrgKdeKwinDpms>,
    /// Whether at least one output answered `supported`. KWin advertises the manager on
    /// every session and only some outputs can actually be powered down.
    dpms_supported: bool,
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        (): &(),
        _: &Connection,
        handle: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global {
            name,
            interface,
            version,
        } = event
        else {
            return;
        };

        match interface.as_str() {
            "ext_idle_notifier_v1" => {
                state.notifier = Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wl_seat" => {
                // The first seat only. A multi-seat machine is a machine with more than
                // one human on it, and idle on seat 0 says nothing about seat 1 -- but
                // each seat gets its own agent, so each reports its own.
                if state.seat.is_none() {
                    state.seat = Some(registry.bind(name, version.min(7), handle, ()));
                }
            }
            "zwlr_output_power_manager_v1" => {
                state.power_manager = Some(registry.bind(name, version.min(1), handle, ()));
            }
            "org_kde_kwin_dpms_manager" => {
                state.dpms_manager = Some(registry.bind(name, version.min(1), handle, ()));
            }
            "wl_output" => {
                state
                    .outputs
                    .push(registry.bind(name, version.min(4), handle, ()));
            }
            _ => {}
        }
    }
}

impl Dispatch<ExtIdleNotificationV1, ()> for State {
    fn event(
        state: &mut Self,
        _: &ExtIdleNotificationV1,
        event: ext_idle_notification_v1::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        let Ok(mut shared) = state.shared.lock() else {
            return;
        };
        match event {
            ext_idle_notification_v1::Event::Idled => {
                shared.tracked = idled(shared.tracked, clock::now());
            }
            // Input, right now. This is the one event that is a direct observation of a
            // human, so it overrules anything carried over from the previous instance.
            ext_idle_notification_v1::Event::Resumed => {
                shared.tracked = Tracked::Observed(clock::now());
            }
            _ => {}
        }
    }
}

impl Dispatch<OrgKdeKwinDpms, ()> for State {
    fn event(
        state: &mut Self,
        _: &OrgKdeKwinDpms,
        event: org_kde_kwin_dpms::Event,
        (): &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        // `supported` is per output and it is the only honest answer to "can this session
        // blank". KWin advertises the manager unconditionally, so binding it says nothing;
        // an output saying `supported = 1` does.
        if let org_kde_kwin_dpms::Event::Supported { supported } = event
            && supported != 0
        {
            state.dpms_supported = true;
        }
    }
}

// Nothing is done with the events of these objects. They are bound because the protocol
// requires the object to exist, not because they say anything the agent reads.
delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: ignore wl_output::WlOutput);
delegate_noop!(State: ExtIdleNotifierV1);
delegate_noop!(State: ZwlrOutputPowerManagerV1);
delegate_noop!(State: ignore ZwlrOutputPowerV1);
delegate_noop!(State: OrgKdeKwinDpmsManager);

#[cfg(test)]
mod tests {
    use super::{NOTIFY_TIMEOUT, Tracked, idled};
    use idlectl_policy::BootInstant;

    /// The regression this whole mechanism exists for. An agent that started while the seat
    /// was already quiet must not conclude that the last input was ten seconds ago.
    ///
    /// Measured before the fix, with nobody in the room: restarting the agent moved the
    /// daemon's `human_input` origin from +25823 s to +26299 s, which was exactly the
    /// uptime at the restart.
    #[test]
    fn a_carried_instant_survives_the_first_idled() {
        let carried = BootInstant::from_secs(1000);
        let after = idled(Tracked::Carried(carried), BootInstant::from_secs(9000));
        assert_eq!(after, Tracked::Observed(carried));
    }

    #[test]
    fn without_anything_carried_the_input_was_one_timeout_ago() {
        let at = BootInstant::from_secs(9000);
        let after = idled(Tracked::Unknown, at);
        assert_eq!(after, Tracked::Observed(at.saturating_sub(NOTIFY_TIMEOUT)));
    }

    /// After a `resumed`, the next `idled` is genuine news and moves the instant forward.
    #[test]
    fn an_observed_instant_moves_forward_on_the_next_idled() {
        let at = BootInstant::from_secs(9000);
        let after = idled(Tracked::Observed(BootInstant::from_secs(500)), at);
        assert_eq!(after, Tracked::Observed(at.saturating_sub(NOTIFY_TIMEOUT)));
    }

    /// Defensive: a carried instant newer than the transition allows cannot happen -- the
    /// compositor would not have sent `idled` -- but if it ever did, the older answer wins
    /// rather than the machine gaining idle time it never had.
    #[test]
    fn the_older_answer_wins() {
        let at = BootInstant::from_secs(9000);
        let after = idled(Tracked::Carried(BootInstant::from_secs(8999)), at);
        assert_eq!(after, Tracked::Observed(at.saturating_sub(NOTIFY_TIMEOUT)));
    }
}
