//! `idlectl` — the command-line client.
//!
//! Everything here goes over D-Bus to `idlepolicyd`. The tool holds no privilege of its
//! own and knows no policy: it cannot suspend a machine, it can only ask, and it is
//! authorized by the same polkit actions any other caller would be. That is deliberate —
//! a command-line tool with a private back door is a second interface to audit.
//!
//! The one exception is `check-config`, which parses files and contacts nothing. It has to
//! work when the daemon will not start, because "the daemon will not start" is usually a
//! configuration file.

#![forbid(unsafe_code)]

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use idlectl_config::{ADMIN_CONFIG, DROPIN_DIR, Source, VENDOR_CONFIG};

/// The well-known bus name of the daemon.
///
/// Lower-case and without a hyphen on purpose. The D-Bus specification restricts the
/// elements of an interface name to `[A-Za-z0-9_]`, so a forge handle containing a hyphen
/// cannot appear literally. This spelling is permanent: it is baked into the bus policy
/// filename, the polkit action ids, the introspection XML and every client.
const BUS_NAME: &str = "io.github.ericcanas.Idlectl1";
const OBJECT_PATH: &str = "/io/github/ericcanas/Idlectl1";

#[zbus::proxy(
    interface = "io.github.ericcanas.Idlectl1.Manager",
    default_service = "io.github.ericcanas.Idlectl1",
    default_path = "/io/github/ericcanas/Idlectl1",
    gen_blocking = false
)]
trait Manager {
    fn explain(&self, action: &str) -> zbus::Result<String>;
    fn doctor(&self) -> zbus::Result<(String, bool)>;
    fn report(&self) -> zbus::Result<String>;
    fn rest(&self, action: &str) -> zbus::Result<bool>;
    fn rest_forced(&self, action: &str, why: &str) -> zbus::Result<()>;
    fn acquire_lease(
        &self,
        who: &str,
        why: &str,
        ttl_usec: u64,
    ) -> zbus::Result<zbus::zvariant::OwnedFd>;
    fn release_lease(&self, who: &str) -> zbus::Result<bool>;
    fn list_leases(&self) -> zbus::Result<Vec<(String, String, u64, u64, u32)>>;
    fn reload(&self) -> zbus::Result<Vec<String>>;

    #[zbus(property)]
    fn version(&self) -> zbus::Result<String>;
    #[zbus(property)]
    fn config_layers(&self) -> zbus::Result<Vec<String>>;
    #[zbus(property)]
    fn facts(&self) -> zbus::Result<Vec<(String, String)>>;
    #[zbus(property)]
    fn deadlines(&self) -> zbus::Result<Vec<(String, u64, bool)>>;
    #[zbus(property)]
    fn next_deadline_usec(&self) -> zbus::Result<u64>;
    #[zbus(property)]
    fn dry_run(&self) -> zbus::Result<bool>;
}

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
    ///
    /// Exits non-zero if any configuration fault, any indeterminate fact, or any standing
    /// hazard is present.
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

#[derive(Debug, Args)]
struct RestArgs {
    /// Which action to request: suspend, hibernate or poweroff.
    ///
    /// Defaults to suspend, always. There is no configurable default rest action: with
    /// poweroff requiring this word to be typed, no request can close somebody's session
    /// unless its sender said so.
    #[arg(long, value_name = "ACTION", default_value = "suspend")]
    action: String,

    /// Accepted and does nothing: this is what `rest` already does.
    ///
    /// Every rest request satisfies the base schedule and the post-resume settle window,
    /// and only those two. It does NOT collapse condition blocks: a running game, an
    /// active download, a held lease and an open remote session are all still evaluated
    /// normally, and any of them can still refuse. The flag is accepted because the
    /// specification names the command `rest --now`.
    #[arg(long, hide = true)]
    now: bool,

    /// Override every block, including the human-presence floor.
    ///
    /// Deliberately separate from the ordinary request, deliberately named differently,
    /// gated by its own polkit action, and always logged with the reason. This is the only
    /// way to suspend a machine somebody is actively using, and it should feel like it.
    ///
    /// What it is actually for: a machine wedged awake by something broken rather than by
    /// something happening — a dead detector, or a configuration file that had to be
    /// rejected. Neither of those is a block, so an ordinary request cannot help.
    #[arg(long, requires = "why")]
    force: bool,

    /// Reason recorded in the journal. Required by --force.
    #[arg(long, value_name = "TEXT")]
    why: Option<String>,
}

