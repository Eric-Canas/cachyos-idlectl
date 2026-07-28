//! `systemd-logind` proxies.
//!
//! Everything this daemon does to the machine's power state goes through here, and
//! deliberately nothing else does. There is no `systemctl` invocation anywhere in this
//! tree: [ACT-1] forbids delegating any part of the decision to a tool that re-decides,
//! and both available modes of the obvious helper fail that test in opposite directions —
//! without a controlling terminal it returns success and ignores inhibitors and sessions
//! entirely, and the mode that does check refuses whenever any user is logged in, which on
//! a console with autologin is always.
//!
//! Going through logind also means the daemon needs no capabilities of its own; see
//! `CapabilityBoundingSet=` in the unit file.

use zbus::proxy;

/// The logind manager.
///
/// `Suspend`, `Hibernate` and `PowerOff` take an `interactive` flag, which is passed as
/// `false` everywhere in this daemon. `true` asks polkit to prompt, and there is nobody to
/// prompt: this is a system service with no terminal, and a dialogue nobody sees is a
/// transition that hangs rather than one that is refused.
#[proxy(
    interface = "org.freedesktop.login1.Manager",
    default_service = "org.freedesktop.login1",
    default_path = "/org/freedesktop/login1",
    gen_blocking = false
)]
pub trait LogindManager {
    /// `(session_id, uid, user_name, seat_id, object_path)`.
    fn list_sessions(
        &self,
    ) -> zbus::Result<Vec<(String, u32, String, String, zbus::zvariant::OwnedObjectPath)>>;

    /// `(what, who, why, mode, uid, pid)`.
    ///
    /// `what` is a colon-separated list drawn from `shutdown`, `sleep`, `idle`,
    /// `handle-power-key`, and friends; `mode` is `block` or `delay`.
    fn list_inhibitors(&self) -> zbus::Result<Vec<(String, String, String, String, u32, u32)>>;

    fn suspend(&self, interactive: bool) -> zbus::Result<()>;
    fn hibernate(&self, interactive: bool) -> zbus::Result<()>;
    fn power_off(&self, interactive: bool) -> zbus::Result<()>;

    /// Whether the machine can perform each action at all: `"yes"`, `"no"`, `"na"` or
    /// `"challenge"`.
    ///
    /// `"na"` means the machine is not capable of it. For hibernate that is the common case
    /// and it is invisible until the moment of truth: measured on a machine whose only swap
    /// is zram and which has no `resume=` on its kernel command line, logind answered `na`
    /// and `doctor` reported nothing, while a forced hibernate came back with
    /// `SleepVerbNotSupported: Not enough suitable swap space`. [ACT-13] says an action
    /// with no mechanism is reported as unavailable rather than merely failing later.
    ///
    /// Read rather than recomputed. logind already inspects the swap devices, the resume
    /// configuration and `/sys/power/state`; deriving that again here would be a second
    /// opinion that can disagree with the mechanism which will actually run.
    ///
    /// `"challenge"` is not unavailable — it means an unprivileged caller would be asked to
    /// authenticate, and this daemon is root.
    fn can_suspend(&self) -> zbus::Result<String>;
    fn can_hibernate(&self) -> zbus::Result<String>;
    fn can_power_off(&self) -> zbus::Result<String>;

    /// Colon-separated list of what is currently inhibited in `block` mode.
    ///
    /// Read in preference to filtering [`LogindManagerProxy::list_inhibitors`] because it
    /// is a property: it arrives with a change signal, so the daemon learns that an
    /// inhibitor appeared without polling for it.
    #[zbus(property)]
    fn block_inhibited(&self) -> zbus::Result<String>;

    /// Colon-separated list of what is currently inhibited in `delay` mode.
    ///
    /// Read only so that `doctor` can show it. It MUST NOT make `inhibitor_block` true:
    /// a `delay` lock postpones a transition so its holder can tidy up, and treating one
    /// as a veto makes the fact permanently true on any machine running a network manager
    /// — measured, and the reason [FACT-11] says so explicitly.
    #[zbus(property)]
    fn delay_inhibited(&self) -> zbus::Result<String>;

    /// What logind itself is configured to do when it considers the session idle.
    ///
    /// Not used for any decision. It is read for the conflict scan of [OBS-3].11: a
    /// second owner of the machine's power state is the failure this project exists to
    /// remove, and `doctor` names every candidate it can find.
    #[zbus(property)]
    fn idle_action(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn idle_action_usec(&self) -> zbus::Result<u64>;

    /// `true` just before sleeping, `false` just after resuming.
    ///
    /// Subscribed for promptness only. Resume detection does not depend on it — see the
    /// module documentation of [`crate::clock`] for why it cannot.
    #[zbus(signal)]
    fn prepare_for_sleep(&self, start: bool) -> zbus::Result<()>;
}

/// One login session.
#[proxy(
    interface = "org.freedesktop.login1.Session",
    default_service = "org.freedesktop.login1",
    gen_blocking = false
)]
pub trait LogindSession {
    /// `user`, `greeter`, `lock-screen`, `background`, `manager`, ...
    #[zbus(property)]
    fn class(&self) -> zbus::Result<String>;

    /// `x11`, `wayland`, `tty`, `unspecified`.
    #[zbus(property)]
    fn type_(&self) -> zbus::Result<String>;

    /// The PAM service that opened it: `sshd`, `login`, `sddm`, ...
    #[zbus(property)]
    fn service(&self) -> zbus::Result<String>;

    /// logind's own verdict on whether the session is remote.
    #[zbus(property)]
    fn remote(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn active(&self) -> zbus::Result<bool>;

    /// When the session started, in microseconds since the Unix epoch.
    ///
    /// Reported by `doctor` per [FACT-10]: a process detached with `setsid` from a remote
    /// shell stays inside the session scope, so the session never closes and this fact
    /// becomes a permanent veto for the rest of the boot. Showing the age is what lets
    /// somebody recognise that shape instead of hunting a detector bug.
    ///
    /// `TimestampMonotonic` is deliberately not used, for two reasons that both bite on
    /// exactly the machines this daemon is for. It is an absolute reading rather than an
    /// elapsed one, so it has to be subtracted from a matching now — and this daemon's
    /// clock is `CLOCK_BOOTTIME`, which is not that now. And `CLOCK_MONOTONIC` stops
    /// during a suspend, so even subtracted correctly it under-reports by however long
    /// the machine slept.
    #[zbus(property)]
    fn timestamp(&self) -> zbus::Result<u64>;

    #[zbus(property)]
    fn name(&self) -> zbus::Result<String>;
}
