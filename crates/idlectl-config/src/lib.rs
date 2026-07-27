//! Parsing, drop-in merging and validation for `idlectl.toml`.
//!
//! # Layering
//!
//! idlectl reads its configuration from three places, in this order, and later layers
//! override earlier ones key by key ([CFG-3]):
//!
//! 1. [`VENDOR_CONFIG`] -- shipped by the package, replaced on every upgrade. This is
//!    layer 1 of the resolution chain and is read on every start: it is **not** an example
//!    to copy. A machine with an empty `/etc` has a complete, safe policy, because the
//!    day-one answer to "did you copy the example yet?" is always no.
//! 2. [`ADMIN_CONFIG`] -- written by the administrator. **The package never creates,
//!    owns, modifies or removes anything under `/etc`**, so an upgrade can never quietly
//!    revert a local decision, and uninstalling never leaves a stale file behind.
//! 3. `*.toml` inside [`DROPIN_DIR`], in ascending byte-wise order of basename, each later
//!    file overriding earlier ones. Drop-ins are applied **after** [`ADMIN_CONFIG`], so a
//!    drop-in wins over it -- the systemd drop-in contract, which administrators already
//!    know.
//!
//! There is no vendor drop-in directory in v1, so there is nothing to shadow and no need
//! for zero-byte files. A vendor block is switched off with `enabled = false` ([CFG-11])
//! and a vendor fact with `[facts.<name>] enabled = false` ([CFG-12]); `doctor` reports
//! both. `/usr/lib/idlectl/` is package-owned: nothing here writes to it and no
//! documentation may tell anyone to edit it ([CFG-7]).
//!
//! An example file may ship (conventionally `/usr/share/idlectl/idlectl.example.toml`),
//! but it is not part of the chain and the daemon never reads it ([CFG-26]).
//!
//! This crate does not read any of them: it takes the text and a label. The caller does
//! the I/O and decides the order, passing the vendor layer **first**.
//!
//! # Merging
//!
//! Merging happens at the level of a single key, not a whole block. A drop-in containing
//!
//! ```toml
//! [while.always]
//! suspend = "45m"
//! ```
//!
//! changes the suspend timeout of the base schedule and leaves its `screen_off`,
//! `hibernate` and `poweroff` exactly as the vendor file set them.
//!
//! There is no syntax for deleting a block, because a silent deletion is indistinguishable
//! from a typo in the block name. To switch one off, say so:
//!
//! ```toml
//! [while.media_playing]
//! enabled = false
//! ```
//!
//! # Validation is loud, and a broken file is not fatal
//!
//! Every failure here is an error with a source label and a key path, never a default and
//! never a shrug. Three rules pay for themselves:
//!
//! * **Unknown condition names are fatal to their file.** `[while.game_runing]` is a typo,
//!   and a typo that is silently ignored produces a config that looks like it protects a
//!   running game and does not. The error lists every valid name.
//! * **Unknown keys are fatal to their file** (`deny_unknown_fields`). Same argument: a
//!   misspelled `suspned = "never"` that parses cleanly is a machine that suspends during
//!   a game.
//! * **A broken file is dropped whole, and the daemon keeps running.** [`load`] never
//!   fails. A file that will not parse, or that carries one bad value, is dropped as a
//!   whole ([CFG-10]) and recorded as a fault; every other layer stays in force and
//!   evaluation continues. While any fault is present the engine floors suspend, hibernate
//!   and poweroff at `+infinity` and leaves `screen_off` alone ([CFG-16]), and `doctor`
//!   exits non-zero naming the key, the file and the offending text.
//!
//! The precedent for both halves is specific. A unit that silently did nothing, and a
//! config value that could not be parsed, have each already cost a real outage: one
//! shipped a feature that never ran and logged nothing at all, and the other killed the
//! whole decider before a single rule was evaluated. "Reject the configuration and keep
//! the previous policy" does not cover the case that matters either -- on a first start
//! after a bad edit there **is** no previous policy, and a machine with no policy is a
//! machine with no owner of its power state.
//!
//! The one remaining hole is a broken *vendor* layer, which would leave a fresh install
//! with no policy at all. If the first layer is dropped and nothing else sets a
//! `screen_off` floor, [`load`] installs the compiled-in fallback
//! `[while.always] clock = "human_input", screen_off = "15m"` and nothing else, so a
//! broken package cannot leave a panel lit indefinitely.

#![forbid(unsafe_code)]
#![warn(missing_docs)]
#![warn(clippy::all)]

use std::collections::BTreeMap;
use std::fmt;
use std::time::Duration;

use idlectl_policy::{
    Block, BlockId, BlockKind, Clock, Condition, ConfigFault, FactId, FactSettings, Policy,
    Timeout, Timeouts,
};
use serde::Deserialize;

/// The vendor default, shipped by the package. Replaced wholesale on upgrade, and read on
/// every start: it is layer 1 of the chain, not an example.
pub const VENDOR_CONFIG: &str = "/usr/lib/idlectl/idlectl.toml";

/// The administrator's override. **Not owned by the package**: never created, never
/// modified, never removed by an install or an uninstall.
pub const ADMIN_CONFIG: &str = "/etc/idlectl/idlectl.toml";

/// Drop-in directory. `*.toml` files are applied in ascending byte-wise order of basename,
/// after [`ADMIN_CONFIG`].
pub const DROPIN_DIR: &str = "/etc/idlectl/conf.d";

/// The compiled-in `screen_off` floor used when the vendor layer itself is unusable.
pub const FALLBACK_SCREEN_OFF: Duration = Duration::from_secs(900);

// ---------------------------------------------------------------------------------
// Timeout parsing
// ---------------------------------------------------------------------------------

/// Why a timeout string could not be parsed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[non_exhaustive]
pub enum TimeoutError {
    /// The value was empty or only whitespace.
    Empty,
    /// A number with no unit, such as `30` -- including a bare `0`, which must be written
    /// `"0s"`. Rejected deliberately: `30` could mean seconds or minutes, and guessing
    /// wrong by a factor of sixty is the difference between a nuisance and a machine that
    /// sleeps mid-game. Allowing a bare `0` would leave exactly one integer in the format
    /// that means something, which is how the next one gets written.
    MissingUnit,
    /// A unit with no number in front of it, such as `m`.
    MissingNumber,
    /// An unrecognised unit character. Valid units are `s`, `m`, `h`, `d`.
    ///
    /// `Never` and `NEVER` land here too: the sentinel is the exact lower-case string
    /// `never`, because every other name in the format is lower-case and a
    /// case-insensitive island is a dialect nobody asked for.
    UnknownUnit(char),
    /// The value does not fit in a duration.
    Overflow,
    /// `never` where a finite duration is required.
    NeverNotAllowed,
}

