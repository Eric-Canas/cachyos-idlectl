//! `explain`, `doctor` and the machine-readable report.
//!
//! # The rule this module exists to keep
//!
//! [OBS-2]: the numbers reported here MUST be the numbers the decision actually used. Not
//! a recomputation, not a cheaper approximation printed alongside a decision made on a
//! different value. Everything below reads the stored [`Decision`] and the
//! [`idlectl_policy::ClockSnapshot`] it was computed against; nothing here calls
//! `resolve` again.
//!
//! The measurement behind the rule is [FACT-23]: a correct decision reported with an
//! approximate number sent somebody looking in exactly the wrong place, for hours. The ten
//! milliseconds it saved were not worth it.

use std::fmt::Write as _;

use idlectl_policy::{
    Action, ActionResolution, Block, BlockKind, ClockOrigin, ClockSnapshot, Condition, Deadline,
    Decision, FactId, FactState, ImplicitFloor, Policy,
};

use crate::engine::Engine;
use crate::facts::ago;

/// A one-line answer to "why did this fire?", for the [OBS-5] log record.
#[must_use]
pub fn why_due(decision: &Decision, action: Action, policy: &Policy) -> String {
    let resolution = decision.get(action);

    if resolution.ceiling_defeated_floor() {
        let ceilings: Vec<String> = resolution
            .ceiling_blocks()
            .iter()
            .map(ToString::to_string)
            .collect();
        let defeated: Vec<String> = resolution
            .holding()
            .iter()
            .map(ToString::to_string)
            .chain(
                resolution
                    .implicit_floors
                    .iter()
                    .map(|f| floor_name(*f, policy)),
            )
            .collect();
        return format!(
            "ceiling {} fired, defeating {}",
            ceilings.join(" + "),
            if defeated.is_empty() {
                "no floor".to_owned()
            } else {
                defeated.join(" + ")
            }
        );
    }

    let winners: Vec<String> = resolution
        .reasons
        .iter()
        .filter(|r| r.block.kind == BlockKind::While && r.deadline == resolution.floor)
        .map(|r| format!("{} ({} on the {} clock)", r.block, r.deadline, r.clock))
        .collect();

    if winners.is_empty() {
        // No block set a floor for this action at all: the maximum over the empty set is
        // "now", and the action was permitted because nothing objected. Saying so is
        // better than printing an empty reason, which reads like a bug.
        return "no block sets a floor for this action".to_owned();
    }
    winners.join(" + ")
}

fn floor_name(floor: ImplicitFloor, policy: &Policy) -> String {
    match floor {
        ImplicitFloor::Doubt(fact) => format!("doubt about {}", fact.name()),
        ImplicitFloor::ConfigFault(index) => policy.faults.get(index).map_or_else(
            || "a configuration fault".to_owned(),
            |f| {
                format!(
                    "the configuration fault in {} ({})",
                    f.source,
                    if f.location.is_empty() {
                        "whole file"
                    } else {
                        &f.location
                    }
                )
            },
        ),
    }
}

/// [OBS-1]: the full explanation of one action, or of all four.
#[must_use]
pub fn explain(engine: &Engine, only: Option<Action>) -> String {
    let Some(decision) = &engine.decision else {
        return "no evaluation has completed yet\n".to_owned();
    };
    let mut out = String::new();

    let _ = writeln!(
        out,
        "evaluated {}",
        engine
            .last_eval
            .map_or_else(|| "never".to_owned(), |t| ago(decision.now.since(t)))
    );
    let _ = writeln!(out, "min_idle  {}s", engine.policy.min_idle.as_secs());
    let _ = writeln!(out);

    for action in Action::ALL {
        if only.is_some_and(|wanted| wanted != action) {
            continue;
        }
        explain_action(&mut out, engine, decision, action);
        let _ = writeln!(out);
    }

    out
}