#[derive(Debug, Subcommand)]
enum LeaseCommand {
    /// Take a lease. The machine will not sleep while it is held.
    ///
    /// The lease is released when the returned handle is closed, so a job that crashes
    /// cannot pin a machine awake forever, and it also expires after its TTL.
    ///
    /// Because the handle is a file descriptor, `idlectl lease acquire` **stays in the
    /// foreground** holding it: run the work as a child of it, or background it and stop
    /// it when the work is done. A command that exited immediately would release the
    /// lease it had just taken.
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

        /// Run this command, hold the lease for exactly as long as it runs, and exit with
        /// its status.
        #[arg(last = true, value_name = "COMMAND")]
        command: Vec<String>,
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
        Ok(code) => code,
        Err(err) => {
            eprintln!("idlectl: {err:#}");
            std::process::ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<std::process::ExitCode> {
    // check-config contacts nothing, on purpose: it has to work when the daemon will not
    // start, and the usual reason the daemon will not start is a configuration file.
    if let Command::CheckConfig { files, output } = cli.command {
        return check_config(files, output.json);
    }

    async_io::block_on(async {
        let bus = zbus::Connection::system()
            .await
            .context("cannot reach the D-Bus system bus")?;
        let manager = ManagerProxy::new(&bus).await.map_err(not_running)?;

        match cli.command {
            Command::Status(output) => status(&manager, output.json).await,
            Command::Explain { action, output } => explain(&manager, action, output.json).await,
            Command::Doctor(output) => doctor(&manager, output.json).await,
            Command::Rest(args) => rest(&manager, args).await,
            Command::Lease(command) => lease(&manager, command).await,
            Command::Reload => {
                let layers = manager.reload().await?;
                for layer in layers {
                    println!("{layer}");
                }
                Ok(std::process::ExitCode::SUCCESS)
            }
            Command::CheckConfig { .. } => unreachable!("handled above"),
        }
    })
}

/// Turns "nothing owns that bus name" into the sentence that actually helps.
///
/// The most likely reason a command fails on a fresh install is that nothing enabled the
/// service — the package deliberately does not, because installing a program that can
/// suspend a machine is not the same act as permitting it to. That deserves a different
/// message from a genuine bug.
fn not_running(err: zbus::Error) -> anyhow::Error {
    anyhow::anyhow!(
        "cannot reach idlepolicyd ({err}).\n\
         The package does not enable the service; enabling it is a deliberate act:\n\
         \x20   sudo systemctl enable --now idlepolicyd.service\n\
         `idlectl check-config` works today and needs no daemon.\n\
         (bus name: {BUS_NAME}, object: {OBJECT_PATH})"
    )
}

/// Now, on the clock the daemon's deadlines are expressed in.
///
/// Every instant on this interface is an absolute point on `CLOCK_BOOTTIME`, because that
/// is the one clock that keeps counting across a suspend and therefore the only one a
/// policy about sleeping can be written against. Printing one of those raw is honest and
/// useless: "+6907s" is a fact about a machine's boot, not an answer to "how long have I
/// got". Turning it into a duration needs the current value of the same clock, and since
/// this client talks to a daemon over the *system* bus it is by construction on the same
/// machine, so it can simply read it.
///
/// `/proc/uptime` **is** `CLOCK_BOOTTIME`, which is the whole reason it is used here
/// rather than a new dependency for one `clock_gettime` call. Measured on a machine that
/// had suspended: `/proc/uptime` 7384.87, `CLOCK_BOOTTIME` 7384.87, `CLOCK_MONOTONIC`
/// 7270.40 — the 114 s difference being exactly the time it had spent asleep.
fn boot_now_usec() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/uptime").ok()?;
    let secs: f64 = text.split_whitespace().next()?.parse().ok()?;
    if secs.is_finite() && secs >= 0.0 {
        Some((secs * 1_000_000.0) as u64)
    } else {
        None
    }
}

/// A boot-clock instant as "in 12m30s", or `None` when the clock cannot be read.
///
/// Deliberately a gloss on the absolute value rather than a replacement for it: the
/// absolute instant is what the D-Bus property carries, what the journal logs and what a
/// second tool would compare against, so dropping it would make two views of the same
/// machine impossible to line up. This mirrors how `idlectl explain` already prints
/// `origin=+4775s (30s ago)`.
fn in_from_now(deadline_usec: u64) -> Option<String> {
    let now = boot_now_usec()?;
    Some(if deadline_usec <= now {
        "now".to_owned()
    } else {
        format!(
            "in {}",
            human(std::time::Duration::from_micros(deadline_usec - now))
        )
    })
}

/// A duration a person can read at a glance.
///
/// A second copy of `idlepolicyd`'s `report::human`, and not shared with it on purpose.
/// That one lives in a binary crate, so nothing can import it; the alternative is moving
/// presentation into `idlectl-policy`, which is the pure engine and carries no formatting
/// at all. Eight lines duplicated is a smaller price than blurring that boundary.
fn human(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

async fn status(manager: &ManagerProxy<'_>, json: bool) -> Result<std::process::ExitCode> {
    if json {
        println!("{}", manager.report().await?);
        return Ok(std::process::ExitCode::SUCCESS);
    }

    println!("idlepolicyd {}", manager.version().await?);
    if manager.dry_run().await.unwrap_or(false) {
        println!("MODE       dry run: decisions are logged and never applied");
    }
    for layer in manager.config_layers().await? {
        println!("layer      {layer}");
    }

    println!();
    println!("facts");
    for (name, state) in manager.facts().await? {
        println!("  {name:<20} {state}");
    }

    println!();
    println!("actions");
    for (name, deadline, due) in manager.deadlines().await? {
        let when = if due {
            "DUE".to_owned()
        } else if deadline == u64::MAX {
            "never".to_owned()
        } else {
            match in_from_now(deadline) {
                Some(relative) => {
                    format!("{relative}  (at +{}s since boot)", deadline / 1_000_000)
                }
                None => format!("at +{}s since boot", deadline / 1_000_000),
            }
        };
        println!("  {name:<20} {when}");
    }
    println!();
    println!(
        "Run `idlectl explain` for the whole computation, or `idlectl doctor` for what is broken."
    );
    Ok(std::process::ExitCode::SUCCESS)
}

async fn explain(
    manager: &ManagerProxy<'_>,
    action: Option<String>,
    json: bool,
) -> Result<std::process::ExitCode> {
    if json {
        println!("{}", manager.report().await?);
        return Ok(std::process::ExitCode::SUCCESS);
    }
    print!(
        "{}",
        manager.explain(action.as_deref().unwrap_or("")).await?
    );
    Ok(std::process::ExitCode::SUCCESS)
}

async fn doctor(manager: &ManagerProxy<'_>, json: bool) -> Result<std::process::ExitCode> {
    let (text, healthy) = manager.doctor().await?;
    if json {
        println!("{}", manager.report().await?);
    } else {
        print!("{text}");
    }
    // The exit status is the daemon's verdict, not a re-derivation from the text. A client
    // that parsed prose to decide would disagree with the daemon the first time a word
    // changed.
    Ok(if healthy {
        std::process::ExitCode::SUCCESS
    } else {
        std::process::ExitCode::FAILURE
    })
}

async fn rest(manager: &ManagerProxy<'_>, args: RestArgs) -> Result<std::process::ExitCode> {
    if args.force {
        let why = args.why.unwrap_or_default();
        manager.rest_forced(&args.action, &why).await?;
        println!("forced {}: {why}", args.action);
        return Ok(std::process::ExitCode::SUCCESS);
    }

    if manager.rest(&args.action).await? {
        println!("{}: accepted", args.action);
        return Ok(std::process::ExitCode::SUCCESS);
    }

    // Not an error. Something is still holding the machine awake, and the next line says
    // what -- which is the whole reason a refusal is worth more than a silent no-op.
    println!("{}: refused, the machine is not free to rest", args.action);
    println!();
    print!(
        "{}",
        manager.explain(&args.action).await.unwrap_or_default()
    );
    Ok(std::process::ExitCode::FAILURE)
}

async fn lease(
    manager: &ManagerProxy<'_>,
    command: LeaseCommand,
) -> Result<std::process::ExitCode> {
    match command {
        LeaseCommand::Acquire {
            id,
            ttl,
            why,
            command,
        } => {
            let ttl = idlectl_config::parse_duration(&ttl)
                .map_err(|err| anyhow::anyhow!("--ttl: {err}"))?;
            let handle = manager
                .acquire_lease(
                    &id,
                    why.as_deref().unwrap_or(""),
                    u64::try_from(ttl.as_micros()).unwrap_or(u64::MAX),
                )
                .await?;

            if command.is_empty() {
                eprintln!(
                    "lease {id} held for up to {}s. Close this process to release it.",
                    ttl.as_secs()
                );
                // Holding the descriptor IS the lease. Returning here would release it,
                // which is why this blocks rather than printing a handle and exiting.
                futures_lite::future::pending::<()>().await;
                unreachable!()
            }

            let status = std::process::Command::new(&command[0])
                .args(&command[1..])
                .status()
                .with_context(|| format!("cannot run {}", command[0]))?;
            // Explicit, so the lease's lifetime is visibly tied to the child's and not to
            // whatever the optimiser decides about an unused binding.
            drop(handle);
            Ok(std::process::ExitCode::from(
                u8::try_from(status.code().unwrap_or(1)).unwrap_or(1),
            ))
        }
        LeaseCommand::Release { id } => {
            if manager.release_lease(&id).await? {
                println!("released {id}");
                Ok(std::process::ExitCode::SUCCESS)
            } else {
                bail!("no lease named {id} is held")
            }
        }
        LeaseCommand::List(output) => {
            let leases = manager.list_leases().await?;
            if output.json {
                let rows: Vec<_> = leases
                    .iter()
                    .map(|(who, why, acquired, expires, uid)| {
                        serde_json::json!({
                            "who": who,
                            "why": why,
                            "acquired_usec": acquired,
                            "expires_usec": expires,
                            "uid": uid,
                        })
                    })
                    .collect();
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else if leases.is_empty() {
                println!("no leases held");
            } else {
                for (who, why, _acquired, expires, uid) in leases {
                    let when = in_from_now(expires).map_or_else(
                        || format!("expires at +{}s since boot", expires / 1_000_000),
                        |relative| format!("expires {relative}"),
                    );
                    println!("{who:<24} uid {uid:<6} {when:<22} {why}");
                }
            }
            Ok(std::process::ExitCode::SUCCESS)
        }
    }
}

fn check_config(files: Vec<PathBuf>, json: bool) -> Result<std::process::ExitCode> {
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
        return Ok(if faults.is_empty() {
            std::process::ExitCode::SUCCESS
        } else {
            std::process::ExitCode::FAILURE
        });
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
        return Ok(std::process::ExitCode::FAILURE);
    }

    println!();
    println!(
        "{} block(s), configuration is valid.",
        loaded.policy.blocks.len()
    );
    Ok(std::process::ExitCode::SUCCESS)
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
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
            .collect();
        dropins.sort();
        paths.extend(dropins);
    }

    paths
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durations_round_down_rather_than_inventing_precision() {
        assert_eq!(human(std::time::Duration::from_secs(59)), "59s");
        assert_eq!(human(std::time::Duration::from_secs(90)), "1m30s");
        assert_eq!(
            human(std::time::Duration::from_secs(3 * 3600 + 61)),
            "3h01m"
        );
    }

    // A deadline that has already passed is "now", never a wrapped duration. The
    // subtraction is on unsigned microseconds, so getting this wrong would not print a
    // negative number -- it would print several hundred thousand years, on the one screen
    // a person reads when they want to know whether their machine is about to sleep.
    // /proc is Linux, and so is this daemon; the tests below are gated rather than made
    // portable because there is no second implementation to be portable to. The formatter
    // above has no such dependency and runs everywhere.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_deadline_in_the_past_reads_as_now_and_never_wraps() {
        let now = boot_now_usec().expect("/proc/uptime is readable on any Linux");
        assert_eq!(in_from_now(0).as_deref(), Some("now"));
        assert_eq!(
            in_from_now(now.saturating_sub(60_000_000)).as_deref(),
            Some("now")
        );
    }

    // The point of the change: a lease taken out for a minute must read as a minute,
    // whatever the machine's uptime happens to be.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_future_deadline_reads_as_the_time_remaining_not_as_the_boot_offset() {
        let now = boot_now_usec().expect("/proc/uptime is readable on any Linux");
        let text = in_from_now(now + 60_000_000).expect("the clock was just read");
        assert!(
            text.starts_with("in 59s") || text.starts_with("in 1m00s"),
            "expected roughly a minute of remaining time, got {text:?}"
        );
    }

    // /proc/uptime is CLOCK_BOOTTIME, so it cannot go backwards and cannot be zero on a
    // running system. This is the assumption the two functions above are built on.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_boot_clock_is_readable_and_moves_forward() {
        let first = boot_now_usec().expect("/proc/uptime is readable on any Linux");
        assert!(first > 0);
        let second = boot_now_usec().expect("/proc/uptime is readable on any Linux");
        assert!(second >= first);
    }
}
