//! `idlectl-agent` — the unprivileged per-session agent.
//!
//! # What it is for
//!
//! Two things a root daemon cannot do, both because they are properties of a *session*
//! rather than of the machine:
//!
//! 1. **Observe real human input.** On Wayland only the compositor knows, and it tells
//!    nobody who has not asked through `ext-idle-notify-v1`; on X11 the server maintains
//!    the counter. Either way the signal is session-scoped.
//! 2. **Read MPRIS.** A root daemon cannot connect to a user's session bus at all —
//!    measured, the bus authenticates by uid over `EXTERNAL` and closes the connection,
//!    and no amount of file permission changes that. See [`mpris`].
//! 3. **Blank the screen.** On Wayland only the compositor, or whoever holds DRM master,
//!    can turn an output off, and a root daemon has no business being either.
//!
//! The third is the only one that is not reporting. The agent commands nothing but its own
//! screen, and only when the caller is the daemon.
//!
//! # What it is not for
//!
//! Everything else. The agent cannot suspend, hibernate or power off anything, and no
//! configuration can give it those. It reports two session-scoped facts and commands
//! nothing but its own screen, and only when the caller is the daemon — the bus policy
//! restricts its interface to uid 0.
//!
//! In particular it does NOT attribute GPU load or look for running games. Those are read
//! from `/proc` by the daemon, which is privileged and does not have to take an
//! unprivileged process's word for what is running on the machine.
//!
//! Two guards make that hold in practice rather than on paper. The agent owns **no
//! well-known bus name**, so an ordinary user cannot squat the interface by claiming one;
//! it registers with the daemon, and the daemon calls back on the unique connection name
//! the bus assigned. And the interface it exports has exactly two methods, both about the
//! screen.
//!
//! # The heartbeat
//!
//! `ReportSession` is sent at the interval the daemon returns from `RegisterAgent`, and
//! that periodic report is not redundant with reporting on change. [CLK-5] requires the
//! human-presence signal to disappear on failure, because **while somebody is holding a
//! controller an idle notifier emits nothing at all**. A crashed agent and a person
//! playing a game look identical from outside; only the missing heartbeat tells them apart.

#![forbid(unsafe_code)]

mod backend;
mod mpris;
mod wayland;
mod x11;

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::backend::{Backend, Idle};

const MANAGER_SERVICE: &str = "io.github.ericcanas.Idlectl1";
const MANAGER_PATH: &str = "/io/github/ericcanas/Idlectl1";
const AGENT_PATH: &str = "/io/github/ericcanas/Idlectl1/Agent";

#[zbus::proxy(
    interface = "io.github.ericcanas.Idlectl1.Manager",
    default_service = "io.github.ericcanas.Idlectl1",
    default_path = "/io/github/ericcanas/Idlectl1",
    gen_blocking = false
)]
trait Manager {
    /// Returns the interval, in microseconds, at which `ReportSession` must be called.
    fn register_agent(&self, session_id: &str, can_blank: bool) -> zbus::Result<u64>;
    fn report_session(&self, idle_usec: u64, media_playing: &str) -> zbus::Result<()>;
    #[allow(dead_code)]
    fn unregister_agent(&self) -> zbus::Result<()>;
}

#[derive(Debug, Parser)]
#[command(
    name = "idlectl-agent",
    version,
    about = "Reports this session's idle state to idlepolicyd and blanks its screen on request.",
    long_about = None
)]
struct Args {
    /// Force a backend instead of detecting one: `wayland` or `x11`.
    #[arg(long, value_name = "KIND")]
    backend: Option<String>,

    /// Report what this session offers and exit, without registering.
    ///
    /// The first thing to run when `idlectl doctor` says `screen_off` is unavailable or
    /// `human_active` is indeterminate: it answers "can this session do it at all?"
    /// separately from "is the agent working?".
    #[arg(long)]
    probe: bool,
}

/// The agent's own D-Bus interface, exported on the **system** bus.
///
/// System, not session, and it is the one thing here that looks wrong and is not: the
/// daemon runs as root outside every session, so a session bus is exactly the place it
/// cannot reach. The narrowness comes from the bus policy (uid 0 only) and from the
/// interface having nothing else in it.
struct AgentInterface {
    backend: Arc<dyn Backend>,
    blanked: Arc<std::sync::atomic::AtomicBool>,
}

