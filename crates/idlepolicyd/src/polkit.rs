//! Authorization, by asking polkit rather than by deciding here.
//!
//! # Why not a uid check
//!
//! Because "may this user suspend this machine?" is a question the administrator has
//! already answered, in a file they control, in the same place they answered it for
//! `logind`, `systemd` and everything else. A uid check written here would be a second,
//! invisible policy that no `pkaction` output mentions and no `pkcheck` can test.
//!
//! There is exactly **one** uid check, for uid 0, and [`check`] explains why it is not the
//! thing this paragraph argues against.
//!
//! # What happens when polkit is not installed
//!
//! Root is allowed and everybody else is refused, with a message that says why. Failing
//! open would mean any local user could suspend the machine on a minimal install; failing
//! closed for root as well would make the daemon unusable there, including for the
//! administrator trying to diagnose it. This is the same trade `systemd` makes.

use serde::Deserialize;
use tracing::warn;
use zbus::zvariant::{Type, Value};

/// polkit's answer.
///
/// A **derived struct**, not a Rust tuple, and the difference is not cosmetic.
/// `CheckAuthorization` returns a single value of D-Bus type `(bba{ss})`; a tuple return
/// type is interpreted by zbus as the reply body's fields, i.e. three separate out
/// arguments with signature `bba{ss}`. The two are different signatures, the call fails
/// with a type error, and — because this code fell back to "polkit is unreachable" on any
/// error — the failure presented as *"polkit is not available"* on a machine where polkit
/// was running and `pkaction` answered correctly. That cost an hour. Hence the explicit
/// struct, and hence the warning logged on the fallback path below.
#[derive(Debug, Deserialize, Type)]
struct AuthorizationResult {
    is_authorized: bool,
    is_challenge: bool,
    #[allow(dead_code)]
    details: std::collections::HashMap<String, String>,
}

/// The polkit action ids. These strings appear in four places — here, the polkit policy
/// file, the D-Bus introspection annotations and the specification — and a mismatch is
/// silent: an action id nobody registered simply always denies.
pub const ACTION_REST: &str = "io.github.ericcanas.Idlectl1.rest";
pub const ACTION_REST_FORCED: &str = "io.github.ericcanas.Idlectl1.rest-forced";
pub const ACTION_LEASE: &str = "io.github.ericcanas.Idlectl1.lease";
pub const ACTION_RELOAD: &str = "io.github.ericcanas.Idlectl1.reload";

#[zbus::proxy(
    interface = "org.freedesktop.PolicyKit1.Authority",
    default_service = "org.freedesktop.PolicyKit1",
    default_path = "/org/freedesktop/PolicyKit1/Authority",
    gen_blocking = false
)]
trait Authority {
    #[zbus(name = "CheckAuthorization")]
    fn check_authorization(
        &self,
        subject: &(&str, std::collections::HashMap<&str, Value<'_>>),
        action_id: &str,
        details: std::collections::HashMap<&str, &str>,
        flags: u32,
        cancellation_id: &str,
    ) -> zbus::Result<AuthorizationResult>;
}

/// Asks polkit whether `sender` may perform `action_id`.
///
/// The subject is the caller's **unique bus name**, not a pid. A pid can be recycled
/// between the call arriving and the check being made, and polkit's own documentation
/// says so; the bus name cannot, because the bus holds it for the lifetime of the
/// connection.
pub async fn check(
    bus: &zbus::Connection,
    sender: &str,
    action_id: &str,
) -> Result<(), zbus::fdo::Error> {
    let uid = caller_uid(bus, sender).await;

    // Root is allowed without asking, and this is not a shortcut taken for convenience.
    //
    // polkit's three implicit tiers are `active` (the local foreground session),
    // `inactive` (a local session in the background) and `any` (everything else --
    // including every SSH login, which is not a local session at all). An action with
    // `allow_any = no` is therefore refused to root over SSH, and the usual remedy,
    // `auth_admin`, needs an authentication agent that a non-interactive relay does not
    // have. Measured: `idlectl lease acquire` over SSH failed with AccessDenied against a
    // policy file whose own comments said remote callers were the intended users.
    //
    // Asking polkit whether root may do this adds nothing, because root can already stop
    // this daemon, rewrite its configuration, or call logind's Suspend directly. The check
    // exists to constrain UNPRIVILEGED callers, and it still does.
    if uid == Some(0) {
        return Ok(());
    }

    let authority = match AuthorityProxy::new(bus).await {
        Ok(a) => a,
        Err(err) => return fallback(uid, action_id, &err.to_string()),
    };

    let mut subject_details = std::collections::HashMap::new();
    subject_details.insert("name", Value::from(sender));

    // Flag 1 is AllowUserInteraction. A human at a terminal with an agent running gets
    // prompted; a relay or a script without one gets a plain refusal rather than a hang,
    // because polkit returns `is_challenge` instead of blocking.
    let result = authority
        .check_authorization(
            &("system-bus-name", subject_details),
            action_id,
            std::collections::HashMap::new(),
            1,
            "",
        )
        .await;

    match result {
        Ok(result) if result.is_authorized => Ok(()),
        Ok(result) => Err(zbus::fdo::Error::AccessDenied(format!(
            "not authorized for {action_id}{}",
            if result.is_challenge {
                " (authentication is required and no agent answered)"
            } else {
                ""
            }
        ))),
        Err(err) => fallback(uid, action_id, &err.to_string()),
    }
}

/// Root only, when polkit cannot be reached.
///
/// The error is logged rather than folded into the refusal message, because "polkit is not
/// available" is what a *bug in this file* looks like from outside as well as a genuinely
/// missing polkit. Without the log line the two are indistinguishable, and one of them is
/// a security control that has quietly stopped working.
fn fallback(uid: Option<u32>, action_id: &str, why: &str) -> Result<(), zbus::fdo::Error> {
    warn!(
        action = action_id,
        error = why,
        "polkit could not be consulted; falling back to uid 0 only"
    );
    if uid == Some(0) {
        return Ok(());
    }
    Err(zbus::fdo::Error::AccessDenied(format!(
        "polkit could not be consulted ({why}), so only uid 0 may perform {action_id}"
    )))
}

/// The uid behind a unique bus name, straight from the bus daemon.
///
/// Taken from the bus and never from anything the caller said about itself. A method
/// argument claiming a uid is a claim; this is a fact the bus observed at connection time.
pub async fn caller_uid(bus: &zbus::Connection, sender: &str) -> Option<u32> {
    let proxy = zbus::fdo::DBusProxy::new(bus).await.ok()?;
    let name = zbus::names::BusName::try_from(sender.to_owned()).ok()?;
    proxy.get_connection_unix_user(name).await.ok()
}

/// The pid behind a unique bus name, straight from the bus daemon.
///
/// **For reporting, never for authorization.** [`check`] hands polkit a bus name precisely
/// because a pid can be recycled between a call arriving and a check being made, and nothing
/// here reopens that: this number is only ever shown to a human, next to the lease it took.
///
/// The objection is fatal for a decision and survivable for a report, but only if the report
/// pays for it — so [`crate::leases::Holder`] stores this pid together with the process's
/// start time and re-checks both before printing, and says "recycled" rather than naming
/// whatever holds the number later. A pid without that pairing would be worse than no pid at
/// all, because it looks equally authoritative when it is wrong.
pub async fn caller_pid(bus: &zbus::Connection, sender: &str) -> Option<u32> {
    let proxy = zbus::fdo::DBusProxy::new(bus).await.ok()?;
    let name = zbus::names::BusName::try_from(sender.to_owned()).ok()?;
    proxy.get_connection_unix_process_id(name).await.ok()
}
