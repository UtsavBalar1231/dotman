//! Status-bound approval of pending hooks.

use crate::protocol::{self, HookProjection, StatusContext, StatusReport};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;

const LIMIT: usize = 2048;

/// An eligible projection retained with its successful status report.
pub struct HookReview<'a> {
    report: &'a StatusReport,
    hooks: &'a HookProjection,
}

fn path_value(path: &Path) -> Value {
    path.to_str().map_or_else(
        || json!({"bytes": path.as_os_str().as_bytes()}),
        |raw| json!({"raw": raw}),
    )
}

fn escaped(value: &str) -> String {
    let mut text = String::new();
    for c in value.chars() {
        if crate::runner::unsafe_display_char(c) || matches!(c, '"' | '\\') {
            text.extend(c.escape_default());
        } else {
            text.push(c);
        }
    }
    text
}

impl<'a> HookReview<'a> {
    /// Validates the projection before it can authorize execution.
    ///
    /// # Errors
    /// Rejects empty, malformed, unsupported and oversized reviews.
    pub fn new(report: &'a StatusReport) -> Result<Self, &'static str> {
        report
            .context
            .validate("status")
            .map_err(|_| "invalid context")?;
        crate::decisions::validate_identity(&report.context)?;
        let (standing, hooks) = report.hooks.as_deref().ok_or("hooks unavailable")?;
        if !matches!(
            standing.as_str(),
            "unapproved" | "changed" | "old_format" | "approved"
        ) || hooks.entries.is_empty()
            || hooks.pending_keys.is_empty()
            || hooks.pending_commands.is_empty()
        {
            return Err("no eligible pending hooks");
        }
        let mut table = BTreeMap::new();
        for entry in &hooks.entries {
            if entry.pattern.raw.is_empty()
                || entry.command.raw.is_empty()
                || entry.pattern.raw.contains('\0')
                || entry.command.raw.contains('\0')
                || table
                    .insert(&entry.pattern.raw, &entry.command.raw)
                    .is_some()
            {
                return Err("invalid hook table");
            }
        }
        if !hooks.entries.windows(2).all(|pair| {
            pair.first()
                .zip(pair.get(1))
                .is_some_and(|(a, b)| a.pattern.raw < b.pattern.raw)
        }) || serde_json::to_string(&table).map_err(|_| "invalid hook table")?
            != hooks.canonical.raw
        {
            return Err("hook table mismatch");
        }
        if !hooks.pending_keys.windows(2).all(|pair| {
            pair.first()
                .zip(pair.get(1))
                .is_some_and(|(a, b)| a.raw < b.raw)
        }) || hooks
            .pending_keys
            .iter()
            .any(|key| key.raw.is_empty() || key.raw.contains('\0'))
        {
            return Err("invalid pending keys");
        }
        let mut commands = hooks.entries.iter().map(|entry| &entry.command.raw);
        for command in &hooks.pending_commands {
            if hooks
                .pending_commands
                .iter()
                .filter(|item| item.raw == command.raw)
                .count()
                != 1
                || !commands.any(|entry| entry == &command.raw)
            {
                return Err("invalid pending commands");
            }
        }
        let review = Self { report, hooks };
        review.modal()?;
        review.encode()?;
        Ok(review)
    }

    /// Bound context from the original status.
    #[must_use]
    pub const fn context(&self) -> &StatusContext {
        &self.report.context
    }

    /// Full escaped native confirmation text.
    ///
    /// # Errors
    /// Rejects reviews too large for a complete confirmation.
    pub fn modal(&self) -> Result<String, &'static str> {
        let mut text =
            String::from("Approve this entire hook table and execute the pending commands?\n\n");
        for entry in &self.hooks.entries {
            write!(
                text,
                "Pattern: \"{}\"\nCommand: \"{}\"\n",
                escaped(&entry.pattern.raw),
                escaped(&entry.command.raw)
            )
            .map_err(|_| "cannot format hook review")?;
        }
        text.push_str("\nAffected pending keys:\n");
        for key in &self.hooks.pending_keys {
            writeln!(text, "\"{}\"", escaped(&key.raw)).map_err(|_| "cannot format hook review")?;
        }
        text.push_str("\nCommands to execute now, in order:\n");
        for command in &self.hooks.pending_commands {
            writeln!(text, "\"{}\"", escaped(&command.raw))
                .map_err(|_| "cannot format hook review")?;
        }
        text.push_str("\nThis grants standing trust to the entire table and executes the commands now.\nCommands can change files and the repository.\nScripts called by those commands can change later.\n\nContinue?");
        if text.len() > LIMIT
            || text.lines().count() > 22
            || text.lines().any(|line| line.chars().count() > 100)
        {
            return Err("hook review too large");
        }
        Ok(text)
    }

