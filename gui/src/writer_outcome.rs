use crate::protocol::{
    Header, MAX_RECORD, MAX_RECORDS, MAX_REPORT, RecoveryStep, ResultRecord, StatusContext,
    SyncRecord, literal_relative, same_context, validate_recovery, validate_sync_record,
};
use std::path::Path;

/// The phase in which an occurrence was emitted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WriterPhase {
    /// The reviewed writer's preliminary capture projection.
    Preflight,
    /// A reached actual stage, not a completed transaction.
    Actual,
}

/// A protocol occurrence and its emitting phase.
#[derive(Debug)]
pub struct WriterEvent {
    /// Projection or actual execution.
    pub phase: WriterPhase,
    /// The original typed occurrence.
    pub record: SyncRecord,
}

/// A context-bound writer receipt.
#[derive(Debug)]
pub struct WriterReceipt {
    /// Resolved writer context.
    pub context: StatusContext,
    /// Ordered occurrences, including projections and repeated decisions.
    pub events: Vec<WriterEvent>,
    /// Authoritative exit, either success or attention.
    pub exit_code: i32,
    /// CLI error classification, if present.
    pub error_kind: Option<String>,
}

/// A complete report or a terminal failure without an authoritative report.
#[derive(Debug)]
pub enum WriterOutcome {
    /// A bound success or attention receipt.
    Receipt(Box<WriterReceipt>),
    /// Early refusal or terminal error. Events exist only when context-bound.
    Error {
        /// Authoritative process exit.
        exit_code: i32,
        /// Validated context when emitted.
        context: Option<Box<StatusContext>>,
        /// Bound occurrences before the error.
        events: Vec<WriterEvent>,
        /// CLI error classification.
        error_kind: Option<String>,
        /// Terminal-only recovery data; never an execution request.
        recovery: Option<Vec<RecoveryStep>>,
    },
}

fn valid_key(key: &str, scope: &str) -> bool {
    let key = Path::new(key);
    match scope {
        "home" => literal_relative(key),
        "system" => key.strip_prefix("/").is_ok_and(literal_relative),
        _ => false,
    }
}

fn valid_apply_key(key: &str, scope: &str) -> bool {
    if scope == "repo_source" {
        literal_relative(Path::new(key))
    } else {
        valid_key(key, scope)
    }
}

fn validate_event(record: &SyncRecord, phase: WriterPhase) -> Result<(), String> {
    match record {
        SyncRecord::Stage { stage, dry_run } => {
            if phase == WriterPhase::Preflight {
                if stage != "capture" || !dry_run {
                    return Err("invalid preflight stage".into());
                }
            } else if *dry_run
                || !matches!(
                    stage.as_str(),
                    "capture"
                        | "commit"
                        | "pull"
                        | "apply"
                        | "reencrypt"
                        | "hooks"
                        | "push"
                        | "blocked_by_repo_hold"
                )
            {
                return Err("invalid actual stage".into());
            }
        }
        SyncRecord::Capture(_) if phase == WriterPhase::Preflight => {
            validate_sync_record(record)?;
        }
        SyncRecord::Resolved {
            key,
            scope,
            hypothetical,
            effect,
        } => {
            if *hypothetical != (phase == WriterPhase::Preflight)
                || !valid_key(&key.raw, scope)
                || !matches!(
                    effect.as_str(),
                    "captured" | "written" | "forgotten" | "kept"
                )
            {
                return Err("invalid resolution".into());
            }
        }
        SyncRecord::Undecided { scope, .. } => {
            if !matches!(
                scope.as_str(),
                "home" | "system" | "repo_source" | "context" | "hooks"
            ) {
                return Err("invalid undecided scope".into());
            }
        }
        SyncRecord::Hook {
            command,
            outcome,
            exit_code,
        } if phase == WriterPhase::Actual
            && command.is_none()
            && matches!(outcome.as_str(), "disabled" | "deferred")
            && exit_code.is_none() => {}
        SyncRecord::Refusal { kind, .. }
            if phase == WriterPhase::Actual && matches!(kind.as_str(), "push" | "render") => {}
        _ => return Err("unexpected writer event".into()),
    }
    Ok(())
}

/// A reached actual stage; it does not imply completion or success.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressStage {
    /// Capture local changes.
    Capture,
    /// Create a commit.
    Commit,
    /// Pull remote changes.
    Pull,
    /// Apply changes.
    Apply,
    /// Re-encrypt managed files.
    Reencrypt,
    /// Reach the hooks stage.
    Hooks,
    /// Push changes.
    Push,
    /// Work blocked by a repository hold.
    BlockedByRepoHold,
}

/// Incrementally projects bounded writer stdout into actual stage names.
pub struct WriterProgress<'a> {
    expected: &'a StatusContext,
    start: usize,
    scanned: usize,
    records: usize,
    emitted: usize,
    phase: Option<WriterPhase>,
    preflight: bool,
    stopped: bool,
}

impl<'a> WriterProgress<'a> {
    /// Binds progress to the retained status context.
    ///
    /// # Errors
    /// Rejects an invalid status context or missing repository identity.
    pub fn new(expected: &'a StatusContext) -> Result<Self, String> {
        expected.validate("status")?;
        if expected.repo_id.is_none() {
            return Err("missing repository identity".into());
        }
        Ok(Self {
            expected,
            start: 0,
            scanned: 0,
            records: 0,
            emitted: 0,
            phase: None,
            preflight: false,
            stopped: false,
        })
    }