fn explain_action(out: &mut String, engine: &Engine, decision: &Decision, action: Action) {
    let resolution = decision.get(action);
    let _ = writeln!(out, "── {} ──", action.name());

    // [ACT-13]: an action with no mechanism is reported as unavailable rather than
    // silently treated as done, and every block that sets a key for it is named as inert.
    if action == Action::ScreenOff && !engine.screen_off_available {
        let _ = writeln!(
            out,
            "  UNAVAILABLE as an action: no session agent offers a blanking mechanism."
        );
        let inert: Vec<String> = engine
            .policy
            .blocks
            .iter()
            .filter(|b| b.timeouts.get(Action::ScreenOff).is_some())
            .map(|b| b.id.to_string())
            .collect();
        if !inert.is_empty() {
            let _ = writeln!(out, "  inert blocks: {}", inert.join(", "));
        }
        let _ = writeln!(
            out,
            "  This raises no veto on anything: an action that is absent is knowledge, not doubt."
        );
    }

    for block in &engine.policy.blocks {
        explain_block(out, engine, decision, resolution, action, block);
    }

    for floor in &resolution.implicit_floors {
        let _ = writeln!(
            out,
            "  implicit floor: never   {}",
            floor_reason(*floor, engine)
        );
    }

    let _ = writeln!(out, "  ---");
    let _ = writeln!(out, "  floor    {}", resolution.floor);
    let _ = writeln!(out, "  ceiling  {}", resolution.ceiling);
    let _ = writeln!(
        out,
        "  resolved {}   {}",
        resolution.deadline,
        match resolution.deadline.remaining_at(decision.now) {
            _ if resolution.due => "DUE".to_owned(),
            Some(d) => format!("in {}", human(d)),
            None => "never".to_owned(),
        }
    );

    let won = won_by(resolution);
    if !won.is_empty() {
        let _ = writeln!(out, "  held by  {}", won.join(", "));
    }
    if resolution.ceiling_defeated_floor() {
        let _ = writeln!(
            out,
            "  WARNING  a ceiling is pulling this action earlier than its floor allows"
        );
    }
}

fn explain_block(
    out: &mut String,
    engine: &Engine,
    decision: &Decision,
    resolution: &ActionResolution,
    action: Action,
    block: &Block,
) {
    let state = condition_state(engine, block.id.condition);
    let detail = condition_detail(engine, block.id.condition);

    // [COMP-2b]: a block that sets no key for this action does not participate in it,
    // whatever its condition says. Silence is not `never`, and printing the distinction is
    // the difference between "why is my download block not stopping the screen from
    // blanking?" taking a minute or an afternoon.
    let Some(timeout) = block.timeouts.get(action) else {
        let _ = writeln!(
            out,
            "  {:<28} {:<14} does not participate in {}",
            block.id.to_string(),
            state.to_string(),
            action.name()
        );
        return;
    };

    let reason = resolution.reasons.iter().find(|r| r.block == block.id);
    let origin_kind = self_origin(engine, block, &engine.clocks);

    let mut line = format!(
        "  {:<28} {:<14} {:<9} clock={:<10}",
        block.id.to_string(),
        state.to_string(),
        idlectl_config::format_timeout(timeout),
        block.clock.to_string()
    );

    if !block.enabled {
        let _ = writeln!(line, " (disabled)");
        out.push_str(&line);
        return;
    }

    match origin_kind {
        ClockOrigin::At(origin) => {
            let _ = write!(
                line,
                " origin=+{}s ({})",
                origin.as_nanos() / 1_000_000_000,
                ago(decision.now.since(origin))
            );
        }
        // [CLK-12]: normal absence and a fault must be distinguishable. They compose
        // alike and they mean completely different things to whoever is reading this.
        ClockOrigin::NotYet => line.push_str(" origin=not yet this boot"),
        ClockOrigin::Unreadable => {
            line.push_str(" origin=UNREADABLE (the detector feeding this clock is faulted)")
        }
    }

    match reason {
        Some(r) if r.satisfied_by_request => {
            let _ = write!(line, " -> now (satisfied by the request)");
        }
        Some(r) => {
            let _ = write!(line, " -> {}", r.deadline);
        }
        None => line.push_str(" -> contributes nothing"),
    }

    let _ = writeln!(line);
    out.push_str(&line);

    if matches!(state, FactState::Indeterminate | FactState::Unavailable) && !detail.is_empty() {
        let _ = writeln!(out, "  {:<28}   {}", "", detail);
    }
}

/// The origin the engine resolved for this block's clock, with [CLK-12]'s distinction
/// preserved.
fn self_origin(engine: &Engine, block: &Block, clocks: &ClockSnapshot) -> ClockOrigin {
    let since = engine.edges.since(block.id);
    clocks.origin_kind(block.clock, since)
}

fn condition_state(engine: &Engine, condition: Condition) -> FactState {
    match condition {
        Condition::Always => FactState::True,
        Condition::Fact(id) => engine
            .readings
            .get(&id)
            .map_or(FactState::Unavailable, |r| r.state),
        // A condition this build does not know how to read is not knowledge. Doubt is the
        // only honest answer, and it is the one that holds the sleep actions.
        _ => FactState::Indeterminate,
    }
}

