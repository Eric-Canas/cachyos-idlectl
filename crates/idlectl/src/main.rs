//! `idlectl` -- the command-line client.
//!
//! # Status
//!
//! Pre-release skeleton. `check-config` works and is genuinely useful; every command that
//! needs a running `idlepolicyd` reports that it is not implemented yet.
//!
//! The command surface below is the design, not a placeholder. It is written out in full
//! so that `--help` documents the intended contract while the daemon is being built.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use idlectl_config::{ADMIN_CONFIG, DROPIN_DIR, Source, VENDOR_CONFIG};

/// The well-known bus name of the daemon. See the note in `idlepolicyd` on why it is
/// spelled this way.
const BUS_NAME: &str = "io.github.ericcanas.Idlectl1";

#[derive(Debug, Parser)]
#[command(
    name = "idlectl",
    version,
    about = "Inspect and control this machine's idle and power policy.",
    long_about = "idlectl talks to idlepolicyd, the daemon that decides when this machine \
                  may blank its screen, suspend, hibernate or power off.\n\n\
                  It is a policy tool, not a CPU tuning tool. For frequency scaling and \
                  power profiles, see tlp(8), tuned(8) or powerprofilesctl(1)."
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

// The arguments of every daemon-backed command are parsed but not yet read, because the
// daemon side does not exist. They are written out in full anyway: `--help` is the design
// document a user actually reads, and shipping a CLI whose help text grows later means
// shipping one whose contract was never pinned down.
#[allow(dead_code)]
#[derive(Debug, Subcommand)]
enum Command {
    /// Show what the daemon currently believes and what it will do next.
    Status(OutputArgs),

    /// Explain why the machine is or is not allowed to perform an action.
    ///
    /// Prints, for each action, the composed deadline, which clock it was measured on,
    /// and every block holding it back.
    Explain {
        /// Restrict the explanation to one action: screen_off, suspend, hibernate,
        /// poweroff. Omit for all four.
        action: Option<String>,

        #[command(flatten)]
        output: OutputArgs,
    },

    /// Report which detectors work on this machine and which capabilities are missing.
    ///
    /// Facts whose capability is absent read as `unavailable` and behave as false. Facts
    /// whose detector is broken read as `indeterminate` and veto. doctor is how the
    /// difference is confirmed rather than assumed.
    Doctor(OutputArgs),

    /// Ask the machine to rest now.
    Rest(RestArgs),

    /// Hold or inspect leases: "I am working, do not sleep".
    #[command(subcommand)]
    Lease(LeaseCommand),

    /// Reload the configuration without restarting the daemon.
    Reload,

    /// Parse, merge and validate configuration files without contacting the daemon.
    ///
    /// With no arguments, checks the layers the daemon would read: the vendor default,
    /// the administrator file if present, and every `*.toml` drop-in in filename order.
    CheckConfig {
        /// Files to check, in layer order. Overrides the default layer list.
        #[arg(value_name = "FILE")]
        files: Vec<PathBuf>,

        #[command(flatten)]
        output: OutputArgs,
    },
}

#[derive(Debug, Clone, Args)]
struct OutputArgs {
    /// Emit machine-readable JSON instead of text.
    #[arg(long)]
    json: bool,
}

#[allow(dead_code)]
#[derive(Debug, Args)]
struct RestArgs {
    /// Which action to request. Defaults to suspend.
    #[arg(default_value = "suspend")]
    action: String,

    /// Satisfy the base schedule and the post-resume settle window immediately, instead
    /// of waiting for them to elapse.
    ///
    /// This does NOT collapse condition blocks. A running game, an active download, a
    /// held lease or an open remote session are all still evaluated normally, each on its
    /// own clock. `--now` is what a remote relay sends when it is finished with a
    /// machine; it must never be able to suspend one that is in the middle of something.
    #[arg(long)]
    now: bool,

    /// Override every block, including the human_active floor.
    ///
    /// Deliberately separate from `--now`, deliberately named differently, gated by its
    /// own polkit action, and always logged with the reason. This is the only way to
    /// suspend a machine somebody is actively using, and it should feel like it.
    #[arg(long, requires = "why")]
    force: bool,

    /// Reason recorded in the journal. Required by --force.
    #[arg(long, value_name = "TEXT")]
    why: Option<String>,
}

#[allow(dead_code)]
#[derive(Debug, Subcommand)]
enum LeaseCommand {
    /// Take a lease. The machine will not sleep while it is held.
    ///
    /// The lease is released when the returned handle is closed, so a job that crashes
    /// cannot pin a machine awake forever, and it also expires after its TTL.
    Acquire {
        /// Identifier for the holder, shown by `idlectl lease list`.
        #[arg(value_name = "ID")]
        id: String,

        /// Time to live. The lease expires on its own after this.
        #[arg(long, value_name = "DURATION", default_value = "1h")]
        ttl: String,

        /// Reason, shown by `idlectl lease list` and recorded in the journal.
        #[arg(long, value_name = "TEXT")]
        why: Option<String>,
    },

    /// Release a lease early.
    Release {
        /// The identifier used when the lease was acquired.
        #[arg(value_name = "ID")]
        id: String,
    },

    /// List the leases currently held.
    List(OutputArgs),
}