    /// Encode the exact hook-only decision envelope.
    ///
    /// # Errors
    /// Rejects invalid or oversized input.
    pub fn encode(&self) -> Result<Vec<u8>, &'static str> {
        let c = self.context();
        let m = &c.machine;
        let bytes = serde_json::to_vec(&json!({
            "schema_version": 1,
            "repo_id": c.repo_id.as_ref().ok_or("missing repo id")?.raw,
            "home": path_value(&c.home.path), "repo": path_value(&c.repo.path),
            "machine": {"host":m.host.raw,"user":m.user.raw,"os":m.os.raw,"distro":m.distro.raw,"like":m.like.raw,"arch":m.arch.raw,"profiles":m.profiles.iter().map(|p| &p.raw).collect::<Vec<_>>()},
            "files": [],
            "approvals":{"hooks":{"canonical":self.hooks.canonical.raw,"pending_keys":self.hooks.pending_keys.iter().map(|v| &v.raw).collect::<Vec<_>>(),"commands":self.hooks.pending_commands.iter().map(|v| &v.raw).collect::<Vec<_>>()}}
        })).map_err(|_| "cannot encode hook review")?;
        if bytes.len() > LIMIT {
            return Err("hook review too large");
        }
        Ok(bytes)
    }

    /// Checks a fresh status after a successful receipt.
    #[must_use]
    pub fn verified(&self, after: &StatusReport) -> bool {
        protocol::same_context(self.context(), &after.context)
            && after.hooks.as_deref().is_some_and(|(standing, hooks)| {
                standing == "approved"
                    && hooks.canonical.raw == self.hooks.canonical.raw
                    && hooks.pending_keys.is_empty()
            })
    }

    /// Validates the complete hook-only receipt and its process exit.
    #[must_use]
    pub fn receipt(&self, stdout: &[u8], exit: Option<i32>) -> bool {
        if exit != Some(0) || stdout.len() > protocol::MAX_REPORT || !stdout.ends_with(b"\n") {
            return false;
        }
        let mut lines = stdout.split(|byte| *byte == b'\n');
        let Some(first) = lines.next() else {
            return false;
        };
        if first.len() > protocol::MAX_RECORD
            || !matches!(serde_json::from_slice::<protocol::Header>(first), Ok(header) if header.record == "context")
        {
            return false;
        }
        let Ok(context) = serde_json::from_slice::<StatusContext>(first) else {
            return false;
        };
        if context.validate("apply").is_err() || !protocol::same_context(self.context(), &context) {
            return false;
        }
        for command in &self.hooks.pending_commands {
            for (expected, code) in [("started", None), ("succeeded", Some(0))] {
                let Some(line) = lines.next() else {
                    return false;
                };
                if line.len() > protocol::MAX_RECORD {
                    return false;
                }
                let Ok(protocol::SyncRecord::Hook {
                    command: Some(actual),
                    outcome,
                    exit_code,
                }) = serde_json::from_slice(line)
                else {
                    return false;
                };
                if actual.raw != command.raw || outcome != expected || exit_code != code {
                    return false;
                }
            }
        }
        let Some(last) = lines.next() else {
            return false;
        };
        if last.len() > protocol::MAX_RECORD
            || !matches!(serde_json::from_slice::<protocol::Header>(last), Ok(header) if header.record == "result")
        {
            return false;
        }
        let Ok(result) = serde_json::from_slice::<protocol::ResultRecord>(last) else {
            return false;
        };
        result.outcome == "done"
            && result.exit_code == 0
            && result.error_kind.is_none()
            && result.recovery.is_none()
            && lines.next() == Some(&[][..])
            && lines.next().is_none()
    }

    /// Recognizes a context-bound stale review without any hook execution.
    #[must_use]
    pub fn stale_receipt(&self, stdout: &[u8], exit: Option<i32>) -> bool {
        if exit != Some(3) || stdout.len() > protocol::MAX_REPORT || !stdout.ends_with(b"\n") {
            return false;
        }
        let mut lines = stdout.split(|byte| *byte == b'\n');
        let (Some(first), Some(event), Some(last)) = (lines.next(), lines.next(), lines.next())
        else {
            return false;
        };
        if [first, event, last]
            .iter()
            .any(|line| line.len() > protocol::MAX_RECORD)
            || !matches!(serde_json::from_slice::<protocol::Header>(first), Ok(header) if header.record == "context")
            || !matches!(serde_json::from_slice::<protocol::Header>(last), Ok(header) if header.record == "result")
        {
            return false;
        }
        let Ok(context) = serde_json::from_slice::<StatusContext>(first) else {
            return false;
        };
        let Ok(protocol::SyncRecord::Undecided {
            reason,
            key,
            scope,
            pass,
            attention,
            skip_next_pass,
        }) = serde_json::from_slice(event)
        else {
            return false;
        };
        let Ok(result) = serde_json::from_slice::<protocol::ResultRecord>(last) else {
            return false;
        };
        context.validate("apply").is_ok()
            && protocol::same_context(self.context(), &context)
            && reason == "review_stale"
            && key.is_none()
            && scope == "home"
            && pass == "apply"
            && attention
            && !skip_next_pass
            && result.outcome == "error"
            && result.exit_code == 3
            && result.error_kind.is_none()
            && result.recovery.is_none()
            && lines.next() == Some(&[][..])
            && lines.next().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::{StatusReply, parse_status};

    const CONTEXT: &str = r#"{"record":"context","schema_version":1,"experimental":true,"command":"status","home":{"raw":"/home","shown":"/home"},"repo":{"raw":"/repo","shown":"/repo"},"config":{"raw":"/config","shown":"/config"},"repo_config":{"raw":"/repo/.dotman.toml","shown":"/repo/.dotman.toml"},"state":{"raw":"/state","shown":"/state"},"identity":{"raw":"/identity","shown":"/identity"},"age_key_file":null,"repo_id":{"raw":"id","shown":"id"},"machine":{"host":{"raw":"h","shown":"h"},"user":{"raw":"u","shown":"u"},"os":{"raw":"linux","shown":"linux"},"distro":{"raw":"","shown":""},"like":{"raw":"","shown":""},"arch":{"raw":"x","shown":"x"},"profiles":[]}}"#;
    const SUMMARY: &str = r#"{"record":"summary","rows":0,"clean":0,"new":0,"held":0,"held_in_home":0,"held_in_repo":0,"unrecorded":0}"#;
    const RESULT: &str = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;

    fn fixture(
        command: &str,
        standing: &str,
        pending: bool,
    ) -> Result<StatusReport, Box<dyn std::error::Error>> {
        let text = |s: &str| json!({"raw":s, "shown":s});
        let canonical = serde_json::to_string(&BTreeMap::from([(".a", command)]))?;
        let hook = json!({"standing":standing,"canonical":text(&canonical),"entries":[{"pattern":text(".a"),"command":text(command)}],
            "pending_keys": if pending {vec![text(".a")]} else {vec![]},
            "pending_commands": if pending {vec![text(command)]} else {vec![]}});
        let marker = json!({"record":"review_context","hooks":hook,"remote":{"basis":"unknown","ahead":null,"behind":null}});
        let input = format!("{CONTEXT}\n{marker}\n{SUMMARY}\n{RESULT}\n");
        match parse_status(input.as_bytes(), Some(0))? {
            StatusReply::Report(report) => Ok(report),
            StatusReply::Error { .. } => Err("not a report".into()),
        }
    }

    #[test]
    fn review_encodes_exact_input_and_validates_receipt_and_fresh_status()
    -> Result<(), Box<dyn std::error::Error>> {
        let report = fixture("echo ran >> marker", "unapproved", true)?;
        let review = HookReview::new(&report)?;
        let document: Value = serde_json::from_slice(&review.encode()?)?;
        assert_eq!(
            document
                .pointer("/approvals/hooks/canonical")
                .and_then(Value::as_str)
                .ok_or("missing canonical hook table")?,
            r#"{".a":"echo ran >> marker"}"#
        );
        assert_eq!(
            document
                .pointer("/approvals/hooks/pending_keys")
                .ok_or("missing pending hook keys")?,
            &json!([".a"])
        );
        assert_eq!(
            document
                .pointer("/approvals/hooks/commands")
                .ok_or("missing pending hook commands")?,
            &json!(["echo ran >> marker"])
        );
        assert_eq!(document.get("files").ok_or("missing files")?, &json!([]));
        assert!(
            review
                .modal()?
                .contains("Scripts called by those commands can change later")
        );
        let context = CONTEXT.replace(r#""command":"status""#, r#""command":"apply""#);
        let start = json!({"record":"hook","command":{"raw":"echo ran >> marker","shown":"redacted"},"outcome":"started","exit_code":null});
        let success = json!({"record":"hook","command":{"raw":"echo ran >> marker","shown":"redacted"},"outcome":"succeeded","exit_code":0});
        let receipt = format!("{context}\n{start}\n{success}\n{RESULT}\n");
        assert!(review.receipt(receipt.as_bytes(), Some(0)));
        assert!(!review.receipt(receipt.as_bytes(), Some(3)));
        assert!(!review.receipt(
            receipt.replace("echo ran", "echo stale").as_bytes(),
            Some(0)
        ));
        assert!(review.verified(&fixture("echo ran >> marker", "approved", false)?));
        assert!(!review.verified(&fixture("echo changed", "approved", false)?));
        assert!(!review.verified(&fixture("echo ran >> marker", "approved", true)?));
        Ok(())
    }

    #[test]
    fn rejects_oversized_and_inconsistent_projection() -> Result<(), Box<dyn std::error::Error>> {
        let large = fixture(&"x".repeat(LIMIT), "unapproved", true)?;
        assert!(HookReview::new(&large).is_err());
        let mut report = fixture("echo ran", "unapproved", true)?;
        let (_, hooks) = report.hooks.as_deref_mut().ok_or("missing hooks")?;
        hooks
            .pending_commands
            .first_mut()
            .ok_or("missing command")?
            .raw = "echo stale".into();
        assert!(HookReview::new(&report).is_err());
        Ok(())
    }

    #[test]
    #[ignore = "requires DOTMAN_TEST_CLI pointing to the built CLI"]
    #[expect(
        clippy::too_many_lines,
        reason = "One hermetic lifecycle checks stale and successful transport"
    )]
    fn real_cli_transport_runs_only_the_reviewed_pending_hook()
    -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        use std::process::{Command, Output, Stdio};
        struct Fixture(std::path::PathBuf);
        impl Drop for Fixture {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root = std::env::temp_dir().join(format!("dotman-hook-review-{}", std::process::id()));
        std::fs::create_dir(&root)?;
        let fixture = Fixture(root);
        let home = fixture.0.join("home");
        let config = fixture.0.join("config");
        let data = fixture.0.join("data");
        let state = fixture.0.join("state");
        let cache = fixture.0.join("cache");
        for path in [&home, &config, &data, &state, &cache] {
            std::fs::create_dir(path)?;
        }
        std::fs::write(
            home.join(".gitconfig"),
            "[user]\nname = Test\nemail = test@example.invalid\n",
        )?;
        let cli = std::env::var_os("DOTMAN_TEST_CLI").ok_or("missing CLI")?;
        let command = || {
            let mut cmd = Command::new(&cli);
            cmd.env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &home)
                .env("XDG_CONFIG_HOME", &config)
                .env("XDG_DATA_HOME", &data)
                .env("XDG_STATE_HOME", &state)
                .env("XDG_CACHE_HOME", &cache)
                .current_dir(&home);
            cmd
        };
        let run =
            |args: &[&str], input: Option<&[u8]>| -> Result<Output, Box<dyn std::error::Error>> {
                let mut cmd = command();
                cmd.args(args).stdout(Stdio::piped()).stderr(Stdio::piped());
                if input.is_some() {
                    cmd.stdin(Stdio::piped());
                }
                let mut child = cmd.spawn()?;
                if let Some(bytes) = input {
                    child
                        .stdin
                        .take()
                        .ok_or("missing stdin")?
                        .write_all(bytes)?;
                }
                Ok(child.wait_with_output()?)
            };
        assert!(run(&["init"], None)?.status.success());
        let repo = data.join("dotman");
        std::fs::write(repo.join(".a"), "base")?;
        std::fs::write(repo.join(".b"), "base")?;
        assert!(run(&["sync"], None)?.status.success());
        std::fs::write(
            repo.join(".dotman.toml"),
            "[hooks]\n\".a\" = \"printf ran >> marker\"\n",
        )?;
        std::fs::write(repo.join(".a"), "changed")?;
        assert!(run(&["apply", ".a"], None)?.status.success());
        std::fs::write(repo.join(".b"), "unrelated")?;
        let status = run(&["status", "--json"], None)?;
        let StatusReply::Report(report) = parse_status(&status.stdout, status.status.code())?
        else {
            return Err("status failed".into());
        };
        let review = HookReview::new(&report)?;
        let encoded = review.encode()?;
        let baseline = std::fs::read(state.join("dotman/state.toml"))?;
        let refs = || -> Result<Vec<u8>, Box<dyn std::error::Error>> {
            let output = Command::new("git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", &home)
                .arg("-C")
                .arg(&repo)
                .args(["for-each-ref", "--format=%(refname) %(objectname)"])
                .output()?;
            if !output.status.success() {
                return Err("cannot inspect refs".into());
            }
            Ok(output.stdout)
        };
        let initial_refs = refs()?;
        assert!(!home.join("marker").exists());
        for (field, value) in [
            ("canonical", json!("{}")),
            ("commands", json!(["different"])),
            ("pending_keys", json!([".b"])),
        ] {
            let mut input: Value = serde_json::from_slice(&encoded)?;
            *input
                .pointer_mut(&format!("/approvals/hooks/{field}"))
                .ok_or("missing hook approval field")? = value;
            let outcome = run(
                &["apply", "--review-hooks", "--decide-from", "-", "--json"],
                Some(&serde_json::to_vec(&input)?),
            )?;
            assert_eq!(outcome.status.code(), Some(3));
            assert!(review.stale_receipt(&outcome.stdout, outcome.status.code()));
            assert_eq!(std::fs::read(state.join("dotman/state.toml"))?, baseline);
            assert!(!home.join("marker").exists());
            assert_eq!(refs()?, initial_refs);
        }
        let outcome = run(
            &["apply", "--review-hooks", "--decide-from", "-", "--json"],
            Some(&encoded),
        )?;
        assert!(review.receipt(&outcome.stdout, outcome.status.code()));
        assert_eq!(std::fs::read(home.join("marker"))?, b"ran");
        assert_eq!(std::fs::read(home.join(".b"))?, b"base");
        assert_eq!(std::fs::read(repo.join(".b"))?, b"unrelated");
        assert_eq!(refs()?, initial_refs);
        let after = run(&["status", "--json"], None)?;
        let StatusReply::Report(after) = parse_status(&after.stdout, after.status.code())? else {
            return Err("post-status failed".into());
        };
        assert!(review.verified(&after));
        Ok(())
    }
}
