//! What a session backend has to be able to do.
//!
//! Two, and only two. Report how long the session has been idle, and turn its outputs off
//! and on again. The agent commands nothing else, in its own session or anywhere else, and
//! there is no configuration that can give it more.

use std::time::Duration;

/// The idle state of a session.
///
/// Deliberately not a bare [`Duration`]. [CLK-5]: the human-activity signal is only
/// trustworthy in the negative direction, and a backend that has lost its idle protocol
/// must be able to say so. Reporting "idle for a long time" in that state would permit a
/// sleep on the strength of an observation nobody made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Idle {
    /// The session has been idle this long.
    For(Duration),
    /// The idle protocol is not answering. The daemon turns this into
    /// `human_active = INDETERMINATE`, which vetoes every sleep action.
    Unknown,
}

impl Idle {
    /// The wire encoding: microseconds, with `UINT64_MAX` meaning "unknown".
    #[must_use]
    pub fn to_usec(self) -> u64 {
        match self {
            Idle::For(d) => u64::try_from(d.as_micros()).unwrap_or(u64::MAX - 1),
            Idle::Unknown => u64::MAX,
        }
    }
}

/// A session the agent can observe and blank.
pub trait Backend: Send + Sync {
    /// How long since the last real input.
    ///
    /// "Real input" is load-bearing: a compositor heartbeat that ticks regardless of input
    /// is not an answer to this question, and a backend built on one reports an idle
    /// session as active forever.
    fn idle(&self) -> Idle;

    /// Turn the session's outputs off (`true`) or on (`false`).
    ///
    /// Idempotent in both directions. Blanking an already-blank session is not an error;
    /// the daemon relies on that, because it does not re-issue an action already in effect
    /// and would otherwise have to track the panel state twice.
    fn set_blank(&self, blank: bool) -> Result<(), String>;

    /// Whether this session offers a blanking mechanism at all.
    ///
    /// `false` is a real answer and not a failure. The daemon reports `screen_off` as
    /// UNAVAILABLE as an action, names every block whose `screen_off` key is therefore
    /// inert, and raises no veto on anything — an action that is absent is knowledge, not
    /// doubt.
    fn can_blank(&self) -> bool;

    /// What this backend is, for the journal and for `doctor`.
    fn describe(&self) -> String;
}