fn main() -> std::process::ExitCode {
    match run(Cli::parse()) {
        Ok(()) => std::process::ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("idlectl: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<()> {
    match cli.command {
        Command::CheckConfig { files, output } => check_config(files, output.json),

        Command::Status(_)
        | Command::Explain { .. }
        | Command::Doctor(_)
        | Command::Rest(_)
        | Command::Lease(_)
        | Command::Reload => not_implemented(),
    }
}

/// Confirms the bus is reachable, then reports honestly that the daemon side does not
/// exist yet.
///
/// Checking the bus first is not ceremony: the most likely reason a command fails on a
/// fresh install is that nothing enabled the service, and that deserves a different
/// message from a genuine bug.
fn not_implemented() -> Result<()> {
    // The connection is dropped immediately and on purpose: we only want to know whether
    // the bus answers. `let _ =` is explicit about that, because dropping a Connection
    // closes its socket and a silent discard would read like an oversight.
    let _ = async_io::block_on(async {
        zbus::Connection::system()
            .await
            .context("cannot reach the D-Bus system bus")
    })?;

    bail!(
        "this command needs idlepolicyd, which is not implemented in this pre-release.\n\
         Note that the package does not enable the service; enabling it is a deliberate \
         act:\n    systemctl enable --now idlepolicyd.service\n\
         `idlectl check-config` works today and needs no daemon.\n\
         (bus name: {BUS_NAME})"
    )
}

fn check_config(files: Vec<PathBuf>, json: bool) -> Result<()> {
    let paths = if files.is_empty() {
        default_layers()
    } else {
        files
    };

    if paths.is_empty() {
        bail!(
            "no configuration found. Expected the vendor default at {VENDOR_CONFIG}; \
             pass explicit files to check something else."
        );
    }

    let mut sources = Vec::with_capacity(paths.len());
    for path in &paths {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("cannot read {}", path.display()))?;
        sources.push(Source::new(path.display().to_string(), text));
    }

    // [CFG-16]: loading never fails outright -- a bad value drops its own file and is
    // reported as a fault. `check-config` is the one place that must still exit non-zero
    // for one, because its whole job is to answer "would the daemon accept this?" before
    // anyone reloads it. A checker that prints a fault and then exits 0 is worse than no
    // checker: it launders a broken file as a good one.
    let (loaded, faults) = idlectl_config::load(&sources);

    if json {
        let report = serde_json::json!({
            "layers": loaded.layers,
            "faults": faults
                .iter()
                .map(|f| serde_json::json!({
                    "source": f.source,
                    "location": f.location,
                    "message": f.detail(),
                }))
                .collect::<Vec<_>>(),
            "warnings": loaded
                .warnings
                .iter()
                .map(|w| serde_json::json!({
                    "source": w.source,
                    "location": w.location,
                    "message": w.message,
                }))
                .collect::<Vec<_>>(),
            "policy": loaded.policy,
        });
        println!("{}", serde_json::to_string_pretty(&report)?);
        if !faults.is_empty() {
            bail!("{} configuration fault(s)", faults.len());
        }
        return Ok(());
    }

    for layer in &loaded.layers {
        println!("layer   {layer}");
    }
    println!("min_idle {:?}", loaded.policy.min_idle);
    println!();

    for block in &loaded.policy.blocks {
        let status = if block.enabled { "" } else { "  (disabled)" };
        println!("[{}]  clock = {}{}", block.id, block.clock, status);
        for action in idlectl_policy::Action::ALL {
            if let Some(timeout) = block.timeouts.get(action) {
                println!(
                    "    {:<11} {}",
                    action.name(),
                    idlectl_config::format_timeout(timeout)
                );
            }
        }
    }

    if !loaded.warnings.is_empty() {
        println!();
        for warning in &loaded.warnings {
            println!("warning: {warning}");
        }
    }

    if !faults.is_empty() {
        eprintln!();
        for fault in &faults {
            eprintln!("fault: {fault}");
        }
        eprintln!();
        eprintln!("The daemon would still run: a faulted file is dropped, not fatal. But it would");
        eprintln!(
            "hold suspend, hibernate and poweroff until this is fixed. screen_off is unaffected."
        );
        bail!("{} configuration fault(s)", faults.len());
    }

    println!();
    println!(
        "{} block(s), configuration is valid.",
        loaded.policy.blocks.len()
    );
    Ok(())
}

/// The layers the daemon would read, skipping the ones that do not exist.
///
/// Duplicated from idlepolicyd rather than shared: idlectl-config is deliberately pure and
/// opens no files, and a two-binary duplication of twenty lines is cheaper than putting a
/// filesystem walk inside the crate the decision loop calls.
fn default_layers() -> Vec<PathBuf> {
    let mut paths = Vec::new();

    let vendor = PathBuf::from(VENDOR_CONFIG);
    if vendor.is_file() {
        paths.push(vendor);
    }

    let admin = PathBuf::from(ADMIN_CONFIG);
    if admin.is_file() {
        paths.push(admin);
    }

    if let Ok(entries) = std::fs::read_dir(DROPIN_DIR) {
        let mut dropins: Vec<PathBuf> = entries
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        dropins.sort();
        paths.extend(dropins);
    }

    paths
}