fn condition_detail(engine: &Engine, condition: Condition) -> String {
    match condition {
        Condition::Always => String::new(),
        Condition::Fact(id) => engine
            .readings
            .get(&id)
            .map(|r| r.detail.clone())
            .unwrap_or_default(),
        _ => "this build does not know how to read this condition".to_owned(),
    }
}

fn floor_reason(floor: ImplicitFloor, engine: &Engine) -> String {
    match floor {
        ImplicitFloor::Doubt(fact) => {
            let detail = engine
                .readings
                .get(&fact)
                .map(|r| r.detail.clone())
                .unwrap_or_default();
            format!("{} is indeterminate: {detail}", fact.name())
        }
        ImplicitFloor::ConfigFault(_) => floor_name(floor, &engine.policy),
    }
}

fn won_by(resolution: &ActionResolution) -> Vec<String> {
    let mut names: Vec<String> = resolution
        .holding()
        .iter()
        .map(ToString::to_string)
        .collect();
    names.extend(resolution.implicit_floors.iter().map(|f| match f {
        ImplicitFloor::Doubt(fact) => format!("doubt about {}", fact.name()),
        ImplicitFloor::ConfigFault(_) => "a configuration fault".to_owned(),
    }));
    names
}

/// [OBS-3]: what works here, what does not, and what else claims to own this machine.
///
/// Returns the text and whether everything is in order. [OBS-4] pins the second: `doctor`
/// exits non-zero if **any** configuration fault, **any** indeterminate fact, or **any**
/// standing hazard is present. All three, not just the first.
#[must_use]
pub async fn doctor(engine: &Engine) -> (String, bool) {
    let mut out = String::new();
    let mut healthy = true;
    let now = crate::clock::now();

    // 8. The configuration actually read, in effective order.
    let _ = writeln!(out, "configuration");
    for layer in &engine.layers {
        let _ = writeln!(out, "  layer     {layer}");
    }
    // 9. Any fault currently vetoing.
    for fault in &engine.faults {
        healthy = false;
        let _ = writeln!(out, "  FAULT     {fault}");
    }
    if !engine.faults.is_empty() {
        let _ = writeln!(
            out,
            "  Suspend, hibernate and poweroff are held at `never` until this is fixed."
        );
        let _ = writeln!(
            out,
            "  screen_off is deliberately unaffected: a typo must not be able to burn a panel."
        );
    }
    for warning in &engine.warnings {
        let _ = writeln!(out, "  warning   {warning}");
    }

    // 7. The last completed evaluation. This is the check that catches an [ACT-8]-class
    // failure -- a unit skipped by a condition, a code path that is correct and
    // unreachable -- from the outside, which is the only place it is visible.
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "last evaluation  {}",
        engine.last_eval.map_or_else(
            || "NEVER -- the daemon has not evaluated once".to_owned(),
            |t| ago(now.since(t))
        )
    );
    if engine.last_eval.is_none() {
        healthy = false;
    }
    let _ = writeln!(
        out,
        "suspended total  {} this boot",
        human(engine.resume.suspended_total())
    );
    if engine.dry_run {
        let _ = writeln!(
            out,
            "MODE             dry run: decisions are logged and never applied"
        );
    }

    // 1, 2, 3. Every fact, its state, and the specific fault or missing capability.
    let _ = writeln!(out);
    let _ = writeln!(out, "facts");
    for id in FactId::ALL {
        let enabled = engine.policy.fact_enabled(id);
        let reading = engine.readings.get(&id);
        let state = reading.map_or(FactState::Unavailable, |r| r.state);
        let detail = reading.map(|r| r.detail.as_str()).unwrap_or("not sampled");

        let flag = if !enabled { " [disabled]" } else { "" };
        let _ = writeln!(
            out,
            "  {:<20} {:<14}{flag} {detail}",
            id.name(),
            state.to_string()
        );

        if state == FactState::Indeterminate && enabled {
            healthy = false;
            let inert: Vec<String> = engine
                .policy
                .blocks
                .iter()
                .filter(|b| b.id.condition == Condition::Fact(id))
                .map(|b| b.id.to_string())
                .collect();
            if !inert.is_empty() {
                let _ = writeln!(
                    out,
                    "  {:<20}   blocks made inert: {}",
                    "",
                    inert.join(", ")
                );
            }
        }
        if state == FactState::Unavailable && enabled {
            let inert: Vec<String> = engine
                .policy
                .blocks
                .iter()
                .filter(|b| b.id.condition == Condition::Fact(id))
                .map(|b| b.id.to_string())
                .collect();
            if !inert.is_empty() {
                let _ = writeln!(out, "  {:<20}   inert blocks: {}", "", inert.join(", "));
            }
        }
    }

    // 10. Which of the three human_active cases is in force. A machine permanently awake
    // because its idle agent died must be diagnosable in one command, and this is it.
    let _ = writeln!(out);
    let _ = writeln!(out, "human presence");
    let human_state = engine
        .readings
        .get(&FactId::HumanActive)
        .map_or(FactState::Unavailable, |r| r.state);
    let case = match (human_state, engine.clocks.human_input) {
        (FactState::True, _) => "case 1: a human touched an input device within min_idle",
        (FactState::Indeterminate, _) => {
            "case 3: THE IDLE CLOCK IS UNREADABLE -- the agent is absent, stale, or its protocol is erroring"
        }
        (_, ClockOrigin::NotYet) => {
            "case 2: nothing has been touched this boot; the input path is healthy"
        }
        _ => "case 2: no input within min_idle; the input path is healthy",
    };
    let _ = writeln!(out, "  {case}");
    let _ = writeln!(out, "  human_input origin: {}", engine.clocks.human_input);
    if engine.agents.is_empty() {
        let _ = writeln!(out, "  no session agent is registered");
    }
    for (name, agent) in engine.agents.iter() {
        let _ = writeln!(
            out,
            // The GPU holder count is here because it is the only visible sign that the
            // DRM fdinfo source is alive: the daemon cannot read fdinfo itself, so a
            // `gpu_busy_*` reading `unavailable` on an AMD or Intel machine is explained
            // by this number being zero and by nothing else in this report.
            "  agent {name} in session {} (uid {}), can_blank={}, {} GPU holder(s), last report {}",
            agent.session_id,
            agent.uid,
            agent.can_blank,
            agent.gpu_holders.len(),
            ago(now.since(agent.last_report))
        );
    }

    // 12. Whether screen_off is available as an action at all.
    let _ = writeln!(out);
    let _ = writeln!(
        out,
        "screen_off       {}",
        if engine.screen_off_available {
            "available"
        } else {
            "UNAVAILABLE: no agent offers a blanking mechanism"
        }
    );

    // 4. Every ceiling, as a standing hazard. A ceiling is the only construct that can act
    // against a floor, so its mere existence is worth reporting whether or not it is true
    // right now.
    let ceilings: Vec<&Block> = engine.policy.blocks_of(BlockKind::When).collect();
    if !ceilings.is_empty() {
        healthy = false;
        let _ = writeln!(out);
        let _ = writeln!(out, "standing hazards");
        for block in ceilings {
            let _ = writeln!(
                out,
                "  ceiling {} can pull an action earlier than any floor allows",
                block.id
            );
        }
    }

    // Leases held. Not numbered in [OBS-3], and included anyway: `lease_held` above says
    // only that one exists, and "which job is holding this machine awake" is the next
    // question every single time.
    let _ = writeln!(out);
    if engine.leases.is_empty() {
        let _ = writeln!(out, "leases           none held");
    } else {
        let _ = writeln!(out, "leases");
        for lease in engine.leases.iter() {
            let _ = writeln!(
                out,
                "  {:<24} uid {:<6} expires in {:<10} {}",
                lease.who,
                lease.uid,
                lease.expires.since(now).as_secs().to_string(),
                lease.why
            );
        }
    }

    // 6. Forced actions since boot.
    if !engine.forced.is_empty() {
        let _ = writeln!(out);
        let _ = writeln!(out, "forced actions this boot: {}", engine.forced.len());
        for forced in &engine.forced {
            let _ = writeln!(
                out,
                "  {} {} by uid {}: {}",
                ago(now.since(forced.at)),
                forced.action.name(),
                forced.uid,
                forced.why
            );
        }
    }

    // 11. The conflict scan. Cheap, and it tests the conclusion that matters more than any
    // single bug: that exactly one thing decides when this machine sleeps.
    let conflicts = crate::facts::sessions::conflict_scan(&engine.bus).await;
    let _ = writeln!(out);
    let _ = writeln!(out, "conflict scan");
    if conflicts.is_empty() {
        let _ = writeln!(
            out,
            "  no other candidate owner of this machine's power state found"
        );
    } else {
        healthy = false;
        for conflict in conflicts {
            let _ = writeln!(out, "  CONFLICT  {conflict}");
        }
        let _ = writeln!(
            out,
            "  Two things deciding when a machine sleeps is worse than either one deciding badly."
        );
    }

    // 5. WITHDRAWN -- there is no `collapsible` key to report.

    (out, healthy)
}

