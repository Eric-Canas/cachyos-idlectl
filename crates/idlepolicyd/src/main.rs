//! `idlepolicyd` — the resident root daemon that decides when this machine may blank its
//! screen, suspend, hibernate or power off.
//!
//! # The loop
//!
//! One event loop on one reactor ([`async_io`]), waiting on exactly three things:
//!
//! * the D-Bus system bus, for client calls and for logind's `PrepareForSleep`;
//! * an internal wake channel, poked by anything that changes what the daemon knows —
//!   an agent's idle report, a lease taken or released, a reload;
//! * one timer, armed at the earliest pending deadline ([COMP-6]).
//!
//! It is **not** a periodic poll, and §13.4 explains why at length: a five-minute tick
//! misses up to a full period between a condition clearing and the machine acting on it,
//! and the tick missed during a sleep runs at resume, landing the evaluation in the same
//! second as the wake. A backstop sweep exists for the detectors that genuinely cannot be
//! watched, and it is armed **only when one of them is currently holding the machine** —
//! see [`engine::Engine::sweep_needed`]. An idle machine with nothing running arms one
//! timer at its next real deadline and does not wake up in between.
//!
//! # Why one reactor
//!
//! Every source above is a file descriptor and `async-io` already drives all of them. A
//! second runtime would mean a task blocked in one scheduler could stall the loop that
//! decides whether a machine may sleep. zbus is pinned to its `async-io` feature and
//! `cargo deny` refuses to build a tree containing tokio.
//!
//! # Why the daemon never calls `systemctl`
//!
//! Power transitions go to logind over D-Bus, never by shelling out. `systemctl poweroff`
//! invoked without a controlling terminal returns 0 and ignores inhibitors and open
//! sessions entirely, so it is no safety net; the mode that does check refuses whenever any
//! user is logged in, which on a console with autologin is always. There is no usable
//! middle setting, so the decision is made here and only the mechanical transition is
//! delegated ([ACT-1]). Going through logind also means this daemon needs no capabilities
//! of its own — see `CapabilityBoundingSet=` in the unit file.

#![forbid(unsafe_code)]

mod actions;
mod agents;
mod clock;
mod engine;
mod facts;
mod leases;
mod logind;
mod manager;
mod pending;
mod polkit;
mod proc;
mod report;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{Context as _, Result};
use async_lock::Mutex;
use clap::Parser;
use futures_lite::StreamExt as _;
use idlectl_config::{ADMIN_CONFIG, DROPIN_DIR, VENDOR_CONFIG};
use idlectl_policy::Request;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::engine::{Engine, Sources};
use crate::manager::{BUS_NAME, Manager, OBJECT_PATH};

#[derive(Debug, Parser)]
#[command(
    name = "idlepolicyd",
    version,
    about = "Decides when this machine may blank its screen, suspend, hibernate or power off.",
    long_about = None
)]
struct Args {
    /// Vendor default configuration, shipped by the package.
    #[arg(long, value_name = "PATH", default_value = VENDOR_CONFIG)]
    vendor_config: PathBuf,

    /// Administrator configuration. Never created or modified by the package.
    #[arg(long, value_name = "PATH", default_value = ADMIN_CONFIG)]
    config: PathBuf,

    /// Drop-in directory; `*.toml` inside it is applied in filename order.
    #[arg(long, value_name = "DIR", default_value = DROPIN_DIR)]
    config_dir: PathBuf,

    /// Validate the configuration, print the resolved policy and exit.
    #[arg(long)]
    check: bool,

    /// Evaluate and log decisions, but never touch the machine.
    ///
    /// The supported way to run this alongside an existing power manager while migrating:
    /// every decision appears in the journal and nothing is acted on, so the two can be
    /// compared before either is switched off.
    #[arg(long)]
    dry_run: bool,
}