#[zbus::interface(name = "io.github.ericcanas.Idlectl1.Agent")]
impl AgentInterface {
    /// Blank the outputs of this session. Idempotent.
    async fn blank(&self) -> zbus::fdo::Result<()> {
        self.backend
            .set_blank(true)
            .map_err(zbus::fdo::Error::Failed)?;
        self.blanked
            .store(true, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    /// Unblank.
    ///
    /// Called when a new origin re-arms the schedule, and on the agent's own shutdown, so
    /// that stopping the agent can never leave a session dark with nothing left running
    /// that knows how to bring it back.
    async fn unblank(&self) -> zbus::fdo::Result<()> {
        self.backend
            .set_blank(false)
            .map_err(zbus::fdo::Error::Failed)?;
        self.blanked
            .store(false, std::sync::atomic::Ordering::Relaxed);
        Ok(())
    }

    #[zbus(property)]
    async fn blanked(&self) -> bool {
        self.blanked.load(std::sync::atomic::Ordering::Relaxed)
    }

    #[zbus(property)]
    async fn can_blank(&self) -> bool {
        self.backend.can_blank()
    }
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
    let backend = pick_backend(args.backend.as_deref())?;
    info!(backend = backend.describe(), "session backend ready");

    if args.probe {
        println!("backend   {}", backend.describe());
        println!("can_blank {}", backend.can_blank());
        println!(
            "idle      {}",
            match backend.idle() {
                Idle::For(d) => format!("{}s", d.as_secs()),
                Idle::Unknown => "unknown (the idle protocol is not answering)".to_owned(),
            }
        );
        return Ok(());
    }

    async_io::block_on(serve(backend))
}

fn pick_backend(forced: Option<&str>) -> Result<Arc<dyn Backend>> {
    match forced {
        Some("wayland") => Ok(Arc::new(
            wayland::WaylandBackend::connect().map_err(anyhow::Error::msg)?,
        )),
        Some("x11") => Ok(Arc::new(
            x11::X11Backend::connect().map_err(anyhow::Error::msg)?,
        )),
        Some(other) => anyhow::bail!("unknown backend {other:?}: expected `wayland` or `x11`"),
        None => {
            // Wayland first, even where an X server is also reachable: inside a Wayland
            // session the X server is XWayland, and its idle counter is fed by the same
            // compositor. Asking the compositor directly is both more accurate and the
            // protocol [CLK-4] names.
            let wayland_error = match wayland::WaylandBackend::connect() {
                Ok(backend) => return Ok(Arc::new(backend)),
                Err(err) => err,
            };
            match x11::X11Backend::connect() {
                Ok(backend) => Ok(Arc::new(backend)),
                Err(x11_error) => anyhow::bail!(
                    "no usable session backend.\n  wayland: {wayland_error}\n  x11: {x11_error}\n\n\
                     This agent must run inside a graphical session, and on a machine with \
                     none it should not run at all. idlepolicyd treats an agentless machine \
                     with no compositor as having nobody at the keyboard, which is correct, \
                     and an agentless machine WITH a compositor as a fault, which is also \
                     correct. Starting this and letting it fail turns the first case into \
                     the second."
                ),
            }
        }
    }
}

async fn serve(backend: Arc<dyn Backend>) -> Result<()> {
    let session_id = std::env::var("XDG_SESSION_ID").unwrap_or_else(|_| "unknown".to_owned());
    let blanked = Arc::new(std::sync::atomic::AtomicBool::new(false));

    let connection = zbus::connection::Builder::system()
        .context("cannot reach the D-Bus system bus")?
        .serve_at(
            AGENT_PATH,
            AgentInterface {
                backend: Arc::clone(&backend),
                blanked: Arc::clone(&blanked),
            },
        )
        .context("cannot export the agent object")?
        // Deliberately no `.name(...)`: the agent owns no well-known name. The daemon calls
        // back on the unique name the bus assigned to this connection, so there is nothing
        // for another user to claim and no per-user ownership rule to write.
        .build()
        .await
        .context("cannot connect to the system bus")?;

    let manager = ManagerProxy::new(&connection).await.with_context(|| {
        format!("cannot reach idlepolicyd on {MANAGER_SERVICE} at {MANAGER_PATH}")
    })?;

    let interval = register(&manager, &session_id, backend.can_blank()).await?;
    info!(
        session = session_id,
        can_blank = backend.can_blank(),
        interval_s = interval.as_secs(),
        "registered with idlepolicyd"
    );

    let mut last_state: Option<bool> = None;
    loop {
        let idle = backend.idle();
        let (media, media_detail) = mpris::read().await;

        // One log line per transition into the broken state, not one per report. A session
        // whose protocol died would otherwise write a line every thirty seconds for as
        // long as the user stays logged in.
        let broken = idle == Idle::Unknown;
        if last_state != Some(broken) {
            if broken {
                warn!("this session's idle protocol stopped answering; reporting unknown");
            } else if last_state.is_some() {
                info!("the idle protocol is answering again");
            }
            last_state = Some(broken);
        }

        if let Err(err) = manager.report_session(idle.to_usec(), media).await {
            // A daemon that restarted has forgotten this agent, and the report is refused
            // with "this connection has not registered". Re-registering is the whole of
            // the recovery: nothing here holds state worth preserving across it.
            warn!(error = %err, media = media_detail, "report refused; re-registering");
            match register(&manager, &session_id, backend.can_blank()).await {
                Ok(_) => info!("re-registered with idlepolicyd"),
                Err(err) => warn!(error = %format!("{err:#}"), "idlepolicyd is not reachable"),
            }
        }

        // Report sooner than the heartbeat while the session is active, so that a machine
        // somebody has just touched is known to be occupied within seconds rather than
        // within a heartbeat. Costs one D-Bus call on a machine in use and nothing at all
        // on an idle one, which is the machine that matters for power.
        let wait = match idle {
            Idle::For(d) if d < Duration::from_secs(60) => interval.min(Duration::from_secs(5)),
            _ => interval,
        };
        async_io::Timer::after(wait).await;
    }
}

async fn register(
    manager: &ManagerProxy<'_>,
    session_id: &str,
    can_blank: bool,
) -> Result<Duration> {
    let usec = manager
        .register_agent(session_id, can_blank)
        .await
        .context("idlepolicyd refused the registration")?;
    // Clamped rather than trusted. The daemon is the authority on the interval, but a zero
    // from a future version would turn this into a busy loop inside somebody's session,
    // and an hour would make every agent look dead.
    Ok(Duration::from_micros(usec.clamp(1_000_000, 300_000_000)))
}