impl fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TimeoutError::Empty => f.write_str("empty value"),
            TimeoutError::MissingUnit => f.write_str(
                "missing unit: write \"30s\", \"10m\", \"2h\", \"1d\" or \"0s\", not a bare number",
            ),
            TimeoutError::MissingNumber => f.write_str("unit with no number in front of it"),
            TimeoutError::UnknownUnit(c) => {
                write!(
                    f,
                    "unknown unit '{c}': valid units are s, m, h, d, and the only sentinel is \
                     the exact lower-case \"never\""
                )
            }
            TimeoutError::Overflow => f.write_str("value too large"),
            TimeoutError::NeverNotAllowed => {
                f.write_str("\"never\" is not allowed here; a finite duration is required")
            }
        }
    }
}

impl std::error::Error for TimeoutError {}

/// Parses a timeout: the exact string `never`, or a sequence of `<number><unit>` with
/// units `s`, `m`, `h`, `d`.
///
/// Compound values are accepted (`1h30m`). Underscores and spaces are ignored, so
/// `1h 30m` and `90_000s` both work -- harmless, and documented in the man page.
///
/// Every bare number is rejected, `0` included -- see [`TimeoutError::MissingUnit`]. Zero
/// is spelled `"0s"`.
///
/// ```
/// use idlectl_config::parse_timeout;
/// use idlectl_policy::Timeout;
/// use std::time::Duration;
///
/// assert_eq!(parse_timeout("30m"), Ok(Timeout::After(Duration::from_secs(1800))));
/// assert_eq!(parse_timeout("1h30m"), Ok(Timeout::After(Duration::from_secs(5400))));
/// assert_eq!(parse_timeout("0s"), Ok(Timeout::After(Duration::ZERO)));
/// assert_eq!(parse_timeout("never"), Ok(Timeout::Never));
/// assert!(parse_timeout("30").is_err());
/// assert!(parse_timeout("0").is_err());
/// assert!(parse_timeout("Never").is_err());
/// ```
pub fn parse_timeout(input: &str) -> Result<Timeout, TimeoutError> {
    let text = input.trim();
    if text.is_empty() {
        return Err(TimeoutError::Empty);
    }
    if text == "never" {
        return Ok(Timeout::Never);
    }

    let mut total_secs: u64 = 0;
    let mut digits = String::new();
    let mut saw_unit = false;

    for ch in text.chars() {
        if ch.is_ascii_digit() {
            digits.push(ch);
            continue;
        }
        if ch == '_' || ch.is_whitespace() {
            continue;
        }
        let multiplier: u64 = match ch {
            's' => 1,
            'm' => 60,
            'h' => 3_600,
            'd' => 86_400,
            other => return Err(TimeoutError::UnknownUnit(other)),
        };
        if digits.is_empty() {
            return Err(TimeoutError::MissingNumber);
        }
        let value: u64 = digits.parse().map_err(|_| TimeoutError::Overflow)?;
        total_secs = value
            .checked_mul(multiplier)
            .and_then(|scaled| total_secs.checked_add(scaled))
            .ok_or(TimeoutError::Overflow)?;
        digits.clear();
        saw_unit = true;
    }

    if !digits.is_empty() || !saw_unit {
        return Err(TimeoutError::MissingUnit);
    }
    Ok(Timeout::After(Duration::from_secs(total_secs)))
}

/// Parses a value that must be a finite duration, rejecting `never`.
pub fn parse_duration(input: &str) -> Result<Duration, TimeoutError> {
    match parse_timeout(input)? {
        Timeout::After(d) => Ok(d),
        Timeout::Never => Err(TimeoutError::NeverNotAllowed),
    }
}

/// Renders a timeout the way a config file would spell it. Round-trips through
/// [`parse_timeout`].
#[must_use]
pub fn format_timeout(timeout: Timeout) -> String {
    let duration = match timeout {
        Timeout::Never => return "never".to_owned(),
        Timeout::After(d) => d,
    };
    let mut secs = duration.as_secs();
    if secs == 0 {
        // Not "0": a bare integer is not a legal timeout, and this function must produce
        // something the parser accepts.
        return "0s".to_owned();
    }
    let mut out = String::new();
    for (unit, size) in [('d', 86_400u64), ('h', 3_600), ('m', 60), ('s', 1)] {
        let count = secs / size;
        if count > 0 {
            out.push_str(&count.to_string());
            out.push(unit);
            secs -= count * size;
        }
    }
    out
}

// ---------------------------------------------------------------------------------
// Errors and warnings
// ---------------------------------------------------------------------------------

/// What went wrong.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub enum ErrorKind {
    /// The file is not valid TOML, or a value had the wrong TOML type. The string is the
    /// message from the TOML parser, which already carries a line and column.
    Syntax(String),
    /// A `[while <name>]` or `[when <name>]` table named something that is not `always`
    /// and not a known fact.
    UnknownCondition {
        /// The name as written.
        name: String,
    },
    /// A `[facts.<name>]` table named something that is not a known fact.
    UnknownFact {
        /// The name as written.
        name: String,
    },
    /// A `[facts.<name>]` table carried a key that is not defined for that particular
    /// fact, such as `steam_root` under `local_service_busy`.
    UnknownFactKey {
        /// The fact the table configures.
        fact: &'static str,
        /// The key that does not belong to it.
        key: &'static str,
    },
    /// A `clock = "..."` value that is not a known clock.
    UnknownClock {
        /// The name as written.
        name: String,
    },
    /// A timeout or duration that could not be parsed.
    BadTimeout {
        /// The value as written.
        value: String,
        /// Why it failed.
        reason: TimeoutError,
    },
}

impl fmt::Display for ErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ErrorKind::Syntax(message) => write!(f, "{message}"),
            ErrorKind::UnknownCondition { name } => {
                write!(
                    f,
                    "unknown condition \"{name}\". Valid conditions are: always"
                )?;
                for fact in FactId::ALL {
                    write!(f, ", {}", fact.name())?;
                }
                Ok(())
            }
            ErrorKind::UnknownFact { name } => {
                write!(f, "unknown fact \"{name}\". Valid facts are: ")?;
                for (index, fact) in FactId::ALL.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(fact.name())?;
                }
                Ok(())
            }
            ErrorKind::UnknownFactKey { fact, key } => {
                write!(f, "\"{key}\" is not a key of the \"{fact}\" fact")
            }
            ErrorKind::UnknownClock { name } => {
                write!(f, "unknown clock \"{name}\". Valid clocks are: ")?;
                for (index, clock) in Clock::ALL.iter().enumerate() {
                    if index > 0 {
                        f.write_str(", ")?;
                    }
                    f.write_str(clock.name())?;
                }
                Ok(())
            }
            ErrorKind::BadTimeout { value, reason } => {
                write!(f, "cannot parse \"{value}\": {reason}")
            }
        }
    }
}

/// A configuration error, with enough context to fix it without guessing.
///
/// Returned by [`load`] as a **fault**: the file it came from was dropped, the rest of the
/// configuration is still in force, and the machine will not sleep until it is fixed.
#[derive(Clone, Debug)]
pub struct ConfigError {
    /// The label of the source it came from, as supplied by the caller. Normally a path.
    pub source: String,
    /// The key path, such as `while.always.suspend`. Empty for whole-file errors.
    pub location: String,
    /// What went wrong.
    pub kind: ErrorKind,
}

impl ConfigError {
    /// The message without the source and location prefix.
    #[must_use]
    pub fn detail(&self) -> String {
        self.kind.to_string()
    }