    /// Reads complete LF records from the same accumulated stdout after each poll.
    /// The final receipt and process exit remain authoritative.
    pub fn observe(&mut self, stdout: &[u8], mut emit: impl FnMut(ProgressStage)) {
        if self.stopped || stdout.len() > MAX_REPORT || stdout.len() < self.scanned {
            self.stopped = true;
            return;
        }
        while let Some(byte) = stdout.get(self.scanned) {
            self.scanned = self.scanned.saturating_add(1);
            if *byte != b'\n' {
                if self.scanned.saturating_sub(self.start) > MAX_RECORD {
                    self.stopped = true;
                    return;
                }
                continue;
            }
            let end = self.scanned.saturating_sub(1);
            let Some(line) = stdout.get(self.start..end) else {
                self.stopped = true;
                return;
            };
            self.start = self.scanned;
            if self.records >= MAX_RECORDS || line.is_empty() || line.last() == Some(&b'\r') {
                self.stopped = true;
                return;
            }
            let first = self.records == 0;
            self.records = self.records.saturating_add(1);
            let Ok(header) = serde_json::from_slice::<Header>(line) else {
                self.stopped = true;
                return;
            };
            if first {
                let valid = header.record == "context"
                    && serde_json::from_slice::<StatusContext>(line).is_ok_and(|context| {
                        context.validate("sync").is_ok() && same_context(self.expected, &context)
                    });
                if !valid {
                    self.stopped = true;
                    return;
                }
                continue;
            }
            if header.record == "result" {
                self.stopped = true;
                return;
            }
            let Ok(record) = serde_json::from_slice::<SyncRecord>(line) else {
                self.stopped = true;
                return;
            };
            if let SyncRecord::Stage { stage, dry_run } = &record {
                if *dry_run {
                    if self.preflight || self.phase.is_some() || stage != "capture" {
                        self.stopped = true;
                        return;
                    }
                    self.preflight = true;
                    self.phase = Some(WriterPhase::Preflight);
                } else {
                    self.phase = Some(WriterPhase::Actual);
                }
            }
            let Some(phase) = self.phase else {
                self.stopped = true;
                return;
            };
            if validate_event(&record, phase).is_err() {
                self.stopped = true;
                return;
            }
            if let SyncRecord::Stage {
                stage,
                dry_run: false,
            } = record
            {
                let stage = match stage.as_str() {
                    "capture" => ProgressStage::Capture,
                    "commit" => ProgressStage::Commit,
                    "pull" => ProgressStage::Pull,
                    "apply" => ProgressStage::Apply,
                    "reencrypt" => ProgressStage::Reencrypt,
                    "hooks" => ProgressStage::Hooks,
                    "push" => ProgressStage::Push,
                    "blocked_by_repo_hold" => ProgressStage::BlockedByRepoHold,
                    _ => {
                        self.stopped = true;
                        return;
                    }
                };
                self.emitted = self.emitted.saturating_add(1);
                emit(stage);
                if self.emitted >= 64 {
                    self.stopped = true;
                    return;
                }
            }
        }
    }
}

fn parse_result(line: &[u8], exit: i32) -> Result<ResultRecord, String> {
    let value: ResultRecord = serde_json::from_slice(line).map_err(|_| "invalid result")?;
    if value.exit_code != exit
        || value.outcome != if exit == 0 { "done" } else { "error" }
        || !matches!(exit, 0 | 1 | 3 | 4)
        || (exit == 0 && (value.error_kind.is_some() || value.recovery.is_some()))
    {
        return Err("writer exit mismatch".into());
    }
    if let Some(steps) = &value.recovery {
        validate_recovery(steps)?;
    }
    Ok(value)
}

/// The push behavior bound to the reviewed selected-line operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectedTarget {
    /// The review explicitly bound a local-only commit.
    Local,
    /// The review explicitly bound a push target.
    Push,
}

