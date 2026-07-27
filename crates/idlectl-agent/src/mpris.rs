//! `media_playing`, read on the agent's own session bus.
//!
//! # Why this is in the agent and not in the daemon
//!
//! Because a root daemon **cannot connect to a user's session bus**, and that is not a
//! permissions problem it can sandbox its way around. Measured: `busctl
//! --address=unix:path=/run/user/1000/bus list` as root returns *"Transport endpoint is
//! not connected"*. The socket is world-writable and root can open it; the bus then
//! authenticates the connection by uid over `EXTERNAL` and closes it, because a session
//! bus serves exactly one user and root is not special to D-Bus.
//!
//! So the fact is read here, where the session bus is simply *ours*, and reported to the
//! daemon over the system bus alongside the idle time. This is also a privilege reduction
//! rather than a workaround: the privileged half of this project now touches no session
//! bus at all.
//!
//! # Why not the desktop's power-inhibition API
//!
//! [FACT-18], and it is measured rather than argued. With a real desktop power-inhibit
//! held: three separate inhibition query interfaces reported `false`, and two returned
//! empty lists. A veto built on any of them would have been dead letter — present in the
//! code, visible in review, and never once true. MPRIS `PlaybackStatus` was verified to
//! change in both directions on the same machine, and every serious player publishes it:
//! browsers, mpv, VLC, and the media-centre clients this matters most for.

/// A fact state, in the same four-name vocabulary the daemon publishes.
///
/// A string rather than a boolean, and that is the point: `unavailable` (this session has
/// no media player) and `indeterminate` (a player is here and would not say what it is
/// doing) are not interchangeable, and collapsing them into `false` is how a veto stops
/// existing without anybody noticing.
pub const UNAVAILABLE: &str = "unavailable";
pub const INDETERMINATE: &str = "indeterminate";
pub const TRUE: &str = "true";
pub const FALSE: &str = "false";

/// Reads every MPRIS player on this session bus.
///
/// Returns the state and the evidence for it. The evidence goes into the daemon's log and
/// into `idlectl doctor`, so "why is this machine awake?" is answered with the name of the
/// player rather than with the word "media".
pub async fn read() -> (&'static str, String) {
    let bus = match zbus::Connection::session().await {
        Ok(bus) => bus,
        // No session bus is [FACT-17]'s UNAVAILABLE case, not a fault. On a machine
        // running a compositor without a session bus there is nothing that could be
        // playing, and doubt here would veto every sleep action forever.
        Err(err) => return (UNAVAILABLE, format!("no session bus: {err}")),
    };

    let dbus = match zbus::fdo::DBusProxy::new(&bus).await {
        Ok(proxy) => proxy,
        Err(err) => {
            return (
                INDETERMINATE,
                format!("cannot query the session bus: {err}"),
            );
        }
    };
    let names = match dbus.list_names().await {
        Ok(names) => names,
        Err(err) => return (INDETERMINATE, format!("ListNames failed: {err}")),
    };

    let mut playing = Vec::new();
    let mut seen = 0usize;
    for name in names {
        let name = name.as_str();
        if !name.starts_with("org.mpris.MediaPlayer2.") {
            continue;
        }
        seen += 1;
        match status(&bus, name).await {
            Ok(Some(state)) if state == "Playing" => {
                playing.push(
                    name.trim_start_matches("org.mpris.MediaPlayer2.")
                        .to_owned(),
                );
            }
            Ok(_) => {}
            // A player that answered its name on the bus and then refused to say what it
            // is doing IS doubt: something normally visible has stopped being visible.
            // This is the one path here that vetoes.
            Err(err) => {
                return (
                    INDETERMINATE,
                    format!("{name} did not report PlaybackStatus: {err}"),
                );
            }
        }
    }

    if playing.is_empty() {
        (FALSE, format!("{seen} MPRIS player(s), none playing"))
    } else {
        (TRUE, format!("playing: {}", playing.join(", ")))
    }
}

async fn status(bus: &zbus::Connection, name: &str) -> zbus::Result<Option<String>> {
    let proxy = zbus::fdo::PropertiesProxy::builder(bus)
        .destination(name.to_owned())?
        .path("/org/mpris/MediaPlayer2")?
        .build()
        .await?;
    let value = proxy
        .get(
            "org.mpris.MediaPlayer2.Player".try_into()?,
            "PlaybackStatus",
        )
        .await?;
    Ok(String::try_from(value).ok())
}