    /// The engine-facing form: what `explain` and `doctor` name as the reason the three
    /// sleep actions are floored at `+infinity`.
    #[must_use]
    pub fn as_fault(&self) -> ConfigFault {
        ConfigFault {
            source: self.source.clone(),
            location: self.location.clone(),
            message: self.detail(),
        }
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.source)?;
        if !self.location.is_empty() {
            write!(f, ": {}", self.location)?;
        }
        write!(f, ": {}", self.kind)
    }
}

impl std::error::Error for ConfigError {}

/// Something suspicious that is not fatal.
///
/// Warnings are surfaced by `idlectl check-config` and logged by the daemon at startup.
/// They never stop a machine from booting with a working policy.
#[derive(Clone, Debug)]
pub struct Warning {
    /// Where it came from.
    pub source: String,
    /// The key path it concerns.
    pub location: String,
    /// What to look at.
    pub message: String,
}

impl fmt::Display for Warning {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}: {}", self.source, self.location, self.message)
    }
}

// ---------------------------------------------------------------------------------
// The file format
// ---------------------------------------------------------------------------------

/// One configuration layer: its text, and a label used in diagnostics.
#[derive(Clone, Debug)]
pub struct Source {
    /// Normally the path the text was read from. Only ever used in messages.
    pub label: String,
    /// The file contents.
    pub text: String,
}

impl Source {
    /// Builds a source.
    pub fn new(label: impl Into<String>, text: impl Into<String>) -> Self {
        Source {
            label: label.into(),
            text: text.into(),
        }
    }
}

/// A duration as written in a config file.
///
/// The integer arm exists **only** so that this crate owns the error message. `suspend =
/// 30` must say "missing unit: write 30s or 30m", not serde's "invalid type: integer",
/// because the person who wrote it is one keystroke away from a machine that sleeps sixty
/// times too early or sixty times too late.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum TimeoutValue {
    /// A quoted value: the only legal spelling.
    Text(String),
    /// A bare TOML integer. Always an error; kept so the message can be a good one.
    Integer(i64),
}

impl TimeoutValue {
    /// The value exactly as the file spelled it, for error messages.
    #[must_use]
    pub fn as_written(&self) -> String {
        match self {
            TimeoutValue::Text(text) => text.clone(),
            TimeoutValue::Integer(number) => number.to_string(),
        }
    }

    /// Parses the value.
    ///
    /// # Errors
    ///
    /// Whatever [`parse_timeout`] returns, plus [`TimeoutError::MissingUnit`] for a bare
    /// integer.
    pub fn parse(&self) -> Result<Timeout, TimeoutError> {
        match self {
            TimeoutValue::Text(text) => parse_timeout(text),
            TimeoutValue::Integer(_) => Err(TimeoutError::MissingUnit),
        }
    }

    /// Parses the value, rejecting `never`.
    ///
    /// # Errors
    ///
    /// As [`TimeoutValue::parse`], plus [`TimeoutError::NeverNotAllowed`].
    pub fn parse_finite(&self) -> Result<Duration, TimeoutError> {
        match self.parse()? {
            Timeout::After(duration) => Ok(duration),
            Timeout::Never => Err(TimeoutError::NeverNotAllowed),
        }
    }
}

/// The `[general]` table. Carries `min_idle` and nothing else.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GeneralSection {
    /// How recent human input must be for `human_active` to hold. Default `5m`.
    pub min_idle: Option<TimeoutValue>,
}

/// One `[facts.<name>]` table.
///
/// `enabled` is defined for every fact; the remaining keys are defined for specific facts
/// only, and a key that is not defined for that particular fact is a configuration error
/// -- `steam_root` under `local_service_busy` is a mistake, and a silently-ignored mistake
/// here is a detector that is not configured the way its author believes.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FactSection {
    /// Whether the fact participates at all. Default `true`, except
    /// `local_service_busy`, which ships disabled until `counters_url` is set.
    pub enabled: Option<bool>,
    /// `local_service_busy` only: the cumulative-counter endpoint.
    pub counters_url: Option<String>,
    /// `local_service_busy` only: how long without a counter moving means "not in use".
    pub idle_window: Option<TimeoutValue>,
    /// `steam_game_running` and `steam_downloading` only: the Steam installation root.
    pub steam_root: Option<String>,
    /// `steam_downloading` only: how recent a staging-tree modification must be.
    pub window: Option<TimeoutValue>,
}

impl FactSection {
    /// Overlays `other` on top of `self`, key by key.
    fn overlay(&mut self, other: &FactSection) {
        if other.enabled.is_some() {
            self.enabled = other.enabled;
        }
        if other.counters_url.is_some() {
            self.counters_url.clone_from(&other.counters_url);
        }
        if other.idle_window.is_some() {
            self.idle_window.clone_from(&other.idle_window);
        }
        if other.steam_root.is_some() {
            self.steam_root.clone_from(&other.steam_root);
        }
        if other.window.is_some() {
            self.window.clone_from(&other.window);
        }
    }

    /// The keys this fact actually defines, other than `enabled`.
    fn allowed_keys(fact: FactId) -> &'static [&'static str] {
        match fact {
            FactId::LocalServiceBusy => &["counters_url", "idle_window"],
            FactId::SteamGameRunning => &["steam_root"],
            FactId::SteamDownloading => &["steam_root", "window"],
            _ => &[],
        }
    }

    /// Which optional keys this table actually set.
    fn keys_present(&self) -> Vec<&'static str> {
        let mut present = Vec::new();
        if self.counters_url.is_some() {
            present.push("counters_url");
        }
        if self.idle_window.is_some() {
            present.push("idle_window");
        }
        if self.steam_root.is_some() {
            present.push("steam_root");
        }
        if self.window.is_some() {
            present.push("window");
        }
        present
    }
}

/// One `[while ...]` or `[when ...]` table.
///
/// There is no `hard` key and no `collapsible` key. v1 ships **one** ceiling class: every
/// `[when]` block is absolute, is logged at load and is logged again when it fires. And
/// `rest --now` satisfies exactly two blocks, chosen by condition name (`always` and
/// `after_resume`), so there is no per-block key with which an administrator could make
/// the human-presence floor collapsible by typing one word.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BlockSection {
    /// Whether the block participates. Default `true`. Setting it to `false` in a drop-in
    /// is how a vendor block is switched off.
    pub enabled: Option<bool>,
    /// Which clock the timeouts are counted from. Default `human_input`.
    pub clock: Option<String>,
    /// Timeout for `screen_off`.
    pub screen_off: Option<TimeoutValue>,
    /// Timeout for `suspend`.
    pub suspend: Option<TimeoutValue>,
    /// Timeout for `hibernate`.
    pub hibernate: Option<TimeoutValue>,
    /// Timeout for `poweroff`.
    pub poweroff: Option<TimeoutValue>,
}

