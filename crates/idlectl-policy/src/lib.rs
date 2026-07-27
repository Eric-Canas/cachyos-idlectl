//! The idlectl policy engine.
//!
//! # What this crate decides
//!
//! Given a [`Policy`] (a set of `[while ...]` and `[when ...]` blocks), a [`FactSet`] (what
//! the machine currently looks like) and a [`ClockSnapshot`] (a set of monotonic
//! origins), [`resolve`] returns a [`Decision`]: for each of the four actions
//! ([`Action::ScreenOff`], [`Action::Suspend`], [`Action::Hibernate`],
//! [`Action::PowerOff`]) the instant at which that action becomes permissible, and
//! whether that instant has passed.
//!
//! # The composition rule
//!
//! The config is a set of blocks. Each block gives a timeout **per action**. For each
//! action `a` **independently**:
//!
//! ```text
//! participates(b, a) <=> block b sets a key for action a. A block that does not set a
//!                        key for a does not participate in a, whatever its condition
//!                        state.
//!
//! deadline(b, a)  =  +infinity                          if timeout_b(a) = "never"
//!                 =  origin(clock_b) + timeout_b(a)     otherwise
//!
//! floor(a)        =  MAX over deadline(b, a) for every participating, enabled,
//!                    currently-TRUE [while] block b, together with one +infinity term
//!                    for each implicit floor in force (the doubt floor of [FACT-4] and
//!                    the configuration-fault floor of [CFG-16]; neither applies to
//!                    screen_off).  MAX over the empty set = +infinity.
//!
//! ceiling(a)      =  MIN over deadline(b, a) for every participating, enabled,
//!                    currently-TRUE [when] block b, plus the ephemeral ceiling
//!                    installed by `--force`.  MIN over the empty set = +infinity.
//!
//! resolved(a)     =  MIN(floor(a), ceiling(a))
//!
//! a is permitted <=> now >= resolved(a)
//! ```
//!
//! Order can never matter. There is no priority list. The default is to not act.
//!
//! ## Silence is not `never` ([COMP-2b])
//!
//! A block that does not set a key for action `a` contributes **nothing** to `floor(a)`
//! and nothing to `ceiling(a)`, whatever its condition's state. This is what makes
//! per-action policy work: `[while.steam_downloading]` vetoes suspend while having no
//! opinion about the panel, and `[while.human_active]` holds the machine awake without
//! pinning a lit image on it. Reading an unset action key as `never` would pin an OLED
//! panel lit for the length of a six-hour download, which is the exact failure the
//! screen-off carve-out exists to prevent. An implementation that treats an unset action
//! key as `never` is non-conforming.
//!
//! ## One ceiling class ([CEIL-3], [CEIL-4])
//!
//! v1 ships no soft/hard split. Every `[when]` block is absolute, and because ceilings
//! compose by MIN over instants, `resolved(a) <= floor(a)` always: a ceiling can only
//! ever pull an action **earlier** and can never delay one. A ceiling therefore defeats
//! everything a floor can raise -- `never` floors, the human-presence floor, the implicit
//! doubt floor of [FACT-4] and the configuration-fault floor of [CFG-16]. There is
//! exactly one override channel in the system and this is it, which is why every ceiling
//! is logged at load and again when it fires.
//!
//! ## Doubt is a floor over the whole fact set ([FACT-4], [FACT-4b])
//!
//! If **any enabled fact** reads [`FactState::Indeterminate`], every action for which
//! [`Action::vetoed_by_doubt`] holds gets an implicit `+infinity` floor -- whether or not
//! any configured block references that fact. The rule is about the machine no longer
//! being able to see something it normally sees; whether the operator happened to write a
//! block for it is irrelevant to that. Conversely, a block whose *condition* is
//! indeterminate is FALSE as a selector and contributes nothing, in `[while]` and in
//! `[when]` alike: doubt may **prevent** an action, it may never **cause** one.
//!
//! A fact whose capability is absent must report [`FactState::Unavailable`], never
//! `Indeterminate`: unavailable is knowledge, indeterminate is doubt. The auditable way
//! out of a permanently-broken detector is to disable the fact
//! ([`FactSettings::enabled`]), which stops its detector running and removes it from this
//! scan.
//!
//! ## Depth ([ACT-7])
//!
//! [`Action::ScreenOff`] is not a change of the system's power state: it may be applied
//! in the same evaluation as a sleep action and never suppresses one. Among
//! [`Action::Suspend`], [`Action::Hibernate`] and [`Action::PowerOff`] at most one may be
//! applied per evaluation, and it must be the **shallowest** one that is due -- see
//! [`Decision::shallowest_sleep_due`]. idlectl therefore does not escalate.
//!
//! # Composition happens over instants, not durations
//!
//! This is the single most important property in the crate, and getting it wrong
//! reproduces a real incident verbatim.
//!
//! Blocks are measured on **different clocks**. `[while.always]` counts from the last
//! human input; `[while.after_resume]` counts from the last resume. `max(30m, 5m)` is
//! meaningless when the two are counted from different origins, so the engine never
//! composes durations. It converts each block's timeout into an **absolute deadline** on
//! `CLOCK_BOOTTIME` first, and composes those deadlines, exactly as in the formula above.
//! Maximum and minimum over instants are still commutative and associative, so
//! order-independence survives intact.
//!
//! The worked example, which is a test in this file
//! (`resume_window_beats_nine_hours_of_accumulated_idle`): a machine carrying
//! nine hours of accumulated human-idle resumes from suspend. `[while.always]` says
//! `suspend = 30m` on the human-input clock; `[while.after_resume]` says `suspend = 5m`
//! on the resume clock. Idle counters do **not** reset across a suspend, so the human
//! clock still reads nine hours.
//!
//! * Composing **durations**: `max(30m, 5m) = 30m`, measured from an origin nine hours
//!   in the past, so the deadline is 8h30m in the past and the machine suspends roughly
//!   one second after waking up. That is the bug, and it was measured: wake at 08:35:45,
//!   `SUSPENDING` at 08:35:46.
//! * Composing **instants**: `max(input + 30m, resume + 5m) = resume + 5m`, five minutes
//!   in the future. Correct.
//!
//! # Why `CLOCK_BOOTTIME`
//!
//! `CLOCK_MONOTONIC` stops during suspend on Linux, so a deadline computed before a
//! suspend would come back wrong by exactly the length of the sleep. `CLOCK_BOOTTIME`
//! keeps counting. `CLOCK_REALTIME` is out of the question: NTP can step it, and a
//! backwards step would make a deadline unreachable.
//!
//! # The core invariant
//!
//! **A machine that stays awake wrongly is a cheap error. One that sleeps wrongly is
//! not.** Every ambiguous case in this crate resolves towards staying awake. The one
//! documented exception is [`Action::ScreenOff`], where the asymmetry reverses: a panel
//! left on all night is a real cost on OLED, and nothing is lost by blanking it.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

#[cfg(feature = "serde")]
use serde::Serialize;

// ---------------------------------------------------------------------------------
// Instants and deadlines
// ---------------------------------------------------------------------------------

/// A point on `CLOCK_BOOTTIME`, in nanoseconds since boot.
///
/// Deliberately not [`std::time::Instant`]: `Instant` is `CLOCK_MONOTONIC` on Linux,
/// which freezes across suspend and would silently corrupt every deadline this engine
/// computes. This type is also plain data, so a whole scenario can be constructed in a
/// unit test without touching a clock.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct BootInstant(u64);

impl BootInstant {
    /// The instant the kernel started counting: `CLOCK_BOOTTIME` zero.
    pub const BOOT: Self = BootInstant(0);

    /// Builds an instant from nanoseconds since boot.
    #[must_use]
    pub const fn from_nanos(nanos: u64) -> Self {
        BootInstant(nanos)
    }

    /// Builds an instant from whole seconds since boot. Saturates rather than wrapping.
    #[must_use]
    pub const fn from_secs(secs: u64) -> Self {
        BootInstant(secs.saturating_mul(1_000_000_000))
    }

    /// Nanoseconds since boot.
    #[must_use]
    pub const fn as_nanos(self) -> u64 {
        self.0
    }

    /// Microseconds since boot.
    ///
    /// This is the unit logind uses on D-Bus (`IdleSinceHint` and friends), so it is the
    /// unit the daemon publishes.
    #[must_use]
    pub const fn as_micros(self) -> u64 {
        self.0 / 1_000
    }

    /// Adds a duration, returning [`None`] on overflow.
    ///
    /// Callers map [`None`] to [`Deadline::Never`]: a deadline that cannot be
    /// represented is one that will never arrive, which is the safe direction.
    #[must_use]
    pub fn checked_add(self, delta: Duration) -> Option<Self> {
        let nanos = u64::try_from(delta.as_nanos()).ok()?;
        self.0.checked_add(nanos).map(BootInstant)
    }

    /// Subtracts a duration, saturating at [`BootInstant::BOOT`].
    #[must_use]
    pub fn saturating_sub(self, delta: Duration) -> Self {
        let nanos = u64::try_from(delta.as_nanos()).unwrap_or(u64::MAX);
        BootInstant(self.0.saturating_sub(nanos))
    }

    /// How long ago `earlier` was, saturating at zero if `earlier` is in the future.
    #[must_use]
    pub fn since(self, earlier: Self) -> Duration {
        Duration::from_nanos(self.0.saturating_sub(earlier.0))
    }
}

/// The instant an action becomes permissible, or [`Deadline::Never`].
///
/// [`Deadline::Never`] is `+infinity`, and its [`Ord`] implementation says so. That makes
/// `never` win a maximum and lose a minimum, exactly as the composition rule requires.
///
/// # The empty-set trap
///
/// `Never` is *also* the answer when no block contributed anything at all -- but for a
/// completely different reason ("if nothing says the machine may sleep, it may not").
/// Because `Never` is the top element, seeding a maximum-fold with it would absorb every
/// real deadline and the machine would never act. So the folds in [`compose_floor`] and
/// [`compose_ceiling`] are seeded with [`None`] and only collapse to `Never` when the
/// iterator turned out to be empty. Do not "simplify" them into `fold(Never, max)`.
// Externally tagged on purpose: serde's internally-tagged representation cannot encode a
// newtype variant whose payload is not a map, and `At(BootInstant)` is a bare integer.
// That combination compiles and then fails at run time, which is the worst place for it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Deadline {
    /// The action becomes permissible at this instant.
    At(
        /// The instant, on `CLOCK_BOOTTIME`.
        BootInstant,
    ),
    /// The action never becomes permissible. `+infinity`.
    Never,
}

impl Deadline {
    /// Builds a deadline `timeout` after `origin`.
    ///
    /// [`Timeout::Never`] yields [`Deadline::Never`], and so does an arithmetic overflow.
    #[must_use]
    pub fn from_origin(origin: BootInstant, timeout: Timeout) -> Self {
        match timeout {
            Timeout::Never => Deadline::Never,
            Timeout::After(d) => match origin.checked_add(d) {
                Some(instant) => Deadline::At(instant),
                None => Deadline::Never,
            },
        }
    }

    /// Whether `now` has reached this deadline. [`Deadline::Never`] is never reached.
    #[must_use]
    pub fn is_due_at(self, now: BootInstant) -> bool {
        match self {
            Deadline::Never => false,
            Deadline::At(instant) => now >= instant,
        }
    }

    /// How long remains until this deadline, or [`None`] for [`Deadline::Never`] and for
    /// deadlines already in the past.
    #[must_use]
    pub fn remaining_at(self, now: BootInstant) -> Option<Duration> {
        match self {
            Deadline::Never => None,
            Deadline::At(instant) if instant > now => Some(instant.since(now)),
            Deadline::At(_) => None,
        }
    }
}

impl Ord for Deadline {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Deadline::Never, Deadline::Never) => Ordering::Equal,
            (Deadline::Never, Deadline::At(_)) => Ordering::Greater,
            (Deadline::At(_), Deadline::Never) => Ordering::Less,
            (Deadline::At(a), Deadline::At(b)) => a.cmp(b),
        }
    }
}

impl PartialOrd for Deadline {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl fmt::Display for Deadline {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Deadline::Never => f.write_str("never"),
            Deadline::At(instant) => write!(f, "boottime+{}s", instant.as_nanos() / 1_000_000_000),
        }
    }
}

/// Folds the deadlines of the currently-true `[while]` blocks into a floor: the earliest
/// instant at which the action is permitted.
///
/// This is the "longest wins" half of the composition rule. See [`Deadline`] for why the
/// fold is seeded with [`None`] and not with [`Deadline::Never`].
///
/// An empty iterator yields [`Deadline::Never`]: no block said the machine may act, so it
/// may not.
#[must_use]
pub fn compose_floor<I: IntoIterator<Item = Deadline>>(deadlines: I) -> Deadline {
    let mut acc: Option<Deadline> = None;
    for deadline in deadlines {
        acc = Some(match acc {
            None => deadline,
            Some(current) => current.max(deadline),
        });
    }
    acc.unwrap_or(Deadline::Never)
}

