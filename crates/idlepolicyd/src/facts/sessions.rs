//! `remote_session` and `inhibitor_block`. Both read logind and nothing else.

use std::time::{Duration, SystemTime, UNIX_EPOCH};

use idlectl_policy::FactState;

use super::{Context, Reading, ago};
use crate::logind::{LogindManagerProxy, LogindSessionProxy};

/// How long ago a session opened, from logind's realtime `Timestamp`.
///
/// Split out from [`remote`] so that the property which actually matters can be checked
/// without a bus: the number must get SMALLER as a session gets newer.
///
/// It exists because the first implementation handed logind's absolute timestamp straight
/// to [`ago`], which is an elapsed-duration formatter. Every session then reported the
/// same figure — uptime minus suspended time — and the later of two sessions reported the
/// LARGER one, so a session opened seconds ago read as "8h24m ago". Measured on a machine
/// with 14h37m of suspend behind it. [FACT-10] says the age is what makes a session-scope
/// veto recognisable rather than looking like a broken detector; printed that way it did
/// the opposite, and made a healthy detector look broken.
fn session_age(opened_usec: u64, now: SystemTime) -> String {
    let Some(opened) = UNIX_EPOCH.checked_add(Duration::from_micros(opened_usec)) else {
        return "age unknown".to_owned();
    };
    // A session stamped in the future is a clock that moved, not an age. Saying so is
    // better than rendering a wrapped duration.
    now.duration_since(opened)
        .map_or_else(|_| "age unknown".to_owned(), ago)
}

/// [FACT-8]: true iff logind reports at least one open **user** session that is remote.
///
/// Two independent signals are accepted, because neither is reliable alone. logind's own
/// `Remote` property is authoritative when it is set, but it is set from PAM data that
/// some login paths do not provide; the `Service` being `sshd` catches those. A session is
/// remote if either says so.
///
/// The class filter is not cosmetic. `greeter`, `lock-screen`, `background` and `manager`
/// sessions all exist on an ordinary desktop and none of them is somebody working; only
/// `user` sessions count.
pub async fn remote(ctx: &Context<'_>) -> Reading {
    let manager = match LogindManagerProxy::new(ctx.bus).await {
        Ok(m) => m,
        // logind absent is not doubt: it is a machine without a session manager, and
        // [FACT-8] says the fact is UNAVAILABLE there. Doubt would freeze such a machine
        // awake permanently.
        Err(err) => return Reading::absent(format!("no session manager: {err}")),
    };

    let sessions = match manager.list_sessions().await {
        Ok(s) => s,
        // logind present but not answering IS doubt. Something that normally works has
        // stopped working, and the daemon can no longer see whether somebody is logged in
        // over the network.
        Err(err) => return Reading::doubt(format!("logind did not answer ListSessions: {err}")),
    };

    let mut found = Vec::new();
    for (id, uid, user, _seat, path) in sessions {
        // [FACT-9]: a relay that opened a session in order to ask the machine to rest
        // would otherwise veto its own request. The exclusion is per request and never
        // the default, so every other caller still counts their own session and the
        // machine cannot sleep out from underneath the person driving it.
        if ctx.excluded_sessions.contains(&id) {
            continue;
        }

        let session = match LogindSessionProxy::builder(ctx.bus).path(path) {
            Ok(b) => match b.build().await {
                Ok(s) => s,
                Err(err) => {
                    return Reading::doubt(format!("cannot read session {id}: {err}"));
                }
            },
            Err(err) => return Reading::doubt(format!("bad session path for {id}: {err}")),
        };

        // A session that vanished between ListSessions and here is not a fault: sessions
        // close all the time and the race is expected. Anything else is.
        let class = match session.class().await {
            Ok(c) => c,
            Err(err) if is_gone(&err) => continue,
            Err(err) => return Reading::doubt(format!("cannot read class of {id}: {err}")),
        };
        if class != "user" {
            continue;
        }

        let remote_flag = session.remote().await.unwrap_or(false);
        let service = session.service().await.unwrap_or_default();
        let kind = session.type_().await.unwrap_or_default();
        if !remote_flag && service != "sshd" {
            continue;
        }

        // [FACT-10]: report the age. A process detached with setsid from a remote shell
        // stays inside the session scope, so the session never closes and this fact
        // becomes a permanent veto for the rest of the boot. That is a legitimate
        // diagnostic technique and an illegitimate way to leave something running -- a
        // lease is the mechanism for the latter -- and the age is what makes the shape
        // recognisable instead of looking like a broken detector.
        let age = match session.timestamp().await {
            Ok(usec) => session_age(usec, SystemTime::now()),
            Err(_) => "age unknown".to_owned(),
        };
        found.push(format!(
            "{id} ({user}, uid {uid}, {}, opened {age})",
            if service.is_empty() { &kind } else { &service }
        ));
    }

    if found.is_empty() {
        Reading::no("no remote user session")
    } else {
        Reading::yes(found.join("; "))
    }
}

