//! Gamepad input, because no display server reports it as input.
//!
//! # Why this exists
//!
//! `ext-idle-notify-v1` resets on what the compositor processes as **seat** input, and
//! libinput does not handle joysticks: a device whose udev classification is
//! `ID_INPUT_JOYSTICK` and nothing else is never opened as a keyboard or as a pointer. On a
//! desktop that is invisible, because the keyboard is right there. On a machine driven from
//! a sofa it is total — the only input device in the room produces nothing the idle protocol
//! can see.
//!
//! Measured on such a machine, mid-game, with a wireless pad as its only input: 38 580 axis
//! events and 54 button presses in eight minutes, and `human_active` went **false** while
//! somebody was demonstrably playing. Then the panel blanked on schedule, and 45 seconds of
//! working the stick did not bring it back — because bringing it back needs seat input too.
//!
//! The game platform knows about this and tries the old desktop remedy: it called
//! `org.freedesktop.ScreenSaver.SimulateUserActivity` 1.46 times a second throughout. The
//! compositor does not route that into `ext-idle-notify-v1`, so the reading climbed anyway.
//!
//! # Why the joystick interface and not `evdev`
//!
//! Two reasons, and the second one is the one that decided it.
//!
//! `/dev/input/js*` reports every axis **already normalised** to `-32767..=32767`, whatever
//! the hardware's own range is. On `evdev` the same information needs an `EVIOCGABS` ioctl
//! per axis — and this crate is `#![forbid(unsafe_code)]`, which an ioctl cannot honour.
//! That forbid is not decoration: this is the half of the project that reads a display
//! server and now reads input devices, and "contains no unsafe code at all" is a claim worth
//! more than the convenience of a nicer interface.
//!
//! And the interface itself is the permission boundary. A `js` node exists **only** for a
//! joystick — `joydev` binds to devices that have absolute axes and buttons, so a keyboard
//! never gets one. This module therefore cannot read a keyboard even by mistake: not because
//! it promises not to, but because there is nothing there to open. Measured on the receiver
//! of the pad above, whose keyboard and mouse interfaces are separate devices on the same
//! USB dongle:
//!
//! ```text
//! event18  8BitDo Ultimate 2 Wireless Controller   js1     ID_INPUT_JOYSTICK
//! event19  8BitDo UM 2 Receiver Keyboard           no js   ID_INPUT_KEY,ID_INPUT_KEYBOARD
//! event20  8BitDo UM 2 Receiver Mouse              no js   ID_INPUT_MOUSE
//! ```
//!
//! The cost of the choice is that `joydev` is a module: where it is absent there are no `js`
//! nodes, this module watches nothing, and the session behaves exactly as it did before.
//! That is a degradation with a floor, not a fault.
//!
//! # Why an axis needs a deadzone and a button does not
//!
//! A press is unambiguous: somebody pressed it. An axis is not. The interface is
//! edge-triggered, so a stick resting off-centre is *silent* — but a **noisy** one emits a
//! change forever, and counting those would pin a machine awake with nobody in the room. On
//! the machine this was written for that means an OLED television holding a static HUD until
//! morning, which is the exact failure the blanking policy exists to prevent.
//!
//! So an axis counts only when it moves more than an eighth of the normalised range. The
//! resting position it is measured from costs nothing to learn: opening a `js` node makes the
//! kernel replay the current value of every axis and button with `JS_EVENT_INIT` set, and
//! those synthetic events seed the baseline without ever counting as a person.
//!
//! # Why a thread per pad, and not a poll on the heartbeat
//!
//! Because `joydev` demotes a slow reader, and the demotion is silent. A client whose buffer
//! overruns is put back into its **initial replay**, so it stops seeing real events entirely
//! and keeps being handed the same synthetic ones instead.
//!
//! Measured, reading one device every five seconds while a stick swept continuously:
//!
//! ```text
//! round 1: 3 events, types={0x81: 1, 0x82: 2}
//! round 5: 3 events, types={0x81: 1, 0x82: 2}
//! ```
//!
//! Three init events, five times, and not one movement — a detector built on the agent's
//! thirty-second heartbeat would have reported "nobody is here" through an entire game while
//! reading the device on schedule and finding it healthy. So each pad gets a thread parked in
//! a blocking `read`, which is the same shape the daemon uses to watch a lease handle: one
//! syscall, one stack, and no polling interval to be wrong about.

use std::collections::{HashMap, HashSet};
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use idlectl_policy::BootInstant;
use rustix::fs::{Mode, OFlags};
use rustix::io::Errno;
use tracing::{info, warn};