/// Folds the deadlines of the currently-true `[when]` blocks into a ceiling: the latest
/// instant by which the action must happen.
///
/// There is one ceiling class and it is absolute. A ceiling is the only thing that can
/// beat a `never` floor -- and equally the only thing that can beat the implicit doubt
/// floor of [FACT-4] or the configuration-fault floor of [CFG-16], neither of which is a
/// block. That makes it the single override channel in the system, which is why every
/// ceiling is announced at load and logged again when it fires. `idlectl rest --force`
/// installs an ephemeral one, due now.
///
/// Because this is a minimum over instants, a ceiling can only ever pull an action
/// **earlier**: `MIN(floor, ceiling) <= floor` for every input, so no `[when]` block can
/// be used to keep a machine awake.
///
/// An empty iterator yields [`Deadline::Never`]: no ceiling, so the floor governs alone.
#[must_use]
pub fn compose_ceiling<I: IntoIterator<Item = Deadline>>(deadlines: I) -> Deadline {
    let mut acc: Option<Deadline> = None;
    for deadline in deadlines {
        acc = Some(match acc {
            None => deadline,
            Some(current) => current.min(deadline),
        });
    }
    acc.unwrap_or(Deadline::Never)
}

// ---------------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------------

/// Something the daemon can do to the machine.
///
/// The [`Ord`] implementation is by **depth**: `ScreenOff < Suspend < Hibernate <
/// PowerOff`. Depth is an ordering, **not** a precedence: when several sleep actions are
/// due at once the *shallowest* one is performed ([`Decision::shallowest_sleep_due`]),
/// because a deeper action is never a safe substitute for a shallower one. Suspending
/// loses nothing; powering off ends every session on the machine and, on the hardware
/// this policy was extracted from, hung roughly one time in four, leaving a box that only
/// a physical trip could revive.
///
/// The consequence is documented rather than discovered: **idlectl does not escalate.**
/// With `suspend = "30m"` and `poweroff = "8h"` both eventually due, the machine suspends
/// and never powers itself off. An operator who wants escalation sets `suspend = "never"`
/// and lets the deeper action own the schedule, delegates to logind's
/// suspend-then-hibernate, or asks for the deep action explicitly.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Action {
    /// Blank the display. Reversible by any input, costs nothing to get wrong.
    ScreenOff,
    /// Suspend to RAM (S3). Resumes in seconds; the default resting state.
    Suspend,
    /// Hibernate to disk (S4).
    Hibernate,
    /// Power the machine off.
    PowerOff,
}

impl Action {
    /// Every action, shallowest first.
    pub const ALL: [Action; 4] = [
        Action::ScreenOff,
        Action::Suspend,
        Action::Hibernate,
        Action::PowerOff,
    ];

    /// The name used in config files, on D-Bus and on the command line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Action::ScreenOff => "screen_off",
            Action::Suspend => "suspend",
            Action::Hibernate => "hibernate",
            Action::PowerOff => "poweroff",
        }
    }

    /// Parses a config/CLI/D-Bus action name.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Action::ALL.into_iter().find(|a| a.name() == name)
    }

    /// Whether this action changes the system's power state.
    ///
    /// True for [`Action::Suspend`], [`Action::Hibernate`] and [`Action::PowerOff`], and
    /// false for [`Action::ScreenOff`] -- blanking a panel is not a power-state change
    /// ([ACT-7]). At most one sleep action may be applied per evaluation; `screen_off`
    /// may always accompany it.
    #[must_use]
    pub const fn is_sleep(self) -> bool {
        !matches!(self, Action::ScreenOff)
    }

    /// Whether an [`FactState::Indeterminate`] fact vetoes this action.
    ///
    /// This is the per-action half of [FACT-4]: the veto is raised by a scan of the whole
    /// enabled fact set, once per evaluation, and lands on every action for which this
    /// returns true. [FACT-5] is the other half -- no implicit floor may ever apply to
    /// [`Action::ScreenOff`].
    ///
    /// True for everything except [`Action::ScreenOff`]. "Any doubt is a veto" is the
    /// right rule when the cost of being wrong is a machine that sleeps during a game or
    /// a nine-hour training run -- but it is the wrong rule for blanking a panel. If a
    /// single unreadable detector could pin the display on, one flaky sensor burns an
    /// OLED screen overnight. Screen-off is cheap to get wrong in both directions, so
    /// doubt does not veto it.
    #[must_use]
    pub const fn vetoed_by_doubt(self) -> bool {
        !matches!(self, Action::ScreenOff)
    }
}

impl fmt::Display for Action {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

// ---------------------------------------------------------------------------------
// Facts
// ---------------------------------------------------------------------------------

/// The state of a fact. Four states, and the difference between the last two matters.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum FactState {
    /// The condition holds.
    True,
    /// The condition does not hold.
    False,
    /// The detector exists but could not produce an answer: a read failed, a timeout
    /// expired, a parse failed, a helper crashed.
    ///
    /// Contributes [`Deadline::Never`] to every action for which
    /// [`Action::vetoed_by_doubt`] is true. This is the "any doubt is a veto" rule.
    Indeterminate,
    /// The capability does not exist on this system: no MPRIS player has ever appeared,
    /// Steam is not installed, there is no discrete GPU.
    ///
    /// Behaves exactly like [`FactState::False`] for decision purposes, and is a separate
    /// state only so `doctor` and `explain` can say "not applicable here" instead of
    /// "false". A missing capability must never wedge a machine awake; if it did, every
    /// server without a sound card would refuse to suspend.
    Unavailable,
}

impl FactState {
    /// Whether this state counts as the condition holding.
    ///
    /// [`FactState::Indeterminate`] answers `false` here: doubt does not make a condition
    /// true, it is handled separately and more conservatively by [`resolve`].
    #[must_use]
    pub const fn is_true(self) -> bool {
        matches!(self, FactState::True)
    }

    /// Whether this state is a definite negative.
    ///
    /// [`FactState::Unavailable`] counts; [`FactState::Indeterminate`] does not.
    #[must_use]
    pub const fn is_definitely_false(self) -> bool {
        matches!(self, FactState::False | FactState::Unavailable)
    }

    /// The name used on D-Bus and in `idlectl explain`.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            FactState::True => "true",
            FactState::False => "false",
            FactState::Indeterminate => "indeterminate",
            FactState::Unavailable => "unavailable",
        }
    }
}

impl fmt::Display for FactState {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A fact the daemon can observe.
///
/// This is a closed enum rather than an open string namespace on purpose. A `[while]`
/// block naming a fact that does not exist is a typo, and a typo that is silently ignored
/// produces a config that looks like it protects a running game and does not. Unknown
/// names are a hard config error, raised by `idlectl-config` at load time.
///
/// Marked `#[non_exhaustive]` so new detectors can ship without a semver break.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum FactId {
    /// A human has touched an input device within `min_idle` (default five minutes).
    ///
    /// This is the hard floor, and it is a first-class fact rather than merely the clock
    /// that timeouts are counted on. If human presence existed only as a clock origin,
    /// nothing would stop a remote request or an override from acting while somebody is
    /// typing -- the request would simply arrive and be honoured.
    ///
    /// Two edge cases, both deliberate:
    ///
    /// * **Never touched this boot** -- [`FactState::False`], not
    ///   [`FactState::Indeterminate`]. A machine that booted by wake-on-LAN and has never
    ///   seen a keypress has demonstrably no human at it, and this preserves the fast
    ///   path where a relay wakes a machine, uses it and lets it rest again.
    /// * **Clock unreadable** -- the session agent is not running, its heartbeat is
    ///   stale, or the idle protocol errored: [`FactState::Indeterminate`]
    ///   ([HUM-4]). Deliberately **not** `True`: the detector did not observe a human,
    ///   and reporting an observation that was not made collapses doubt into knowledge.
    ///   The veto arrives instead through [FACT-4], which is block-independent and
    ///   therefore raises suspend, hibernate and poweroff to `+infinity` with exactly the
    ///   same force as a `never` floor -- while, unlike `True`, it cannot make a
    ///   `[when.human_active]` ceiling fire on a machine whose agent has died. The three
    ///   states are also what makes "this machine is awake forever because its agent
    ///   died" diagnosable from the fact table alone.
    HumanActive,

    /// At least one resume from suspend or hibernate has happened this boot.
    ///
    /// The companion to [`Clock::Resume`]. A block conditioned on this and measured on
    /// the resume clock is the settle window: it constrains the first few minutes after a
    /// wake and then expires harmlessly, because its deadline recedes into the past.
    AfterResume,

    /// A remote login session is open (SSH or any other non-seat logind session).
    RemoteSession,

    /// At least one unexpired lease is held.
    ///
    /// A lease is how a long job says "I am working, do not sleep". It carries a TTL so a
    /// job that crashes cannot pin the machine awake forever.
    LeaseHeld,

    /// A logind inhibitor lock of mode `block` is held against sleep or shutdown.
    ///
    /// Read as a **signal**, never trusted as enforcement, for two measured reasons.
    /// First, `systemctl poweroff` invoked without a controlling terminal returns 0 and
    /// ignores inhibitors and sessions entirely, so no guarantee can be built on them.
    /// Second, Steam declares no inhibitor at all while downloading -- with a 150 GB
    /// download in flight the only locks present were NetworkManager's and UPower's, both
    /// mode `delay`, plus a power-key handler. An inhibitor-based design cannot see a
    /// download, which is why this project has detectors instead.
    ///
    /// Mode `delay` locks are deliberately not reported here: NetworkManager and UPower
    /// hold them permanently, so honouring them would mean never sleeping.
    InhibitorBlock,

    /// An MPRIS player reports `PlaybackStatus = Playing`.
    MediaPlaying,

    /// A Steam application is running.
    SteamGameRunning,

    /// Steam is downloading or verifying content.
    ///
    /// Deliberately separate from [`FactId::SteamGameRunning`], because the two want
    /// different answers per action: a download must not be interrupted by a suspend, but
    /// there is no reason to keep a panel lit for it.
    SteamDownloading,

    /// The GPU is busy and the load is attributable to a running game.
    GpuBusyGame,

    /// The GPU is busy but the load is not attributable to a game.
    ///
    /// Split from [`FactId::GpuBusyGame`] because the two deserve different policy. A
    /// compositor keeping a GPU at 8% is not a reason to stay awake; a game at 90% is.
    GpuBusyOther,

    /// A long-running local service has actually served something recently.
    ///
    /// Note the wording: **busy**, not **running**. "The service is up" is not "the
    /// service is in use". A local model server sitting idle overnight while holding 12 GB
    /// of VRAM is a process that exists, not a machine being used, and treating its mere
    /// presence as activity means the machine never sleeps again. The detector behind this
    /// fact reads cumulative request counters and reports true only if they moved inside
    /// the idle window.
    LocalServiceBusy,
}

impl FactId {
    /// Every fact, in declaration order. Used to build the "valid names are ..." message
    /// when a config file names something that does not exist.
    pub const ALL: [FactId; 11] = [
        FactId::HumanActive,
        FactId::AfterResume,
        FactId::RemoteSession,
        FactId::LeaseHeld,
        FactId::InhibitorBlock,
        FactId::MediaPlaying,
        FactId::SteamGameRunning,
        FactId::SteamDownloading,
        FactId::GpuBusyGame,
        FactId::GpuBusyOther,
        FactId::LocalServiceBusy,
    ];

    /// The name used in config files, on D-Bus and on the command line.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            FactId::HumanActive => "human_active",
            FactId::AfterResume => "after_resume",
            FactId::RemoteSession => "remote_session",
            FactId::LeaseHeld => "lease_held",
            FactId::InhibitorBlock => "inhibitor_block",
            FactId::MediaPlaying => "media_playing",
            FactId::SteamGameRunning => "steam_game_running",
            FactId::SteamDownloading => "steam_downloading",
            FactId::GpuBusyGame => "gpu_busy_game",
            FactId::GpuBusyOther => "gpu_busy_other",
            FactId::LocalServiceBusy => "local_service_busy",
        }
    }

    /// Parses a fact name. Returns [`None`] for anything unknown; the caller is expected
    /// to turn that into a loud error rather than a silently-dead block.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        FactId::ALL.into_iter().find(|f| f.name() == name)
    }
}

impl fmt::Display for FactId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What the machine currently looks like.
///
/// # Contract for detectors
///
/// A fact that is **absent** from the set reads as [`FactState::Unavailable`], which
/// behaves as false. That is correct for a capability the machine does not have, and
/// dangerous for a detector that merely broke: a detector whose helper crashed must
/// publish [`FactState::Indeterminate`], not stop publishing. Removing a key here says
/// "this machine cannot have a game running"; publishing `Indeterminate` says "there might
/// be one and I cannot tell". Only the second one vetoes.
#[derive(Clone, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(transparent))]
pub struct FactSet {
    readings: BTreeMap<FactId, FactState>,
}

impl FactSet {
    /// An empty set: every fact reads [`FactState::Unavailable`].
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Records a reading, replacing any previous one.
    pub fn set(&mut self, fact: FactId, state: FactState) {
        self.readings.insert(fact, state);
    }

    /// Builder form of [`FactSet::set`], for tests and for assembling a set inline.
    #[must_use]
    pub fn with(mut self, fact: FactId, state: FactState) -> Self {
        self.set(fact, state);
        self
    }

