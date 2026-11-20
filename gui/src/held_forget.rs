use crate::protocol::{
    FindingPlace, StatusContext, StatusEntry, StatusObservation, literal_relative,
};
use serde::{Deserialize, Serialize};
use std::path::Path;

const LIMIT: usize = 65_536;
const INVALID: &str = "Forget receipt could not be verified";

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Finding {
    rule: String,
    place: String,
    line: usize,
}

#[derive(Clone, Debug, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Occurrence {
    scope: String,
    source: String,
    reason: String,
    finding: Finding,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Source {
    path: String,
    kind: String,
    hash: String,
    mode: u32,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct InnerReview {
    key: String,
    repo_id: String,
    sources: Vec<Source>,
    home: Option<Source>,
    holds: Vec<(String, String)>,
    context: String,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    key: String,
    review: InnerReview,
    occurrence: Occurrence,
    source_oids: Vec<String>,
    home_oid: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListedSource {
    source: String,
    kind: String,
    mode: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReview {
    schema_version: u32,
    record: String,
    operation: String,
    executable: bool,
    occurrence: Occurrence,
    source_count: usize,
    sources: Vec<ListedSource>,
    backup: String,
    home_preserved: bool,
    metadata: Vec<String>,
    snapshot: Option<Snapshot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalResult {
    record: String,
    outcome: String,
    exit_code: i32,
    error_kind: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Recovery {
    state_backup_run: String,
    repo_git_quarantines: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Success {
    schema_version: u32,
    record: String,
    operation: String,
    result: String,
    private_recovery: Recovery,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Partial {
    schema_version: u32,
    record: String,
    code: String,
    message: String,
    private_recovery: Recovery,
}

/// A native-selected managed hold, bound to its status context.
pub struct ForgetTarget {
    key: String,
    repo_id: String,
    occurrence: Occurrence,
    raw_home: Option<crate::protocol::StatusTuple>,
    expected_sources: Option<Vec<String>>,
    source_tuple: Option<crate::protocol::StatusTuple>,
}

/// A CLI snapshot retained only by native code.
pub struct ForgetReview {
    target: ForgetTarget,
    snapshot: Snapshot,
}

/// A source displayed in the native confirmation.
#[derive(Serialize)]
pub struct SourceView {
    pub source: String,
    pub mode: String,
}

/// Consequences for the native confirmation, without the executable snapshot.
#[derive(Serialize)]
pub struct Confirmation {
    pub question: String,
    pub key: String,
    pub occurrence_scope: String,
    pub occurrence_source: String,
    pub finding_rule: String,
    pub finding_place: &'static str,
    pub finding_line: usize,
    pub sources: Vec<SourceView>,
    pub consequences: Vec<&'static str>,
}

/// A validated successful or partial execution receipt.
#[derive(Debug)]
pub enum Execution {
    Forgotten {
        backup_run: String,
    },
    Partial {
        backup_run: String,
        quarantines: Vec<String>,
    },
}

fn valid_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && literal_relative(Path::new(path))
        && !path.contains("<redacted>")
        && !path.contains("[redacted]")
        && !path.chars().any(crate::runner::unsafe_display_char)
}

fn targets_key(source: &str, key: &str) -> bool {
    let (directory, name) = source
        .rsplit_once('/')
        .map_or(("", source), |(d, n)| (d, n));
    let mut target = None;
    for (start, _) in name.match_indices('{') {
        let Some(rest) = name.get(start.saturating_add(1)..) else {
            return false;
        };
        let Some(end) = rest.find('}') else { break };
        let Some(block) = rest.get(..end) else {
            return false;
        };
        let mut flags = 0_u8;
        let mut recognized = true;
        for term in block.split('+') {
            if matches!(term, "tmpl" | "age") {
                flags = flags.saturating_add(1);
                continue;
            }
            let Some((field, value)) = term.split_once('=') else {
                recognized = false;
                break;
            };
            let field = field.strip_suffix('!').unwrap_or(field);
            if !matches!(
                field,
                "host" | "user" | "profile" | "distro" | "like" | "os" | "arch"
            ) || value.is_empty()
                || !value
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
            {
                recognized = false;
                break;
            }
        }
        if recognized {
            if flags > 1 || target.is_some() {
                return false;
            }
            let Some(before) = name.get(..start) else {
                return false;
            };
            let Some(after) = rest.get(end.saturating_add(1)..) else {
                return false;
            };
            target = Some(format!("{before}{after}"));
        }
    }
    let name = target.as_deref().unwrap_or(name);
    if directory.is_empty() {
        name == key
    } else {
        format!("{directory}/{name}") == key
    }
}

fn hex(value: &str, lengths: &[usize]) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn valid_source(source: &Source) -> bool {
    valid_path(&source.path)
        && source.kind == "file"
        && hex(&source.hash, &[32])
        && source.mode <= 0o7777
}

fn records(bytes: &[u8], status: Option<i32>) -> Result<(&[u8], FinalResult), &'static str> {
    let code = status.ok_or(INVALID)?;
    if bytes.len() > LIMIT {
        return Err(INVALID);
    }
    let mut lines = bytes.split(|b| *b == b'\n');
    let first = lines.next().ok_or(INVALID)?;
    let last = lines.next().ok_or(INVALID)?;
    if lines.next() != Some(&[][..]) || lines.next().is_some() {
        return Err(INVALID);
    }
    let final_result: FinalResult = serde_json::from_slice(last).map_err(|_| INVALID)?;
    if final_result.record != "result"
        || final_result.exit_code != code
        || final_result.outcome != if code == 0 { "done" } else { "error" }
        || (code == 0 && final_result.error_kind.is_some())
    {
        return Err(INVALID);
    }
    Ok((first, final_result))
}

/// Checks a CLI-generated private recovery handle before joining it to a trusted root.
#[must_use]
pub fn safe_handle(value: &str, prefix: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value != "."
        && value != ".."
        && value.starts_with(prefix)
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
}

fn status_finding(entry: &StatusEntry) -> Result<Finding, &'static str> {
    let found = entry.finding.as_ref().ok_or(INVALID)?;
    if !matches!(found.place, FindingPlace::Content)
        || found.line == 0
        || found.rule.is_empty()
        || found.rule.len() > 128
        || !found
            .rule
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
    {
        return Err(INVALID);
    }
    Ok(Finding {
        rule: found.rule.clone(),
        place: "content".to_owned(),
        line: found.line,
    })
}

impl ForgetTarget {
    /// Binds one native-selected logical key to its exact retained status occurrence.
    ///
    /// # Errors
    /// Rejects redacted, unsupported, ambiguous, or non-content holds.
    pub fn from_status(
        context: &StatusContext,
        key: &str,
        entry: &StatusEntry,
    ) -> Result<Self, &'static str> {
        context.validate("status").map_err(|_| INVALID)?;
        let repo_id = &context.repo_id.as_ref().ok_or(INVALID)?.raw;
        if repo_id.is_empty()
            || repo_id.len() > 256
            || !repo_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            || !valid_path(key)
            || entry.system
        {
            return Err(INVALID);
        }
        let found = status_finding(entry)?;
        let (scope, source, raw_home, expected_sources, source_tuple) =
            match (entry.scope.as_str(), entry.status.as_str()) {
                ("home", "held-in-home") if entry.key.raw == key => {
                    let source = &entry.source.as_ref().ok_or(INVALID)?.raw;
                    let StatusObservation::Present { tuple } = &entry.raw_h else {
                        return Err(INVALID);
                    };
                    entry.raw_h.validate().map_err(|_| INVALID)?;
                    if tuple.kind != "file" {
                        return Err(INVALID);
                    }
                    if entry.encrypted || entry.template {
                        return Err(INVALID);
                    }
                    let StatusObservation::Present {
                        tuple: source_tuple,
                    } = &entry.source_tuple
                    else {
                        return Err(INVALID);
                    };
                    entry.source_tuple.validate().map_err(|_| INVALID)?;
                    if source_tuple.kind != "file" {
                        return Err(INVALID);
                    }
                    let mut names: Vec<String> = entry
                        .alternatives
                        .iter()
                        .map(|item| item.raw.clone())
                        .collect();
                    if names.is_empty()
                        || names.len() > 1024
                        || names.iter().any(|name| !valid_path(name))
                    {
                        return Err(INVALID);
                    }
                    names.sort();
                    if names.windows(2).any(|pair| pair.first() == pair.get(1))
                        || !names.contains(source)
                    {
                        return Err(INVALID);
                    }
                    (
                        "home",
                        source.clone(),
                        Some(tuple.clone()),
                        Some(names),
                        Some(source_tuple.clone()),
                    )
                }
                ("repo_source", "held-in-repo") => {
                    let source = entry
                        .path
                        .as_ref()
                        .ok_or(INVALID)?
                        .path
                        .to_str()
                        .ok_or(INVALID)?;
                    if entry.key.raw != source {
                        return Err(INVALID);
                    }
                    ("repo", source.to_owned(), None, None, None)
                }
                _ => return Err(INVALID),
            };
        if !valid_path(&source) {
            return Err(INVALID);
        }
        Ok(Self {
            key: key.to_owned(),
            repo_id: repo_id.clone(),
            occurrence: Occurrence {
                scope: scope.to_owned(),
                source,
                reason: "content".to_owned(),
                finding: found,
            },
            raw_home,
            expected_sources,
            source_tuple,
        })
    }

    /// Encodes the fixed read-only review request.
    ///
    /// # Errors
    /// Fails if the bounded request cannot be encoded.
    pub fn review_request(&self) -> Result<Vec<u8>, &'static str> {
        encode(&serde_json::json!({
            "schema_version": 1, "operation": "review_forget",
            "key": self.key, "occurrence": self.occurrence
        }))
    }

    /// Verifies CLI stdout and retains its snapshot in native memory.
    ///
    /// # Errors
    /// Rejects incomplete, stale, malformed, or non-executable reviews.
    pub fn parse_review(
        self,
        bytes: &[u8],
        status: Option<i32>,
    ) -> Result<ForgetReview, &'static str> {
        let (first, result) = records(bytes, status)?;
        if result.exit_code != 0 {
            return Err(INVALID);
        }
        let wire: WireReview = serde_json::from_slice(first).map_err(|_| INVALID)?;
        let snapshot = wire.snapshot.ok_or(INVALID)?;
        if wire.schema_version != 1
            || wire.record != "remedy_review"
            || wire.operation != "review_forget"
            || !wire.executable
            || wire.occurrence != self.occurrence
            || snapshot.occurrence != self.occurrence
            || snapshot.key != self.key
            || snapshot.review.key != self.key
            || snapshot.review.repo_id != self.repo_id
            || wire.backup != "private_copy_of_each_source"
            || !wire.home_preserved
            || wire.metadata != ["source_modes", "recorded_file", "capture_ignore_if_needed"]
            || wire.source_count == 0
            || wire.source_count > 1024
            || wire.source_count != wire.sources.len()
            || wire.source_count != snapshot.review.sources.len()
            || wire.source_count != snapshot.source_oids.len()
            || !hex(&snapshot.review.context, &[32])
        {
            return Err(INVALID);
        }
        let mut prior: Option<&str> = None;
        for ((listed, source), oid) in wire
            .sources
            .iter()
            .zip(&snapshot.review.sources)
            .zip(&snapshot.source_oids)
        {
            let valid_binding =
                valid_source(source) && targets_key(&source.path, &self.key) && hex(oid, &[40, 64]);
            let reviewed_path = source.path.as_str();
            let displayed_path = listed.source.as_str();
            let same_path = displayed_path == reviewed_path;
            let same_kind = listed.kind == source.kind;
            let same_mode = listed.mode == format!("{:04o}", source.mode);
            let valid_listing = same_path && same_kind && same_mode;
            let sorted = prior.is_none_or(|previous| previous < source.path.as_str());
            if !valid_binding || !valid_listing || !sorted {
                return Err(INVALID);
            }
            prior = Some(&source.path);
        }
        if self.expected_sources.as_ref().is_some_and(|names| {
            !names.iter().map(String::as_str).eq(snapshot
                .review
                .sources
                .iter()
                .map(|source| source.path.as_str()))
        }) {
            return Err(INVALID);
        }
        if let Some(observed) = &self.source_tuple {
            let source = snapshot
                .review
                .sources
                .iter()
                .find(|source| source.path == self.occurrence.source)
                .ok_or(INVALID)?;
            // Status applies configured modes; the review reports the physical source mode.
            if observed.kind != source.kind || observed.hash != source.hash {
                return Err(INVALID);
            }
        }
        if !snapshot
            .review
            .sources
            .iter()
            .any(|s| s.path == self.occurrence.source)
            || snapshot.review.holds.len() > wire.source_count
            || snapshot.review.holds.iter().any(|(path, finding)| {
                !snapshot.review.sources.iter().any(|s| &s.path == path)
                    || finding.is_empty()
                    || finding.len() > 512
            })
            || snapshot
                .home_oid
                .as_ref()
                .is_some_and(|oid| !hex(oid, &[40, 64]))
        {
            return Err(INVALID);
        }
        if !home_matches(&self, &snapshot) {
            return Err(INVALID);
        }
        Ok(ForgetReview {
            target: self,
            snapshot,
        })
    }
}

fn home_matches(target: &ForgetTarget, snapshot: &Snapshot) -> bool {
    match (&snapshot.review.home, &target.raw_home) {
        (Some(home), observed) => {
            valid_source(home)
                && home.path == target.key
                && observed.as_ref().is_none_or(|tuple| {
                    tuple.kind == home.kind
                        && tuple.hash == home.hash
                        && tuple.mode == format!("{:04o}", home.mode)
                })
                && snapshot.home_oid.is_some()
        }
        (None, None) => snapshot.home_oid.is_none(),
        _ => false,
    }
}

fn encode(value: &serde_json::Value) -> Result<Vec<u8>, &'static str> {
    let bytes = serde_json::to_vec(value).map_err(|_| INVALID)?;
    if bytes.len() > LIMIT {
        Err(INVALID)
    } else {
        Ok(bytes)
    }
}