impl BlockSection {
    /// The timeout keys, paired with their action names.
    fn timeout_keys(&self) -> [(&'static str, &Option<TimeoutValue>); 4] {
        [
            ("screen_off", &self.screen_off),
            ("suspend", &self.suspend),
            ("hibernate", &self.hibernate),
            ("poweroff", &self.poweroff),
        ]
    }

    /// Overlays `other` on top of `self`, key by key. A key the drop-in did not mention
    /// keeps its previous value.
    fn overlay(&mut self, other: &BlockSection) {
        if other.enabled.is_some() {
            self.enabled = other.enabled;
        }
        if other.clock.is_some() {
            self.clock.clone_from(&other.clock);
        }
        for slot in [
            (&mut self.screen_off, &other.screen_off),
            (&mut self.suspend, &other.suspend),
            (&mut self.hibernate, &other.hibernate),
            (&mut self.poweroff, &other.poweroff),
        ] {
            if slot.1.is_some() {
                slot.0.clone_from(slot.1);
            }
        }
    }
}

/// One parsed file, before merging and before validation.
///
/// The top-level tables of v1, frozen: `version`, `[general]`, `[facts.<name>]`,
/// `[while.<condition>]` and `[when.<condition>]`. There is no `[rest]` table -- `idlectl
/// rest` with no `--action` resolves to `suspend`, always -- and no `gpu_min_mem`, which is
/// the compiled-in constant 512 MiB.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ConfigFile {
    /// Format version. Optional, defaults to 1.
    pub version: Option<u32>,
    /// The `[general]` table.
    #[serde(default)]
    pub general: Option<GeneralSection>,
    /// The `[facts ...]` tables, keyed by fact name.
    #[serde(default)]
    pub facts: BTreeMap<String, FactSection>,
    /// The `[while ...]` tables, keyed by condition name.
    #[serde(default, rename = "while")]
    pub while_blocks: BTreeMap<String, BlockSection>,
    /// The `[when ...]` tables, keyed by condition name.
    #[serde(default)]
    pub when: BTreeMap<String, BlockSection>,
}

impl ConfigFile {
    /// Overlays `other` on top of `self`.
    pub fn overlay(&mut self, other: &ConfigFile) {
        if other.version.is_some() {
            self.version = other.version;
        }
        if let Some(general) = &other.general {
            let target = self.general.get_or_insert_with(GeneralSection::default);
            if general.min_idle.is_some() {
                target.min_idle.clone_from(&general.min_idle);
            }
        }
        for (name, section) in &other.facts {
            self.facts.entry(name.clone()).or_default().overlay(section);
        }
        for (name, section) in &other.while_blocks {
            self.while_blocks
                .entry(name.clone())
                .or_default()
                .overlay(section);
        }
        for (name, section) in &other.when {
            self.when.entry(name.clone()).or_default().overlay(section);
        }
    }
}

/// The result of a load.
#[derive(Clone, Debug)]
pub struct Loaded {
    /// The validated policy, including any faults recorded while loading.
    pub policy: Policy,
    /// The labels of the layers that were **applied**, in order. A dropped layer is absent
    /// from this list and present in the fault list, so `doctor` can say "which file set
    /// this?" and "which file was ignored?" separately.
    pub layers: Vec<String>,
    /// Non-fatal complaints.
    pub warnings: Vec<Warning>,
}

/// Parses one layer.
///
/// # Errors
///
/// [`ErrorKind::Syntax`] if the text is not valid TOML, contains an unknown key, or gives
/// a key the wrong TOML type.
pub fn parse(source: &Source) -> Result<ConfigFile, ConfigError> {
    toml::from_str::<ConfigFile>(&source.text).map_err(|err| ConfigError {
        source: source.label.clone(),
        location: String::new(),
        kind: ErrorKind::Syntax(err.to_string()),
    })
}

/// Parses every layer, drops the broken ones, merges the rest in order and validates the
/// result.
///
/// **Never fails.** The returned [`Vec<ConfigError>`] is the fault list: one entry for
/// every problem found, each naming the file it came from. A file with any fault is
/// dropped as a whole ([CFG-10]) and does not contribute a single key; the layers around
/// it are unaffected. The faults are also copied onto [`Policy::faults`], where the engine
/// turns them into an implicit `+infinity` floor on suspend, hibernate and poweroff --
/// and never on `screen_off` ([CFG-16], [FACT-40]).
///
/// The caller must:
///
/// * pass the vendor layer **first**, so that a broken vendor file can be recognised;
/// * log every fault and keep running;
/// * make `doctor` exit non-zero while the list is non-empty.
#[must_use]
pub fn load(sources: &[Source]) -> (Loaded, Vec<ConfigError>) {
    let mut merged = ConfigFile::default();
    let mut layers = Vec::with_capacity(sources.len());
    let mut faults: Vec<ConfigError> = Vec::new();
    let mut warnings: Vec<Warning> = Vec::new();
    // Track the last layer that mentioned each block, so a validation error blames a
    // plausible file rather than always the last one read. This is an approximation --
    // per-key provenance would need a spanned parse -- but it points at the right file in
    // the case that matters, which is a drop-in that broke a block the vendor file set.
    let mut origin: BTreeMap<(BlockKind, String), String> = BTreeMap::new();
    let mut fact_origin: BTreeMap<String, String> = BTreeMap::new();
    let mut general_origin = String::new();
    let mut vendor_dropped = false;

    for (index, source) in sources.iter().enumerate() {
        // Two ways a file can be dropped, and they are the same thing to everyone
        // downstream: it will not parse, or it carries a value that is not usable.
        let parsed = match parse(source) {
            Ok(parsed) => parsed,
            Err(err) => {
                faults.push(err);
                vendor_dropped |= index == 0;
                continue;
            }
        };

        let errors = validate(&parsed, &source.label);
        if !errors.is_empty() {
            faults.extend(errors);
            vendor_dropped |= index == 0;
            continue;
        }

        if parsed.general.is_some() {
            general_origin = source.label.clone();
        }
        for name in parsed.facts.keys() {
            fact_origin.insert(name.clone(), source.label.clone());
        }
        for name in parsed.while_blocks.keys() {
            origin.insert((BlockKind::While, name.clone()), source.label.clone());
        }
        for name in parsed.when.keys() {
            origin.insert((BlockKind::When, name.clone()), source.label.clone());
        }
        merged.overlay(&parsed);
        layers.push(source.label.clone());
    }

    let mut blocks = Vec::new();
    for (kind, sections) in [
        (BlockKind::While, &merged.while_blocks),
        (BlockKind::When, &merged.when),
    ] {
        for (name, section) in sections {
            let label = origin
                .get(&(kind, name.clone()))
                .cloned()
                .unwrap_or_else(|| "<merged>".to_owned());
            // Every surviving layer was validated on its own, so this cannot fail. If it
            // ever does, the block is dropped and recorded rather than panicking: this
            // crate is on the path that decides whether a machine may sleep.
            match build_block(kind, name, section, &label, &mut warnings) {
                Ok(block) => blocks.push(block),
                Err(err) => faults.push(err),
            }
        }
    }

    let min_idle = match merged.general.as_ref().and_then(|g| g.min_idle.as_ref()) {
        Some(raw) => match raw.parse_finite() {
            Ok(duration) => duration,
            Err(reason) => {
                faults.push(ConfigError {
                    source: general_origin.clone(),
                    location: "general.min_idle".to_owned(),
                    kind: ErrorKind::BadTimeout {
                        value: raw.as_written(),
                        reason,
                    },
                });
                Policy::DEFAULT_MIN_IDLE
            }
        },
        None => Policy::DEFAULT_MIN_IDLE,
    };

    let facts = build_facts(&merged, &fact_origin, &mut warnings);

    // The one case per-file dropping does not cover: if the vendor layer itself is
    // unusable and nothing else sets a screen_off floor, a broken package would leave a
    // panel lit indefinitely. Install the compiled-in fallback, loudly.
    if vendor_dropped
        && !blocks
            .iter()
            .any(|b| b.enabled && b.id.kind == BlockKind::While && b.timeouts.screen_off.is_some())
    {
        blocks.push(fallback_screen_off_block());
        warnings.push(Warning {
            source: sources
                .first()
                .map_or_else(|| VENDOR_CONFIG.to_owned(), |s| s.label.clone()),
            location: "while.always.screen_off".to_owned(),
            message: format!(
                "the vendor layer was dropped and no other layer sets screen_off; applying the \
                 compiled-in fallback of {} so a broken package cannot leave a panel lit",
                format_timeout(Timeout::After(FALLBACK_SCREEN_OFF))
            ),
        });
    }

    let policy = Policy {
        blocks,
        min_idle,
        facts,
        faults: faults.iter().map(ConfigError::as_fault).collect(),
    };

    (
        Loaded {
            policy,
            layers,
            warnings,
        },
        faults,
    )
}