use crate::backend::Idle;
use crate::clock;

/// How often the set of devices is re-read.
///
/// A pad appears and disappears with its receiver — the one this was measured on exposes no
/// device at all until the controller is switched on — so the set cannot be decided once at
/// startup. Five seconds is one `read_dir`; the cost of being slower is that the first
/// seconds of a session go uncounted, on a machine whose whole job is to notice somebody is
/// there.
const RESCAN: Duration = Duration::from_secs(5);

/// How stale the previous input has to be for a touch to be worth waking the agent for.
///
/// A pad in continuous use produces tens of events a second. Signalling on each of them would
/// turn the agent's loop into a busy one and its D-Bus reports into a flood, so only the
/// *first* touch after a quiet spell wakes it — which is the one that matters, because it is
/// somebody picking up a controller in front of a dark panel.
const NEWS_AFTER: Duration = Duration::from_secs(5);

/// `struct js_event`: a `u32` of milliseconds, an `i16` value, then type and number.
const EVENT_SIZE: usize = 8;

/// The normalised half-range every `js` axis is reported in.
const AXIS_RANGE: i32 = 32767;

/// How far an axis has to move to be a person, as a fraction of [`AXIS_RANGE`].
///
/// An eighth — about 4 096 counts. Generous enough that no real push of a stick is missed,
/// and far above the jitter of a worn one, which is tens of counts.
const DEADZONE: i32 = AXIS_RANGE / 8;

const JS_EVENT_BUTTON: u8 = 0x01;
const JS_EVENT_AXIS: u8 = 0x02;
/// Set on the synthetic events the kernel replays when a device is opened.
const JS_EVENT_INIT: u8 = 0x80;

/// One decoded event from a joystick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Event {
    value: i16,
    /// The type with [`JS_EVENT_INIT`] already stripped.
    kind: u8,
    number: u8,
    /// Whether this was the kernel replaying the device's current state on open.
    initial: bool,
}

/// Decodes one `js_event`.
fn decode(raw: &[u8]) -> Option<Event> {
    let raw: &[u8; EVENT_SIZE] = raw.try_into().ok()?;
    let kind = raw[6];
    Some(Event {
        // Bytes 0..4 are a millisecond timestamp on a clock this project does not anchor
        // deadlines to, so they are deliberately not read. See `Gamepads::poll`.
        value: i16::from_ne_bytes([raw[4], raw[5]]),
        kind: kind & !JS_EVENT_INIT,
        number: raw[7],
        initial: kind & JS_EVENT_INIT != 0,
    })
}

/// Whether an event is a person, given where each axis was last *counted from*.
///
/// The baseline moves only when a movement is counted, and that is the whole subtlety of this
/// function. Advancing it on every event instead looks equivalent and is not: a stick is swept,
/// not teleported, so steering through a corner arrives as a long run of small steps. Measured
/// against the previous *event* every step is tiny and nothing ever exceeds the deadzone — the
/// detector would have been blind to exactly the input it exists to see. Measured against the
/// last counted position, the sweep accumulates and crosses it.
///
/// Noise is still rejected, because noise oscillates *around* a point instead of leaving it.
fn is_human(axes: &mut HashMap<u8, i16>, event: Event) -> bool {
    match event.kind {
        JS_EVENT_AXIS => {
            if event.initial {
                // The kernel replaying where this axis rests. Seeds the baseline, and is
                // never a person.
                axes.insert(event.number, event.value);
                return false;
            }
            let Some(&baseline) = axes.get(&event.number) else {
                // An axis that speaks before any replay: remembered, not counted. Nothing is
                // known yet about where it was resting.
                axes.insert(event.number, event.value);
                return false;
            };
            if i32::from(event.value).abs_diff(i32::from(baseline))
                > u32::try_from(DEADZONE).unwrap_or(0)
            {
                axes.insert(event.number, event.value);
                return true;
            }
            false
        }
        // Presses only, and never the replay. A release is the same person, and counting one
        // of the two keeps "this pad was touched N times" honest in the log.
        JS_EVENT_BUTTON => !event.initial && event.value == 1,
        _ => false,
    }
}

/// The most threads that will ever be parked on joysticks at once.
///
/// A bound rather than a limit anybody meets: three pads and a platform's virtual mirror is
/// four. It exists so that a machine with a misbehaving driver churning out device nodes
/// cannot turn this into a thread bomb inside the agent that reports whether a human is here.
const MAX_WATCHED: usize = 8;