/// Parses a selected-line writer receipt against its reviewed push behavior.
///
/// # Errors
/// Rejects incomplete, unbound, malformed or contradictory responses.
pub fn parse_selected_outcome(
    stdout: &[u8],
    exit: Option<i32>,
    expected: &StatusContext,
    target: SelectedTarget,
) -> Result<WriterOutcome, String> {
    expected.validate("status")?;
    if expected.repo_id.is_none() || stdout.len() > MAX_REPORT || !stdout.ends_with(b"\n") {
        return Err("invalid selected writer framing or expected context".into());
    }
    let exit = exit.ok_or("abnormal selected writer exit")?;
    let stages = match target {
        SelectedTarget::Local => &["capture", "commit"][..],
        SelectedTarget::Push => &["capture", "commit", "push"][..],
    };
    let mut context = None;
    let mut events = Vec::new();
    let mut stage_count = 0_usize;
    let mut attention = false;
    let mut result = None;
    for (index, line) in stdout
        .strip_suffix(b"\n")
        .ok_or("missing newline")?
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if index >= MAX_RECORDS
            || line.is_empty()
            || line.len() > MAX_RECORD
            || line.last() == Some(&b'\r')
            || result.is_some()
        {
            return Err("invalid selected writer framing".into());
        }
        let header: Header = serde_json::from_slice(line).map_err(|_| "invalid writer record")?;
        match header.record.as_str() {
            "context" if index == 0 => {
                let value: StatusContext =
                    serde_json::from_slice(line).map_err(|_| "invalid context")?;
                value.validate("sync")?;
                context = Some(value);
            }
            "result" => result = Some(parse_result(line, exit)?),
            _ => {
                if !context
                    .as_ref()
                    .is_some_and(|ctx| same_context(expected, ctx))
                {
                    return Err("selected writer context changed before events".into());
                }
                let record: SyncRecord =
                    serde_json::from_slice(line).map_err(|_| "invalid writer event")?;
                match &record {
                    SyncRecord::Stage {
                        stage,
                        dry_run: false,
                    } => {
                        if stages.get(stage_count).copied() != Some(stage.as_str()) {
                            return Err("unexpected selected stage".into());
                        }
                        stage_count = stage_count.checked_add(1).ok_or("too many stages")?;
                    }
                    SyncRecord::Resolved { .. } if stage_count > 0 => {
                        validate_event(&record, WriterPhase::Actual)?;
                    }
                    SyncRecord::Undecided { .. } | SyncRecord::Refusal { .. }
                        if stage_count > 0 =>
                    {
                        validate_event(&record, WriterPhase::Actual)?;
                        attention = true;
                    }
                    _ => return Err("unexpected selected writer event".into()),
                }
                events.push(WriterEvent {
                    phase: WriterPhase::Actual,
                    record,
                });
            }
        }
    }
    let result = result.ok_or("missing final result")?;
    let bound = context
        .as_ref()
        .is_some_and(|ctx| same_context(expected, ctx));
    if exit == 0 {
        if !bound || stage_count != stages.len() || attention {
            return Err("incomplete selected writer receipt".into());
        }
        return Ok(WriterOutcome::Receipt(Box::new(WriterReceipt {
            context: context.ok_or("missing context")?,
            events,
            exit_code: exit,
            error_kind: result.error_kind,
        })));
    }
    Ok(WriterOutcome::Error {
        exit_code: exit,
        context: context.map(Box::new),
        events,
        error_kind: result.error_kind,
        recovery: result.recovery,
    })
}

#[derive(Default)]
struct HookStages {
    reached: bool,
    received: bool,
}

#[derive(Default)]
struct SyncStages {
    phase: Option<WriterPhase>,
    preflight: bool,
    actual_capture: bool,
    hooks: HookStages,
    current_stage: String,
}

impl SyncStages {
    fn observe(&mut self, record: &SyncRecord) -> Result<WriterPhase, String> {
        if let SyncRecord::Stage { stage, dry_run } = record {
            if *dry_run {
                if self.preflight || self.phase.is_some() || stage != "capture" {
                    return Err("unexpected preflight".into());
                }
                self.preflight = true;
                self.phase = Some(WriterPhase::Preflight);
            } else {
                self.phase = Some(WriterPhase::Actual);
                self.current_stage.clone_from(stage);
                self.actual_capture |= stage == "capture";
                if stage == "hooks" && self.hooks.reached {
                    return Err("duplicate hooks stage".into());
                }
                self.hooks.reached |= stage == "hooks" && self.actual_capture;
            }
        }
        let current = self.phase.ok_or("event before stage")?;
        validate_event(record, current)?;
        if let SyncRecord::Hook { outcome, .. } = record {
            if self.current_stage != "hooks" || outcome != "deferred" || self.hooks.received {
                return Err("invalid deferred hook receipt".into());
            }
            self.hooks.received = true;
        }
        Ok(current)
    }
}

/// Parses the bounded stdout of a reviewed `sync --defer-hooks` writer.
///
/// # Errors
/// Rejects incomplete, unbound, malformed or contradictory responses.
pub fn parse_writer_outcome(
    stdout: &[u8],
    exit: Option<i32>,
    expected: &StatusContext,
) -> Result<WriterOutcome, String> {
    expected.validate("status")?;
    if expected.repo_id.is_none() || stdout.len() > MAX_REPORT || !stdout.ends_with(b"\n") {
        return Err("invalid writer framing or expected context".into());
    }
    let exit = exit.ok_or("abnormal writer exit")?;
    let mut context = None;
    let mut events = Vec::new();
    let mut stages = SyncStages::default();
    let mut result = None;
    for (index, line) in stdout
        .strip_suffix(b"\n")
        .ok_or("missing newline")?
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if index >= MAX_RECORDS
            || line.is_empty()
            || line.len() > MAX_RECORD
            || line.last() == Some(&b'\r')
            || result.is_some()
        {
            return Err("invalid writer framing".into());
        }
        let header: Header = serde_json::from_slice(line).map_err(|_| "invalid writer record")?;
        match header.record.as_str() {
            "context" if index == 0 => {
                let value: StatusContext =
                    serde_json::from_slice(line).map_err(|_| "invalid context")?;
                value.validate("sync")?;
                context = Some(value);
            }
            "result" => {
                result = Some(parse_result(line, exit)?);
            }
            _ => {
                if !context
                    .as_ref()
                    .is_some_and(|ctx| same_context(expected, ctx))
                {
                    return Err("writer context changed before events".into());
                }
                let record: SyncRecord =
                    serde_json::from_slice(line).map_err(|_| "invalid writer event")?;
                let current = stages.observe(&record)?;
                events.push(WriterEvent {
                    phase: current,
                    record,
                });
            }
        }
    }
    let result = result.ok_or("missing final result")?;
    let bound = context
        .as_ref()
        .is_some_and(|ctx| same_context(expected, ctx));
    if stages.hooks.reached && !stages.hooks.received && matches!(exit, 0 | 3) {
        return Err("missing deferred hook receipt".into());
    }
    if exit == 0
        && (!bound
            || !stages.preflight
            || !stages.actual_capture
            || !stages.hooks.reached
            || !stages.hooks.received)
    {
        return Err("incomplete writer receipt".into());
    }
    if matches!(exit, 0 | 3) && bound {
        if result.recovery.is_some() {
            return Err("recovery on writer receipt".into());
        }
        return Ok(WriterOutcome::Receipt(Box::new(WriterReceipt {
            context: context.ok_or("missing context")?,
            events,
            exit_code: exit,
            error_kind: result.error_kind,
        })));
    }
    if exit == 0 {
        return Err("unbound writer success".into());
    }
    Ok(WriterOutcome::Error {
        exit_code: exit,
        context: context.map(Box::new),
        events,
        error_kind: result.error_kind,
        recovery: result.recovery,
    })
}