/// The machine-readable report.
///
/// This is what an external agent reads: a remote relay that woke this machine and needs
/// to know whether it may put it back to sleep, a monitoring check, or a desktop applet.
/// It carries the same numbers `explain` prints, from the same stored decision.
///
/// `schema` is a plain integer and is the contract. Fields may be added without changing
/// it; a field that changes meaning or disappears bumps it.
#[must_use]
pub fn report_json(engine: &Engine) -> serde_json::Value {
    use serde_json::json;

    let now = crate::clock::now();
    let facts: Vec<_> = FactId::ALL
        .into_iter()
        .map(|id| {
            let reading = engine.readings.get(&id);
            json!({
                "name": id.name(),
                "state": reading.map_or(FactState::Unavailable, |r| r.state).to_string(),
                "detail": reading.map(|r| r.detail.clone()).unwrap_or_default(),
                "enabled": engine.policy.fact_enabled(id),
            })
        })
        .collect();

    let actions: Vec<_> = Action::ALL
        .into_iter()
        .map(|action| {
            let resolution = engine.decision.as_ref().map(|d| d.get(action));
            json!({
                "name": action.name(),
                "due": resolution.is_some_and(|r| r.due),
                "deadline_usec": resolution.map_or(u64::MAX, |r| deadline_usec(r.deadline)),
                "remaining_secs": resolution
                    .and_then(|r| r.deadline.remaining_at(now))
                    .map(|d| d.as_secs()),
                "floor_usec": resolution.map_or(u64::MAX, |r| deadline_usec(r.floor)),
                "ceiling_usec": resolution.map_or(u64::MAX, |r| deadline_usec(r.ceiling)),
                "held_by": resolution.map(won_by).unwrap_or_default(),
                "available": action != Action::ScreenOff || engine.screen_off_available,
            })
        })
        .collect();

    json!({
        "schema": 1,
        "version": env!("CARGO_PKG_VERSION"),
        "dry_run": engine.dry_run,
        "layers": engine.layers,
        "faults": engine.faults.iter().map(|f| json!({
            "source": f.source,
            "location": f.location,
            "message": f.detail(),
        })).collect::<Vec<_>>(),
        "min_idle_secs": engine.policy.min_idle.as_secs(),
        "after_resume": engine.resume.after_resume(),
        "suspended_total_secs": engine.resume.suspended_total().as_secs(),
        "last_evaluation_secs_ago": engine.last_eval.map(|t| now.since(t).as_secs()),
        "human_input_origin": engine.clocks.human_input.to_string(),
        "resume_origin": engine.clocks.resume.to_string(),
        "facts": facts,
        "actions": actions,
        "leases": engine.leases.iter().map(|l| json!({
            "who": l.who,
            "why": l.why,
            "uid": l.uid,
            "expires_usec": l.expires.as_micros(),
        })).collect::<Vec<_>>(),
    })
}