/// What every watcher thread writes to and the agent reads from.
#[derive(Default)]
struct Shared {
    /// When a pad was last touched. `None` until one is.
    last_input: Option<BootInstant>,
    /// The devices that currently have a live thread. A watcher removes its own path on the
    /// way out, which is what lets the next rescan reopen a pad that came back.
    watching: HashSet<PathBuf>,
}

/// Every joystick this session can read, and when one was last touched.
pub struct Gamepads {
    shared: Arc<Mutex<Shared>>,
    /// Signalled by a watcher when a quiet pad is touched, so the agent reports at once
    /// instead of on its heartbeat. Bounded at one: the signal is "look now", not a queue.
    wake_tx: async_channel::Sender<()>,
    wake_rx: async_channel::Receiver<()>,
    next_scan: Option<BootInstant>,
    /// Whether the pad, rather than the compositor, is currently what says somebody is here.
    /// Kept so the journal records the transition once instead of every heartbeat.
    pad_is_source: bool,
}

/// Reads one joystick until it goes away.
///
/// Parked in a blocking `read` rather than polling, for the reason in the module note: a
/// `joydev` client that falls behind is silently put back into its initial replay and never
/// sees another real event.
fn watch(path: PathBuf, shared: &Arc<Mutex<Shared>>, wake: &async_channel::Sender<()>) {
    // Blocking on purpose. CLOEXEC so a pad's descriptor is not inherited by anything this
    // process might spawn.
    let opened = rustix::fs::open(&path, OFlags::RDONLY | OFlags::CLOEXEC, Mode::empty());
    let fd: OwnedFd = match opened {
        Ok(fd) => fd,
        Err(err) => {
            // At info, once: a joystick this session may not read is a configuration the
            // agent cannot fix, and the rescan would otherwise say so every five seconds.
            info!(path = %path.display(), error = %err, "cannot read this joystick");
            forget(&path, shared);
            return;
        }
    };
    info!(path = %path.display(), "watching this joystick for input");

    let mut axes = HashMap::new();
    let mut buf = [0u8; 32 * EVENT_SIZE];
    loop {
        match rustix::io::read(&fd, &mut buf) {
            // End of file on a character device: the pad is gone.
            Ok(0) => break,
            Ok(read) => {
                let mut touched = false;
                for chunk in buf[..read].chunks_exact(EVENT_SIZE) {
                    if let Some(event) = decode(chunk) {
                        if is_human(&mut axes, event) {
                            touched = true;
                        }
                    }
                }
                if touched {
                    // The instant is recorded here, where the read returned, and not on the
                    // agent's heartbeat: the whole point of the thread is that this is the
                    // moment the input actually arrived.
                    let now = clock::now();
                    let mut news = false;
                    if let Ok(mut shared) = shared.lock() {
                        news = shared
                            .last_input
                            .is_none_or(|last| now.since(last) >= NEWS_AFTER);
                        shared.last_input = Some(now);
                    }
                    if news {
                        // Reporting this on the next heartbeat would be up to thirty seconds
                        // away, and a panel that comes back half a minute after somebody
                        // picks up the controller is not a console. Measured before this
                        // existed: fifty seconds.
                        let _ = wake.try_send(());
                    }
                }
            }
            // A signal is not news about the pad.
            Err(Errno::INTR) => continue,
            // ENODEV is the receiver being unplugged or the pad switching itself off, which
            // is ordinary and happens several times a day.
            Err(_) => break,
        }
    }
    info!(path = %path.display(), "this joystick is gone");
    forget(&path, shared);
}

/// Drops a path from the watched set, so a device that returns is opened again.
fn forget(path: &PathBuf, shared: &Arc<Mutex<Shared>>) {
    if let Ok(mut shared) = shared.lock() {
        shared.watching.remove(path);
    }
}

impl Gamepads {
    #[must_use]
    pub fn new() -> Self {
        let (wake_tx, wake_rx) = async_channel::bounded(1);
        Self {
            shared: Arc::new(Mutex::new(Shared::default())),
            wake_tx,
            wake_rx,
            next_scan: None,
            pad_is_source: false,
        }
    }

    /// Fires when a quiet pad is touched. The agent races this against its heartbeat.
    #[must_use]
    pub fn woken(&self) -> async_channel::Receiver<()> {
        self.wake_rx.clone()
    }