fn main() -> std::process::ExitCode {
    // Timestamps and levels come from the journal, so printing our own would duplicate
    // them on every line. ANSI is off for the same reason: the journal is not a terminal.
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
            // `{err:#}` prints the whole anyhow chain on one line. A daemon that refuses to
            // start must say exactly why in the journal; a unit that fails silently is a
            // feature that does not exist and nobody finds out for months.
            error!("{err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<()> {
    let sources = Sources {
        vendor: args.vendor_config.clone(),
        admin: args.config.clone(),
        dropin_dir: args.config_dir.clone(),
    };

    if args.check {
        let raw = sources.read()?;
        let (loaded, faults) = idlectl_config::load(&raw);
        print_policy(&loaded);
        for fault in &faults {
            eprintln!("fault: {fault}");
        }
        return if faults.is_empty() {
            Ok(())
        } else {
            anyhow::bail!("{} configuration fault(s)", faults.len())
        };
    }

    async_io::block_on(serve(sources, args.dry_run))
}

async fn serve(sources: Sources, dry_run: bool) -> Result<()> {
    let bus = zbus::Connection::system()
        .await
        .context("cannot reach the D-Bus system bus")?;

    // Capacity, not unbounded: every sender uses `try_send` and treats a full channel as
    // "a wake-up is already pending", which is exactly right. An unbounded channel would
    // instead queue thousands of redundant wakeups from a flapping agent.
    let (wake_tx, wake_rx) = async_channel::bounded::<&'static str>(16);

    let engine = Engine::new(sources, bus.clone(), dry_run, wake_tx)?;
    for fault in &engine.faults {
        error!("{fault}");
    }
    if !engine.faults.is_empty() {
        error!(
            faults = engine.faults.len(),
            "running in degraded mode: the sleep actions are held until the configuration is fixed"
        );
    }
    for warning in &engine.warnings {
        warn!("{warning}");
    }
    info!(
        layers = engine.layers.len(),
        blocks = engine.policy.blocks.len(),
        dry_run,
        "configuration loaded"
    );

    let engine = Arc::new(Mutex::new(engine));

    // Serve the object before taking the name. The other order has a window in which a
    // client that saw the name appear calls a method that is not yet exported.
    let connection = zbus::connection::Builder::system()
        .context("cannot reach the D-Bus system bus")?
        .serve_at(
            OBJECT_PATH,
            Manager {
                engine: Arc::clone(&engine),
                bus: bus.clone(),
            },
        )
        .context("cannot export the manager object")?
        .name(BUS_NAME)
        .context("cannot claim the bus name")?
        .build()
        .await
        .with_context(|| format!("cannot own {BUS_NAME}"))?;

    info!(name = BUS_NAME, path = OBJECT_PATH, "bus name acquired");

    // Promptness only. Resume detection does not depend on this signal and cannot: a sleep
    // entered by writing `/sys/power/state` directly never emits it. See `clock`.
    let logind = crate::logind::LogindManagerProxy::new(&bus).await.ok();
    let mut sleep_signal = match &logind {
        Some(manager) => manager.receive_prepare_for_sleep().await.ok(),
        None => {
            warn!("logind is not reachable: this daemon can decide, but cannot act");
            None
        }
    };

    let emitter = connection
        .object_server()
        .interface::<_, Manager>(OBJECT_PATH)
        .await?;

    loop {
        let performed = {
            let mut guard = engine.lock().await;
            guard.tick(&Request::none()).await
        };

        // Signals are emitted outside the lock: a subscriber that is slow to read must not
        // be able to hold up the next evaluation.
        {
            let guard = engine.lock().await;
            if let Some(decision) = &guard.decision {
                let ctx = emitter.signal_emitter();
                if let Some(action) = performed {
                    let reason = report::why_due(decision, action, &guard.policy);
                    let _ = Manager::action_taken(ctx, action.name().to_owned(), reason).await;
                } else {
                    for action in decision.due_actions() {
                        if action == idlectl_policy::Action::ScreenOff {
                            continue;
                        }
                        let held: Vec<String> = decision
                            .get(action)
                            .holding()
                            .iter()
                            .map(ToString::to_string)
                            .collect();
                        let _ = Manager::action_held(ctx, action.name().to_owned(), held).await;
                    }
                }
            }
        }

        let wait = {
            let guard = engine.lock().await;
            guard.next_wakeup()
        };

        let reason = next_event(wait, &wake_rx, sleep_signal.as_mut()).await;
        match reason {
            Event::Timer => {}
            Event::Wake(why) => info!(reason = why, "re-evaluating"),
            Event::SleepStart => {
                info!("logind announced a sleep; standing by");
                // Nothing to do: the machine is about to freeze. Deliberately no action --
                // trying to be helpful here is how a daemon ends up racing the kernel.
                continue;
            }
            Event::Resume => {
                let mut guard = engine.lock().await;
                guard.resume.note_announced_resume(clock::now());
                // The suspend this daemon issued has completed. Until this point a second
                // attempt would be refused by logind with `OperationInProgress`, which is
                // how every ordinary suspend used to log a warning.
                guard.note_transition_finished();
                info!("resume announced by logind");
            }
            Event::BusClosed => {
                warn!("the system bus closed; exiting so systemd can restart us");
                return Ok(());
            }
        }
    }
}

enum Event {
    Timer,
    Wake(&'static str),
    SleepStart,
    Resume,
    BusClosed,
}

/// Waits for whichever comes first: the armed timer, an internal wake, or a sleep signal.
///
/// `wait` of [`None`] means no deadline is pending and no polled fact is holding anything,
/// so there is genuinely nothing to wake up for. The daemon then waits on events alone and
/// costs nothing at all until one arrives.
async fn next_event(
    wait: Option<std::time::Duration>,
    wake: &async_channel::Receiver<&'static str>,
    sleep_signal: Option<&mut crate::logind::PrepareForSleepStream>,
) -> Event {
    let timer = async {
        match wait {
            Some(d) => {
                async_io::Timer::after(d).await;
                Event::Timer
            }
            None => futures_lite::future::pending().await,
        }
    };

    let woken = async {
        match wake.recv().await {
            Ok(why) => Event::Wake(why),
            // Every sender lives as long as the daemon, so this only happens at shutdown.
            Err(_) => futures_lite::future::pending().await,
        }
    };

    let slept = async {
        match sleep_signal {
            Some(stream) => match stream.next().await {
                Some(signal) => match signal.args() {
                    Ok(args) if args.start => Event::SleepStart,
                    Ok(_) => Event::Resume,
                    Err(_) => Event::Timer,
                },
                None => Event::BusClosed,
            },
            None => futures_lite::future::pending().await,
        }
    };

    futures_lite::future::or(timer, futures_lite::future::or(woken, slept)).await
}

fn print_policy(loaded: &idlectl_config::Loaded) {
    println!("layers:");
    for layer in &loaded.layers {
        println!("  {layer}");
    }
    println!("min_idle: {:?}", loaded.policy.min_idle);
    println!("blocks:");
    for block in &loaded.policy.blocks {
        let status = if block.enabled { "" } else { " (disabled)" };
        println!("  [{}] clock={}{}", block.id, block.clock, status);
        for action in idlectl_policy::Action::ALL {
            if let Some(timeout) = block.timeouts.get(action) {
                println!(
                    "      {:<11} {}",
                    action.name(),
                    idlectl_config::format_timeout(timeout)
                );
            }
        }
    }
}