    /// Reads a fact. Absent facts are [`FactState::Unavailable`] -- see the type-level
    /// note on why that is not the same as a broken detector.
    #[must_use]
    pub fn get(&self, fact: FactId) -> FactState {
        self.readings
            .get(&fact)
            .copied()
            .unwrap_or(FactState::Unavailable)
    }

    /// Iterates over the recorded readings, in [`FactId`] order.
    pub fn iter(&self) -> impl Iterator<Item = (FactId, FactState)> + '_ {
        self.readings.iter().map(|(id, state)| (*id, *state))
    }
}

// ---------------------------------------------------------------------------------
// Clocks
// ---------------------------------------------------------------------------------

/// The origin a block's timeouts are counted from.
///
/// Every clock is on `CLOCK_BOOTTIME`. A block declares exactly one; that declaration is
/// what makes instant-based composition necessary in the first place.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum Clock {
    /// The last human input: key, pointer, touch, gamepad, or an idle-notification reset
    /// from the compositor. The classic idle timer, and the default for a block that does
    /// not say otherwise.
    HumanInput,
    /// The last resume from suspend or hibernate.
    Resume,
    /// The last false-to-true edge of this block's own condition. "Ten minutes after the
    /// download started", not "ten minutes after the last keypress".
    ConditionEdge,
    /// Boot. Rarely what anyone wants; present because a machine with no human input and
    /// no resume still needs a defined origin.
    Boot,
}

impl Clock {
    /// Every clock, in declaration order.
    pub const ALL: [Clock; 4] = [
        Clock::HumanInput,
        Clock::Resume,
        Clock::ConditionEdge,
        Clock::Boot,
    ];

    /// The name used in config files (`clock = "..."`).
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Clock::HumanInput => "human_input",
            Clock::Resume => "resume",
            Clock::ConditionEdge => "condition",
            Clock::Boot => "boot",
        }
    }

    /// Parses a `clock = "..."` value.
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        Clock::ALL.into_iter().find(|c| c.name() == name)
    }
}

impl fmt::Display for Clock {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// Where one clock's origin stands.
///
/// Three states, and the third is the whole reason this type exists. A bare
/// [`Option<BootInstant>`] once meant both "this has not happened yet" and "the detector
/// that would tell me is broken", and [CLK-12] requires those two to be distinguishable:
/// the first is normal, the second is a fault that `doctor` must report and exit non-zero
/// for.
///
/// They also do not compose alike. A never-touched human-input clock falls back to the
/// boot instant ([CLK-7]) precisely so that a machine woken by a remote relay -- which by
/// definition nobody has touched -- can still reach its base deadline and go back to
/// sleep. An **unreadable** one contributes `+infinity` to the three sleep actions
/// ([CLK-11]). Collapsing the two would silently hand the remote-relay fast path to every
/// machine whose input agent had died.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ClockOrigin {
    /// The instant the event last happened.
    At(BootInstant),
    /// The event has not happened this boot, and the path that would report it is healthy.
    NotYet,
    /// The detector feeding this clock is erroring, stale or absent -- [CLK-11].
    Unreadable,
}

impl ClockOrigin {
    /// The instant, if one is recorded. [`ClockOrigin::NotYet`] and
    /// [`ClockOrigin::Unreadable`] both yield [`None`]; use the variant itself when the
    /// difference matters, which is everywhere it is reported to a human.
    #[must_use]
    pub const fn instant(self) -> Option<BootInstant> {
        match self {
            ClockOrigin::At(t) => Some(t),
            ClockOrigin::NotYet | ClockOrigin::Unreadable => None,
        }
    }

    /// Whether this origin is a fault rather than a normal absence ([CLK-12]).
    #[must_use]
    pub const fn is_unreadable(self) -> bool {
        matches!(self, ClockOrigin::Unreadable)
    }
}

impl fmt::Display for ClockOrigin {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ClockOrigin::At(t) => write!(f, "+{}s since boot", t.as_nanos() / 1_000_000_000),
            ClockOrigin::NotYet => f.write_str("not yet this boot"),
            ClockOrigin::Unreadable => f.write_str("unreadable"),
        }
    }
}

/// The origins available at one evaluation, plus the instant the evaluation happens at.
///
/// Read once per evaluation and passed in whole, so the whole decision is computed against
/// a single consistent view. Reading `now` twice inside one evaluation is how a deadline
/// ends up being compared against a later instant than the one that produced it.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ClockSnapshot {
    /// The instant this evaluation is happening at.
    pub now: BootInstant,
    /// The last human input.
    pub human_input: ClockOrigin,
    /// The last resume from suspend or hibernate.
    pub resume: ClockOrigin,
}

impl ClockSnapshot {
    /// A snapshot at `now` with no human input and no resume recorded, both healthy.
    #[must_use]
    pub fn at(now: BootInstant) -> Self {
        ClockSnapshot {
            now,
            human_input: ClockOrigin::NotYet,
            resume: ClockOrigin::NotYet,
        }
    }

    /// The raw state of one clock's origin, for [CLK-12] reporting.
    ///
    /// [`Clock::Boot`] is always known; [`Clock::ConditionEdge`] is known whenever an edge
    /// was recorded and otherwise reads as "now", which [`ClockSnapshot::origin`]
    /// documents.
    #[must_use]
    pub fn origin_kind(&self, clock: Clock, condition_since: Option<BootInstant>) -> ClockOrigin {
        match clock {
            Clock::HumanInput => self.human_input,
            Clock::Resume => self.resume,
            Clock::ConditionEdge => ClockOrigin::At(condition_since.unwrap_or(self.now)),
            Clock::Boot => ClockOrigin::At(BootInstant::BOOT),
        }
    }

    /// Resolves a clock to its origin instant, or [`None`] when the origin is **unknown**.
    ///
    /// The fallbacks are chosen so that a missing origin never causes an action to fire
    /// earlier than it should:
    ///
    /// * [`Clock::HumanInput`] with nothing recorded falls back to [`BootInstant::BOOT`].
    ///   A machine that has never been touched has been idle since boot, and pretending
    ///   otherwise would keep every headless machine awake forever. This is [CLK-7] and it
    ///   applies **only** to [`ClockOrigin::NotYet`]. [`ClockOrigin::Unreadable`] is
    ///   [`None`]: an agent that has died must not be able to hand the machine the
    ///   remote-relay fast path.
    /// * [`Clock::Resume`] with nothing recorded is **unknown**: [`None`]. There is no
    ///   safe numeric fallback. Falling back to [`BootInstant::BOOT`] would make
    ///   `[while.always] clock = "resume"` -- which the parser accepts -- fire
    ///   immediately on a machine that has never slept. The old justification ("in
    ///   practice such a block is also conditioned on [`FactId::AfterResume`]") was a
    ///   coincidence that nothing enforced. [`resolve`] applies [CLK-11] to the [`None`]:
    ///   `+infinity` for the three sleep actions, and the boot origin for
    ///   [`Action::ScreenOff`], which no implicit infinity may ever reach.
    /// * [`Clock::ConditionEdge`] with nothing recorded falls back to `now`, i.e. "it just
    ///   became true". That is the *later* of the plausible answers, so it errs towards
    ///   staying awake. It should not happen: [`BlockEdges::observe`] records the edge
    ///   before [`resolve`] reads it.
    #[must_use]
    pub fn origin(
        &self,
        clock: Clock,
        condition_since: Option<BootInstant>,
    ) -> Option<BootInstant> {
        match clock {
            Clock::HumanInput => match self.human_input {
                ClockOrigin::At(t) => Some(t),
                ClockOrigin::NotYet => Some(BootInstant::BOOT),
                ClockOrigin::Unreadable => None,
            },
            Clock::Resume => self.resume.instant(),
            Clock::ConditionEdge => Some(condition_since.unwrap_or(self.now)),
            Clock::Boot => Some(BootInstant::BOOT),
        }
    }
}

// ---------------------------------------------------------------------------------
// Policy
// ---------------------------------------------------------------------------------

/// Whether a block sets a floor or a ceiling.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum BlockKind {
    /// `[while <condition>]`: while the condition holds, do not act before the deadline.
    /// Contributes to the maximum. This is what a user writes.
    While,
    /// `[when <condition>]`: when the condition holds, act by the deadline at the latest.
    /// Contributes to the minimum, and is therefore the only thing that can beat a `never`
    /// floor. Reserved for explicit, logged overrides.
    When,
}

impl BlockKind {
    /// The table prefix used in config files.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            BlockKind::While => "while",
            BlockKind::When => "when",
        }
    }
}

impl fmt::Display for BlockKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// What makes a block apply.
///
/// v1 keeps this deliberately small: a block is conditioned on exactly one fact, or on
/// nothing at all. Boolean combinators are not offered, because two blocks on the same
/// action compose correctly by construction -- `and` is what the maximum already does --
/// and because an expression language would need its own parser, its own error messages
/// and its own three-state truth table before it earned anything.
///
/// `#[non_exhaustive]` keeps the door open.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
#[non_exhaustive]
pub enum Condition {
    /// Always true. The base schedule.
    Always,
    /// True when the named fact is true.
    Fact(
        /// The fact to read.
        FactId,
    ),
}

impl Condition {
    /// The name used as the table key in a config file.
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Condition::Always => "always",
            Condition::Fact(fact) => fact.name(),
        }
    }

    /// Parses a table key. `always` maps to [`Condition::Always`]; anything else must be a
    /// known [`FactId`].
    #[must_use]
    pub fn from_name(name: &str) -> Option<Self> {
        if name == "always" {
            return Some(Condition::Always);
        }
        FactId::from_name(name).map(Condition::Fact)
    }

    /// Evaluates the condition against a fact set.
    #[must_use]
    pub fn evaluate(self, facts: &FactSet) -> FactState {
        match self {
            Condition::Always => FactState::True,
            Condition::Fact(fact) => facts.get(fact),
        }
    }
}

impl fmt::Display for Condition {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// A block's identity: its kind and its condition, which together are its table path.
///
/// Copy and allocation-free, so it can be a `BTreeMap` key in [`BlockEdges`] and be
/// carried around in [`Reason`] without cloning strings on every evaluation.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct BlockId {
    /// `while` or `when`.
    pub kind: BlockKind,
    /// The condition, which is also the table key.
    pub condition: Condition,
}

impl BlockId {
    /// Builds an id.
    #[must_use]
    pub const fn new(kind: BlockKind, condition: Condition) -> Self {
        BlockId { kind, condition }
    }
}

impl fmt::Display for BlockId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}", self.kind.name(), self.condition.name())
    }
}

/// A timeout as written in a config file.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum Timeout {
    /// Act this long after the block's clock origin.
    After(
        /// The delay.
        Duration,
    ),
    /// Never act while this block is true.
    Never,
}

/// The per-action timeouts of one block.
///
/// [`None`] means the block says nothing about that action, and contributes nothing to
/// its fold -- to the floor and to the ceiling alike, whatever the condition's state
/// ([COMP-2b]). **Silence is not `never`.** This is what makes per-action policy work: a
/// block can veto suspend without having an opinion about blanking the screen. Reading an
/// unset key as `never` instead would let a download, or a dead idle agent, pin a lit
/// image on an OLED panel for hours.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Timeouts {
    /// Timeout for [`Action::ScreenOff`].
    pub screen_off: Option<Timeout>,
    /// Timeout for [`Action::Suspend`].
    pub suspend: Option<Timeout>,
    /// Timeout for [`Action::Hibernate`].
    pub hibernate: Option<Timeout>,
    /// Timeout for [`Action::PowerOff`].
    pub poweroff: Option<Timeout>,
}

impl Timeouts {
    /// The timeout for one action, or [`None`] if the block is silent about it.
    #[must_use]
    pub const fn get(&self, action: Action) -> Option<Timeout> {
        match action {
            Action::ScreenOff => self.screen_off,
            Action::Suspend => self.suspend,
            Action::Hibernate => self.hibernate,
            Action::PowerOff => self.poweroff,
        }
    }

    /// Sets the timeout for one action.
    pub fn set(&mut self, action: Action, timeout: Option<Timeout>) {
        match action {
            Action::ScreenOff => self.screen_off = timeout,
            Action::Suspend => self.suspend = timeout,
            Action::Hibernate => self.hibernate = timeout,
            Action::PowerOff => self.poweroff = timeout,
        }
    }

    /// Whether the block is silent about every action.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.screen_off.is_none()
            && self.suspend.is_none()
            && self.hibernate.is_none()
            && self.poweroff.is_none()
    }
}

/// One `[while ...]` or `[when ...]` block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Block {
    /// Kind and condition.
    pub id: BlockId,
    /// The origin this block's timeouts are counted from.
    pub clock: Clock,
    /// Whether the block participates. A disabled block is kept in the [`Policy`] so
    /// `idlectl explain` can say "this exists and is switched off" rather than staying
    /// silent about it.
    pub enabled: bool,
    /// The per-action timeouts.
    pub timeouts: Timeouts,
}

