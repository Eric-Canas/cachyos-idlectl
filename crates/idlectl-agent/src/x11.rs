//! The X11 backend: the XScreenSaver idle counter for input, DPMS for blanking.
//!
//! [CLK-4] names this as the X11 equivalent of `ext-idle-notify-v1`, and for the same
//! reason: `MIT-SCREEN-SAVER`'s `QueryInfo` returns milliseconds since the last **real**
//! input event, maintained by the server itself. It is not a heartbeat and it cannot be
//! reset by a program that merely wants to keep the machine awake.
//!
//! Unlike the Wayland side this is a poll, because the extension exposes a counter rather
//! than transitions. The poll runs at the heartbeat interval the agent is already awake
//! for, so it costs one round trip every thirty seconds and adds no wakeups of its own.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use x11rb::connection::Connection as _;
use x11rb::protocol::dpms::ConnectionExt as _;
use x11rb::protocol::screensaver::ConnectionExt as _;
use x11rb::rust_connection::RustConnection;

use crate::backend::{Backend, Idle};

pub struct X11Backend {
    connection: Arc<Mutex<RustConnection>>,
    root: u32,
    can_blank: bool,
}

impl X11Backend {
    pub fn connect() -> Result<Self, String> {
        let (connection, screen_index) =
            RustConnection::connect(None).map_err(|err| format!("no X11 session: {err}"))?;
        let root = connection.setup().roots[screen_index].root;

        // The idle counter is the whole point of this backend. Without the extension there
        // is no way to observe real input, and an agent that reported "unknown" forever
        // would wedge the machine awake with the fault attributed to the wrong place.
        connection
            .screensaver_query_version(1, 0)
            .map_err(|err| format!("MIT-SCREEN-SAVER is not available: {err}"))?
            .reply()
            .map_err(|err| format!("MIT-SCREEN-SAVER did not answer: {err}"))?;

        // DPMS is optional. Its absence means `screen_off` is UNAVAILABLE as an action,
        // which the daemon reports rather than silently treating as done.
        let can_blank = connection
            .dpms_capable()
            .ok()
            .and_then(|cookie| cookie.reply().ok())
            .is_some_and(|reply| reply.capable);

        Ok(X11Backend {
            connection: Arc::new(Mutex::new(connection)),
            root,
            can_blank,
        })
    }
}

impl Backend for X11Backend {
    fn idle(&self) -> Idle {
        let Ok(connection) = self.connection.lock() else {
            return Idle::Unknown;
        };
        let Ok(cookie) = connection.screensaver_query_info(self.root) else {
            return Idle::Unknown;
        };
        // A server that stopped answering is unknown, not idle. Reporting a large idle
        // time here would permit a sleep on the strength of an answer nobody gave.
        match cookie.reply() {
            Ok(info) => Idle::For(Duration::from_millis(u64::from(info.ms_since_user_input))),
            Err(_) => Idle::Unknown,
        }
    }

    fn set_blank(&self, blank: bool) -> Result<(), String> {
        if !self.can_blank {
            return Err("this X server has no DPMS".to_owned());
        }
        let connection = self
            .connection
            .lock()
            .map_err(|_| "the X11 connection is poisoned".to_owned())?;

        // Enabling DPMS before forcing a level is not optional: forcing a level on a
        // server where DPMS is disabled succeeds and does nothing, which is the worst of
        // both -- a lit panel and a daemon that believes it is dark.
        connection
            .dpms_enable()
            .map_err(|err| err.to_string())?
            .check()
            .map_err(|err| err.to_string())?;

        let level = if blank {
            x11rb::protocol::dpms::DPMSMode::OFF
        } else {
            x11rb::protocol::dpms::DPMSMode::ON
        };
        connection
            .dpms_force_level(level)
            .map_err(|err| err.to_string())?
            .check()
            .map_err(|err| err.to_string())?;
        connection.flush().map_err(|err| err.to_string())?;
        Ok(())
    }

    fn can_blank(&self) -> bool {
        self.can_blank
    }

    fn describe(&self) -> String {
        format!(
            "x11, MIT-SCREEN-SAVER{}",
            if self.can_blank {
                ", DPMS"
            } else {
                ", no DPMS"
            }
        )
    }
}