#[derive(Default)]
struct ApplyStages {
    apply: bool,
    hooks: HookStages,
    attention: bool,
}

impl ApplyStages {
    fn observe(&mut self, record: &SyncRecord) -> Result<(), String> {
        validate_event(record, WriterPhase::Actual)?;
        match record {
            SyncRecord::Stage {
                stage,
                dry_run: false,
            } if stage == "apply" && !self.apply && !self.hooks.reached => self.apply = true,
            SyncRecord::Stage {
                stage,
                dry_run: false,
            } if stage == "hooks" && self.apply && !self.hooks.reached => self.hooks.reached = true,
            SyncRecord::Undecided {
                reason,
                key,
                scope,
                pass,
                attention: needs_attention,
                skip_next_pass,
            } if !self.hooks.reached
                && pass == "apply"
                && (self.apply
                    || (reason == "review_stale"
                        && scope == "home"
                        && *needs_attention
                        && !skip_next_pass))
                && key
                    .as_ref()
                    .is_none_or(|key| valid_apply_key(&key.raw, scope)) =>
            {
                self.attention |= needs_attention;
            }
            SyncRecord::Resolved { scope, effect, .. }
                if self.apply
                    && !self.hooks.reached
                    && scope == "home"
                    && matches!(effect.as_str(), "written" | "kept") => {}
            SyncRecord::Refusal { kind, .. }
                if self.apply && !self.hooks.reached && kind == "render" => {}
            SyncRecord::Hook { outcome, .. }
                if self.hooks.reached && !self.hooks.received && outcome == "disabled" =>
            {
                self.hooks.received = true;
            }
            _ => return Err("unexpected apply event".into()),
        }
        Ok(())
    }
}

/// Parses a bounded `apply --json --decide-from - --no-hooks` receipt.
///
/// # Errors
/// Rejects incomplete, unbound, malformed or contradictory responses.
pub fn parse_apply_outcome(
    stdout: &[u8],
    exit: Option<i32>,
    expected: &StatusContext,
) -> Result<WriterOutcome, String> {
    expected.validate("status")?;
    if expected.repo_id.is_none() || stdout.len() > MAX_REPORT || !stdout.ends_with(b"\n") {
        return Err("invalid writer framing or expected context".into());
    }
    let exit = exit.ok_or("abnormal writer exit")?;
    let mut context = None;
    let mut events = Vec::new();
    let mut stages = ApplyStages::default();
    let mut result = None;
    for (index, line) in stdout
        .strip_suffix(b"\n")
        .ok_or("missing newline")?
        .split(|byte| *byte == b'\n')
        .enumerate()
    {
        if index >= MAX_RECORDS
            || line.is_empty()
            || line.len() > MAX_RECORD
            || line.last() == Some(&b'\r')
            || result.is_some()
        {
            return Err("invalid writer framing".into());
        }
        let header: Header = serde_json::from_slice(line).map_err(|_| "invalid writer record")?;
        match header.record.as_str() {
            "context" if index == 0 => {
                let value: StatusContext =
                    serde_json::from_slice(line).map_err(|_| "invalid context")?;
                value.validate("apply")?;
                context = Some(value);
            }
            "result" => result = Some(parse_result(line, exit)?),
            _ => {
                if context
                    .as_ref()
                    .is_none_or(|ctx| !same_context(expected, ctx))
                {
                    return Err("writer context changed before events".into());
                }
                let record: SyncRecord =
                    serde_json::from_slice(line).map_err(|_| "invalid writer event")?;
                stages.observe(&record)?;
                events.push(WriterEvent {
                    phase: WriterPhase::Actual,
                    record,
                });
            }
        }
    }
    finish_apply(
        result.ok_or("missing final result")?,
        context,
        events,
        exit,
        expected,
        (
            stages.apply,
            stages.hooks.reached,
            stages.hooks.received,
            stages.attention,
        ),
    )
}