/// `UINT64_MAX` is "never" on the wire.
///
/// Not `0`, which is a real instant (boot), and not a nullable field: a client that
/// forgets to special-case the sentinel gets a deadline absurdly far in the future — a
/// machine that stays awake, which is the cheap error — instead of one already in the past.
#[must_use]
pub fn deadline_usec(deadline: Deadline) -> u64 {
    match deadline {
        Deadline::Never => u64::MAX,
        Deadline::At(at) => at.as_micros(),
    }
}

/// A duration a person can read at a glance.
#[must_use]
pub fn human(d: std::time::Duration) -> String {
    let secs = d.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn never_is_the_uint64_sentinel_and_boot_is_not() {
        assert_eq!(deadline_usec(Deadline::Never), u64::MAX);
        // Boot is a real instant. A client that treated 0 as "never" would read a machine
        // that may sleep immediately as one that may never sleep.
        assert_eq!(
            deadline_usec(Deadline::At(idlectl_policy::BootInstant::BOOT)),
            0
        );
    }

    #[test]
    fn human_durations_round_down_rather_than_inventing_precision() {
        assert_eq!(human(std::time::Duration::from_secs(59)), "59s");
        assert_eq!(human(std::time::Duration::from_secs(90)), "1m30s");
        assert_eq!(
            human(std::time::Duration::from_secs(3 * 3600 + 61)),
            "3h01m"
        );
    }
}