/// Per-fact configuration, from `[facts.<name>]`.
///
/// The only key that changes the composition is [`FactSettings::enabled`]; the rest are
/// detector settings, carried here so that one validated object describes the whole
/// policy and `explain` can show what a detector was told.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct FactSettings {
    /// Whether the fact participates at all.
    ///
    /// A disabled fact reports [`FactState::Unavailable`], its detector MUST NOT run, and
    /// it contributes no doubt veto ([FACT-6], [CFG-12]). This is the supported,
    /// auditable escape hatch from a permanently-indeterminate detector, and `doctor` must
    /// list every fact switched off this way -- an escape hatch nobody can see is a
    /// silent policy change.
    pub enabled: bool,
    /// `counters_url` for [`FactId::LocalServiceBusy`]: the cumulative-counter endpoint
    /// the detector samples between evaluations.
    pub counters_url: Option<String>,
    /// `idle_window` for [`FactId::LocalServiceBusy`].
    pub idle_window: Option<Duration>,
    /// `steam_root` for [`FactId::SteamGameRunning`] and [`FactId::SteamDownloading`].
    pub steam_root: Option<String>,
    /// `window` for [`FactId::SteamDownloading`]: how recent a staging-tree modification
    /// must be.
    pub window: Option<Duration>,
}

impl FactSettings {
    /// The shipped defaults for one fact.
    ///
    /// Everything is enabled except [`FactId::LocalServiceBusy`], which ships **disabled**
    /// and becomes enabled only once `[facts.local_service_busy] counters_url` is set. Its
    /// counter endpoint is off by default on the service it was written for, and because
    /// the doubt veto of [FACT-4] is block-independent, a shipped-enabled unreadable
    /// detector would freeze every machine on install.
    #[must_use]
    pub fn default_for(fact: FactId) -> Self {
        FactSettings {
            enabled: !matches!(fact, FactId::LocalServiceBusy),
            counters_url: None,
            idle_window: None,
            steam_root: None,
            window: None,
        }
    }
}

/// A fault in the effective configuration, recorded rather than fatal.
///
/// A file that fails to parse or carries a bad value is dropped as a whole and recorded
/// here ([CFG-10], [CFG-16]); the remaining layers stay in force and evaluation continues.
/// While any fault is present, [`resolve`] adds an implicit `+infinity` floor to suspend,
/// hibernate and poweroff -- and **not** to [`Action::ScreenOff`] -- and `doctor` must
/// exit non-zero naming the key, the file and the offending text.
///
/// The requirement comes from a measured outage: a single unparseable value once killed a
/// decider before it had evaluated anything at all. Refusing to run is not the safe
/// direction; refusing to *sleep* is.
#[derive(Clone, Debug, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ConfigFault {
    /// The file the fault came from.
    pub source: String,
    /// The key path, such as `while.always.suspend`. Empty for whole-file faults.
    pub location: String,
    /// A rendered, human-readable description.
    pub message: String,
}

/// A validated policy.
///
/// Built by `idlectl-config`, never deserialized directly: a policy that reached this type
/// has had every condition name, clock name and duration checked.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Policy {
    /// The blocks, in no significant order. Order is not allowed to matter; the tests
    /// assert it.
    pub blocks: Vec<Block>,
    /// How recent human input must be for [`FactId::HumanActive`] to be true.
    ///
    /// Lives in the policy rather than in the detector so that `explain` can show the
    /// threshold that produced the answer.
    pub min_idle: Duration,
    /// Per-fact configuration. Every [`FactId`] has an entry; missing entries read as
    /// [`FactSettings::default_for`].
    pub facts: BTreeMap<FactId, FactSettings>,
    /// Faults recorded while loading. Non-empty means the machine is running a degraded
    /// configuration and must not sleep ([CFG-16]).
    pub faults: Vec<ConfigFault>,
}

impl Policy {
    /// The default `min_idle`: five minutes.
    pub const DEFAULT_MIN_IDLE: Duration = Duration::from_secs(300);

    /// An empty policy. Every action resolves to [`Deadline::Never`]: with no blocks, the
    /// maximum over the empty set applies and the machine does nothing at all.
    #[must_use]
    pub fn empty() -> Self {
        Policy {
            blocks: Vec::new(),
            min_idle: Policy::DEFAULT_MIN_IDLE,
            facts: FactId::ALL
                .into_iter()
                .map(|fact| (fact, FactSettings::default_for(fact)))
                .collect(),
            faults: Vec::new(),
        }
    }

    /// The enabled blocks of one kind.
    pub fn blocks_of(&self, kind: BlockKind) -> impl Iterator<Item = &Block> + '_ {
        self.blocks
            .iter()
            .filter(move |b| b.enabled && b.id.kind == kind)
    }

    /// Looks a block up by id, enabled or not.
    #[must_use]
    pub fn block(&self, id: BlockId) -> Option<&Block> {
        self.blocks.iter().find(|b| b.id == id)
    }

    /// The settings for one fact, falling back to the shipped defaults.
    #[must_use]
    pub fn fact_settings(&self, fact: FactId) -> FactSettings {
        self.facts
            .get(&fact)
            .cloned()
            .unwrap_or_else(|| FactSettings::default_for(fact))
    }

    /// Whether a fact participates: whether its detector runs, and whether doubt about it
    /// can veto.
    #[must_use]
    pub fn fact_enabled(&self, fact: FactId) -> bool {
        self.facts
            .get(&fact)
            .map_or_else(|| FactSettings::default_for(fact).enabled, |s| s.enabled)
    }

    /// Every enabled fact that currently reads [`FactState::Indeterminate`], in
    /// [`FactId`] order.
    ///
    /// This is the [FACT-4] scan, and it deliberately does **not** look at the blocks: the
    /// rule is about the machine no longer being able to see something it normally sees,
    /// not about which detectors an operator happened to write a block for. A disabled
    /// fact never appears here, whatever its detector last published.
    #[must_use]
    pub fn doubtful_facts(&self, facts: &FactSet) -> Vec<FactId> {
        FactId::ALL
            .into_iter()
            .filter(|fact| self.fact_enabled(*fact) && facts.get(*fact) == FactState::Indeterminate)
            .collect()
    }
}

// ---------------------------------------------------------------------------------
// Condition edges
// ---------------------------------------------------------------------------------

/// When each block's condition last became true.
///
/// The engine is stateless and therefore cannot observe an edge -- an edge is a difference
/// between two evaluations. The caller owns this map, calls [`BlockEdges::observe`] once
/// per evaluation before [`resolve`], and keeps it across evaluations.
///
/// It must survive a daemon reload but not a reboot, which is exactly what
/// `CLOCK_BOOTTIME` instants give for free.
///
/// # Edges survive a suspend
///
/// There is deliberately **no** `clear()`, and nothing may reset these edges on resume.
/// An implementation must not reset the human-input clock "or any other clock" at a
/// resume ([COMP-9]), and the condition clock is defined as the instant of the most recent
/// FALSE-to-TRUE edge -- a suspend is not such an edge. Clearing on resume looks
/// conservative and is not: with `[while.gpu_busy_other] clock = "condition", suspend =
/// "20m"`, a machine woken periodically by a relay would re-arm a fresh twenty minutes
/// after every wake and never reach that deadline at all. That is an unbounded,
/// undocumented, invisible re-arm -- the exact bug class this project was extracted to
/// remove. Only a definite FALSE (or UNAVAILABLE) clears an edge, and
/// [`FactState::Indeterminate`] never does.
#[derive(Clone, Debug, Default)]
pub struct BlockEdges {
    since: BTreeMap<BlockId, BootInstant>,
}

impl BlockEdges {
    /// An empty map.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Updates the recorded edges for `now`.
    ///
    /// A block that has just become true gets `now` recorded. A block that is *definitely*
    /// false ([`FactState::False`] or [`FactState::Unavailable`]) has its edge cleared.
    ///
    /// [`FactState::Indeterminate`] deliberately does **not** clear the edge. A detector
    /// that fails to read for one cycle must not restart a timer that has been running for
    /// half an hour; if it did, a flaky detector would keep resetting the clock and the
    /// machine would never reach any deadline at all.
    pub fn observe(&mut self, policy: &Policy, facts: &FactSet, now: BootInstant) {
        for block in &policy.blocks {
            let state = block.id.condition.evaluate(facts);
            if state.is_true() {
                self.since.entry(block.id).or_insert(now);
            } else if state.is_definitely_false() {
                self.since.remove(&block.id);
            }
        }
    }

    /// When the block's condition last became true, if it is currently true.
    #[must_use]
    pub fn since(&self, id: BlockId) -> Option<BootInstant> {
        self.since.get(&id).copied()
    }
}

// ---------------------------------------------------------------------------------
// Decision
// ---------------------------------------------------------------------------------

/// Why one block contributed what it did. The raw material for `idlectl explain`.
#[derive(Clone, Copy, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Reason {
    /// The block.
    pub block: BlockId,
    /// The state its condition evaluated to.
    pub state: FactState,
    /// The clock its timeout was counted from.
    pub clock: Clock,
    /// The origin instant that clock resolved to, or [`None`] when the origin is unknown
    /// -- which today means [`Clock::Resume`] on a machine that has not slept this boot.
    /// See [`ClockSnapshot::origin`] and [CLK-11].
    pub origin: Option<BootInstant>,
    /// The deadline it contributed.
    pub deadline: Deadline,
    /// Whether this block contributed `now` because the request satisfied it ([REQ-3],
    /// `rest --now`) rather than because of its configured timeout.
    pub satisfied_by_request: bool,
}

/// A floor that no block wrote, added by the engine itself.
///
/// Implicit floors are `+infinity` on suspend, hibernate and poweroff, and never apply to
/// [`Action::ScreenOff`] ([FACT-5], [FACT-40]). They are **not** blocks: `rest --now`
/// cannot satisfy them ([REQ-3]) and only a ceiling defeats them ([CEIL-4]). `explain` and
/// `doctor` must name every one of them as a reason the machine is awake.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
#[cfg_attr(feature = "serde", serde(rename_all = "snake_case"))]
pub enum ImplicitFloor {
    /// [FACT-4]: this enabled fact reads [`FactState::Indeterminate`].
    Doubt(
        /// The fact that could not be read.
        FactId,
    ),
    /// [CFG-16]: the effective configuration carries a fault, identified by its index in
    /// [`Policy::faults`].
    ConfigFault(
        /// Index into [`Policy::faults`].
        usize,
    ),
}

/// The resolution of one action.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct ActionResolution {
    /// The action.
    pub action: Action,
    /// The maximum over the true `[while]` blocks **and** the implicit floors: the
    /// earliest instant permitted.
    pub floor: Deadline,
    /// The minimum over the true `[when]` blocks and the ephemeral `--force` ceiling: the
    /// latest instant tolerated.
    pub ceiling: Deadline,
    /// `min(floor, ceiling)`. The instant the action may fire.
    pub deadline: Deadline,
    /// Whether `now` has reached [`ActionResolution::deadline`].
    pub due: bool,
    /// Every block that contributed, in policy order.
    pub reasons: Vec<Reason>,
    /// Every implicit floor in force for this action.
    pub implicit_floors: Vec<ImplicitFloor>,
}

impl ActionResolution {
    /// The blocks currently holding the floor, i.e. the ones a user would name when asked
    /// "why is this machine still awake?".
    ///
    /// Returns every block whose contributed deadline equals the floor, not just one: two
    /// blocks both saying `never` are both the answer, and reporting one of them would
    /// send somebody chasing the wrong detector. When the floor is held only by an
    /// implicit floor this is empty and [`ActionResolution::implicit_floors`] is the
    /// answer instead.
    #[must_use]
    pub fn holding(&self) -> Vec<BlockId> {
        self.reasons
            .iter()
            .filter(|r| r.block.kind == BlockKind::While && r.deadline == self.floor)
            .map(|r| r.block)
            .collect()
    }

    /// Whether a ceiling pulled this action earlier than the floor allowed.
    ///
    /// When this is true the event must be logged at warning level naming the ceiling, the
    /// file it came from and every floor it defeated with that floor's reason ([CEIL-6],
    /// [CEIL-7], [CEIL-8], [REQ-11]). A ceiling is the only construct in the system that
    /// can act against a floor, so it is never allowed to fire quietly.
    #[must_use]
    pub fn ceiling_defeated_floor(&self) -> bool {
        self.ceiling < self.floor
    }

    /// The `[when]` blocks that set the ceiling, for the log record [CEIL-8] requires.
    #[must_use]
    pub fn ceiling_blocks(&self) -> Vec<BlockId> {
        self.reasons
            .iter()
            .filter(|r| r.block.kind == BlockKind::When && r.deadline == self.ceiling)
            .map(|r| r.block)
            .collect()
    }
}