/// [FACT-11]: true iff a `block`-mode logind inhibitor covers `sleep` or `shutdown`.
///
/// **[FACT-12] applies to the caller, not to this function.** The daemon reads inhibitors
/// as a signal it chooses to honour and never as a mechanism it relies on. Four separate
/// measurements are behind that: the software this daemon most needs to not interrupt —
/// a game platform mid-download — declares no inhibitor at all, and the desktop's own
/// power-inhibit interfaces reported `false` and empty lists while an inhibit was
/// genuinely held. Anything that trusted inhibitors to prevent a suspend would be trusting
/// a signal that is absent exactly when it matters.
pub async fn inhibitor(ctx: &Context<'_>) -> Reading {
    let manager = match LogindManagerProxy::new(ctx.bus).await {
        Ok(m) => m,
        Err(err) => return Reading::absent(format!("no session manager: {err}")),
    };

    let blocked = match manager.block_inhibited().await {
        Ok(v) => v,
        Err(err) => return Reading::doubt(format!("cannot read BlockInhibited: {err}")),
    };

    // Only `sleep` and `shutdown`. `idle` in particular is NOT included: an idle
    // inhibitor asks the session to not go idle, which is a statement about the screen
    // saver, not about the machine's power state, and every media player sets it.
    let relevant: Vec<&str> = blocked
        .split(':')
        .filter(|w| *w == "sleep" || *w == "shutdown")
        .collect();

    if relevant.is_empty() {
        return Reading::no(if blocked.is_empty() {
            "no block inhibitor".to_owned()
        } else {
            // Naming what was ignored matters: somebody debugging a machine that will not
            // sleep needs to see that a `handle-lid-switch` lock was found and correctly
            // disregarded, rather than wonder whether it was seen at all.
            format!("block inhibitors present but not on sleep/shutdown: {blocked}")
        });
    }

    // Who holds it. Best-effort: the property above already decided the answer, so a
    // failure here degrades the message rather than the verdict.
    let who = match manager.list_inhibitors().await {
        Ok(list) => list
            .into_iter()
            .filter(|(what, _, _, mode, _, _)| {
                mode == "block" && what.split(':').any(|w| w == "sleep" || w == "shutdown")
            })
            .map(|(what, who, why, _, uid, pid)| {
                format!("{who} (uid {uid}, pid {pid}): {why} [{what}]")
            })
            .collect::<Vec<_>>()
            .join("; "),
        Err(_) => String::new(),
    };

    let held = relevant.join("+");
    if who.is_empty() {
        Reading::yes(format!("block inhibitor on {held}"))
    } else {
        Reading::yes(format!("block inhibitor on {held}: {who}"))
    }
}

/// The uids of open **graphical** user sessions.
///
/// Two callers, one definition. `media_playing` uses it to find the session buses a player
/// could be on; `human_active` uses it to tell a *fault* from a *machine shape* — an
/// agentless machine with a compositor running is a dead agent ([HUM-4], case 3), and an
/// agentless machine with no compositor at all is a headless box that must still be able
/// to finish its job and sleep ([CLK-7]). Answering that question two different ways in
/// two places is how those two cases end up conflated.
pub async fn graphical_uids(bus: &zbus::Connection) -> Result<Vec<u32>, String> {
    let manager = LogindManagerProxy::new(bus)
        .await
        .map_err(|err| format!("no session manager: {err}"))?;
    let sessions = manager
        .list_sessions()
        .await
        .map_err(|err| format!("logind did not answer ListSessions: {err}"))?;

    let mut uids = Vec::new();
    for (_id, uid, _user, _seat, path) in sessions {
        let Ok(builder) = LogindSessionProxy::builder(bus).path(path) else {
            continue;
        };
        let Ok(session) = builder.build().await else {
            continue;
        };
        let kind = session.type_().await.unwrap_or_default();
        if (kind == "wayland" || kind == "x11") && !uids.contains(&uid) {
            uids.push(uid);
        }
    }
    Ok(uids)
}