/// `[while.always] clock = "human_input", screen_off = "15m"`, and nothing else.
fn fallback_screen_off_block() -> Block {
    Block {
        id: BlockId::new(BlockKind::While, Condition::Always),
        clock: Clock::HumanInput,
        enabled: true,
        timeouts: Timeouts {
            screen_off: Some(Timeout::After(FALLBACK_SCREEN_OFF)),
            ..Timeouts::default()
        },
    }
}

/// Checks one parsed file on its own, before it is merged into anything.
///
/// Validating per file rather than after merging is what makes [CFG-10] true: the file
/// that carries the mistake is the file that is dropped, and one typo in one drop-in can
/// no longer take the vendor defaults down with it.
fn validate(file: &ConfigFile, label: &str) -> Vec<ConfigError> {
    let mut errors = Vec::new();

    if let Some(raw) = file.general.as_ref().and_then(|g| g.min_idle.as_ref()) {
        if let Err(reason) = raw.parse_finite() {
            errors.push(ConfigError {
                source: label.to_owned(),
                location: "general.min_idle".to_owned(),
                kind: ErrorKind::BadTimeout {
                    value: raw.as_written(),
                    reason,
                },
            });
        }
    }

    for (name, section) in &file.facts {
        let path = format!("facts.{name}");
        let Some(fact) = FactId::from_name(name) else {
            errors.push(ConfigError {
                source: label.to_owned(),
                location: path,
                kind: ErrorKind::UnknownFact { name: name.clone() },
            });
            continue;
        };

        let allowed = FactSection::allowed_keys(fact);
        for key in section.keys_present() {
            if !allowed.contains(&key) {
                errors.push(ConfigError {
                    source: label.to_owned(),
                    location: format!("{path}.{key}"),
                    kind: ErrorKind::UnknownFactKey {
                        fact: fact.name(),
                        key,
                    },
                });
            }
        }

        for (key, raw) in [
            ("idle_window", &section.idle_window),
            ("window", &section.window),
        ] {
            if let Some(raw) = raw {
                if let Err(reason) = raw.parse_finite() {
                    errors.push(ConfigError {
                        source: label.to_owned(),
                        location: format!("{path}.{key}"),
                        kind: ErrorKind::BadTimeout {
                            value: raw.as_written(),
                            reason,
                        },
                    });
                }
            }
        }
    }

    for (kind, sections) in [
        (BlockKind::While, &file.while_blocks),
        (BlockKind::When, &file.when),
    ] {
        for (name, section) in sections {
            let path = format!("{}.{name}", kind.name());

            if Condition::from_name(name).is_none() {
                errors.push(ConfigError {
                    source: label.to_owned(),
                    location: path.clone(),
                    kind: ErrorKind::UnknownCondition { name: name.clone() },
                });
            }

            if let Some(raw) = section.clock.as_deref() {
                if Clock::from_name(raw).is_none() {
                    errors.push(ConfigError {
                        source: label.to_owned(),
                        location: format!("{path}.clock"),
                        kind: ErrorKind::UnknownClock {
                            name: raw.to_owned(),
                        },
                    });
                }
            }

            for (key, raw) in section.timeout_keys() {
                if let Some(raw) = raw {
                    if let Err(reason) = raw.parse() {
                        errors.push(ConfigError {
                            source: label.to_owned(),
                            location: format!("{path}.{key}"),
                            kind: ErrorKind::BadTimeout {
                                value: raw.as_written(),
                                reason,
                            },
                        });
                    }
                }
            }
        }
    }

    errors
}

/// Resolves the per-fact settings, filling in the shipped defaults.
fn build_facts(
    merged: &ConfigFile,
    fact_origin: &BTreeMap<String, String>,
    warnings: &mut Vec<Warning>,
) -> BTreeMap<FactId, FactSettings> {
    let mut facts: BTreeMap<FactId, FactSettings> = FactId::ALL
        .into_iter()
        .map(|fact| (fact, FactSettings::default_for(fact)))
        .collect();

    for (name, section) in &merged.facts {
        // Unknown names were rejected in validate(), so the file they came from was
        // dropped and this cannot be reached with a bad name.
        let Some(fact) = FactId::from_name(name) else {
            continue;
        };
        let label = fact_origin
            .get(name)
            .cloned()
            .unwrap_or_else(|| "<merged>".to_owned());
        let settings = facts
            .entry(fact)
            .or_insert_with(|| FactSettings::default_for(fact));

        settings.counters_url.clone_from(&section.counters_url);
        settings.steam_root.clone_from(&section.steam_root);
        settings.idle_window = section
            .idle_window
            .as_ref()
            .and_then(|v| v.parse_finite().ok());
        settings.window = section.window.as_ref().and_then(|v| v.parse_finite().ok());

        if let Some(enabled) = section.enabled {
            settings.enabled = enabled;
        } else if fact == FactId::LocalServiceBusy {
            // Shipped disabled; a counters endpoint is what turns it on.
            settings.enabled = settings.counters_url.is_some();
        }

        if fact == FactId::LocalServiceBusy && settings.enabled && settings.counters_url.is_none() {
            // Enabling it without an endpoint would give a detector that can only report
            // INDETERMINATE, and because the doubt veto is block-independent that is a
            // machine that never sleeps again. Refuse the enablement and say so.
            settings.enabled = false;
            warnings.push(Warning {
                source: label.clone(),
                location: format!("facts.{name}.enabled"),
                message: "local_service_busy was enabled without counters_url, so it has no way \
                          to answer; leaving it disabled. Set counters_url to enable it."
                    .to_owned(),
            });
        }

        if !settings.enabled {
            warnings.push(Warning {
                source: label,
                location: format!("facts.{name}"),
                message: "fact is disabled: its detector will not run, it reports unavailable, \
                          and doubt about it no longer vetoes"
                    .to_owned(),
            });
        }
    }

    facts
}