    /// Merges what the pads have seen into what the compositor said.
    ///
    /// A pad can only ever make the idle time **shorter**, and it never overrules
    /// [`Idle::Unknown`]. Doubt means this session's idle protocol is not answering, which
    /// the daemon turns into `INDETERMINATE` and which vetoes every sleep action; a pad that
    /// has been quiet for an hour is not evidence that the protocol recovered, and reporting
    /// that hour here would retire a fault signal on the word of a device that cannot see
    /// keyboards. So doubt stays doubt.
    pub fn merge(&mut self, seen: Idle) -> Idle {
        let pad = self.poll();
        let merged = match (seen, pad) {
            (Idle::Unknown, _) => Idle::Unknown,
            (Idle::For(compositor), Some(pad)) if pad < compositor => Idle::For(pad),
            (other, _) => other,
        };

        let now_source = matches!((seen, merged), (Idle::For(a), Idle::For(b)) if b < a);
        if now_source != self.pad_is_source {
            self.pad_is_source = now_source;
            if now_source {
                info!("a gamepad is what says somebody is here; the compositor does not see it");
            }
        }
        merged
    }

    /// Starts watchers for pads that have appeared, and reports how long since one was
    /// touched.
    fn poll(&mut self) -> Option<Duration> {
        let now = clock::now();
        if self.next_scan.is_none_or(|due| now >= due) {
            self.next_scan = now.checked_add(RESCAN);
            self.rescan();
        }
        let last = self.shared.lock().ok()?.last_input?;
        Some(clock::now().since(last))
    }

    /// Spawns a watcher for every joystick that does not already have one.
    fn rescan(&mut self) {
        for path in joystick_paths() {
            let shared = Arc::clone(&self.shared);
            {
                let Ok(mut state) = self.shared.lock() else {
                    return;
                };
                if state.watching.len() >= MAX_WATCHED || !state.watching.insert(path.clone()) {
                    continue;
                }
            }
            // Recorded as watched *before* the thread starts, so a rescan that lands while
            // the thread is still opening the device cannot start a second one on it.
            let name = path.clone();
            let wake = self.wake_tx.clone();
            if let Err(err) = std::thread::Builder::new()
                .name(format!("idlectl-pad-{}", path.display()))
                .spawn(move || watch(name, &shared, &wake))
            {
                warn!(path = %path.display(), error = %err, "cannot watch this joystick");
                forget(&path, &self.shared);
            }
        }
    }
}

#[cfg(test)]
impl Gamepads {
    /// Pretends a pad was touched at `at`, so the merge can be tested without a device.
    fn pretend_touched(&mut self, at: BootInstant) {
        self.shared
            .lock()
            .expect("uncontended in a test")
            .last_input = Some(at);
    }
}

impl Default for Gamepads {
    fn default() -> Self {
        Self::new()
    }
}