/// Whether a D-Bus error means "that object is gone", which is a race and not a fault.
fn is_gone(err: &zbus::Error) -> bool {
    matches!(
        err,
        zbus::Error::MethodError(name, _, _)
            if name.as_str() == "org.freedesktop.DBus.Error.UnknownObject"
                || name.as_str() == "org.freedesktop.DBus.Error.NoSuchUnit"
                || name.as_str() == "org.freedesktop.DBus.Error.UnknownInterface"
    )
}

/// The conflict scan of [OBS-3].11: every other candidate owner of this machine's power
/// state.
///
/// Cheap — one property pair, one bus-name list, one process scan — and it checks the
/// conclusion that matters more than any single bug: that exactly one thing decides when
/// this machine sleeps. A release protocol that gates on this check tests nothing unless
/// the check is required, so it is not optional and not behind a flag.
pub async fn conflict_scan(bus: &zbus::Connection) -> Vec<String> {
    let mut found = Vec::new();

    if let Ok(manager) = LogindManagerProxy::new(bus).await {
        match (
            manager.idle_action().await,
            manager.idle_action_usec().await,
        ) {
            (Ok(action), Ok(usec)) if action != "ignore" && usec != u64::MAX => {
                found.push(format!(
                    "logind IdleAction={action} after {}s -- logind will act on its own",
                    usec / 1_000_000
                ));
            }
            _ => {}
        }
    }

    // Desktop power managers that own a suspend timer of their own. Owning one of these
    // names is not itself a fault -- they do many other things -- but it is the first
    // place to look when two things disagree about when a machine should sleep.
    const POWER_BUS_NAMES: [&str; 4] = [
        "org.freedesktop.PowerManagement",
        "org.kde.Solid.PowerManagement",
        "org.gnome.SettingsDaemon.Power",
        "org.freedesktop.ScreenSaver",
    ];
    if let Ok(proxy) = zbus::fdo::DBusProxy::new(bus).await
        && let Ok(names) = proxy.list_names().await
    {
        for candidate in POWER_BUS_NAMES {
            if names.iter().any(|n| n.as_str() == candidate) {
                found.push(format!(
                    "{candidate} is owned -- a desktop power manager is running"
                ));
            }
        }
    }

    for helper in ["swayidle", "hypridle", "xautolock", "xidlehook"] {
        if crate::proc::any_process_named(helper) {
            found.push(format!("{helper} is running -- a second idle helper"));
        }
    }

    found
}

/// Whether a reading should be treated as holding the machine awake, for the sweep
/// predicate in [`crate::engine`].
#[must_use]
pub fn is_holding(state: FactState) -> bool {
    matches!(state, FactState::True | FactState::Indeterminate)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// An arbitrary but fixed "now", so the tests say nothing about the wall clock.
    const NOW_SECS: u64 = 1_800_000_000;

    fn now() -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(NOW_SECS)
    }

    fn opened_secs_ago(secs: u64) -> u64 {
        (NOW_SECS - secs) * 1_000_000
    }

    #[test]
    fn a_session_age_is_elapsed_time_and_not_a_timestamp() {
        assert_eq!(session_age(opened_secs_ago(0), now()), "0s ago");
        assert_eq!(session_age(opened_secs_ago(120), now()), "2m ago");
        // `ago` only reaches for hours past ninety minutes, so two hours is the shortest
        // span that exercises the format the bug was reported in.
        assert_eq!(session_age(opened_secs_ago(7200), now()), "2h00m ago");
    }

    /// The regression this function was extracted for. Two sessions opened a minute apart:
    /// the newer one must read as the younger one. Handing logind's absolute timestamp to
    /// `ago` inverted this, because the later session has the larger timestamp.
    #[test]
    fn a_newer_session_reads_younger_than_an_older_one() {
        let older = session_age(opened_secs_ago(7260), now());
        let newer = session_age(opened_secs_ago(7200), now());
        assert_eq!((older.as_str(), newer.as_str()), ("2h01m ago", "2h00m ago"));
    }

    #[test]
    fn a_session_stamped_in_the_future_is_not_rendered_as_an_age() {
        let ahead = (NOW_SECS + 60) * 1_000_000;
        assert_eq!(session_age(ahead, now()), "age unknown");
    }
}