fn build_block(
    kind: BlockKind,
    name: &str,
    section: &BlockSection,
    label: &str,
    warnings: &mut Vec<Warning>,
) -> Result<Block, ConfigError> {
    let path = format!("{}.{name}", kind.name());

    let condition = Condition::from_name(name).ok_or_else(|| ConfigError {
        source: label.to_owned(),
        location: path.clone(),
        kind: ErrorKind::UnknownCondition {
            name: name.to_owned(),
        },
    })?;

    let clock = match section.clock.as_deref() {
        None => Clock::HumanInput,
        Some(raw) => Clock::from_name(raw).ok_or_else(|| ConfigError {
            source: label.to_owned(),
            location: format!("{path}.clock"),
            kind: ErrorKind::UnknownClock {
                name: raw.to_owned(),
            },
        })?,
    };

    let mut timeouts = Timeouts::default();
    for (key, raw) in section.timeout_keys() {
        let Some(raw) = raw else { continue };
        let timeout = raw.parse().map_err(|reason| ConfigError {
            source: label.to_owned(),
            location: format!("{path}.{key}"),
            kind: ErrorKind::BadTimeout {
                value: raw.as_written(),
                reason,
            },
        })?;
        // `key` came from the array above, so this cannot fail.
        if let Some(action) = idlectl_policy::Action::from_name(key) {
            timeouts.set(action, Some(timeout));
        }
    }

    let enabled = section.enabled.unwrap_or(true);

    if timeouts.is_empty() && enabled {
        warnings.push(Warning {
            source: label.to_owned(),
            location: path.clone(),
            message: "block sets no timeout for any action and therefore does nothing".to_owned(),
        });
    }

    if kind == BlockKind::When && enabled {
        // [CEIL-6] Every ceiling in the effective configuration is announced at load,
        // naming its file. There is one ceiling class and it is absolute: this is the only
        // construct in the system that can act against a floor, so it is never allowed to
        // be acquired quietly.
        warnings.push(Warning {
            source: label.to_owned(),
            location: path.clone(),
            message: "[when] blocks are ceilings: they force an action by their deadline and \
                      override every floor, including `never`, the human_active floor, the \
                      doubt floor of an unreadable detector and the configuration-fault \
                      floor. Confirm this is intended."
                .to_owned(),
        });
    }

    Ok(Block {
        id: BlockId::new(kind, condition),
        clock,
        enabled,
        timeouts,
    })
}

