use super::{Bounded, SetupIntent, SetupPublication, input_path, same_paths_and_machine, valid_id};
use crate::protocol::{Header, StatusContext, StatusPath, StatusText};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};

const MAX_REVIEW: usize = 900_000;
const MAX_CONFIRM: usize = 32 * 1024;

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Directory {
    path: String,
    dev: u64,
    ino: u64,
    mode: u32,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Entry {
    path: String,
    dev: u64,
    ino: u64,
    mode: u32,
    size: u64,
    oid: String,
}

#[derive(Clone, Deserialize, Serialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
struct Review {
    source: Vec<u8>,
    home: Vec<u8>,
    repo: Vec<u8>,
    parent_dev: u64,
    parent_ino: u64,
    source_dev: u64,
    source_ino: u64,
    manifest_sha256: String,
    directories: Vec<Directory>,
    files: Vec<Entry>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Preview {
    record: String,
    kind: String,
    semantics: String,
    source: SourcePath,
    count: usize,
    review: Review,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SourcePath {
    raw: Option<String>,
    bytes: Option<Vec<u8>>,
    shown: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Receipt {
    record: String,
    operation: String,
    repo_id: StatusText,
    review: Review,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultLine {
    record: String,
    outcome: String,
    exit_code: i32,
    #[serde(deserialize_with = "nullable")]
    error_kind: Option<String>,
    recovery: Option<serde_json::Value>,
}

fn nullable<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    Option::deserialize(deserializer)
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ContextLine {
    record: String,
    schema_version: u32,
    experimental: bool,
    command: String,
    home: StatusPath,
    repo: StatusPath,
    config: StatusPath,
    repo_config: StatusPath,
    state: StatusPath,
    identity: StatusPath,
    age_key_file: Option<StatusPath>,
    repo_id: Option<StatusText>,
    machine: crate::protocol::StatusIdentity,
}

#[derive(Serialize)]
struct Request<'a> {
    schema_version: u8,
    home: super::InputPath<'a>,
    repo: super::InputPath<'a>,
    parent_dev: u64,
    parent_ino: u64,
    operation: Operation<'a>,
}

#[derive(Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum Operation<'a> {
    ImportPreview {
        source: super::InputPath<'a>,
    },
    Import {
        source: super::InputPath<'a>,
        review: &'a Review,
    },
}

/// The native-selected source and pinned fresh setup.
pub struct ImportIntent {
    setup: SetupIntent,
    source: PathBuf,
}

/// A complete import review retained only in native code.
pub struct ReviewedImport {
    intent: ImportIntent,
    review: Review,
}

impl ImportIntent {
    /// Binds a native selection to a fresh setup.
    ///
    /// # Errors
    /// Rejects nonliteral paths and non-fresh intents.
    pub fn new(setup: SetupIntent, source: PathBuf) -> Result<Self, String> {
        if setup.source().is_some()
            || !super::literal(&source)
            || source.as_os_str().as_bytes().len() > 4096
        {
            return Err("Invalid import source".into());
        }
        Ok(Self { setup, source })
    }

    /// The retained fresh setup.
    #[must_use]
    pub const fn setup(&self) -> &SetupIntent {
        &self.setup
    }

    fn encode(&self, operation: Operation<'_>) -> Result<Vec<u8>, String> {
        let request = Request {
            schema_version: 1,
            home: input_path(&self.setup.context.home.path),
            repo: input_path(&self.setup.context.repo.path),
            parent_dev: self.setup.dev,
            parent_ino: self.setup.ino,
            operation,
        };
        let mut out = Bounded(Vec::new());
        serde_json::to_writer(&mut out, &request).map_err(|_| "Invalid import request")?;
        Ok(out.0)
    }

    /// Encodes the fixed preview.
    ///
    /// # Errors
    /// Rejects requests that exceed the protocol limit.
    pub fn encode_preview(&self) -> Result<Vec<u8>, String> {
        self.encode(Operation::ImportPreview {
            source: input_path(&self.source),
        })
    }

    /// Validates the full preview and retains the exact review.
    ///
    /// # Errors
    /// Rejects malformed, incomplete or unbound output.
    pub fn parse_preview(self, stdout: &[u8], exit: Option<i32>) -> Result<ReviewedImport, String> {
        let (context, record) = parse_lines(stdout, exit, "import_preview", &self.setup)?;
        let _ = context;
        let preview: Preview =
            serde_json::from_slice(record).map_err(|_| "Invalid import preview")?;
        let source = match (preview.source.raw, preview.source.bytes) {
            (Some(raw), None) => raw.into_bytes(),
            (None, Some(bytes)) => bytes,
            _ => return Err("Invalid import source".into()),
        };
        let _ = preview.source.shown;
        if preview.record != "import_preview"
            || preview.kind != "static_tree"
            || preview.semantics != "working tree only; no history, remote, hooks or apply"
            || source != self.source.as_os_str().as_bytes()
            || preview.count != preview.review.files.len()
        {
            return Err("Invalid import preview".into());
        }
        validate_review(&self, &preview.review)?;
        Ok(ReviewedImport {
            intent: self,
            review: preview.review,
        })
    }
}

fn hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 4096
        && !value.chars().any(crate::runner::unsafe_display_char)
        && value.split('/').count() <= 32
        && value
            .split('/')
            .all(|part| !matches!(part, "" | "." | ".."))
}

fn validate_review(intent: &ImportIntent, review: &Review) -> Result<(), String> {
    let setup = &intent.setup;
    if review.source != intent.source.as_os_str().as_bytes()
        || review.home != setup.context.home.path.as_os_str().as_bytes()
        || review.repo != setup.context.repo.path.as_os_str().as_bytes()
        || (review.parent_dev, review.parent_ino) != (setup.dev, setup.ino)
        || !hex(&review.manifest_sha256, 64)
        || review.files.is_empty()
        || review.files.len().saturating_add(review.directories.len()) > 4096
        || serde_json::to_vec(review)
            .map_err(|_| "Invalid review")?
            .len()
            > MAX_REVIEW
    {
        return Err("Invalid import review".into());
    }
    let mut names = BTreeSet::new();
    let mut dirs = BTreeSet::new();
    for dir in &review.directories {
        if !key(&dir.path) || dir.mode > 0o777 || !names.insert(dir.path.as_str()) {
            return Err("Invalid import directory".into());
        }
        dirs.insert(dir.path.as_str());
    }
    let mut size = 0_u64;
    for entry in &review.files {
        size = size.checked_add(entry.size).ok_or("Invalid import size")?;
        if !key(&entry.path)
            || entry.mode > 0o777
            || entry.mode & 0o111 != 0
            || !(hex(&entry.oid, 40) || hex(&entry.oid, 64))
            || !names.insert(entry.path.as_str())
        {
            return Err("Invalid import file".into());
        }
    }
    if size > 8 * 1024 * 1024 {
        return Err("Invalid import size".into());
    }
    for path in &names {
        let mut parent = Path::new(path).parent();
        while let Some(p) = parent.filter(|p| !p.as_os_str().is_empty()) {
            if !p.to_str().is_some_and(|p| dirs.contains(p)) {
                return Err("Incomplete import directories".into());
            }
            parent = p.parent();
        }
    }
    Ok(())
}

fn parse_lines<'a>(
    stdout: &'a [u8],
    exit: Option<i32>,
    expected: &str,
    setup: &SetupIntent,
) -> Result<(StatusContext, &'a [u8]), String> {
    if exit != Some(0) || stdout.len() > crate::protocol::MAX_REPORT || !stdout.ends_with(b"\n") {
        return Err("Invalid import result".into());
    }
    let mut lines = stdout
        .strip_suffix(b"\n")
        .ok_or("Invalid import result")?
        .split(|b| *b == b'\n');
    let [Some(context), Some(record), Some(result), None] =
        [lines.next(), lines.next(), lines.next(), lines.next()]
    else {
        return Err("Invalid import framing".into());
    };
    for (line, name) in [(context, "context"), (record, expected), (result, "result")] {
        if line.is_empty() || line.len() > crate::protocol::MAX_RECORD || line.ends_with(b"\r") {
            return Err("Invalid import record".into());
        }
        let header: Header = serde_json::from_slice(line).map_err(|_| "Invalid import record")?;
        if header.record != name {
            return Err("Invalid import record".into());
        }
    }
    let strict: ContextLine =
        serde_json::from_slice(context).map_err(|_| "Invalid import context")?;
    let ContextLine {
        record: context_record,
        schema_version,
        experimental,
        command,
        home,
        repo,
        config,
        repo_config,
        state,
        identity,
        age_key_file,
        repo_id,
        machine,
    } = strict;
    let _ = (
        home,
        repo,
        config,
        repo_config,
        state,
        identity,
        age_key_file,
        machine,
    );
    if context_record != "context"
        || schema_version != 1
        || !experimental
        || command != "init"
        || repo_id.is_some()
    {
        return Err("Invalid import context".into());
    }
    let context: StatusContext =
        serde_json::from_slice(context).map_err(|_| "Invalid import context")?;
    context.validate("init")?;
    if !same_paths_and_machine(&setup.context, &context) {
        return Err("Import context changed".into());
    }
    let result: ResultLine = serde_json::from_slice(result).map_err(|_| "Invalid import result")?;
    if result.record != "result"
        || result.outcome != "done"
        || result.exit_code != 0
        || result.error_kind.is_some()
        || result.recovery.is_some()
    {
        return Err("Invalid import result".into());
    }
    Ok((context, record))
}

fn show_path(text: &mut String, path: &Path) -> Result<(), String> {
    let raw = path.as_os_str().as_bytes();
    if let Ok(value) = std::str::from_utf8(raw) {
        text.push('"');
        for c in value.chars() {
            if crate::runner::unsafe_display_char(c) {
                text.extend(c.escape_unicode());
            } else if matches!(c, '"' | '\\') {
                text.push('\\');
                text.push(c);
            } else {
                text.push(c);
            }
        }
        text.push('"');
    } else {
        text.push_str("Unix bytes: ");
        for byte in raw {
            write!(text, "{byte:02x}").map_err(|_| "Invalid path")?;
        }
    }
    if text.len() > MAX_CONFIRM {
        return Err("Import is too large to review".into());
    }
    Ok(())
}

fn safe_native_path(path: &Path) -> bool {
    let bytes = path.as_os_str().as_bytes();
    bytes.len() <= 128 && bytes.iter().all(|b| b.is_ascii_graphic() || *b == b' ')
}

impl ReviewedImport {
    /// The pinned setup.
    #[must_use]
    pub const fn setup(&self) -> &SetupIntent {
        &self.intent.setup
    }

    /// Builds the complete native confirmation; no entries are omitted.
    ///
    /// # Errors
    /// Rejects a list that cannot fit in the native confirmation.
    pub fn confirmation(&self) -> Result<String, String> {
        if self
            .review
            .files
            .len()
            .saturating_add(self.review.directories.len())
            > 12
            || !safe_native_path(&self.intent.source)
            || !safe_native_path(&self.intent.setup.context.repo.path)
            || self
                .review
                .files
                .iter()
                .any(|entry| entry.path.len() > 64 || !entry.path.is_ascii())
            || self
                .review
                .directories
                .iter()
                .any(|entry| entry.path.len() > 64 || !entry.path.is_ascii())
        {
            return Err("Import is too large to review".into());
        }
        let mut text = String::from("Import this selected working tree:\n");
        show_path(&mut text, &self.intent.source)?;
        text.push_str("\n\nDestination:\n");
        show_path(&mut text, &self.intent.setup.context.repo.path)?;
        text.push_str("\n\nSnapshot only: no history, remote, hooks or automatic home apply.\n\nDirectories:\n");
        for dir in &self.review.directories {
            write!(&mut text, "{:04o}  ", dir.mode).map_err(|_| "Cannot render import")?;
            show_path(&mut text, Path::new(&dir.path))?;
            text.push('\n');
            if text.len() > MAX_CONFIRM {
                return Err("Import is too large to review".into());
            }
        }
        text.push_str("\nFiles:\n");
        for entry in &self.review.files {
            write!(&mut text, "{:04o}  {} bytes  ", entry.mode, entry.size)
                .map_err(|_| "Cannot render import")?;
            show_path(&mut text, Path::new(&entry.path))?;
            text.push('\n');
            if text.len() > MAX_CONFIRM {
                return Err("Import is too large to review".into());
            }
        }
        text.push_str("\nImport this snapshot?");
        if text.len() > MAX_CONFIRM {
            return Err("Import is too large to review".into());
        }
        Ok(text)
    }

    /// Encodes the exact retained review.
    ///
    /// # Errors
    /// Rejects requests that exceed the protocol limit.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        self.intent.encode(Operation::Import {
            source: input_path(&self.intent.source),
            review: &self.review,
        })
    }

    /// Validates a publication receipt bound to the retained review.
    ///
    /// # Errors
    /// Rejects malformed, incomplete or unbound output.
    pub fn parse_publication(
        &self,
        stdout: &[u8],
        exit: Option<i32>,
    ) -> Result<SetupPublication, String> {
        let (_, line) = parse_lines(stdout, exit, "setup", &self.intent.setup)?;
        let receipt: Receipt =
            serde_json::from_slice(line).map_err(|_| "Invalid import receipt")?;
        if receipt.record != "setup"
            || receipt.operation != "import"
            || !valid_id(&receipt.repo_id.raw)
            || receipt.review != self.review
        {
            return Err("Invalid import receipt".into());
        }
        let mut context = self.intent.setup.context.clone();
        context.repo_id = Some(receipt.repo_id);
        Ok(SetupPublication {
            context,
            source: None,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::setup::tests::Fixture;
    use serde_json::{Value, json};

    fn preview(
        fixture: &Fixture,
    ) -> Result<(ImportIntent, Value, Value), Box<dyn std::error::Error>> {
        let setup = SetupIntent::from_status(&fixture.status()?)?;
        let envelope: Value = serde_json::from_slice(&setup.encode()?)?;
        let source = fixture.0.join("selected");
        let mut context = fixture.context(None);
        *context.get_mut("command").ok_or("command")? = json!("init");
        let review = json!({
            "source":source.as_os_str().as_bytes(),
            "home":fixture.0.as_os_str().as_bytes(),
            "repo":fixture.0.join("repo").as_os_str().as_bytes(),
            "parent_dev":envelope.get("parent_dev").ok_or("parent")?,
            "parent_ino":envelope.get("parent_ino").ok_or("parent")?,
            "source_dev":1,"source_ino":2,"manifest_sha256":"a".repeat(64),
            "directories":[{"path":".config","dev":1,"ino":3,"mode":448}],
            "files":[{"path":".config/settings","dev":1,"ino":4,"mode":384,"size":5,"oid":"b".repeat(40)}]
        });
        let record = json!({
            "record":"import_preview","kind":"static_tree",
            "semantics":"working tree only; no history, remote, hooks or apply",
            "source":{"raw":source.to_str().ok_or("source")?,"shown":"untrusted display"},
            "count":1,"review":review
        });
        Ok((ImportIntent::new(setup, source)?, context, record))
    }

    fn framed(context: &Value, record: &Value) -> Vec<u8> {
        format!(
            "{context}\n{record}\n{}\n",
            json!({"record":"result","outcome":"done","exit_code":0,"error_kind":null})
        )
        .into_bytes()
    }

    #[test]
    fn preview_confirmation_and_receipt_stay_bound() -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        let (intent, context, record) = preview(&fixture)?;
        let reviewed = intent.parse_preview(&framed(&context, &record), Some(0))?;
        let text = reviewed.confirmation()?;
        assert!(text.contains(".config/settings"));
        assert!(text.contains("0600"));
        assert!(text.contains("no history, remote, hooks"));
        assert!(!text.contains("untrusted display"));
        assert!(!text.contains(&"a".repeat(64)));
        let request: Value = serde_json::from_slice(&reviewed.encode()?)?;
        assert_eq!(request.pointer("/operation/review"), record.get("review"));
        let receipt = json!({"record":"setup","operation":"import","repo_id":{"raw":"0123456789abcdef0123456789abcdef","shown":"x"},"review":record.get("review")});
        assert!(
            reviewed
                .parse_publication(&framed(&context, &receipt), Some(0))
                .is_ok()
        );
        let mut wrong = receipt.clone();
        *wrong
            .pointer_mut("/review/manifest_sha256")
            .ok_or("digest")? = json!("c".repeat(64));
        assert!(
            reviewed
                .parse_publication(&framed(&context, &wrong), Some(0))
                .is_err()
        );
        assert!(
            reviewed
                .parse_publication(&framed(&context, &receipt), Some(1))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn preview_refuses_unbound_incomplete_or_unsafe_records()
    -> Result<(), Box<dyn std::error::Error>> {
        let fixture = Fixture::new()?;
        for change in ["source", "count", "digest", "path", "unknown", "framing"] {
            let (intent, context, mut record) = preview(&fixture)?;
            match change {
                "source" => {
                    *record.pointer_mut("/review/source").ok_or("source")? = json!([47, 120]);
                }
                "count" => *record.get_mut("count").ok_or("count")? = json!(0),
                "digest" => {
                    *record
                        .pointer_mut("/review/manifest_sha256")
                        .ok_or("digest")? = json!("bad");
                }
                "path" => {
                    *record.pointer_mut("/review/files/0/path").ok_or("path")? = json!("bad\nname");
                }
                "unknown" => {
                    record
                        .as_object_mut()
                        .ok_or("record")?
                        .insert("extra".into(), json!(true));
                }
                "framing" => {}
                _ => return Err("invalid case".into()),
            }
            let mut data = framed(&context, &record);
            if change == "framing" {
                data.pop();
            }
            assert!(intent.parse_preview(&data, Some(0)).is_err(), "{change}");
        }
        Ok(())
    }
}