fn finish_apply(
    result: ResultRecord,
    context: Option<StatusContext>,
    events: Vec<WriterEvent>,
    exit: i32,
    expected: &StatusContext,
    (apply, hooks, disabled, attention): (bool, bool, bool, bool),
) -> Result<WriterOutcome, String> {
    let bound = context
        .as_ref()
        .is_some_and(|ctx| same_context(expected, ctx));
    if exit == 0 && (!bound || !apply || !hooks || !disabled || attention) {
        return Err("incomplete apply receipt".into());
    }
    if matches!(exit, 0 | 3) && bound && apply {
        if result.recovery.is_some() {
            return Err("recovery on writer receipt".into());
        }
        return Ok(WriterOutcome::Receipt(Box::new(WriterReceipt {
            context: context.ok_or("missing context")?,
            events,
            exit_code: exit,
            error_kind: result.error_kind,
        })));
    }
    if exit == 0 {
        return Err("unbound writer success".into());
    }
    Ok(WriterOutcome::Error {
        exit_code: exit,
        context: context.map(Box::new),
        events,
        error_kind: result.error_kind,
        recovery: result.recovery,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{StatusReply, parse_status};

    const CONTEXT: &str = r#"{"record":"context","schema_version":1,"experimental":true,"command":"status","home":{"raw":"/home","shown":"/home"},"repo":{"raw":"/repo","shown":"/repo"},"config":{"raw":"/config","shown":"/config"},"repo_config":{"raw":"/repo/.dotman.toml","shown":"/repo/.dotman.toml"},"state":{"raw":"/state","shown":"/state"},"identity":{"raw":"/identity","shown":"/identity"},"age_key_file":null,"repo_id":{"raw":"id","shown":"id"},"machine":{"host":{"raw":"h","shown":"h"},"user":{"raw":"u","shown":"u"},"os":{"raw":"linux","shown":"linux"},"distro":{"raw":"","shown":""},"like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}}"#;
    const RESULT: &str = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;
    const PREFLIGHT: &str = r#"{"record":"stage","stage":"capture","dry_run":true}"#;
    const CAPTURE: &str = r#"{"record":"capture","key":{"raw":"x","shown":"x"},"source":{"raw":"x","shown":"x"},"scope":"home","raw_h":{"state":"missing"},"portable_h":{"state":"missing"},"stored_before":{"state":"missing"},"stored_after":{"state":"missing"},"logical_r":{"state":"missing"},"kept_mode":null,"repo_mode":null,"hypothetical":true,"encrypted":false}"#;
    const PROJECTED: &str = r#"{"record":"resolved","key":{"raw":"x","shown":"x"},"scope":"home","effect":"captured","hypothetical":true}"#;
    const ACTUAL: &str = r#"{"record":"stage","stage":"capture","dry_run":false}"#;
    const RESOLVED: &str = r#"{"record":"resolved","key":{"raw":"x","shown":"x"},"scope":"home","effect":"captured","hypothetical":false}"#;
    const HOOKS: &str = "{\"record\":\"stage\",\"stage\":\"hooks\",\"dry_run\":false}\n{\"record\":\"hook\",\"command\":null,\"outcome\":\"disabled\",\"exit_code\":null}";
    const DEFERRED: &str = "{\"record\":\"stage\",\"stage\":\"hooks\",\"dry_run\":false}\n{\"record\":\"hook\",\"command\":null,\"outcome\":\"deferred\",\"exit_code\":null}";
    const STALE: &str = r#"{"record":"undecided","reason":"review_stale","key":{"raw":"x","shown":"x"},"scope":"home","pass":"sync","attention":true,"skip_next_pass":false}"#;

    fn expected() -> Result<StatusContext, String> {
        let status = format!(
            "{CONTEXT}\n{{\"record\":\"review_context\",\"hooks\":{{\"standing\":\"empty\"}},\"remote\":{{\"basis\":\"unknown\",\"ahead\":null,\"behind\":null}}}}\n{{\"record\":\"summary\",\"rows\":0,\"clean\":0,\"new\":0,\"held\":0,\"held_in_home\":0,\"held_in_repo\":0,\"unrecorded\":0}}\n{RESULT}\n"
        );
        match parse_status(status.as_bytes(), Some(0))? {
            StatusReply::Report(report) => Ok(report.context),
            StatusReply::Error { .. } => Err("missing status".into()),
        }
    }

    fn input(records: &str, result: &str) -> String {
        format!(
            "{}\n{records}\n{result}\n",
            CONTEXT.replace("\"command\":\"status\"", "\"command\":\"sync\"")
        )
    }

    #[test]
    fn selected_requires_exact_actual_stages_and_bound_result() -> Result<(), String> {
        let expected = expected()?;
        let commit = r#"{"record":"stage","stage":"commit","dry_run":false}"#;
        let push = r#"{"record":"stage","stage":"push","dry_run":false}"#;
        let local = input(&format!("{ACTUAL}\n{RESOLVED}\n{commit}"), RESULT);
        let remote = input(&format!("{ACTUAL}\n{commit}\n{push}"), RESULT);
        assert!(matches!(
            parse_selected_outcome(local.as_bytes(), Some(0), &expected, SelectedTarget::Local)?,
            WriterOutcome::Receipt(_)
        ));
        assert!(matches!(
            parse_selected_outcome(remote.as_bytes(), Some(0), &expected, SelectedTarget::Push)?,
            WriterOutcome::Receipt(_)
        ));
        assert!(
            parse_selected_outcome(local.as_bytes(), Some(0), &expected, SelectedTarget::Push)
                .is_err()
        );
        assert!(
            parse_selected_outcome(remote.as_bytes(), Some(0), &expected, SelectedTarget::Local)
                .is_err()
        );
        for records in [
            ACTUAL.to_owned(),
            format!("{commit}\n{ACTUAL}"),
            format!("{ACTUAL}\n{ACTUAL}\n{commit}"),
            format!("{PREFLIGHT}\n{ACTUAL}\n{commit}"),
            format!("{ACTUAL}\n{commit}\n{HOOKS}"),
            format!(
                "{ACTUAL}\n{commit}\n{{\"record\":\"stage\",\"stage\":\"pull\",\"dry_run\":false}}"
            ),
        ] {
            assert!(
                parse_selected_outcome(
                    input(&records, RESULT).as_bytes(),
                    Some(0),
                    &expected,
                    SelectedTarget::Local
                )
                .is_err(),
                "{records}"
            );
        }
        assert!(
            parse_selected_outcome(
                local
                    .replace(r#""raw":"/repo""#, r#""raw":"/other""#)
                    .as_bytes(),
                Some(0),
                &expected,
                SelectedTarget::Local
            )
            .is_err()
        );
        assert!(
            parse_selected_outcome(local.as_bytes(), Some(3), &expected, SelectedTarget::Local)
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn selected_errors_never_become_success() -> Result<(), String> {
        let expected = expected()?;
        let error = r#"{"record":"result","outcome":"error","exit_code":3,"error_kind":"push"}"#;
        let early = format!("{error}\n");
        assert!(matches!(
            parse_selected_outcome(early.as_bytes(), Some(3), &expected, SelectedTarget::Push)?,
            WriterOutcome::Error { context: None, .. }
        ));
        let commit = r#"{"record":"stage","stage":"commit","dry_run":false}"#;
        let push = r#"{"record":"stage","stage":"push","dry_run":false}"#;
        for records in [
            ACTUAL.to_owned(),
            format!("{ACTUAL}\n{commit}"),
            format!("{ACTUAL}\n{commit}\n{push}"),
        ] {
            let result = input(&records, error);
            assert!(matches!(
                parse_selected_outcome(
                    result.as_bytes(),
                    Some(3),
                    &expected,
                    SelectedTarget::Push
                )?,
                WriterOutcome::Error { .. }
            ));
        }
        let refusal = r#"{"record":"refusal","kind":"push","source":null,"key":null}"#;
        assert!(
            parse_selected_outcome(
                input(&format!("{ACTUAL}\n{commit}\n{push}\n{refusal}"), RESULT).as_bytes(),
                Some(0),
                &expected,
                SelectedTarget::Push
            )
            .is_err()
        );
        Ok(())
    }

    #[test]
    fn progress_split_records_and_final_receipt() -> Result<(), String> {
        let expected = expected()?;
        let text = input(
            &format!("{PREFLIGHT}\n{CAPTURE}\n{ACTUAL}\n{RESOLVED}\n{DEFERRED}"),
            RESULT,
        );
        let mut progress = WriterProgress::new(&expected)?;
        let mut stages = Vec::new();
        for end in 0..=text.len() {
            progress.observe(text.as_bytes().get(..end).ok_or("slice")?, |stage| {
                stages.push(stage);
            });
        }
        assert_eq!(stages, [ProgressStage::Capture, ProgressStage::Hooks]);
        assert!(matches!(
            parse_writer_outcome(text.as_bytes(), Some(0), &expected)?,
            WriterOutcome::Receipt(_)
        ));
        assert!(parse_writer_outcome(text.as_bytes(), Some(3), &expected).is_err());
        Ok(())
    }

    #[test]
    fn progress_stops_on_bad_records_and_result() -> Result<(), String> {
        let expected = expected()?;
        let context = CONTEXT.replace("\"command\":\"status\"", "\"command\":\"sync\"");
        for prefix in [
            context.replace("\"schema_version\":1", "\"schema_version\":2"),
            context.replace("\"raw\":\"/repo\"", "\"raw\":\"/other\""),
            format!("{context}\n{context}"),
            format!("{context}\n{{bad}}"),
            format!("{context}\n{{\"record\":\"stage\",\"stage\":\"unknown\",\"dry_run\":false}}"),
            format!("{context}\r"),
            format!("{context}\n{RESULT}"),
        ] {
            let text = format!("{prefix}\n{ACTUAL}\n");
            let mut progress = WriterProgress::new(&expected)?;
            let mut stages = Vec::new();
            progress.observe(text.as_bytes(), |stage| stages.push(stage));
            assert!(stages.is_empty(), "{prefix}");
            assert!(progress.stopped);
        }
        Ok(())
    }

    #[test]
    fn progress_bounds_partial_records_and_events() -> Result<(), String> {
        let expected = expected()?;
        let context = CONTEXT.replace("\"command\":\"status\"", "\"command\":\"sync\"");
        let mut text = format!("{context}\n{PREFLIGHT}\n").into_bytes();
        let mut progress = WriterProgress::new(&expected)?;
        progress.observe(&text, |_| unreachable!());
        text.extend(vec![b'x'; MAX_RECORD]);
        progress.observe(&text, |_| unreachable!());
        assert!(!progress.stopped);
        assert_eq!(progress.scanned, text.len());
        text.push(b'x');
        progress.observe(&text, |_| unreachable!());
        assert!(progress.stopped);

        let text = format!("{context}\n{}", format!("{ACTUAL}\n").repeat(100));
        let mut progress = WriterProgress::new(&expected)?;
        let mut stages = Vec::new();
        progress.observe(text.as_bytes(), |stage| stages.push(stage));
        assert_eq!(stages.len(), 64);
        assert!(progress.stopped);
        Ok(())
    }

    #[test]
    fn separates_projection_from_actual_occurrences() -> Result<(), String> {
        let expected = expected()?;
        let input = input(
            &format!("{PREFLIGHT}\n{CAPTURE}\n{PROJECTED}\n{ACTUAL}\n{RESOLVED}\n{DEFERRED}"),
            RESULT,
        );
        let WriterOutcome::Receipt(receipt) =
            parse_writer_outcome(input.as_bytes(), Some(0), &expected)?
        else {
            return Err("missing receipt".into());
        };
        assert_eq!(receipt.events.len(), 7);
        assert_eq!(
            receipt.events.get(2).map(|event| event.phase),
            Some(WriterPhase::Preflight)
        );
        assert_eq!(
            receipt.events.get(4).map(|event| event.phase),
            Some(WriterPhase::Actual)
        );
        assert!(matches!(
            receipt.events.get(4).map(|event| &event.record),
            Some(SyncRecord::Resolved {
                hypothetical: false,
                ..
            })
        ));
        Ok(())
    }

    #[test]
    fn retains_stale_occurrences_in_both_phases() -> Result<(), String> {
        let expected = expected()?;
        let result = r#"{"record":"result","outcome":"error","exit_code":3,"error_kind":null}"#;
        let input = input(
            &format!("{PREFLIGHT}\n{STALE}\n{STALE}\n{ACTUAL}\n{STALE}\n{DEFERRED}"),
            result,
        );
        let WriterOutcome::Receipt(receipt) =
            parse_writer_outcome(input.as_bytes(), Some(3), &expected)?
        else {
            return Err("missing receipt".into());
        };
        assert_eq!(receipt.exit_code, 3);
        let phases: Vec<_> = receipt
            .events
            .iter()
            .filter(|event| matches!(event.record, SyncRecord::Undecided { .. }))
            .map(|event| event.phase)
            .collect();
        assert_eq!(
            phases,
            [
                WriterPhase::Preflight,
                WriterPhase::Preflight,
                WriterPhase::Actual
            ]
        );
        Ok(())
    }

    #[test]
    fn terminal_recovery_and_early_refusal() -> Result<(), String> {
        let expected = expected()?;
        let result = r#"{"record":"result","outcome":"error","exit_code":4,"error_kind":"pull_conflict","recovery":[{"kind":"command","argv":["git","status"],"shown":["git","status"],"when":null}]}"#;
        let receipt = input(&format!("{PREFLIGHT}\n{ACTUAL}"), result);
        assert!(
            matches!(parse_writer_outcome(receipt.as_bytes(), Some(4), &expected)?, WriterOutcome::Error { error_kind: Some(kind), recovery: Some(steps), events, .. } if kind == "pull_conflict" && steps.len() == 1 && events.len() == 2)
        );
        let early =
            b"{\"record\":\"result\",\"outcome\":\"error\",\"exit_code\":3,\"error_kind\":null}\n";
        assert!(matches!(
            parse_writer_outcome(early, Some(3), &expected)?,
            WriterOutcome::Error { context: None, .. }
        ));
        let null_id = input("", result)
            .replace(
                "\"repo_id\":{\"raw\":\"id\",\"shown\":\"id\"}",
                "\"repo_id\":null",
            )
            .replace("\n\n", "\n");
        assert!(matches!(
            parse_writer_outcome(null_id.as_bytes(), Some(4), &expected)?,
            WriterOutcome::Error { context: Some(ctx), events, .. } if ctx.repo_id.is_none() && events.is_empty()
        ));
        Ok(())
    }

    #[test]
    fn rejects_incomplete_unbound_or_disallowed_output() -> Result<(), String> {
        let expected = expected()?;
        let good = input(
            &format!("{PREFLIGHT}\n{CAPTURE}\n{PROJECTED}\n{ACTUAL}\n{RESOLVED}\n{DEFERRED}"),
            RESULT,
        );
        for bad in [
            good.trim_end().to_owned(),
            good.replace(RESULT, ""),
            good.replace(RESULT, &format!("{RESULT}\n{RESULT}")),
            good.replace("\"schema_version\":1", "\"schema_version\":2"),
            good.replace("\"raw\":\"/repo\"", "\"raw\":\"/other\""),
            good.replace(ACTUAL, PREFLIGHT),
            good.replace(RESOLVED, PROJECTED),
            good.replace(RESOLVED, CAPTURE),
            good.replace(PREFLIGHT, &format!("{PREFLIGHT}\n{RESOLVED}")),
            good.replace(PREFLIGHT, "{\"record\":\"outgoing\",\"head\":null,\"tracking_ref\":null,\"tracking_commit\":null,\"knowledge\":\"last_fetched\",\"warnings\":[]}"),
            good.replace("\"outcome\":\"deferred\"", "\"outcome\":\"started\""),
            good.replace("\"outcome\":\"deferred\"", "\"outcome\":\"disabled\""),
            good.replace(DEFERRED, &format!("{DEFERRED}\n{DEFERRED}")),
            good.replace('\n', "\r\n"),
            good.replace("\"schema_version\":1", "\"schema_version\":1,\"schema_version\":1"),
            good.replace("\"record\":\"result\"", "\"record\":\"result\",\"record\":\"result\""),
            good.replace("\"scope\":\"home\"", "\"scope\":\"elsewhere\""),
            input(PREFLIGHT, RESULT),
            input(&format!("{PREFLIGHT}\n{HOOKS}\n{ACTUAL}"), RESULT),
        ] {
            assert!(parse_writer_outcome(bad.as_bytes(), Some(0), &expected).is_err());
        }
        assert!(parse_writer_outcome(good.as_bytes(), None, &expected).is_err());
        assert!(parse_writer_outcome(good.as_bytes(), Some(3), &expected).is_err());
        assert!(parse_writer_outcome(&vec![b'x'; MAX_REPORT + 1], Some(0), &expected).is_err());
        let bad_recovery = b"{\"record\":\"result\",\"outcome\":\"error\",\"exit_code\":4,\"error_kind\":\"in_progress\",\"recovery\":[{\"kind\":\"command\",\"argv\":[],\"shown\":[],\"when\":null}]}\n";
        assert!(parse_writer_outcome(bad_recovery, Some(4), &expected).is_err());
        Ok(())
    }

    fn apply_input(records: &str, result: &str) -> String {
        let context = CONTEXT.replace("\"command\":\"status\"", "\"command\":\"apply\"");
        if records.is_empty() {
            format!("{context}\n{result}\n")
        } else {
            format!("{context}\n{records}\n{result}\n")
        }
    }

    #[test]
    fn apply_success_and_stale_independent_write() -> Result<(), String> {
        let expected = expected()?;
        let apply = r#"{"record":"stage","stage":"apply","dry_run":false}"#;
        let written = RESOLVED.replace("captured", "written");
        let kept = RESOLVED.replace("captured", "kept");
        let stale = STALE.replace("\"pass\":\"sync\"", "\"pass\":\"apply\"");
        let success = apply_input(&format!("{apply}\n{written}\n{kept}\n{HOOKS}"), RESULT);
        let WriterOutcome::Receipt(receipt) =
            parse_apply_outcome(success.as_bytes(), Some(0), &expected)?
        else {
            return Err("missing apply receipt".into());
        };
        assert_eq!(receipt.events.len(), 5);
        let attention = r#"{"record":"result","outcome":"error","exit_code":3,"error_kind":null}"#;
        let mixed = apply_input(&format!("{stale}\n{apply}\n{written}\n{HOOKS}"), attention);
        let WriterOutcome::Receipt(receipt) =
            parse_apply_outcome(mixed.as_bytes(), Some(3), &expected)?
        else {
            return Err("missing attention receipt".into());
        };
        assert_eq!(receipt.events.len(), 5);
        assert!(
            matches!(receipt.events.first().map(|event| &event.record), Some(SyncRecord::Undecided { reason, attention: true, .. }) if reason == "review_stale")
        );
        assert!(
            matches!(receipt.events.get(2).map(|event| &event.record), Some(SyncRecord::Resolved { effect, hypothetical: false, .. }) if effect == "written")
        );
        Ok(())
    }

    #[test]
    fn apply_terminal_errors_and_invalid_receipts() -> Result<(), String> {
        let expected = expected()?;
        let apply = r#"{"record":"stage","stage":"apply","dry_run":false}"#;
        let failure = r#"{"record":"result","outcome":"error","exit_code":1,"error_kind":"other"}"#;
        let recovery = r#"{"record":"result","outcome":"error","exit_code":4,"error_kind":"in_progress","recovery":[{"kind":"command","argv":["git","status"],"shown":["git","status"],"when":null}]}"#;
        let early = format!("{failure}\n");
        assert!(matches!(
            parse_apply_outcome(early.as_bytes(), Some(1), &expected)?,
            WriterOutcome::Error { context: None, .. }
        ));
        let terminal = apply_input("", recovery).replace(
            "\"repo_id\":{\"raw\":\"id\",\"shown\":\"id\"}",
            "\"repo_id\":null",
        );
        assert!(
            matches!(parse_apply_outcome(terminal.as_bytes(), Some(4), &expected)?, WriterOutcome::Error { context: Some(ctx), recovery: Some(steps), events, .. } if ctx.repo_id.is_none() && steps.len() == 1 && events.is_empty())
        );
        let good = apply_input(&format!("{apply}\n{HOOKS}"), RESULT);
        let system_write = r#"{"record":"resolved","key":{"raw":"/etc/example","shown":"/etc/example"},"scope":"system","effect":"written","hypothetical":false}"#;
        let forgotten = RESOLVED.replace("captured", "forgotten");
        for bad in [
            good.replace("\"command\":\"apply\"", "\"command\":\"sync\""),
            good.replace("\"raw\":\"/repo\"", "\"raw\":\"/other\""),
            good.replace(apply, ACTUAL),
            good.replace(apply, PREFLIGHT),
            good.replace(apply, &format!("{apply}\n{PROJECTED}")),
            good.replace(apply, &format!("{apply}\n{CAPTURE}")),
            good.replace(apply, &format!("{apply}\n{system_write}")),
            good.replace(apply, &format!("{apply}\n{forgotten}")),
            good.replace("\"outcome\":\"disabled\"", "\"outcome\":\"started\""),
            good.replace(HOOKS, ""),
            good.replace(RESULT, &format!("{RESULT}\n{RESULT}")),
            good.trim_end().to_owned(),
        ] {
            assert!(
                parse_apply_outcome(bad.as_bytes(), Some(0), &expected).is_err(),
                "{bad}"
            );
        }
        assert!(parse_apply_outcome(good.as_bytes(), None, &expected).is_err());
        assert!(parse_apply_outcome(good.as_bytes(), Some(3), &expected).is_err());
        Ok(())
    }
}