/// Every joystick device node, sorted.
///
/// `js` and digits, and nothing else: the numbered nodes are the whole of the interface, and
/// a name that merely starts with `js` is not one of them.
fn joystick_paths() -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return Vec::new();
    };
    let mut out: Vec<PathBuf> = entries
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            let name = name.to_str()?;
            let digits = name.strip_prefix("js")?;
            (!digits.is_empty() && digits.bytes().all(|byte| byte.is_ascii_digit()))
                .then(|| entry.path())
        })
        .collect();
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One event, laid out the way the kernel writes it.
    fn raw(kind: u8, number: u8, value: i16) -> [u8; EVENT_SIZE] {
        let mut out = [0u8; EVENT_SIZE];
        out[0..4].copy_from_slice(&1234u32.to_ne_bytes());
        out[4..6].copy_from_slice(&value.to_ne_bytes());
        out[6] = kind;
        out[7] = number;
        out
    }

    #[test]
    fn an_event_is_decoded_at_the_offsets_the_kernel_uses() {
        // Getting this wrong reads garbage that still parses, so it is asserted rather than
        // assumed: every field would shift.
        let event = decode(&raw(JS_EVENT_AXIS, 3, -20000)).expect("eight bytes decode");
        assert_eq!(
            event,
            Event {
                value: -20000,
                kind: JS_EVENT_AXIS,
                number: 3,
                initial: false
            }
        );
        // The init flag is carried separately, not left in the type, so no comparison
        // against JS_EVENT_AXIS has to remember to mask it.
        let replay = decode(&raw(JS_EVENT_AXIS | JS_EVENT_INIT, 3, 500)).expect("decodes");
        assert_eq!(replay.kind, JS_EVENT_AXIS);
        assert!(replay.initial);
        // Anything that is not a whole event is refused rather than read past.
        assert!(decode(&[0u8; 4]).is_none());
    }

    #[test]
    fn jitter_is_not_a_human_and_a_real_push_is() {
        let mut axes = HashMap::new();
        // The kernel's replay on open: the stick is resting at zero.
        assert!(!is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS | JS_EVENT_INIT, 0, 0)).unwrap()
        ));

        // A worn potentiometer wobbling around its resting point, forever. Counting this is
        // how a machine with nobody in the room never sleeps. Note the values oscillate
        // rather than march: that is what makes them noise, and it is why the baseline has
        // to stay put instead of following each event.
        for noise in [40i16, -40, 300, -300, 4095, -4095, 200] {
            assert!(
                !is_human(&mut axes, decode(&raw(JS_EVENT_AXIS, 0, noise)).unwrap()),
                "{noise} is noise around the resting point, not a person"
            );
        }
        // Somebody steering.
        assert!(is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS, 0, 20000)).unwrap()
        ));
        // Measured from 20000 now: holding the stick over is not repeated news.
        assert!(!is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS, 0, 21000)).unwrap()
        ));
        assert!(is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS, 0, 0)).unwrap()
        ));
    }

    #[test]
    fn a_stick_swept_gradually_is_a_person() {
        // The case this module exists for, and the one an obvious implementation misses. A
        // stick is swept, not teleported: steering through a corner arrives as a long run of
        // small steps, none of them larger than the deadzone. Comparing each event with the
        // previous one would therefore see nothing at all while somebody drove.
        let mut axes = HashMap::new();
        assert!(!is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS | JS_EVENT_INIT, 0, 0)).unwrap()
        ));

        let mut counted = 0;
        for step in 1..=60 {
            // 200 counts at a time: about a fifth of a second of real steering per step, and
            // a twentieth of the deadzone.
            let value = i16::try_from(step * 200).expect("within range");
            if is_human(&mut axes, decode(&raw(JS_EVENT_AXIS, 0, value)).unwrap()) {
                counted += 1;
            }
        }
        assert!(
            counted >= 2,
            "a sweep to full deflection must register, and did {counted} time(s)"
        );
    }

    #[test]
    fn the_replay_on_open_is_never_a_person() {
        // Opening a device makes the kernel resend the current state of every axis and
        // button. Counting that would make every hotplug -- and every agent restart -- look
        // like somebody walking into the room.
        let mut axes = HashMap::new();
        assert!(!is_human(
            &mut axes,
            decode(&raw(JS_EVENT_BUTTON | JS_EVENT_INIT, 0, 1)).unwrap()
        ));
        assert!(!is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS | JS_EVENT_INIT, 1, 32767)).unwrap()
        ));
        // And the replayed value is the baseline: a stick held hard over when the agent
        // starts is not read as a full-deflection push a moment later.
        assert!(!is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS, 1, 32000)).unwrap()
        ));
        assert!(is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS, 1, 0)).unwrap()
        ));
    }

    #[test]
    fn a_press_counts_once_and_a_release_does_not() {
        let mut axes = HashMap::new();
        assert!(is_human(
            &mut axes,
            decode(&raw(JS_EVENT_BUTTON, 2, 1)).unwrap()
        ));
        assert!(!is_human(
            &mut axes,
            decode(&raw(JS_EVENT_BUTTON, 2, 0)).unwrap()
        ));
    }

    #[test]
    fn an_axis_that_speaks_before_its_replay_is_remembered_and_not_counted() {
        // Nothing is known about where it was resting, so the first value is a baseline and
        // not a movement.
        let mut axes = HashMap::new();
        assert!(!is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS, 7, 30000)).unwrap()
        ));
        assert_eq!(axes.get(&7), Some(&30000));
        assert!(is_human(
            &mut axes,
            decode(&raw(JS_EVENT_AXIS, 7, 0)).unwrap()
        ));
    }

    #[test]
    fn the_pad_never_overrules_doubt() {
        // `Unknown` vetoes every sleep action, and a quiet pad is not evidence that the
        // session's idle protocol came back. Retiring the fault here would silence it.
        let mut pads = Gamepads::new();
        pads.pretend_touched(BootInstant::from_secs(10));
        assert_eq!(pads.merge(Idle::Unknown), Idle::Unknown);
    }

    #[test]
    fn the_pad_only_ever_shortens_the_compositors_number() {
        let mut pads = Gamepads::new();
        // No pad has ever been touched: the compositor's answer stands untouched.
        assert_eq!(
            pads.merge(Idle::For(Duration::from_secs(600))),
            Idle::For(Duration::from_secs(600))
        );
        // A pad touched long ago must not make a fresh session look stale.
        pads.pretend_touched(BootInstant::from_secs(1));
        let seen = Idle::For(Duration::from_secs(2));
        assert_eq!(pads.merge(seen), seen, "a pad may not lengthen idle time");
    }
}