/// The result of one evaluation.
#[derive(Clone, Debug)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Decision {
    /// The instant this decision was computed for.
    pub now: BootInstant,
    /// Resolution for [`Action::ScreenOff`].
    pub screen_off: ActionResolution,
    /// Resolution for [`Action::Suspend`].
    pub suspend: ActionResolution,
    /// Resolution for [`Action::Hibernate`].
    pub hibernate: ActionResolution,
    /// Resolution for [`Action::PowerOff`].
    pub poweroff: ActionResolution,
    /// Every enabled fact that read [`FactState::Indeterminate`] at this evaluation: the
    /// [FACT-4] scan, kept so `explain` and `doctor` can name each one as a reason for the
    /// implicit floor. `doctor` exits non-zero when this is non-empty ([OBS-4]).
    pub doubtful_facts: Vec<FactId>,
    /// How many configuration faults were in force ([CFG-16]). Non-zero means the machine
    /// is running a degraded configuration and the three sleep actions are floored at
    /// `+infinity`.
    pub config_faults: usize,
    /// The action a caller asked for by name, carried through from [`Request::requested`]
    /// so that [`Decision::action_to_perform`] can answer without the request in hand.
    pub requested: Option<Action>,
}

impl Decision {
    /// The resolution for one action.
    #[must_use]
    pub fn get(&self, action: Action) -> &ActionResolution {
        match action {
            Action::ScreenOff => &self.screen_off,
            Action::Suspend => &self.suspend,
            Action::Hibernate => &self.hibernate,
            Action::PowerOff => &self.poweroff,
        }
    }

    /// Every due action, shallowest first.
    #[must_use]
    pub fn due_actions(&self) -> Vec<Action> {
        Action::ALL
            .into_iter()
            .filter(|a| self.get(*a).due)
            .collect()
    }

    /// The **shallowest** due sleep action, or [`None`] if no sleep action is due.
    ///
    /// This is [ACT-7], and the direction is the whole point. At most one of
    /// [`Action::Suspend`], [`Action::Hibernate`] and [`Action::PowerOff`] may be applied
    /// per evaluation, and it must be the shallowest one that is due: a deeper action is
    /// never a safe substitute for a shallower one. Suspend loses nothing; poweroff ends
    /// every session on the machine and, on the hardware this policy was extracted from,
    /// hung roughly one time in four.
    ///
    /// [`Action::ScreenOff`] is not returned here and is not suppressed by anything this
    /// returns: it is not a power-state change, so the caller applies it independently
    /// (see [`Decision::due_actions`]). An action already in effect must not be re-issued
    /// until its own deadline has been re-armed by a new origin -- a machine whose panel is
    /// already blank does not re-fire `screen_off`, and therefore never starves the sleep
    /// actions. That last piece is the caller's, because it is state and this crate is
    /// pure.
    #[must_use]
    pub fn shallowest_sleep_due(&self) -> Option<Action> {
        Action::ALL
            .into_iter()
            .find(|a| a.is_sleep() && self.get(*a).due)
    }

    /// The sleep action this evaluation should actually carry out, if any.
    ///
    /// Two rules, and which one applies depends on whether anybody asked:
    ///
    /// * **Nobody asked** -- the schedule is running on its own. The shallowest due action
    ///   wins ([ACT-7]): a deeper action is never a safe substitute for a shallower one,
    ///   because suspend loses nothing and poweroff ends every session on the machine.
    /// * **Somebody asked for one by name** -- that action, and only that action. If it is
    ///   held, nothing is performed.
    ///
    /// The second rule is not a special case bolted onto the first; it is what makes
    /// [ACT-7b] true. `rest --now` collapses the base schedule for *every* action it names,
    /// so `suspend` is due at the same instant as the `poweroff` that was asked for, and
    /// the shallowest-wins rule would quietly substitute it. The caller would be told its
    /// request was not satisfied while the machine went to sleep anyway -- a relay would
    /// record the machine as off, and it would be sitting in S3.
    ///
    /// Refusing rather than downgrading is the same principle read in the other direction:
    /// if the deep action a caller named is held, the answer is "no", not "here is a
    /// shallower one you did not ask for".
    #[must_use]
    pub fn action_to_perform(&self) -> Option<Action> {
        match self.requested {
            Some(action) if action.is_sleep() => self.get(action).due.then_some(action),
            // A request for `screen_off` is not a sleep request and never performs one.
            Some(_) => None,
            None => self.shallowest_sleep_due(),
        }
    }

    /// The earliest deadline still in the future, i.e. when the daemon should next
    /// re-evaluate if nothing else happens.
    ///
    /// [`Deadline::Never`] means there is no scheduled reason to wake up and the daemon
    /// should sit on its event sources alone.
    #[must_use]
    pub fn next_deadline(&self) -> Deadline {
        let mut next = Deadline::Never;
        for action in Action::ALL {
            let deadline = self.get(action).deadline;
            if !deadline.is_due_at(self.now) && deadline < next {
                next = deadline;
            }
        }
        next
    }
}

// ---------------------------------------------------------------------------------
// The engine
// ---------------------------------------------------------------------------------

/// What a single explicit request asks the engine to change, for the lifetime of that one
/// request.
///
/// Both fields are policy-level overrides and neither touches mechanism: the
/// single-transition guard, the logging of a refusal from the sleep mechanism and the
/// re-read-and-recompute before issuing all still apply.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
#[cfg_attr(feature = "serde", derive(Serialize))]
pub struct Request {
    /// `idlectl rest --now` ([REQ-3]).
    ///
    /// Evaluates the normal formula with exactly **two** `[while]` blocks treated as
    /// satisfied -- the block whose condition is `always` and the block whose condition is
    /// `after_resume` -- meaning each of those two contributes `now` instead of its
    /// configured deadline, including when that value is `never`. Every other block, every
    /// ceiling and every implicit floor is evaluated unchanged.
    ///
    /// The set is fixed **by condition name**: there is no per-block key and an
    /// administrator cannot enlarge it. That is deliberate -- a config key would let
    /// somebody make the human-presence floor collapsible by typing one word.
    pub now: bool,
    /// `idlectl rest --force` ([REQ-10]): install an ephemeral ceiling for this action
    /// with deadline `now`.
    ///
    /// Because ceilings compose by MIN and are absolute ([CEIL-4]), `resolved(action)`
    /// becomes `now`, and `--force` therefore defeats -- mechanically, with no special
    /// cases -- every `[while]` floor including `never` and including
    /// `[while.human_active]`, the implicit doubt floor of [FACT-4], the
    /// configuration-fault floor of [CFG-16], and the base schedule. `--now` satisfies two
    /// floors; `--force` defeats all of them.
    ///
    /// The single most likely real use is a machine wedged awake by a dead detector, which
    /// is exactly the case a floor-enumerating definition would have missed, because the
    /// doubt floor and the config-fault floor are not blocks.
    pub force: Option<Action>,
    /// The action a caller asked for **by name**, if this evaluation came from a request.
    ///
    /// [ACT-7] makes the shallowest due action win, which is right for the schedule: a
    /// deeper action is never a safe substitute for a shallower one. It is wrong for a
    /// request. `--now` makes the base schedule contribute `now` for *every* action it
    /// names, so a shallower one is due at the same instant, and taking the shallowest
    /// would answer `idlectl rest --action poweroff` with a suspend -- while reporting the
    /// request unsatisfied, because the caller asked for something else. The machine would
    /// then be asleep, the relay would believe it was off, and nothing would say otherwise.
    ///
    /// [ACT-7b] names this command as one of the three supported routes to a deeper action
    /// than the schedule would ever pick. Honouring the name is what makes that true.
    ///
    /// When set, the requested action is the **only** one that may be performed: if it is
    /// held, nothing happens. Substituting a shallower action would be answering a question
    /// nobody asked.
    pub requested: Option<Action>,
}

impl Request {
    /// The ordinary periodic evaluation: no request in flight.
    #[must_use]
    pub const fn none() -> Self {
        Request {
            now: false,
            force: None,
            requested: None,
        }
    }
}

/// Resolves a policy against a set of facts and a clock snapshot.
///
/// The ordinary periodic evaluation. Equivalent to [`resolve_request`] with
/// [`Request::none`].
///
/// Pure: same inputs, same output, no clock read, no allocation beyond the returned
/// [`Reason`] and [`ImplicitFloor`] vectors.
///
/// Call [`BlockEdges::observe`] with the same `facts` and `clocks.now` first, or blocks on
/// [`Clock::ConditionEdge`] will fall back to "just became true".
#[must_use]
pub fn resolve(
    policy: &Policy,
    facts: &FactSet,
    clocks: &ClockSnapshot,
    edges: &BlockEdges,
) -> Decision {
    resolve_request(policy, facts, clocks, edges, &Request::none())
}

/// Resolves a policy against a set of facts, a clock snapshot and one explicit request.
///
/// # Contribution rules
///
/// For each action independently, each **enabled** block that **sets a key for that
/// action** contributes as follows. A block that is silent about an action contributes
/// nothing to it, whatever its condition says ([COMP-2b]): silence is not `never`.
///
/// | condition state | `[while]` block | `[when]` block |
/// |---|---|---|
/// | [`FactState::True`] | `origin + timeout` | `origin + timeout` |
/// | [`FactState::False`] / [`FactState::Unavailable`] | nothing | nothing |
/// | [`FactState::Indeterminate`] | **nothing** | **nothing** |
///
/// The bottom row is [FACT-4b]: a block whose condition is indeterminate is FALSE as a
/// *selector*. Doubt does not arrive through blocks at all -- it arrives once per
/// evaluation, from a scan of the whole enabled fact set ([FACT-4]), as an implicit floor
/// on every action for which [`Action::vetoed_by_doubt`] holds. That is what stops doubt
/// about fact X from vetoing only the actions some block about X happened to name, and it
/// is what lets doubt **prevent** an action while never being able to **cause** one: an
/// indeterminate `[when]` block contributes nothing, so a dead detector can never force a
/// machine off.
///
/// The second implicit floor is [CFG-16]: while the effective configuration carries any
/// fault, the three sleep actions are floored at `+infinity` and [`Action::ScreenOff`] is
/// left alone.
#[must_use]
pub fn resolve_request(
    policy: &Policy,
    facts: &FactSet,
    clocks: &ClockSnapshot,
    edges: &BlockEdges,
    request: &Request,
) -> Decision {
    // One scan of the fact set per evaluation, shared by all four actions.
    let doubtful_facts = policy.doubtful_facts(facts);

    Decision {
        now: clocks.now,
        requested: request.requested,
        screen_off: resolve_action(
            Action::ScreenOff,
            policy,
            facts,
            clocks,
            edges,
            &doubtful_facts,
            request,
        ),
        suspend: resolve_action(
            Action::Suspend,
            policy,
            facts,
            clocks,
            edges,
            &doubtful_facts,
            request,
        ),
        hibernate: resolve_action(
            Action::Hibernate,
            policy,
            facts,
            clocks,
            edges,
            &doubtful_facts,
            request,
        ),
        poweroff: resolve_action(
            Action::PowerOff,
            policy,
            facts,
            clocks,
            edges,
            &doubtful_facts,
            request,
        ),
        doubtful_facts,
        config_faults: policy.faults.len(),
    }
}

fn resolve_action(
    action: Action,
    policy: &Policy,
    facts: &FactSet,
    clocks: &ClockSnapshot,
    edges: &BlockEdges,
    doubtful_facts: &[FactId],
    request: &Request,
) -> ActionResolution {
    let mut reasons: Vec<Reason> = Vec::new();

    for block in &policy.blocks {
        if !block.enabled {
            continue;
        }
        // [COMP-2b] The block sets no key for this action, so it does not participate in
        // it. Silence is not `never`, whatever the condition says: this is what lets
        // `[while.steam_downloading]` veto suspend without pinning a lit panel through a
        // six-hour download.
        let Some(timeout) = block.timeouts.get(action) else {
            continue;
        };

        let state = block.id.condition.evaluate(facts);
        let origin = clocks.origin(block.clock, edges.since(block.id));

        // [REQ-3] `rest --now` satisfies exactly two [while] blocks, chosen by condition
        // name, and each of them then contributes `now` -- including when its configured
        // value is `never`.
        let satisfied_by_request = request.now
            && block.id.kind == BlockKind::While
            && matches!(
                block.id.condition,
                Condition::Always | Condition::Fact(FactId::AfterResume)
            );

        if satisfied_by_request {
            reasons.push(Reason {
                block: block.id,
                state,
                clock: block.clock,
                origin,
                deadline: Deadline::At(clocks.now),
                satisfied_by_request: true,
            });
            continue;
        }

        // [FACT-4b] Only TRUE selects. False, unavailable and indeterminate all contribute
        // nothing, in `[while]` and in `[when]` alike.
        if !state.is_true() {
            continue;
        }

        let deadline = match origin {
            Some(origin) => Deadline::from_origin(origin, timeout),
            // [CLK-11] The origin is unknown -- today, a block on the resume clock on a
            // machine that has not slept this boot. A floor with an unknown origin is
            // `+infinity` for the three sleep actions, and keeps the boot origin for
            // screen_off, which no implicit infinity may reach. A ceiling with an unknown
            // origin contributes nothing at all: an unknown origin must never be able to
            // cause an action.
            None => match block.id.kind {
                BlockKind::While if action.is_sleep() => Deadline::Never,
                BlockKind::While => Deadline::from_origin(BootInstant::BOOT, timeout),
                BlockKind::When => continue,
            },
        };

        reasons.push(Reason {
            block: block.id,
            state,
            clock: block.clock,
            origin,
            deadline,
            satisfied_by_request: false,
        });
    }

    // The implicit floors. Neither is a block: neither can be satisfied by `rest --now`,
    // and both are defeated only by a ceiling. Neither may ever touch screen_off.
    let mut implicit_floors: Vec<ImplicitFloor> = Vec::new();
    if action.vetoed_by_doubt() {
        // [FACT-4] Any enabled fact that could not be read, whether or not a block
        // references it.
        implicit_floors.extend(doubtful_facts.iter().copied().map(ImplicitFloor::Doubt));
        // [CFG-16] Any file dropped at load time.
        implicit_floors.extend((0..policy.faults.len()).map(ImplicitFloor::ConfigFault));
    }

    let floor = compose_floor(
        reasons
            .iter()
            .filter(|r| r.block.kind == BlockKind::While)
            .map(|r| r.deadline)
            .chain(implicit_floors.iter().map(|_| Deadline::Never)),
    );

    // [REQ-10] `--force` is an ephemeral ceiling due now, and nothing else. Because
    // ceilings compose by MIN, it defeats every floor above -- including the two implicit
    // ones, which no enumeration of blocks could have reached.
    let forced = request.force == Some(action);
    let ceiling = compose_ceiling(
        reasons
            .iter()
            .filter(|r| r.block.kind == BlockKind::When)
            .map(|r| r.deadline)
            .chain(forced.then_some(Deadline::At(clocks.now))),
    );

    // [CEIL-3] MIN over instants cannot produce a later instant than either operand, so
    // resolved <= floor always: a ceiling can only ever pull an action earlier, and a
    // `[when]` block is never a way to keep a machine awake.
    let deadline = floor.min(ceiling);

    ActionResolution {
        action,
        floor,
        ceiling,
        deadline,
        due: deadline.is_due_at(clocks.now),
        reasons,
        implicit_floors,
    }
}

