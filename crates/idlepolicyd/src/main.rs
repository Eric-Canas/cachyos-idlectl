//! `idlepolicyd` -- the resident root daemon.
//!
//! # Status
//!
//! Pre-release skeleton. It reads and validates configuration, takes its D-Bus name and
//! then sits still. **It never acts on the machine.** That is the deliberately safe
//! half-built state: a daemon that stays awake wrongly costs some electricity, and one
//! that sleeps wrongly loses work.
//!
//! # What it will do
//!
//! One event loop on one reactor ([`async_io`]), waiting on:
//!
//! * the D-Bus system bus, for logind's `PrepareForSleep` signal and for client calls;
//! * inotify on the config directory, for reload-on-change;
//! * a udev netlink socket, for input-device arrival and removal;
//! * a timer armed at [`idlectl_policy::Decision::next_deadline`].
//!
//! On every wakeup it refreshes the [`idlectl_policy::FactSet`], reads one
//! [`idlectl_policy::ClockSnapshot`], calls [`idlectl_policy::resolve`], applies
//! `screen_off` if it is due, and asks logind to perform
//! [`idlectl_policy::Decision::shallowest_sleep_due`] if anything deeper is.
//!
//! Shallowest, not deepest, and that direction is the whole point: with both `suspend` and
//! `poweroff` permitted, suspending is recoverable in seconds and powering off is not.
//! Overshooting the action is the expensive error, so the engine never picks the deeper
//! one just because its deadline also happens to have passed.
//!
//! # Why one reactor
//!
//! Every one of those sources is a file descriptor, and `async-io` already drives all of
//! them. A second runtime would mean a task blocked in one scheduler could stall the loop
//! that decides whether a machine may sleep. zbus is pinned to its `async-io` feature and
//! `cargo deny` refuses to build a tree containing tokio.
//!
//! # Why the daemon never calls `systemctl`
//!
//! Power transitions go to logind over D-Bus (`Suspend`, `Hibernate`, `PowerOff`), never
//! by shelling out. `systemctl poweroff` invoked without a controlling terminal returns 0
//! and ignores inhibitors and open sessions entirely, so it cannot be used by anything
//! that claims to arbitrate. Going through logind also means this daemon needs no
//! capabilities of its own: see the `CapabilityBoundingSet=` line in the unit file.

#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use clap::Parser;
use idlectl_config::{ADMIN_CONFIG, DROPIN_DIR, Source, VENDOR_CONFIG};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

/// The well-known bus name.
///
/// Lower-case and without a hyphen on purpose. The D-Bus specification restricts the
/// elements of an interface name to `[A-Za-z0-9_]`, so the forge handle `Eric-Canas`
/// cannot appear literally. This spelling is permanent: it is baked into the bus policy
/// filename, the polkit action ids, the introspection XML and every client.
const BUS_NAME: &str = "io.github.ericcanas.Idlectl1";

/// The manager object path.
const OBJECT_PATH: &str = "/io/github/ericcanas/Idlectl1";

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
            // `{err:#}` prints the whole anyhow chain on one line. A daemon that refuses
            // to start must say exactly why in the journal; a unit that fails silently is
            // a feature that does not exist and nobody finds out for months.
            error!("{err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(args: Args) -> Result<()> {
    let sources = collect_sources(&args)?;
    // [CFG-16]: a bad value drops its own file and records a fault. It MUST NOT terminate
    // the daemon and MUST NOT prevent evaluation -- that requirement exists because a
    // single unparseable value once killed a decider outright, before it had evaluated a
    // single veto, and the machine then had no owner of its power at all. The faults ride
    // in `loaded.policy.faults`, and `resolve` turns each one into an infinite floor on
    // suspend, hibernate and poweroff. It deliberately leaves `screen_off` alone: refusing
    // to blank a panel because a config file has a typo would burn it.
    let (loaded, faults) = idlectl_config::load(&sources);

    for fault in &faults {
        error!("{fault}");
    }
    if !faults.is_empty() {
        error!(
            faults = faults.len(),
            "running in degraded mode: sleep actions are held until the configuration is fixed"
        );
    }

    for warning in &loaded.warnings {
        warn!("{warning}");
    }
    info!(
        layers = loaded.layers.len(),
        blocks = loaded.policy.blocks.len(),
        "configuration loaded"
    );

    if args.check {
        print_policy(&loaded);
        return Ok(());
    }

    async_io::block_on(async {
        // Type=dbus in the unit: systemd treats the service as started the moment this
        // name appears on the bus, which is why the daemon needs no sd_notify.
        let _connection = zbus::connection::Builder::system()
            .context("cannot reach the D-Bus system bus")?
            .name(BUS_NAME)
            .context("cannot claim the bus name")?
            .build()
            .await
            .with_context(|| format!("cannot own {BUS_NAME}"))?;

        info!(name = BUS_NAME, path = OBJECT_PATH, "bus name acquired");
        warn!(
            "policy evaluation is not implemented in this pre-release: no screen will be \
             blanked and the machine will not be suspended, hibernated or powered off"
        );
        if args.dry_run {
            info!("--dry-run: decisions would be logged and not acted on");
        }

        // Park. Holding the name without acting is the safe half-built state: nothing
        // else claims to own this machine's power policy, and nothing happens to it.
        futures_lite::future::pending::<()>().await;
        Ok(())
    })
}

/// Builds the layer list: vendor file, admin file, then `*.toml` drop-ins in filename
/// order, skipping the ones that are absent.
///
/// A missing vendor file is an error -- the package installs it, so its absence means a
/// broken installation and guessing a default would hide that. A missing admin file and
/// a missing drop-in directory are both normal.
fn collect_sources(args: &Args) -> Result<Vec<Source>> {
    let mut sources = Vec::new();

    sources.push(read_source(&args.vendor_config).with_context(|| {
        format!(
            "cannot read the vendor configuration at {}",
            args.vendor_config.display()
        )
    })?);

    if args.config.exists() {
        sources.push(read_source(&args.config)?);
    }

    if args.config_dir.is_dir() {
        let mut dropins: Vec<PathBuf> = std::fs::read_dir(&args.config_dir)
            .with_context(|| format!("cannot list {}", args.config_dir.display()))?
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        // Lexicographic, like systemd's own drop-in ordering, so `10-` really does come
        // before `20-` regardless of what the filesystem returns.
        dropins.sort();
        for path in dropins {
            sources.push(read_source(&path)?);
        }
    }

    Ok(sources)
}

fn read_source(path: &Path) -> Result<Source> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    Ok(Source::new(path.display().to_string(), text))
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