impl ForgetReview {
    /// Produces bounded display-only confirmation data.
    #[must_use]
    pub fn confirmation(&self) -> Confirmation {
        Confirmation {
            question: format!("Forget all {} sources?", self.snapshot.review.sources.len()),
            key: crate::protocol::escape_diff(self.target.key.as_bytes()),
            occurrence_scope: self.target.occurrence.scope.clone(),
            occurrence_source: crate::protocol::escape_diff(
                self.target.occurrence.source.as_bytes(),
            ),
            finding_rule: self.target.occurrence.finding.rule.clone(),
            finding_place: "content",
            finding_line: self.target.occurrence.finding.line,
            sources: self
                .snapshot
                .review
                .sources
                .iter()
                .map(|source| SourceView {
                    source: crate::protocol::escape_diff(source.path.as_bytes()),
                    mode: format!("{:04o}", source.mode),
                })
                .collect(),
            consequences: vec![
                "Home is preserved.",
                "A private backup copy of each source remains.",
                "Source modes and recorded state are removed; an exact ignore may be added.",
                "The Git index and existing history are not changed; later sync may record removals.",
                "No hooks, commit, or sync are run.",
            ],
        }
    }

    /// Encodes execution using only the retained native snapshot.
    ///
    /// # Errors
    /// Fails if the bounded request cannot be encoded.
    pub fn execute_request(&self) -> Result<Vec<u8>, &'static str> {
        encode(&serde_json::json!({
            "schema_version": 1, "operation": "execute_forget",
            "key": self.target.key, "occurrence": self.target.occurrence, "snapshot": self.snapshot
        }))
    }

    /// Verifies a successful or partial CLI receipt without accepting paths from the renderer.
    ///
    /// # Errors
    /// Refuses malformed, mismatched, or uncertain receipts.
    pub fn parse_execution(
        &self,
        bytes: &[u8],
        status: Option<i32>,
    ) -> Result<Execution, &'static str> {
        let (first, result) = records(bytes, status)?;
        let (recovery, partial) = if result.exit_code == 0 {
            let reply: Success = serde_json::from_slice(first).map_err(|_| INVALID)?;
            if reply.schema_version != 1
                || reply.record != "remedy_result"
                || reply.operation != "execute_forget"
                || reply.result != "forgotten"
            {
                return Err(INVALID);
            }
            (reply.private_recovery, false)
        } else {
            let reply: Partial = serde_json::from_slice(first).map_err(|_| INVALID)?;
            if result.exit_code != 1
                || result.error_kind.as_deref() != Some("other")
                || reply.schema_version != 1
                || reply.record != "remedy_error"
                || reply.code != "partial"
                || reply.message != "forget did not finish; inspect the private recovery locations"
            {
                return Err(INVALID);
            }
            (reply.private_recovery, true)
        };
        if !safe_handle(&recovery.state_backup_run, "")
            || recovery.repo_git_quarantines.len() > self.snapshot.review.sources.len()
            || recovery
                .repo_git_quarantines
                .iter()
                .any(|name| !safe_handle(name, ".dotman-forget-"))
        {
            return Err(INVALID);
        }
        if partial {
            Ok(Execution::Partial {
                backup_run: recovery.state_backup_run,
                quarantines: recovery.repo_git_quarantines,
            })
        } else if recovery.repo_git_quarantines.is_empty() {
            Ok(Execution::Forgotten {
                backup_run: recovery.state_backup_run,
            })
        } else {
            Err(INVALID)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn context() -> StatusContext {
        serde_json::from_value(json!({
            "schema_version":1,"experimental":true,"command":"status",
            "home":{"raw":"/home","shown":"/home"},
            "repo":{"raw":"/repo","shown":"/repo"},
            "config":{"raw":"/config","shown":"/config"},
            "repo_config":{"raw":"/repo/.dotman.toml","shown":"/repo/.dotman.toml"},
            "state":{"raw":"/state","shown":"/state"},
            "identity":{"raw":"/identity","shown":"/identity"},
            "age_key_file":null,"repo_id":{"raw":"test","shown":"test"},
            "machine":{"host":{"raw":"h","shown":"h"},"user":{"raw":"u","shown":"u"},
                "os":{"raw":"linux","shown":"linux"},"distro":{"raw":"d","shown":"d"},
                "like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}
        }))
        .unwrap()
    }

    fn entry() -> StatusEntry {
        serde_json::from_value(json!({
            "status":"held-in-home","key":{"raw":"file","shown":"file"},"scope":"home",
            "path":null,"source":{"raw":"file","shown":"file"},"recorded_source":null,
            "alternatives":[{"raw":"file","shown":"file"},{"raw":"file{host=x}","shown":"file{host=x}"}],
            "invalid_sources":[],"encrypted":false,"template":false,"system":false,
            "row":"clean","row_number":4,"category":null,
            "r":{"state":"missing"},"source_tuple":{"state":"present","tuple":{"kind":"file","hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","mode":"0600"}},
            "h":{"state":"missing"},
            "raw_h":{"state":"present","tuple":{"kind":"file","hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","mode":"0600"}},
            "s":{"state":"missing"},"identity_changed":false,"update_state":false,
            "finding":{"rule":"github-token","place":"content","line":1}
        })).unwrap()
    }

    fn wire() -> Value {
        let occurrence = json!({"scope":"home","source":"file","reason":"content",
            "finding":{"rule":"github-token","place":"content","line":1}});
        let source = json!({"path":"file","kind":"file","hash":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","mode":384});
        let variant = json!({"path":"file{host=x}","kind":"file","hash":"cccccccccccccccccccccccccccccccc","mode":420});
        json!({"schema_version":1,"record":"remedy_review","operation":"review_forget",
            "executable":true,"occurrence":occurrence,"source_count":2,
            "sources":[{"source":"file","kind":"file","mode":"0600"},
                {"source":"file{host=x}","kind":"file","mode":"0644"}],
            "backup":"private_copy_of_each_source","home_preserved":true,
            "metadata":["source_modes","recorded_file","capture_ignore_if_needed"],
            "snapshot":{"key":"file","occurrence":occurrence,
                "source_oids":["aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"],
                "home_oid":"cccccccccccccccccccccccccccccccccccccccc",
                "review":{"key":"file","repo_id":"test","sources":[source,variant],
                    "home":{"path":"file","kind":"file","hash":"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","mode":384},
                    "holds":[],"context":"dddddddddddddddddddddddddddddddd"}}})
    }

    fn output(value: &Value, exit: i32) -> Vec<u8> {
        format!("{}\n{}\n", value, json!({"record":"result","outcome":if exit == 0 {"done"} else {"error"},"exit_code":exit,"error_kind":if exit == 0 {None} else {Some("other")}})).into_bytes()
    }

    fn target() -> ForgetTarget {
        ForgetTarget::from_status(&context(), "file", &entry()).unwrap()
    }

    #[test]
    fn review_binds_sources_and_keeps_snapshot_native() -> Result<(), Box<dyn std::error::Error>> {
        let review = target().parse_review(&output(&wire(), 0), Some(0)).unwrap();
        let view = serde_json::to_value(review.confirmation()).unwrap();
        assert_eq!(view.get("question"), Some(&json!("Forget all 2 sources?")));
        assert!(!view.to_string().contains("source_oids"));
        let request: Value = serde_json::from_slice(&review.execute_request().unwrap()).unwrap();
        assert_eq!(request.get("operation"), Some(&json!("execute_forget")));
        assert_eq!(
            request.get("snapshot").ok_or("missing snapshot")?,
            wire().get("snapshot").ok_or("missing fixture snapshot")?
        );
        Ok(())
    }

    #[test]
    fn variants_must_resolve_to_the_exact_key() {
        for (source, key, expected) in [
            (".profile", ".profile", true),
            (".profile{host=x}", ".profile", true),
            ("dir/a{os=linux}.conf", "dir/a.conf", true),
            ("dir/a{host=x+tmpl}.conf", "dir/a.conf", true),
            ("dir/a{tmpl+age}.conf", "dir/a.conf", false),
            ("dir/a{host=x}{os=linux}.conf", "dir/a.conf", false),
            ("dir/a.conf/child", "dir/a.conf", false),
            ("other", "file", false),
        ] {
            assert_eq!(targets_key(source, key), expected, "{source}");
        }
    }

    #[test]
    fn rejects_stale_truncated_oversized_and_wrong_occurrences()
    -> Result<(), Box<dyn std::error::Error>> {
        for changed in [
            json!({"source_count":1}),
            json!({"home_preserved":false}),
            json!({"occurrence":{"scope":"repo","source":"file","reason":"content",
                "finding":{"rule":"github-token","place":"content","line":1}}}),
        ] {
            let mut value = wire();
            for (key, replacement) in changed.as_object().ok_or("invalid fixture")? {
                *value.get_mut(key).ok_or("missing fixture field")? = replacement.clone();
            }
            assert!(target().parse_review(&output(&value, 0), Some(0)).is_err());
        }
        assert!(target().parse_review(b"{}\n", Some(0)).is_err());
        assert!(
            target()
                .parse_review(&vec![b'x'; LIMIT + 1], Some(0))
                .is_err()
        );
        assert!(target().parse_review(&output(&wire(), 0), Some(1)).is_err());
        let mut changed = entry();
        changed.alternatives.pop();
        assert!(
            ForgetTarget::from_status(&context(), "file", &changed)
                .unwrap()
                .parse_review(&output(&wire(), 0), Some(0))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn checks_success_and_partial_handles() -> Result<(), Box<dyn std::error::Error>> {
        let review = target().parse_review(&output(&wire(), 0), Some(0)).unwrap();
        let success = json!({"schema_version":1,"record":"remedy_result","operation":"execute_forget",
            "result":"forgotten","private_recovery":{"state_backup_run":"20260101-42","repo_git_quarantines":[]}});
        assert!(matches!(
            review.parse_execution(&output(&success, 0), Some(0)),
            Ok(Execution::Forgotten { .. })
        ));
        let partial = json!({"schema_version":1,"record":"remedy_error","code":"partial",
            "message":"forget did not finish; inspect the private recovery locations",
            "private_recovery":{"state_backup_run":"20260101-42","repo_git_quarantines":[".dotman-forget-abc"]}});
        assert!(matches!(
            review.parse_execution(&output(&partial, 1), Some(1)),
            Ok(Execution::Partial { .. })
        ));
        let mut bad = partial;
        *bad.pointer_mut("/private_recovery/state_backup_run")
            .ok_or("missing recovery run")? = json!("../secret");
        assert!(review.parse_execution(&output(&bad, 1), Some(1)).is_err());
        Ok(())
    }

    #[test]
    fn repo_occurrence_binds_the_physical_source_and_finding()
    -> Result<(), Box<dyn std::error::Error>> {
        let mut repo = entry();
        repo.scope = "repo_source".to_owned();
        repo.status = "held-in-repo".to_owned();
        repo.key.raw = "file{host=x}".to_owned();
        repo.path = Some(crate::protocol::StatusPath {
            path: "file{host=x}".into(),
            shown: "file{host=x}".to_owned(),
        });
        let mut reply = wire();
        *reply
            .pointer_mut("/occurrence/scope")
            .ok_or("missing scope")? = json!("repo");
        *reply
            .pointer_mut("/occurrence/source")
            .ok_or("missing source")? = json!("file{host=x}");
        let occurrence = reply.get("occurrence").ok_or("missing occurrence")?.clone();
        *reply
            .pointer_mut("/snapshot/occurrence")
            .ok_or("missing snapshot occurrence")? = occurrence;
        let target = ForgetTarget::from_status(&context(), "file", &repo).unwrap();
        assert!(target.parse_review(&output(&reply, 0), Some(0)).is_ok());
        let mut unrelated = reply.clone();
        *unrelated
            .pointer_mut("/sources/0/source")
            .ok_or("missing listed source")? = json!("aaaa");
        *unrelated
            .pointer_mut("/snapshot/review/sources/0/path")
            .ok_or("missing snapshot source")? = json!("aaaa");
        assert!(
            ForgetTarget::from_status(&context(), "file", &repo)
                .unwrap()
                .parse_review(&output(&unrelated, 0), Some(0))
                .is_err()
        );
        repo.key.raw = "file".to_owned();
        assert!(ForgetTarget::from_status(&context(), "file", &repo).is_err());
        repo.key.raw = "file{host=x}".to_owned();
        repo.finding.as_mut().ok_or("missing finding")?.line = 2;
        assert!(
            ForgetTarget::from_status(&context(), "file", &repo)
                .unwrap()
                .parse_review(&output(&reply, 0), Some(0))
                .is_err()
        );
        Ok(())
    }

    struct Fixture {
        root: std::path::PathBuf,
        cli: std::ffi::OsString,
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    impl Fixture {
        fn new() -> Result<Self, Box<dyn std::error::Error>> {
            let cli = std::env::var_os("DOTMAN_TEST_CLI").ok_or("missing DOTMAN_TEST_CLI")?;
            let root = std::env::temp_dir()
                .join(format!("dotman-forget-projection-{}", std::process::id()));
            std::fs::create_dir(&root)?;
            let fixture = Self { root, cli };
            for name in ["home", "config", "state", "data"] {
                std::fs::create_dir_all(fixture.root.join(name))?;
            }
            let config = fixture.root.join("config/dotman");
            std::fs::create_dir_all(&config)?;
            std::fs::write(
                config.join("config.toml"),
                "[identity]\nhost = \"fixture\"\nuser = \"fixture\"\nos = \"linux\"\ndistro = \"fixture\"\nlike = \"fixture\"\narch = \"fixture\"\n",
            )?;
            std::fs::write(
                fixture.root.join("home/.gitconfig"),
                "[user]\nname = Fixture\nemail = fixture@example.invalid\n",
            )?;
            Ok(fixture)
        }

        fn run(
            &self,
            args: &[&str],
            input: Option<&[u8]>,
        ) -> Result<std::process::Output, Box<dyn std::error::Error>> {
            use std::io::Write;
            use std::process::{Command, Stdio};
            let mut child = Command::new(&self.cli)
                .args(args)
                .env_clear()
                .current_dir(self.root.join("home"))
                .env("HOME", self.root.join("home"))
                .env("XDG_CONFIG_HOME", self.root.join("config"))
                .env("XDG_STATE_HOME", self.root.join("state"))
                .env("XDG_DATA_HOME", self.root.join("data"))
                .env("XDG_CACHE_HOME", self.root.join("cache"))
                .env("XDG_RUNTIME_DIR", self.root.join("runtime"))
                .env("TMPDIR", &self.root)
                .env("PATH", "/usr/bin:/bin")
                .env("GIT_CONFIG_NOSYSTEM", "1")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            if let Some(input) = input {
                child
                    .stdin
                    .take()
                    .ok_or("missing CLI stdin")?
                    .write_all(input)?;
            }
            Ok(child.wait_with_output()?)
        }
    }

    #[test]
    #[ignore = "requires DOTMAN_TEST_CLI pointing to the built CLI"]
    fn real_cli_round_trip_and_stale_review() -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let fixture = Fixture::new()?;
        let home = fixture.root.join("home");
        let data = fixture.root.join("data");
        let init = fixture.run(&["init"], None)?;
        assert!(
            init.status.success(),
            "{}",
            String::from_utf8_lossy(&init.stderr)
        );
        std::fs::write(home.join(".profile"), b"original\n")?;
        std::fs::set_permissions(
            home.join(".profile"),
            std::fs::Permissions::from_mode(0o600),
        )?;
        let added = fixture.run(&["add", ".profile"], None)?;
        assert!(
            added.status.success(),
            "{}",
            String::from_utf8_lossy(&added.stderr)
        );
        std::fs::write(data.join("dotman/.profile{host=other}"), b"other machine\n")?;
        let held = format!(
            "token={}\n",
            concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
        );
        std::fs::write(home.join(".profile"), held.as_bytes())?;
        let status = fixture.run(&["status", "--json"], None)?;
        let crate::protocol::StatusReply::Report(report) =
            crate::protocol::parse_status(&status.stdout, status.status.code())?
        else {
            return Err("status refused".into());
        };
        let entry = report
            .entries
            .iter()
            .find(|entry| entry.status == "held-in-home" && entry.key.raw == ".profile")
            .ok_or("missing held entry")?;
        let target = ForgetTarget::from_status(&report.context, ".profile", entry)?;
        let raw = fixture.run(
            &["remedy", "--from", "-", "--json"],
            Some(&target.review_request()?),
        )?;
        let review = target.parse_review(&raw.stdout, raw.status.code())?;
        assert_eq!(review.confirmation().question, "Forget all 2 sources?");
        std::fs::write(home.join(".profile"), b"changed\n")?;
        let refused = fixture.run(
            &["remedy", "--from", "-", "--json"],
            Some(&review.execute_request()?),
        )?;
        assert!(!refused.status.success());
        assert!(
            review
                .parse_execution(&refused.stdout, refused.status.code())
                .is_err()
        );
        std::fs::write(home.join(".profile"), held.as_bytes())?;
        let done = fixture.run(
            &["remedy", "--from", "-", "--json"],
            Some(&review.execute_request()?),
        )?;
        assert!(
            done.status.success(),
            "{}",
            String::from_utf8_lossy(&done.stderr)
        );
        assert!(matches!(
            review.parse_execution(&done.stdout, done.status.code()),
            Ok(Execution::Forgotten { .. })
        ));
        assert_eq!(std::fs::read(home.join(".profile"))?, held.as_bytes());
        let root = fixture.root.clone();
        drop(fixture);
        assert!(!root.exists());
        Ok(())
    }
}