// ---------------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use idlectl_policy::Action;

    fn source(label: &str, text: &str) -> Source {
        Source::new(label, text)
    }

    /// Loads and asserts there were no faults.
    fn load_clean(sources: &[Source]) -> Loaded {
        let (loaded, faults) = load(sources);
        assert!(
            faults.is_empty(),
            "expected a clean load, got {:?}",
            faults.iter().map(ToString::to_string).collect::<Vec<_>>()
        );
        loaded
    }

    #[test]
    fn timeouts_parse_and_round_trip() {
        assert_eq!(parse_timeout("0s"), Ok(Timeout::After(Duration::ZERO)));
        assert_eq!(
            parse_timeout("45s"),
            Ok(Timeout::After(Duration::from_secs(45)))
        );
        assert_eq!(
            parse_timeout("10m"),
            Ok(Timeout::After(Duration::from_secs(600)))
        );
        assert_eq!(
            parse_timeout("2h"),
            Ok(Timeout::After(Duration::from_secs(7_200)))
        );
        assert_eq!(
            parse_timeout("1d"),
            Ok(Timeout::After(Duration::from_secs(86_400)))
        );
        assert_eq!(
            parse_timeout("1h30m"),
            Ok(Timeout::After(Duration::from_secs(5_400)))
        );
        assert_eq!(
            parse_timeout(" 1h 30m "),
            Ok(Timeout::After(Duration::from_secs(5_400))),
            "whitespace and underscores inside a duration are ignored, as documented"
        );
        assert_eq!(
            parse_timeout("90_000s"),
            Ok(Timeout::After(Duration::from_secs(90_000)))
        );
        assert_eq!(parse_timeout("never"), Ok(Timeout::Never));

        for text in ["30m", "1h30m", "never", "0s", "1d2h3m4s"] {
            let parsed = parse_timeout(text).expect("valid");
            assert_eq!(parse_timeout(&format_timeout(parsed)), Ok(parsed));
        }
    }

    /// A non-numeric value in a config file once killed an entire decider before a single
    /// rule was evaluated. Every one of these must be a message, not a panic.
    #[test]
    fn unparseable_timeouts_are_errors_not_panics() {
        assert_eq!(parse_timeout("30"), Err(TimeoutError::MissingUnit));
        assert_eq!(
            parse_timeout("0"),
            Err(TimeoutError::MissingUnit),
            "a bare 0 is a bare integer like any other; zero is spelled \"0s\""
        );
        assert_eq!(parse_timeout(""), Err(TimeoutError::Empty));
        assert_eq!(parse_timeout("   "), Err(TimeoutError::Empty));
        assert_eq!(parse_timeout("m"), Err(TimeoutError::MissingNumber));
        assert_eq!(parse_timeout("10y"), Err(TimeoutError::UnknownUnit('y')));
        assert_eq!(parse_timeout("later"), Err(TimeoutError::UnknownUnit('l')));
        assert_eq!(
            parse_timeout("99999999999999999999d"),
            Err(TimeoutError::Overflow)
        );
        assert_eq!(parse_duration("never"), Err(TimeoutError::NeverNotAllowed));
    }

    /// `never` is the exact lower-case string. Every other name in the format is
    /// lower-case, and a case-insensitive island is a dialect nobody asked for.
    #[test]
    fn never_is_case_sensitive() {
        assert_eq!(parse_timeout("never"), Ok(Timeout::Never));
        assert!(parse_timeout("Never").is_err());
        assert!(parse_timeout("NEVER").is_err());
    }

    /// A bare integer must produce this crate's message, not serde's "invalid type".
    #[test]
    fn a_bare_integer_timeout_says_which_unit_is_missing() {
        let (_, faults) = load(&[source("v", "[while.always]\nsuspend = 30\n")]);
        assert_eq!(faults.len(), 1, "{faults:?}");
        assert_eq!(faults[0].location, "while.always.suspend");
        let text = faults[0].to_string();
        assert!(text.contains("missing unit"), "{text}");
        assert!(text.contains("30s"), "{text}");
    }

    #[test]
    fn a_dropin_overrides_one_key_and_leaves_the_rest() {
        let vendor = source(
            "/usr/lib/idlectl/idlectl.toml",
            r#"
            [while.always]
            screen_off = "10m"
            suspend = "30m"
            poweroff = "never"
            "#,
        );
        let dropin = source(
            "/etc/idlectl/conf.d/10-local.toml",
            r#"
            [while.always]
            suspend = "45m"
            "#,
        );

        let loaded = load_clean(&[vendor, dropin]);
        let block = loaded
            .policy
            .block(BlockId::new(BlockKind::While, Condition::Always))
            .expect("present");

        assert_eq!(
            block.timeouts.get(Action::Suspend),
            Some(Timeout::After(Duration::from_secs(2_700))),
            "a drop-in is applied after the admin file and therefore wins"
        );
        assert_eq!(
            block.timeouts.get(Action::ScreenOff),
            Some(Timeout::After(Duration::from_secs(600))),
            "a drop-in that did not mention screen_off must not clear it"
        );
        assert_eq!(block.timeouts.get(Action::PowerOff), Some(Timeout::Never));
        assert_eq!(loaded.layers.len(), 2);
    }

    #[test]
    fn a_dropin_can_switch_a_vendor_block_off() {
        let vendor = source(
            "vendor",
            r#"
            [while.media_playing]
            suspend = "never"
            "#,
        );
        let dropin = source(
            "dropin",
            r#"
            [while.media_playing]
            enabled = false
            "#,
        );

        let loaded = load_clean(&[vendor, dropin]);
        let block = loaded
            .policy
            .block(BlockId::new(
                BlockKind::While,
                Condition::Fact(FactId::MediaPlaying),
            ))
            .expect("present");
        assert!(!block.enabled);
        assert_eq!(
            block.timeouts.get(Action::Suspend),
            Some(Timeout::Never),
            "a disabled block keeps its values so `explain` can show what it would do"
        );
    }

    #[test]
    fn a_misspelled_condition_is_a_fault_and_the_message_lists_the_valid_ones() {
        let bad = source(
            "/etc/idlectl/idlectl.toml",
            r#"
            [while.game_runing]
            suspend = "never"
            "#,
        );
        let (loaded, faults) = load(&[bad]);
        assert_eq!(faults.len(), 1);
        let text = faults[0].to_string();
        assert!(text.contains("game_runing"), "{text}");
        assert!(text.contains("steam_game_running"), "{text}");
        assert!(text.contains("/etc/idlectl/idlectl.toml"), "{text}");

        // The file carrying the typo is dropped whole, so its block is gone. What survives
        // is the compiled-in screen_off fallback, which is a different thing and is exactly
        // what must survive: a machine whose only configuration file has a typo still must
        // not hold a panel lit. Asserting `blocks.is_empty()` here would be asserting that
        // the safety net does not exist.
        assert_eq!(
            loaded.policy.blocks.len(),
            1,
            "nothing from the dropped file survives: the only block left is the fallback"
        );
        let fallback = loaded
            .policy
            .block(BlockId::new(BlockKind::While, Condition::Always))
            .expect("the compiled-in fallback is installed");
        assert_eq!(
            fallback.timeouts.get(Action::ScreenOff),
            Some(Timeout::After(FALLBACK_SCREEN_OFF))
        );
        assert_eq!(
            fallback.timeouts.get(Action::Suspend),
            None,
            "the fallback sets screen_off and nothing else"
        );
        assert!(
            !loaded.policy.faults.is_empty(),
            "the fault stands: the machine still must not sleep"
        );
    }

    #[test]
    fn a_misspelled_key_is_a_fault() {
        let bad = source(
            "vendor",
            r#"
            [while.always]
            suspned = "never"
            "#,
        );
        let (_, faults) = load(&[bad]);
        assert_eq!(faults.len(), 1);
    }

    #[test]
    fn an_unknown_clock_is_a_fault() {
        let bad = source(
            "vendor",
            r#"
            [while.always]
            clock = "wall"
            suspend = "30m"
            "#,
        );
        let (_, faults) = load(&[bad]);
        assert!(
            faults[0].to_string().contains("human_input"),
            "{}",
            faults[0]
        );
    }

    /// [CFG-10], [CFG-16], [TEST-13], [TEST-14]. One typo in one drop-in drops **that
    /// file** and nothing else: the vendor defaults survive, the daemon keeps running, and
    /// the fault is what stops the machine sleeping.
    ///
    /// The old behaviour -- reject the whole merged configuration -- had no answer for a
    /// first start after a bad edit, because there is no previous policy to keep.
    #[test]
    fn one_bad_file_is_dropped_and_the_others_survive() {
        let vendor = source(
            "vendor.toml",
            r#"
            [while.always]
            screen_off = "10m"
            suspend = "30m"
            "#,
        );
        let dropin = source(
            "99-broken.toml",
            r#"
            [while.always]
            suspend = "half an hour"
            "#,
        );

        let (loaded, faults) = load(&[vendor, dropin]);

        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].source, "99-broken.toml");
        assert_eq!(faults[0].location, "while.always.suspend");

        assert_eq!(
            loaded.layers,
            vec!["vendor.toml".to_owned()],
            "the broken layer is not applied and is not listed as applied"
        );
        let block = loaded
            .policy
            .block(BlockId::new(BlockKind::While, Condition::Always))
            .expect("the vendor block survived");
        assert_eq!(
            block.timeouts.get(Action::Suspend),
            Some(Timeout::After(Duration::from_secs(1_800))),
            "the vendor value is untouched by the file that was dropped"
        );

        // The fault travels to the engine, which floors the sleep actions on it.
        assert_eq!(loaded.policy.faults.len(), 1);
        assert_eq!(loaded.policy.faults[0].source, "99-broken.toml");
        assert!(loaded.policy.faults[0].message.contains("half an hour"));
    }

    /// A file that will not parse at all is dropped exactly like one with a bad value.
    #[test]
    fn an_unparseable_file_is_dropped_not_fatal() {
        let vendor = source("vendor.toml", "[while.always]\nsuspend = \"30m\"\n");
        let broken = source("10-broken.toml", "this is not toml at all\n");

        let (loaded, faults) = load(&[vendor, broken]);
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].source, "10-broken.toml");
        assert_eq!(loaded.policy.blocks.len(), 1);
        assert_eq!(loaded.policy.faults.len(), 1);
    }

    /// The hole per-file dropping does not cover: a broken vendor layer on a machine with
    /// nothing in `/etc`. A broken package must not be able to leave a panel lit.
    #[test]
    fn a_broken_vendor_layer_still_leaves_a_screen_off_floor() {
        let vendor = source(VENDOR_CONFIG, "[while.always]\nsuspend = \"nonsense\"\n");
        let (loaded, faults) = load(&[vendor]);

        assert_eq!(faults.len(), 1);
        let block = loaded
            .policy
            .block(BlockId::new(BlockKind::While, Condition::Always))
            .expect("the compiled-in fallback is installed");
        assert_eq!(
            block.timeouts.get(Action::ScreenOff),
            Some(Timeout::After(FALLBACK_SCREEN_OFF))
        );
        assert_eq!(
            block.timeouts.get(Action::Suspend),
            None,
            "the fallback sets screen_off and nothing else"
        );
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.message.contains("fallback"))
        );
        assert!(
            !loaded.policy.faults.is_empty(),
            "the fallback does not clear the fault: the machine still must not sleep"
        );
    }

    /// A broken vendor layer with a working admin layer keeps the admin layer, and does
    /// not add the fallback on top of an existing screen_off floor.
    #[test]
    fn the_fallback_does_not_fight_a_working_admin_layer() {
        let vendor = source(VENDOR_CONFIG, "[while.always]\nsuspend = \"nonsense\"\n");
        let admin = source(ADMIN_CONFIG, "[while.always]\nscreen_off = \"3m\"\n");
        let (loaded, _) = load(&[vendor, admin]);

        let block = loaded
            .policy
            .block(BlockId::new(BlockKind::While, Condition::Always))
            .expect("present");
        assert_eq!(
            block.timeouts.get(Action::ScreenOff),
            Some(Timeout::After(Duration::from_secs(180)))
        );
        assert_eq!(loaded.policy.blocks.len(), 1, "no duplicate always block");
    }

    #[test]
    fn an_empty_config_is_valid_and_does_nothing() {
        let loaded = load_clean(&[source("empty", "")]);
        assert!(loaded.policy.blocks.is_empty());
        assert_eq!(loaded.policy.min_idle, Policy::DEFAULT_MIN_IDLE);
        assert!(loaded.policy.faults.is_empty());
    }

    #[test]
    fn min_idle_is_read_and_must_be_finite() {
        let loaded = load_clean(&[source("v", "[general]\nmin_idle = \"90s\"\n")]);
        assert_eq!(loaded.policy.min_idle, Duration::from_secs(90));

        let (_, faults) = load(&[source("v", "[general]\nmin_idle = \"never\"\n")]);
        assert_eq!(
            faults.len(),
            1,
            "never is meaningless as a presence threshold"
        );
        assert_eq!(faults[0].location, "general.min_idle");
    }

    #[test]
    fn a_block_that_does_nothing_warns() {
        let loaded = load_clean(&[source("v", "[while.media_playing]\n")]);
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.location == "while.media_playing"),
            "{:?}",
            loaded.warnings
        );
    }

    /// [CEIL-6]. Every ceiling is announced at load, naming its file: there is one ceiling
    /// class, it is absolute, and it beats every floor there is.
    #[test]
    fn every_when_block_warns_that_it_overrides_every_floor() {
        let loaded = load_clean(&[source("v", "[when.lease_held]\nsuspend = \"0s\"\n")]);
        let warning = loaded
            .warnings
            .iter()
            .find(|w| w.location == "when.lease_held")
            .expect("a ceiling must be announced");
        assert_eq!(warning.source, "v");
        assert!(warning.message.contains("human_active"), "{warning}");
        assert!(warning.message.contains("doubt floor"), "{warning}");
    }

    /// [CFG-12]. `[facts.<name>] enabled = false` is the auditable escape hatch from a
    /// permanently-indeterminate detector, and it must be reachable: writing it used to be
    /// a fatal unknown-key error.
    #[test]
    fn a_fact_can_be_disabled_and_the_disabling_is_reported() {
        let loaded = load_clean(&[source(
            "v",
            "[facts.media_playing]\nenabled = false\n[while.always]\nsuspend = \"30m\"\n",
        )]);
        assert!(!loaded.policy.fact_enabled(FactId::MediaPlaying));
        assert!(loaded.policy.fact_enabled(FactId::HumanActive));
        assert!(
            loaded
                .warnings
                .iter()
                .any(|w| w.location == "facts.media_playing"),
            "doctor must be able to list every fact switched off"
        );
    }

    /// `local_service_busy` ships disabled and is switched on by giving it an endpoint --
    /// never by an unconfigured install, where it could only ever report indeterminate and
    /// would wedge the machine awake through the block-independent doubt veto.
    #[test]
    fn local_service_busy_is_enabled_by_its_endpoint() {
        let bare = load_clean(&[source("v", "")]);
        assert!(!bare.policy.fact_enabled(FactId::LocalServiceBusy));

        let configured = load_clean(&[source(
            "v",
            "[facts.local_service_busy]\ncounters_url = \"http://127.0.0.1:8080/metrics\"\n\
             idle_window = \"30m\"\n",
        )]);
        assert!(configured.policy.fact_enabled(FactId::LocalServiceBusy));
        let settings = configured.policy.fact_settings(FactId::LocalServiceBusy);
        assert_eq!(settings.idle_window, Some(Duration::from_secs(1_800)));
        assert!(settings.counters_url.is_some());

        // Enabled with no endpoint: refused, and said out loud.
        let hopeful = load_clean(&[source("v", "[facts.local_service_busy]\nenabled = true\n")]);
        assert!(!hopeful.policy.fact_enabled(FactId::LocalServiceBusy));
        assert!(
            hopeful
                .warnings
                .iter()
                .any(|w| w.message.contains("counters_url"))
        );
    }

    /// A key that is not defined for that particular fact is a configuration error: a
    /// silently-ignored `steam_root` under the service detector is a detector that is not
    /// configured the way its author believes.
    #[test]
    fn a_fact_key_that_belongs_to_another_fact_is_a_fault() {
        let (_, faults) = load(&[source(
            "v",
            "[facts.local_service_busy]\nsteam_root = \"/opt/steam\"\n",
        )]);
        assert_eq!(faults.len(), 1);
        assert_eq!(faults[0].location, "facts.local_service_busy.steam_root");

        let (_, unknown) = load(&[source("v", "[facts.not_a_fact]\nenabled = false\n")]);
        assert_eq!(unknown.len(), 1);
        assert!(unknown[0].to_string().contains("steam_downloading"));
    }

    #[test]
    fn steam_facts_take_their_own_keys() {
        let loaded = load_clean(&[source(
            "v",
            "[facts.steam_downloading]\nsteam_root = \"/games/steam\"\nwindow = \"5m\"\n",
        )]);
        let settings = loaded.policy.fact_settings(FactId::SteamDownloading);
        assert_eq!(settings.steam_root.as_deref(), Some("/games/steam"));
        assert_eq!(settings.window, Some(Duration::from_secs(300)));
    }

    #[test]
    fn both_kinds_of_block_load_together() {
        let text = r#"
            version = 1

            [general]
            min_idle = "5m"

            [while.always]
            screen_off = "10m"
            suspend = "30m"
            hibernate = "never"
            poweroff = "never"

            [while.after_resume]
            clock = "resume"
            suspend = "5m"
            hibernate = "5m"
            poweroff = "5m"

            [while.human_active]
            suspend = "never"
            hibernate = "never"
            poweroff = "never"

            [when.lease_held]
            suspend = "never"
        "#;
        let loaded = load_clean(&[source("v", text)]);
        assert_eq!(loaded.policy.blocks.len(), 4);

        let resume = loaded
            .policy
            .block(BlockId::new(
                BlockKind::While,
                Condition::Fact(FactId::AfterResume),
            ))
            .expect("present");
        assert_eq!(resume.clock, Clock::Resume);
        assert_eq!(
            resume.timeouts.get(Action::ScreenOff),
            None,
            "the settle window must not relight a panel on a headless wake"
        );
    }
}