// ---------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;

    fn secs(n: u64) -> Duration {
        Duration::from_secs(n)
    }

    fn block(kind: BlockKind, condition: Condition, clock: Clock, timeouts: Timeouts) -> Block {
        Block {
            id: BlockId::new(kind, condition),
            clock,
            enabled: true,
            timeouts,
        }
    }

    fn suspend_after(t: Timeout) -> Timeouts {
        Timeouts {
            suspend: Some(t),
            ..Timeouts::default()
        }
    }

    /// The base schedule: `[while.always] suspend = 30m`, counted from human input.
    fn base_schedule() -> Block {
        block(
            BlockKind::While,
            Condition::Always,
            Clock::HumanInput,
            suspend_after(Timeout::After(secs(30 * MINUTE))),
        )
    }

    /// The settle window: `[while.after_resume] suspend = 5m`, counted from the resume.
    fn resume_window() -> Block {
        block(
            BlockKind::While,
            Condition::Fact(FactId::AfterResume),
            Clock::Resume,
            suspend_after(Timeout::After(secs(5 * MINUTE))),
        )
    }

    fn evaluate(policy: &Policy, facts: &FactSet, clocks: &ClockSnapshot) -> Decision {
        evaluate_request(policy, facts, clocks, &Request::none())
    }

    fn evaluate_request(
        policy: &Policy,
        facts: &FactSet,
        clocks: &ClockSnapshot,
        request: &Request,
    ) -> Decision {
        let mut edges = BlockEdges::new();
        edges.observe(policy, facts, clocks.now);
        resolve_request(policy, facts, clocks, &edges, request)
    }

    /// `[while.always] screen_off = 10m, suspend = 30m`, on the human-input clock.
    fn base_panel_and_suspend() -> Block {
        block(
            BlockKind::While,
            Condition::Always,
            Clock::HumanInput,
            Timeouts {
                screen_off: Some(Timeout::After(secs(10 * MINUTE))),
                suspend: Some(Timeout::After(secs(30 * MINUTE))),
                ..Timeouts::default()
            },
        )
    }

    /// Two hours of accumulated idle: every deadline in `base_panel_and_suspend` is past.
    fn long_idle() -> ClockSnapshot {
        let now = BootInstant::from_secs(10 * HOUR);
        ClockSnapshot {
            now,
            human_input: ClockOrigin::At(now.saturating_sub(secs(2 * HOUR))),
            resume: ClockOrigin::NotYet,
        }
    }

    /// THE NORMATIVE TEST.
    ///
    /// A machine carrying nine hours of accumulated human-idle resumed zero seconds ago.
    /// `[while.always] suspend = 30m` and `[while.after_resume] suspend = 5m` are both
    /// true. The answer must be `resume + 5m`.
    ///
    /// If this test is ever "fixed" by composing durations instead of instants, the
    /// deadline becomes `last_input + 30m`, which is eight and a half hours in the past,
    /// and the machine suspends about one second after waking. That is not hypothetical:
    /// it was measured, wake at 08:35:45 and SUSPENDING at 08:35:46.
    #[test]
    fn resume_window_beats_nine_hours_of_accumulated_idle() {
        let now = BootInstant::from_secs(20 * HOUR);
        let clocks = ClockSnapshot {
            now,
            // Idle counters do not reset across a suspend. Nine hours of them survived.
            human_input: ClockOrigin::At(now.saturating_sub(secs(9 * HOUR))),
            // The machine came back one instant ago.
            resume: ClockOrigin::At(now),
        };
        let policy = Policy {
            blocks: vec![base_schedule(), resume_window()],
            ..Policy::empty()
        };
        let facts = FactSet::new()
            .with(FactId::AfterResume, FactState::True)
            .with(FactId::HumanActive, FactState::False);

        let decision = evaluate(&policy, &facts, &clocks);

        assert_eq!(
            decision.suspend.deadline,
            Deadline::At(now.checked_add(secs(5 * MINUTE)).unwrap()),
            "the settle window must push the deadline five minutes into the future"
        );
        assert!(
            !decision.suspend.due,
            "a machine that resumed this instant must not be due to suspend"
        );
        assert_ne!(
            decision.suspend.deadline,
            Deadline::At(now),
            "deadline collapsed onto now: durations were composed instead of instants"
        );
    }

    /// Documents the trap the test above guards, so that the wrong answer is written down
    /// somewhere and cannot be rediscovered as a "simplification".
    #[test]
    fn composing_durations_would_have_fired_immediately() {
        let now = BootInstant::from_secs(20 * HOUR);
        let last_input = now.saturating_sub(secs(9 * HOUR));

        // The tempting, wrong reduction: max over durations, then add once.
        let wrong = last_input
            .checked_add(secs(30 * MINUTE).max(secs(5 * MINUTE)))
            .unwrap();
        assert!(
            wrong < now,
            "max(30m, 5m) from a nine-hour-old origin lands in the past -- fires at once"
        );
    }

    #[test]
    fn empty_policy_never_acts() {
        let policy = Policy::empty();
        let clocks = ClockSnapshot::at(BootInstant::from_secs(10 * HOUR));
        let decision = evaluate(&policy, &FactSet::new(), &clocks);

        for action in Action::ALL {
            assert_eq!(decision.get(action).floor, Deadline::Never);
            assert!(!decision.get(action).due);
        }
        assert_eq!(decision.shallowest_sleep_due(), None);
    }

    #[test]
    fn human_active_is_a_hard_floor() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            // Long past the base schedule's thirty minutes.
            human_input: ClockOrigin::At(now.saturating_sub(secs(2 * HOUR))),
            resume: ClockOrigin::NotYet,
        };
        let policy = Policy {
            blocks: vec![
                base_schedule(),
                block(
                    BlockKind::While,
                    Condition::Fact(FactId::HumanActive),
                    Clock::HumanInput,
                    suspend_after(Timeout::Never),
                ),
            ],
            ..Policy::empty()
        };

        // Without a human: the base schedule governs and the deadline is long past.
        let idle = FactSet::new().with(FactId::HumanActive, FactState::False);
        assert!(evaluate(&policy, &idle, &clocks).suspend.due);

        // With a human present, `never` wins the maximum outright.
        let present = FactSet::new().with(FactId::HumanActive, FactState::True);
        let decision = evaluate(&policy, &present, &clocks);
        assert_eq!(decision.suspend.deadline, Deadline::Never);
        assert!(!decision.suspend.due);
    }

    #[test]
    fn doubt_vetoes_suspend_but_not_screen_off() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            human_input: ClockOrigin::At(now.saturating_sub(secs(2 * HOUR))),
            resume: ClockOrigin::NotYet,
        };
        let policy = Policy {
            blocks: vec![
                block(
                    BlockKind::While,
                    Condition::Always,
                    Clock::HumanInput,
                    Timeouts {
                        screen_off: Some(Timeout::After(secs(10 * MINUTE))),
                        suspend: Some(Timeout::After(secs(30 * MINUTE))),
                        ..Timeouts::default()
                    },
                ),
                block(
                    BlockKind::While,
                    Condition::Fact(FactId::SteamGameRunning),
                    Clock::HumanInput,
                    Timeouts {
                        screen_off: Some(Timeout::Never),
                        suspend: Some(Timeout::Never),
                        ..Timeouts::default()
                    },
                ),
            ],
            ..Policy::empty()
        };

        let facts = FactSet::new().with(FactId::SteamGameRunning, FactState::Indeterminate);
        let decision = evaluate(&policy, &facts, &clocks);

        assert_eq!(
            decision.suspend.deadline,
            Deadline::Never,
            "an unreadable game detector must veto suspend"
        );
        assert!(
            decision.screen_off.due,
            "an unreadable detector must not pin an OLED panel on"
        );
    }

    #[test]
    fn unavailable_behaves_as_false() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            human_input: ClockOrigin::At(now.saturating_sub(secs(2 * HOUR))),
            resume: ClockOrigin::NotYet,
        };
        let policy = Policy {
            blocks: vec![
                base_schedule(),
                block(
                    BlockKind::While,
                    Condition::Fact(FactId::MediaPlaying),
                    Clock::HumanInput,
                    suspend_after(Timeout::Never),
                ),
            ],
            ..Policy::empty()
        };

        // A machine with no MPRIS at all: the fact is simply absent from the set.
        let decision = evaluate(&policy, &FactSet::new(), &clocks);
        assert!(
            decision.suspend.due,
            "a machine without a media player must still be allowed to sleep"
        );
    }

    #[test]
    fn order_never_matters() {
        let now = BootInstant::from_secs(20 * HOUR);
        let clocks = ClockSnapshot {
            now,
            human_input: ClockOrigin::At(now.saturating_sub(secs(9 * HOUR))),
            resume: ClockOrigin::At(now),
        };
        let facts = FactSet::new().with(FactId::AfterResume, FactState::True);

        let forwards = Policy {
            blocks: vec![base_schedule(), resume_window()],
            ..Policy::empty()
        };
        let backwards = Policy {
            blocks: vec![resume_window(), base_schedule()],
            ..Policy::empty()
        };

        assert_eq!(
            evaluate(&forwards, &facts, &clocks).suspend.deadline,
            evaluate(&backwards, &facts, &clocks).suspend.deadline
        );
    }

    #[test]
    fn a_ceiling_is_the_only_thing_that_beats_never() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            human_input: ClockOrigin::At(now),
            resume: ClockOrigin::NotYet,
        };
        let hard_floor = block(
            BlockKind::While,
            Condition::Fact(FactId::HumanActive),
            Clock::HumanInput,
            suspend_after(Timeout::Never),
        );
        let facts = FactSet::new().with(FactId::HumanActive, FactState::True);

        let without = Policy {
            blocks: vec![hard_floor],
            ..Policy::empty()
        };
        assert_eq!(
            evaluate(&without, &facts, &clocks).suspend.deadline,
            Deadline::Never
        );

        // `idlectl rest --force` installs exactly this: a [when] block due immediately.
        let mut with = without.clone();
        with.blocks.push(block(
            BlockKind::When,
            Condition::Always,
            Clock::HumanInput,
            suspend_after(Timeout::After(Duration::ZERO)),
        ));
        let forced = evaluate(&with, &facts, &clocks);
        assert!(
            forced.suspend.due,
            "an explicit, logged override must be able to beat the human_active floor"
        );
    }

    #[test]
    fn doubt_never_forces_an_action() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot::at(now);
        let policy = Policy {
            blocks: vec![block(
                BlockKind::When,
                Condition::Fact(FactId::LeaseHeld),
                Clock::HumanInput,
                suspend_after(Timeout::After(Duration::ZERO)),
            )],
            ..Policy::empty()
        };
        let facts = FactSet::new().with(FactId::LeaseHeld, FactState::Indeterminate);

        let decision = evaluate(&policy, &facts, &clocks);
        assert_eq!(
            decision.suspend.ceiling,
            Deadline::Never,
            "an unreadable [when] condition must not force a suspend"
        );
        assert!(!decision.suspend.due);
    }

    /// [TEST-21], the omission vector. `[while.always] screen_off = "10m", suspend = "30m"`
    /// plus `[while.steam_downloading] suspend = "never"` with the download running yields
    /// `resolved(suspend) = +infinity` and `resolved(screen_off) = origin(human_input) +
    /// 10m`.
    ///
    /// Silence is not `never` ([COMP-2b]). The other reading -- a true block that grants no
    /// permission for an action contributes `+infinity` -- would pin the panel lit for the
    /// whole six-hour download.
    #[test]
    fn silence_about_an_action_is_not_a_veto() {
        let clocks = long_idle();
        let policy = Policy {
            blocks: vec![
                base_panel_and_suspend(),
                // Downloading must not be interrupted, but there is no reason to keep the
                // panel lit for it. The block says nothing about screen_off.
                block(
                    BlockKind::While,
                    Condition::Fact(FactId::SteamDownloading),
                    Clock::HumanInput,
                    suspend_after(Timeout::Never),
                ),
            ],
            ..Policy::empty()
        };
        let facts = FactSet::new().with(FactId::SteamDownloading, FactState::True);

        let decision = evaluate(&policy, &facts, &clocks);
        assert_eq!(decision.suspend.deadline, Deadline::Never);
        assert_eq!(
            decision.screen_off.deadline,
            Deadline::At(
                clocks
                    .human_input
                    .instant()
                    .unwrap()
                    .checked_add(secs(10 * MINUTE))
                    .unwrap()
            ),
            "the download block must contribute nothing at all to screen_off"
        );
        assert!(decision.screen_off.due);
    }

    /// [FACT-4], the doubt veto, in the shape that separates this engine from a
    /// block-scanning one: **no block mentions the fact at all**.
    ///
    /// An admin config with only `[while.always]` and one detector that cannot be read
    /// must not sleep. A block-scanning implementation suspends here, and takes whatever
    /// the machine was doing down with it.
    #[test]
    fn doubt_vetoes_even_when_no_block_mentions_the_fact() {
        let clocks = long_idle();
        let policy = Policy {
            blocks: vec![base_panel_and_suspend()],
            ..Policy::empty()
        };
        // Nothing in the policy references media_playing.
        let facts = FactSet::new().with(FactId::MediaPlaying, FactState::Indeterminate);

        let decision = evaluate(&policy, &facts, &clocks);

        assert_eq!(decision.doubtful_facts, vec![FactId::MediaPlaying]);
        assert_eq!(
            decision.suspend.floor,
            Deadline::Never,
            "one unreadable detector floors every sleep action, block or no block"
        );
        assert_eq!(
            decision.suspend.implicit_floors,
            vec![ImplicitFloor::Doubt(FactId::MediaPlaying)]
        );
        assert!(!decision.suspend.due);
        assert!(!decision.hibernate.due);
        assert!(!decision.poweroff.due);

        // [FACT-5] No implicit floor may ever reach the panel.
        assert!(decision.screen_off.implicit_floors.is_empty());
        assert!(decision.screen_off.due);
    }

    /// [FACT-4b]. A block whose condition is indeterminate is FALSE as a selector: it
    /// contributes nothing, so its `never` does not reach `screen_off`, which the doubt
    /// floor is not allowed to touch either.
    #[test]
    fn an_indeterminate_condition_selects_nothing() {
        let clocks = long_idle();
        let policy = Policy {
            blocks: vec![
                base_panel_and_suspend(),
                block(
                    BlockKind::While,
                    Condition::Fact(FactId::MediaPlaying),
                    Clock::HumanInput,
                    Timeouts {
                        screen_off: Some(Timeout::Never),
                        ..Timeouts::default()
                    },
                ),
            ],
            ..Policy::empty()
        };
        let facts = FactSet::new().with(FactId::MediaPlaying, FactState::Indeterminate);

        let decision = evaluate(&policy, &facts, &clocks);
        assert!(
            decision
                .screen_off
                .reasons
                .iter()
                .all(|r| r.state.is_true()),
            "an indeterminate condition must not contribute a reason"
        );
        assert!(decision.screen_off.due);
        assert!(
            !decision.suspend.due,
            "but doubt still floors the sleep actions"
        );
    }

    /// [CFG-12], [FACT-6]. Disabling a fact is the auditable escape hatch from a
    /// permanently-indeterminate detector: the detector must not run, and doubt about it
    /// stops counting.
    #[test]
    fn a_disabled_fact_contributes_no_doubt() {
        let clocks = long_idle();
        let mut policy = Policy {
            blocks: vec![base_panel_and_suspend()],
            ..Policy::empty()
        };
        policy.facts.insert(
            FactId::MediaPlaying,
            FactSettings {
                enabled: false,
                ..FactSettings::default_for(FactId::MediaPlaying)
            },
        );

        // Even if a stale reading is still in the set, a disabled fact never wedges.
        let facts = FactSet::new().with(FactId::MediaPlaying, FactState::Indeterminate);
        let decision = evaluate(&policy, &facts, &clocks);

        assert!(decision.doubtful_facts.is_empty());
        assert!(decision.suspend.due);
    }

    /// `local_service_busy` ships disabled, because its counter endpoint is off by default
    /// on the service it was written for and the doubt veto is block-independent. A
    /// shipped-enabled unreadable detector would freeze every machine on install.
    #[test]
    fn only_local_service_busy_ships_disabled() {
        let policy = Policy::empty();
        for fact in FactId::ALL {
            assert_eq!(
                policy.fact_enabled(fact),
                fact != FactId::LocalServiceBusy,
                "{fact} has the wrong shipped default"
            );
        }

        let facts = FactSet::new().with(FactId::LocalServiceBusy, FactState::Indeterminate);
        assert!(
            policy.doubtful_facts(&facts).is_empty(),
            "a disabled detector must not be able to wedge a fresh install"
        );
    }

    /// [CFG-16], [TEST-13]. A dropped file leaves the daemon running, floors the three
    /// sleep actions and leaves `screen_off` alone. The alternative -- refusing to start --
    /// is the failure this requirement was written from: one unparseable value once killed
    /// a decider before it evaluated anything, and on a first start after a bad edit there
    /// is no previous policy to fall back to.
    #[test]
    fn a_configuration_fault_floors_sleep_but_not_the_panel() {
        let clocks = long_idle();
        let policy = Policy {
            blocks: vec![base_panel_and_suspend()],
            faults: vec![ConfigFault {
                source: "/etc/idlectl/conf.d/99-broken.toml".to_owned(),
                location: "while.always.suspend".to_owned(),
                message: "cannot parse \"half an hour\"".to_owned(),
            }],
            ..Policy::empty()
        };

        let decision = evaluate(&policy, &FactSet::new(), &clocks);
        assert_eq!(decision.config_faults, 1);
        assert_eq!(decision.suspend.floor, Deadline::Never);
        assert_eq!(
            decision.suspend.implicit_floors,
            vec![ImplicitFloor::ConfigFault(0)]
        );
        assert!(!decision.suspend.due);
        assert!(!decision.hibernate.due);
        assert!(!decision.poweroff.due);
        assert!(decision.screen_off.implicit_floors.is_empty());
        assert!(decision.screen_off.due);
    }

    /// [ACT-7b] promises `idlectl rest --action poweroff` as the supported way to reach a
    /// deeper action than the schedule would pick. That promise is only kept if the
    /// requested action is the one evaluated: `rest --now` makes the base schedule
    /// contribute `now` for EVERY action, so a shallower one is due at the same instant and
    /// a selector that takes the shallowest silently substitutes it.
    ///
    /// A caller that asked to end every session on the machine and got a suspend has been
    /// told "done" about something that did not happen.
    #[test]
    fn an_explicitly_requested_action_is_not_replaced_by_a_shallower_one() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            // Nobody has touched it for hours, so the base schedule is long due.
            human_input: ClockOrigin::At(now.saturating_sub(secs(5 * HOUR))),
            resume: ClockOrigin::NotYet,
        };
        // The vendor shape: the base schedule suspends, and never powers off by itself.
        let policy = Policy {
            blocks: vec![block(
                BlockKind::While,
                Condition::Always,
                Clock::HumanInput,
                Timeouts {
                    suspend: Some(Timeout::After(secs(30 * MINUTE))),
                    poweroff: Some(Timeout::Never),
                    ..Timeouts::default()
                },
            )],
            ..Policy::empty()
        };

        let decision = evaluate_request(
            &policy,
            &FactSet::new(),
            &clocks,
            &Request {
                now: true,
                force: None,
                requested: Some(Action::PowerOff),
            },
        );

        // Both are due: `--now` collapses the base schedule for every action it names,
        // including the `never`.
        assert!(decision.suspend.due, "suspend is due at the same instant");
        assert!(decision.poweroff.due, "the requested action is due");

        // The whole point: what gets performed is what was asked for.
        assert_eq!(decision.action_to_perform(), Some(Action::PowerOff));
    }

    /// With no explicit request the shallowest still wins, which is [ACT-7] and must not
    /// change: an ordinary schedule that has both due suspends rather than powering off.
    #[test]
    fn without_a_request_the_shallowest_action_still_wins() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            human_input: ClockOrigin::At(now.saturating_sub(secs(9 * HOUR))),
            resume: ClockOrigin::NotYet,
        };
        let policy = Policy {
            blocks: vec![block(
                BlockKind::While,
                Condition::Always,
                Clock::HumanInput,
                Timeouts {
                    suspend: Some(Timeout::After(secs(30 * MINUTE))),
                    poweroff: Some(Timeout::After(secs(8 * HOUR))),
                    ..Timeouts::default()
                },
            )],
            ..Policy::empty()
        };

        let decision = evaluate_request(&policy, &FactSet::new(), &clocks, &Request::none());
        assert!(decision.suspend.due);
        assert!(decision.poweroff.due);
        assert_eq!(decision.action_to_perform(), Some(Action::Suspend));
    }

    /// A requested action that is NOT due is refused outright rather than downgraded to
    /// whatever else happens to be due.
    #[test]
    fn a_requested_action_that_is_held_performs_nothing() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            human_input: ClockOrigin::At(now.saturating_sub(secs(5 * HOUR))),
            resume: ClockOrigin::NotYet,
        };
        // A lease holds poweroff for ever; the base schedule still suspends.
        let policy = Policy {
            blocks: vec![
                base_schedule(),
                block(
                    BlockKind::While,
                    Condition::Fact(FactId::LeaseHeld),
                    Clock::HumanInput,
                    Timeouts {
                        poweroff: Some(Timeout::Never),
                        ..Timeouts::default()
                    },
                ),
            ],
            ..Policy::empty()
        };
        let facts = FactSet::new().with(FactId::LeaseHeld, FactState::True);

        let decision = evaluate_request(
            &policy,
            &facts,
            &clocks,
            &Request {
                now: true,
                force: None,
                requested: Some(Action::PowerOff),
            },
        );

        assert!(decision.suspend.due, "the machine could suspend");
        assert!(!decision.poweroff.due, "but poweroff is held by the lease");
        assert_eq!(
            decision.action_to_perform(),
            None,
            "and nothing is performed: a suspend is not a poweroff"
        );
    }

    /// [REQ-3]. `rest --now` satisfies exactly two blocks, chosen by condition name, and
    /// nothing else -- a human at the keyboard still stops it.
    #[test]
    fn rest_now_satisfies_two_blocks_by_name_and_no_others() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            // Somebody typed one second ago, so the base schedule is nowhere near due.
            human_input: ClockOrigin::At(now.saturating_sub(secs(1))),
            resume: ClockOrigin::At(now.saturating_sub(secs(1))),
        };
        let policy = Policy {
            blocks: vec![
                base_schedule(),
                resume_window(),
                block(
                    BlockKind::While,
                    Condition::Fact(FactId::HumanActive),
                    Clock::HumanInput,
                    suspend_after(Timeout::Never),
                ),
            ],
            ..Policy::empty()
        };
        let request = Request {
            now: true,
            force: None,
            requested: None,
        };

        // A human is present: the two satisfied blocks contribute `now`, but the
        // human_active floor is untouched and still says never.
        let present = FactSet::new()
            .with(FactId::HumanActive, FactState::True)
            .with(FactId::AfterResume, FactState::True);
        let held = evaluate_request(&policy, &present, &clocks, &request);
        assert_eq!(held.suspend.deadline, Deadline::Never);
        assert!(!held.suspend.due);

        // Nobody there: both the base schedule and the settle window collapse to `now`.
        let idle = FactSet::new()
            .with(FactId::HumanActive, FactState::False)
            .with(FactId::AfterResume, FactState::True);
        let granted = evaluate_request(&policy, &idle, &clocks, &request);
        assert_eq!(granted.suspend.deadline, Deadline::At(now));
        assert!(granted.suspend.due);
    }

    /// [TEST-22]. With one enabled fact indeterminate and **no block referencing it**,
    /// `rest --now` issues nothing and reports the doubt floor; `rest --now --force`
    /// issues the transition and can name the floor it defeated.
    ///
    /// This is the single most likely real use of `--force`: a machine wedged awake by a
    /// dead detector. A definition of `--force` that enumerated blocks would have missed
    /// it, because the doubt floor is not a block.
    #[test]
    fn force_defeats_the_doubt_floor_that_now_cannot_reach() {
        let clocks = long_idle();
        let policy = Policy {
            blocks: vec![base_panel_and_suspend()],
            ..Policy::empty()
        };
        let facts = FactSet::new().with(FactId::MediaPlaying, FactState::Indeterminate);

        let asked = evaluate_request(
            &policy,
            &facts,
            &clocks,
            &Request {
                now: true,
                force: None,
                requested: None,
            },
        );
        assert!(
            !asked.suspend.due,
            "`rest --now` must not satisfy an implicit floor"
        );
        assert_eq!(
            asked.suspend.implicit_floors,
            vec![ImplicitFloor::Doubt(FactId::MediaPlaying)]
        );

        let forced = evaluate_request(
            &policy,
            &facts,
            &clocks,
            &Request {
                now: true,
                force: Some(Action::Suspend),
                requested: Some(Action::Suspend),
            },
        );
        assert!(forced.suspend.due);
        assert_eq!(forced.suspend.floor, Deadline::Never);
        assert_eq!(forced.suspend.ceiling, Deadline::At(clocks.now));
        assert!(
            forced.suspend.ceiling_defeated_floor(),
            "the event must be loggable as a ceiling defeating a floor"
        );
        assert_eq!(
            forced.suspend.implicit_floors,
            vec![ImplicitFloor::Doubt(FactId::MediaPlaying)],
            "the defeated floor is still reported, so [REQ-11] can name it"
        );
        // Force is per-action and ephemeral: nothing else moved.
        assert!(!forced.poweroff.due);
    }

    /// [CEIL-3]. A ceiling can only ever pull an action earlier. There is no configuration
    /// in which one delays an action, because MIN over instants cannot produce a later
    /// instant than either operand.
    #[test]
    fn a_ceiling_can_never_delay_an_action() {
        let clocks = long_idle();
        let policy = Policy {
            blocks: vec![
                base_panel_and_suspend(),
                block(
                    BlockKind::When,
                    Condition::Always,
                    Clock::HumanInput,
                    suspend_after(Timeout::Never),
                ),
            ],
            ..Policy::empty()
        };

        let decision = evaluate(&policy, &FactSet::new(), &clocks);
        assert!(decision.suspend.deadline <= decision.suspend.floor);
        assert!(
            decision.suspend.due,
            "a `never` ceiling must change nothing"
        );
    }

    /// [CLK-11]. A block on the resume clock, on a machine that has not slept this boot,
    /// has no origin. It must contribute `+infinity` to the sleep actions rather than
    /// falling back to boot -- `[while.always] clock = "resume"` parses, and with a boot
    /// fallback it fires the instant its timeout is older than the machine.
    #[test]
    fn a_resume_clock_with_no_resume_never_fires_a_sleep() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            human_input: ClockOrigin::At(BootInstant::BOOT),
            resume: ClockOrigin::NotYet,
        };
        let policy = Policy {
            blocks: vec![block(
                BlockKind::While,
                Condition::Always,
                Clock::Resume,
                Timeouts {
                    screen_off: Some(Timeout::After(secs(10 * MINUTE))),
                    suspend: Some(Timeout::After(secs(30 * MINUTE))),
                    ..Timeouts::default()
                },
            )],
            ..Policy::empty()
        };

        let decision = evaluate(&policy, &FactSet::new(), &clocks);
        assert_eq!(decision.suspend.deadline, Deadline::Never);
        assert!(!decision.suspend.due);
        // The panel keeps the boot origin: no implicit infinity may reach screen_off.
        assert_eq!(
            decision.screen_off.deadline,
            Deadline::At(BootInstant::BOOT.checked_add(secs(10 * MINUTE)).unwrap())
        );
        assert!(decision.screen_off.due);
    }

    /// [CLK-11] on the human-input clock, which is the case [CLK-7] must not swallow.
    ///
    /// Never-touched and unreadable are one bit apart and pull in opposite directions.
    /// Never-touched is the remote-relay fast path: origin is boot, the base schedule
    /// expires, the machine goes back to sleep as soon as the job is done. Unreadable is a
    /// dead agent, and if it took the same fallback then killing the agent would be an
    /// undocumented way to make any machine sleep on schedule with nothing watching for a
    /// human. So: same `[while.always]` block, same instant, opposite answers.
    #[test]
    fn an_unreadable_human_clock_is_not_the_never_touched_fast_path() {
        let now = BootInstant::from_secs(10 * HOUR);
        let policy = Policy {
            blocks: vec![block(
                BlockKind::While,
                Condition::Always,
                Clock::HumanInput,
                Timeouts {
                    screen_off: Some(Timeout::After(secs(10 * MINUTE))),
                    suspend: Some(Timeout::After(secs(30 * MINUTE))),
                    ..Timeouts::default()
                },
            )],
            ..Policy::empty()
        };

        // Never touched: the schedule runs. This is [CLK-7] and it must keep working.
        let untouched = evaluate(
            &policy,
            &FactSet::new(),
            &ClockSnapshot {
                now,
                human_input: ClockOrigin::NotYet,
                resume: ClockOrigin::NotYet,
            },
        );
        assert!(
            untouched.suspend.due,
            "a machine nobody has touched must still be able to go back to sleep"
        );

        // Unreadable: the same block contributes +infinity to the sleep actions.
        let broken = evaluate(
            &policy,
            &FactSet::new(),
            &ClockSnapshot {
                now,
                human_input: ClockOrigin::Unreadable,
                resume: ClockOrigin::NotYet,
            },
        );
        assert_eq!(broken.suspend.deadline, Deadline::Never);
        assert!(!broken.suspend.due);
        // ...and screen_off still lands on the boot origin, because no implicit infinity
        // may reach it -- [FACT-40] applied to clocks.
        assert_eq!(
            broken.screen_off.deadline,
            Deadline::At(BootInstant::BOOT.checked_add(secs(10 * MINUTE)).unwrap())
        );
        assert!(broken.screen_off.due);
    }

    /// [CLK-12]: the two absences must be distinguishable by the caller that reports them,
    /// not merely by the caller that composes them. `origin()` flattens both to `None`;
    /// `origin_kind()` is what `explain` and `doctor` read.
    #[test]
    fn origin_kind_separates_normal_absence_from_a_fault() {
        let now = BootInstant::from_secs(HOUR);
        let not_yet = ClockSnapshot {
            now,
            human_input: ClockOrigin::NotYet,
            resume: ClockOrigin::NotYet,
        };
        let broken = ClockSnapshot {
            human_input: ClockOrigin::Unreadable,
            ..not_yet
        };

        assert_eq!(
            not_yet.origin_kind(Clock::Resume, None),
            ClockOrigin::NotYet
        );
        assert!(!not_yet.origin_kind(Clock::Resume, None).is_unreadable());
        assert!(broken.origin_kind(Clock::HumanInput, None).is_unreadable());
        // Both flatten to the same composition input for the sleep actions, which is why
        // the raw variant has to survive as far as the reporting layer.
        assert_eq!(broken.origin(Clock::HumanInput, None), None);
        assert_eq!(not_yet.origin(Clock::Resume, None), None);
    }

    /// A ceiling with an unknown origin contributes nothing at all: an origin the engine
    /// could not resolve must never be able to *cause* an action.
    #[test]
    fn an_unknown_origin_never_forces_an_action() {
        let now = BootInstant::from_secs(10 * HOUR);
        let clocks = ClockSnapshot {
            now,
            human_input: ClockOrigin::At(now),
            resume: ClockOrigin::NotYet,
        };
        let policy = Policy {
            blocks: vec![block(
                BlockKind::When,
                Condition::Always,
                Clock::Resume,
                suspend_after(Timeout::After(secs(MINUTE))),
            )],
            ..Policy::empty()
        };

        let decision = evaluate(&policy, &FactSet::new(), &clocks);
        assert_eq!(decision.suspend.ceiling, Deadline::Never);
        assert!(!decision.suspend.due);
    }

    #[test]
    fn condition_clock_counts_from_the_edge_not_from_input() {
        let start = BootInstant::from_secs(HOUR);
        let policy = Policy {
            blocks: vec![block(
                BlockKind::While,
                Condition::Fact(FactId::SteamDownloading),
                Clock::ConditionEdge,
                suspend_after(Timeout::After(secs(10 * MINUTE))),
            )],
            ..Policy::empty()
        };
        let facts = FactSet::new().with(FactId::SteamDownloading, FactState::True);

        let mut edges = BlockEdges::new();
        edges.observe(&policy, &facts, start);

        // Five minutes later the edge is still the original one.
        let later = start.checked_add(secs(5 * MINUTE)).unwrap();
        edges.observe(&policy, &facts, later);
        let decision = resolve(
            &policy,
            &facts,
            &ClockSnapshot {
                now: later,
                human_input: ClockOrigin::At(BootInstant::BOOT),
                resume: ClockOrigin::NotYet,
            },
            &edges,
        );
        assert_eq!(
            decision.suspend.deadline,
            Deadline::At(start.checked_add(secs(10 * MINUTE)).unwrap())
        );
    }

    #[test]
    fn indeterminate_does_not_reset_a_running_edge() {
        let start = BootInstant::from_secs(HOUR);
        let policy = Policy {
            blocks: vec![block(
                BlockKind::While,
                Condition::Fact(FactId::SteamDownloading),
                Clock::ConditionEdge,
                suspend_after(Timeout::After(secs(10 * MINUTE))),
            )],
            ..Policy::empty()
        };
        let id = BlockId::new(BlockKind::While, Condition::Fact(FactId::SteamDownloading));

        let mut edges = BlockEdges::new();
        edges.observe(
            &policy,
            &FactSet::new().with(FactId::SteamDownloading, FactState::True),
            start,
        );

        // One failed read half an hour later must not restart the timer.
        edges.observe(
            &policy,
            &FactSet::new().with(FactId::SteamDownloading, FactState::Indeterminate),
            start.checked_add(secs(30 * MINUTE)).unwrap(),
        );
        assert_eq!(edges.since(id), Some(start));

        // A definite false does clear it.
        edges.observe(
            &policy,
            &FactSet::new().with(FactId::SteamDownloading, FactState::False),
            start.checked_add(secs(31 * MINUTE)).unwrap(),
        );
        assert_eq!(edges.since(id), None);
    }

    #[test]
    fn deadline_ordering_puts_never_at_infinity() {
        let early = Deadline::At(BootInstant::from_secs(1));
        let late = Deadline::At(BootInstant::from_secs(100));
        assert!(Deadline::Never > late);
        assert!(early < late);
        assert_eq!(
            compose_floor([early, late, Deadline::Never]),
            Deadline::Never
        );
        assert_eq!(compose_ceiling([early, late, Deadline::Never]), early);

        // The empty-set rule, in both directions: nothing said the machine may act, so it
        // may not. Seeding either fold with Deadline::Never instead of None would make the
        // maximum absorb every real deadline -- see the note on Deadline.
        assert_eq!(compose_floor(std::iter::empty()), Deadline::Never);
        assert_eq!(compose_ceiling(std::iter::empty()), Deadline::Never);
    }

    /// [ACT-7]. The shallowest due sleep action wins, and `screen_off` is not a sleep
    /// action at all.
    ///
    /// If this is ever "fixed" back to deepest-due, a machine with `suspend = "30m"` and
    /// `poweroff = "8h"` both long overdue powers itself off instead of suspending --
    /// ending every session on it and, on the measured hardware, hanging about one time in
    /// four. The documented price of shallowest-due is that idlectl never escalates.
    #[test]
    fn the_shallowest_due_sleep_action_wins() {
        let now = BootInstant::from_secs(20 * HOUR);
        let clocks = ClockSnapshot {
            now,
            human_input: ClockOrigin::At(BootInstant::BOOT),
            resume: ClockOrigin::NotYet,
        };
        let policy = Policy {
            blocks: vec![block(
                BlockKind::While,
                Condition::Always,
                Clock::HumanInput,
                Timeouts {
                    screen_off: Some(Timeout::After(secs(10 * MINUTE))),
                    suspend: Some(Timeout::After(secs(30 * MINUTE))),
                    hibernate: Some(Timeout::Never),
                    poweroff: Some(Timeout::After(secs(8 * HOUR))),
                },
            )],
            ..Policy::empty()
        };

        let decision = evaluate(&policy, &FactSet::new(), &clocks);
        assert_eq!(
            decision.due_actions(),
            vec![Action::ScreenOff, Action::Suspend, Action::PowerOff],
            "all three are past their deadlines"
        );
        assert_eq!(
            decision.shallowest_sleep_due(),
            Some(Action::Suspend),
            "suspend, never poweroff: a deeper action is not a safe substitute"
        );
    }

    #[test]
    fn names_round_trip() {
        for fact in FactId::ALL {
            assert_eq!(FactId::from_name(fact.name()), Some(fact));
        }
        for action in Action::ALL {
            assert_eq!(Action::from_name(action.name()), Some(action));
        }
        for clock in Clock::ALL {
            assert_eq!(Clock::from_name(clock.name()), Some(clock));
        }
        assert_eq!(Condition::from_name("always"), Some(Condition::Always));
        assert_eq!(Condition::from_name("not_a_fact"), None);
        assert_eq!(
            BlockId::new(BlockKind::While, Condition::Fact(FactId::HumanActive)).to_string(),
            "while.human_active"
        );
    }
}
