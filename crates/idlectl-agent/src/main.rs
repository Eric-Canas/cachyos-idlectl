//! `idlectl-agent` -- the unprivileged per-session agent.
//!
//! # Status
//!
//! Pre-release skeleton. It connects to both buses and then sits still.
//!
//! # Why a session agent exists at all
//!
//! A root daemon cannot see a desktop session. Three of the facts the policy depends on
//! only exist inside one:
//!
//! * **Human idle.** The compositor owns it. On Wayland it comes from
//!   `ext-idle-notify-v1` or the desktop's own idle service, both of which are session-bus
//!   objects that a root process has no business connecting to.
//! * **Media playback.** MPRIS players advertise themselves on the session bus.
//! * **Steam attribution.** Deciding whether GPU load belongs to a game means matching
//!   processes against the user's own Steam library, which the user can read and root
//!   should not need to.
//!
//! The agent reads those, and pushes them to `idlepolicyd` over the system bus. It has no
//! policy, no timers and no ability to act.
//!
//! # Why not logind's own idle hint
//!
//! Because it was measured not to work. `IdleActionSec` in `logind.conf` is driven by the
//! session's `IdleHint` property, and on a Plasma 6 Wayland session that property stayed
//! `no` permanently -- no desktop component ever set it. An idle action keyed off it
//! would never have fired even once. That refutation is written down here, and in
//! `idlectl.toml(5)`, so that nobody later "simplifies" this project away in favour of a
//! setting that does not work.

#![forbid(unsafe_code)]

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// The system-bus name the agent reports to.
const MANAGER_BUS_NAME: &str = "io.github.ericcanas.Idlectl1";

/// The name the agent takes on the *session* bus, so that a second copy started by a
/// stray `systemctl --user start` cannot report conflicting facts for one session.
const AGENT_BUS_NAME: &str = "io.github.ericcanas.Idlectl1.Agent";

#[derive(Debug, Parser)]
#[command(
    name = "idlectl-agent",
    version,
    about = "Reports session-scoped facts (idle, media, game attribution) to idlepolicyd.",
    long_about = None
)]
struct Args {
    /// Report facts to the journal instead of to the daemon.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> std::process::ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_env("IDLECTL_LOG").unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_ansi(false)
        .without_time()
        .with_writer(std::io::stderr)
        .init();

    match run(Args::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            error!("{err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<()> {
    async_io::block_on(async {
        let _session = zbus::connection::Builder::session()
            .context("cannot reach the D-Bus session bus")?
            .name(AGENT_BUS_NAME)
            .context("cannot claim the session bus name")?
            .build()
            .await
            .with_context(|| format!("cannot own {AGENT_BUS_NAME}"))?;
        info!(name = AGENT_BUS_NAME, "session bus name acquired");

        // The daemon may legitimately not be running -- it is not enabled by the package
        // and a user may never have turned it on. That is a warning, not a failure: an
        // agent that exits here would be restarted forever by the user manager for no
        // reason.
        match zbus::Connection::system().await {
            Ok(_system) => info!(peer = MANAGER_BUS_NAME, "system bus reachable"),
            Err(err) => warn!("system bus unavailable, facts will not be reported: {err}"),
        }

        if args.dry_run {
            info!("--dry-run: facts would be logged and not reported");
        }
        warn!("fact detection is not implemented in this pre-release; no facts are reported");

        futures_lite::future::pending::<()>().await;
        Ok(())
    })
}
