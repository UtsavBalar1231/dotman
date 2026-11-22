use super::{AppState, HeldPaths, Inner, WriterView, run_status, wait_apply, writer_view};
use dotman_gui::{protocol, runner};
use serde::Deserialize;
use std::os::unix::ffi::OsStrExt;
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

pub(super) fn eligible(entry: &protocol::StatusEntry, paths: &HeldPaths<'_>) -> bool {
    entry.scope == "home"
        && !entry.system
        && !entry.template
        && !entry.identity_changed
        && matches!(entry.status.as_str(), "clean" | "changed-locally")
        && entry.finding.is_none()
        && !paths.matches(entry, false)
        && !paths.matches(entry, true)
        && entry.source.as_ref().is_some_and(|source| {
            entry
                .recorded_source
                .as_ref()
                .is_some_and(|recorded| recorded.raw == source.raw)
        })
        && matches!(&entry.raw_h, protocol::StatusObservation::Present { tuple } if tuple.kind == "file")
        && matches!(&entry.source_tuple, protocol::StatusObservation::Present { tuple } if tuple.kind == "file")
}

fn select(
    inner: &Inner,
    id: &str,
) -> Result<(protocol::StatusContext, protocol::StatusEntry), &'static str> {
    if inner.active.is_some() || inner.recovery.is_some() || inner.generation == u64::MAX {
        return Err("Another operation is running");
    }
    let (generation, index) = id.split_once(':').ok_or("Invalid selection")?;
    let n: usize = index.parse().map_err(|_| "Invalid selection")?;
    if generation != inner.generation.to_string() || index != n.to_string() {
        return Err("Status selection is stale");
    }
    let Some(protocol::StatusReply::Report(report)) = &inner.reply else {
        return Err("Status is unavailable");
    };
    let entry = report.entries.get(n).ok_or("Invalid selection")?;
    if !eligible(entry, &HeldPaths::new(report)) || report.context.repo_id.is_none() {
        return Err("This file is not eligible");
    }
    Ok((report.context.clone(), entry.clone()))
}

fn same_context(a: &protocol::StatusContext, b: &protocol::StatusContext) -> bool {
    a.home.path == b.home.path
        && a.repo.path == b.repo.path
        && a.config.path == b.config.path
        && a.repo_config.path == b.repo_config.path
        && a.state.path == b.state.path
        && a.identity.path == b.identity.path
        && a.age_key_file == b.age_key_file
        && a.repo_id == b.repo_id
        && a.machine == b.machine
}

fn still_current(
    context: &protocol::StatusContext,
    entry: &protocol::StatusEntry,
    cancelled: &AtomicBool,
) -> bool {
    let Ok(protocol::StatusReply::Report(report)) = run_status(cancelled) else {
        return false;
    };
    let mut matches = report
        .entries
        .iter()
        .filter(|item| item.key.raw == entry.key.raw && eligible(item, &HeldPaths::new(&report)));
    let Some(item) = matches.next() else {
        return false;
    };
    matches.next().is_none()
        && same_context(context, &report.context)
        && item.source == entry.source
        && item.recorded_source == entry.recorded_source
        && item.encrypted == entry.encrypted
        && item.raw_h == entry.raw_h
        && item.source_tuple == entry.source_tuple
        && item.s == entry.s
        && item.r == entry.r
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireReview {
    schema_version: u32,
    record: String,
    operation: String,
    executable: bool,
    key: protocol::StatusText,
    selected: protocol::StatusText,
    host: protocol::StatusText,
    target: protocol::StatusText,
    repo_id: protocol::StatusText,
    encrypted: bool,
    recipients: Vec<String>,
    home_mode: String,
    home: protocol::StatusPath,
    repo: protocol::StatusPath,
    state: protocol::StatusPath,
    config: protocol::StatusPath,
    repo_config: protocol::StatusPath,
    identity: protocol::StatusPath,
    age_key_file: Option<protocol::StatusPath>,
    snapshot: serde_json::Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewError {
    schema_version: u32,
    record: String,
    code: String,
    message: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReviewFinal {
    record: String,
    outcome: String,
    exit_code: i32,
    error_kind: Option<String>,
}

fn terminal_reason(bytes: &[u8], status: Option<i32>) -> Option<&'static str> {
    if bytes.len() > 65_536 || status.is_none_or(|code| code == 0) {
        return None;
    }
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let error: ReviewError = serde_json::from_slice(lines.next()?).ok()?;
    let final_result: ReviewFinal = serde_json::from_slice(lines.next()?).ok()?;
    if lines.next() != Some(&[][..])
        || lines.next().is_some()
        || error.schema_version != 1
        || error.record != "remedy_error"
        || error.code != "terminal_only"
        || final_result.record != "result"
        || final_result.outcome != "error"
        || Some(final_result.exit_code) != status
        || final_result.error_kind.is_none()
    {
        return None;
    }
    match error.message.as_str() {
        "mode settings require terminal review" => {
            Some("Mode settings require terminal review. No variant was created.")
        }
        "recipient type requires terminal review" => {
            Some("The recipient type requires terminal review. No variant was created.")
        }
        _ => None,
    }
}

fn parse_review(
    bytes: &[u8],
    status: Option<i32>,
    context: &protocol::StatusContext,
    entry: &protocol::StatusEntry,
) -> Result<WireReview, &'static str> {
    if status != Some(0) || bytes.len() > 65_536 {
        return Err("Review failed");
    }
    let mut lines = bytes.split(|b| *b == b'\n');
    let wire: WireReview = serde_json::from_slice(lines.next().ok_or("Missing review")?)
        .map_err(|_| "Invalid review")?;
    let result: serde_json::Value = serde_json::from_slice(lines.next().ok_or("Missing result")?)
        .map_err(|_| "Invalid result")?;
    if lines.next() != Some(&[][..])
        || lines.next().is_some()
        || result.get("record").and_then(serde_json::Value::as_str) != Some("result")
        || result.get("outcome").and_then(serde_json::Value::as_str) != Some("done")
        || result.get("exit_code").and_then(serde_json::Value::as_i64) != Some(0)
        || result.get("error_kind") != Some(&serde_json::Value::Null)
        || wire.schema_version != 1
        || wire.record != "remedy_review"
        || wire.operation != "review_host_variant"
        || !wire.executable
        || wire.key.raw != entry.key.raw
        || entry
            .source
            .as_ref()
            .is_none_or(|source| source.raw != wire.selected.raw)
        || wire.host.raw != context.machine.host.raw
        || context
            .repo_id
            .as_ref()
            .is_none_or(|id| id.raw != wire.repo_id.raw)
        || wire.home.path != context.home.path
        || wire.repo.path != context.repo.path
        || wire.state.path != context.state.path
        || wire.config.path != context.config.path
        || wire.repo_config.path != context.repo_config.path
        || wire.identity.path != context.identity.path
        || wire.age_key_file != context.age_key_file
        || wire.encrypted != entry.encrypted
        || (wire.encrypted == wire.recipients.is_empty())
        || wire.recipients.iter().any(|recipient| {
            recipient.len() != 62
                || !recipient.starts_with("age1")
                || !recipient
                    .bytes()
                    .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        })
        || wire
            .snapshot
            .pointer("/review/key")
            .and_then(serde_json::Value::as_str)
            != Some(wire.key.raw.as_str())
        || wire
            .snapshot
            .pointer("/review/selected")
            .and_then(serde_json::Value::as_str)
            != Some(wire.selected.raw.as_str())
        || wire
            .snapshot
            .pointer("/review/target")
            .and_then(serde_json::Value::as_str)
            != Some(wire.target.raw.as_str())
        || wire
            .snapshot
            .pointer("/review/identity/host")
            .and_then(serde_json::Value::as_str)
            != Some(wire.host.raw.as_str())
        || !identity_matches(&wire, context)
        || wire.snapshot.get("occurrence")
            != Some(
                &serde_json::json!({"scope":"home","source":wire.selected.raw,"reason":"host_variant","finding":null}),
            )
        || wire.snapshot.get("config_path")
            != Some(&serde_json::json!(
                context.repo_config.path.as_os_str().as_bytes()
            ))
        || wire
            .snapshot
            .pointer("/review/config_path")
            .and_then(serde_json::Value::as_str)
            != Some("")
        || wire
            .snapshot
            .pointer("/review/repo_id")
            .and_then(serde_json::Value::as_str)
            != Some(wire.repo_id.raw.as_str())
        || wire
            .snapshot
            .pointer("/review/encrypted")
            .and_then(serde_json::Value::as_bool)
            != Some(wire.encrypted)
        || wire.snapshot.pointer("/review/recipients") != Some(&serde_json::json!(wire.recipients))
        || !matches!(&entry.raw_h, protocol::StatusObservation::Present { tuple }
            if wire.snapshot.pointer("/review/home_hash").and_then(serde_json::Value::as_str) == Some(tuple.hash.as_str())
                && wire.home_mode == tuple.mode
                && u32::from_str_radix(&wire.home_mode, 8).ok().map(u64::from)
                    == wire.snapshot.pointer("/review/home_mode").and_then(serde_json::Value::as_u64))
    {
        return Err("Review differs from selected file");
    }
    Ok(wire)
}

fn identity_matches(wire: &WireReview, context: &protocol::StatusContext) -> bool {
    let identity = &context.machine;
    [
        ("/review/identity/user", &identity.user.raw),
        ("/review/identity/os", &identity.os.raw),
        ("/review/identity/distro", &identity.distro.raw),
        ("/review/identity/like", &identity.like.raw),
        ("/review/identity/arch", &identity.arch.raw),
    ]
    .into_iter()
    .all(|(path, expected)| {
        wire.snapshot
            .pointer(path)
            .and_then(serde_json::Value::as_str)
            == Some(expected)
    }) && wire.snapshot.pointer("/review/identity/profiles")
        == Some(&serde_json::json!(
            identity
                .profiles
                .iter()
                .map(|profile| &profile.raw)
                .collect::<Vec<_>>()
        ))
}

fn shown(text: &str) -> String {
    text.chars()
        .flat_map(|c| {
            if runner::unsafe_display_char(c) {
                c.escape_unicode().to_string().chars().collect::<Vec<_>>()
            } else {
                vec![c]
            }
        })
        .collect()
}

fn shown_path(path: &std::path::Path) -> String {
    use std::fmt::Write as _;
    if let Some(text) = path.to_str() {
        return shown(text);
    }
    let mut result = String::new();
    for byte in path.as_os_str().as_bytes() {
        if (byte.is_ascii_graphic() && *byte != b'\\') || *byte == b' ' {
            result.push(char::from(*byte));
        } else {
            let _ = write!(result, "\\x{byte:02x}");
        }
    }
    result
}

#[tauri::command]
pub async fn run_host_variant(
    id: String,
    window: tauri::WebviewWindow,
    state: tauri::State<'_, AppState>,
) -> Result<WriterView, String> {
    if id.len() > 64 {
        return Err("Invalid selection".into());
    }
    let cancelled = Arc::new(AtomicBool::new(false));
    let (context, entry) = {
        let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
        let selected = select(&inner, &id)?;
        inner.reply = None;
        inner.review = None;
        inner.diff_receipts.clear();
        inner.generation = inner.generation.saturating_add(1);
        inner.active = Some(cancelled.clone());
        selected
    };
    let shared = state.inner.clone();
    let flag = cancelled.clone();
    let uncertain = Arc::new(AtomicBool::new(false));
    let pending = uncertain.clone();
    let worker = tauri::async_runtime::spawn_blocking(move || {
        let attention = || writer_view("attention", "Variant could not be verified; refresh status before trying again.", true);
        if !still_current(&context, &entry, &flag) { return attention(); }
        let Some(source) = entry.source.as_ref() else { return attention() };
        let occurrence = serde_json::json!({"scope":"home","source":source.raw,"reason":"host_variant","finding":null});
        let request = serde_json::json!({"schema_version":1,"operation":"review_host_variant","key":entry.key.raw,"occurrence":occurrence});
        let Ok(bytes) = serde_json::to_vec(&request) else { return attention() };
        let Ok(mut run) = runner::start_remedy(&context.home.path, bytes) else { return attention() };
        let Ok(receipt) = wait_apply(&mut run, &flag) else { return attention() };
        if let Some(reason) = terminal_reason(&receipt.stdout, receipt.status.code()) {
            return writer_view("attention", reason, true);
        }
        let Ok(review) = parse_review(&receipt.stdout, receipt.status.code(), &context, &entry) else { return attention() };
        let text = format!("Create a variant for this machine?\n\nHost: {}\nRepository: {}\nState: {}\nKey: {}\nCurrent source: {}\nNew source: {}\nRecipients: {}\n\nThe new source is absent and will be created. The local state entry will select it. Home, the original source, other variants, settings and index stay unchanged. No hooks, commit or sync run.\n\nContinue?",
            shown(&review.host.raw), shown_path(&review.repo.path), shown_path(&review.state.path), shown(&review.key.raw), shown(&review.selected.raw), shown(&review.target.raw),
            if review.encrypted { review.recipients.join(", ") } else { "none (unencrypted)".into() });
        if !crate::confirm::ask(&window, "Create host variant?", "Create host variant", text, flag.clone()).unwrap_or(false) || flag.load(Ordering::Acquire) {
            return writer_view("declined", "Host variant was not created.", true);
        }
        if !still_current(&context, &entry, &flag) { return attention() }
        let request = serde_json::json!({"schema_version":1,"operation":"execute_host_variant","key":entry.key.raw,"occurrence":occurrence,"snapshot":review.snapshot});
        let Ok(bytes) = serde_json::to_vec(&request) else { return attention() };
        let Ok(guard) = shared.lock() else { return attention() };
        if flag.load(Ordering::Acquire) { return attention() }
        pending.store(true, Ordering::Release);
        let Ok(mut run) = runner::start_remedy(&context.home.path, bytes) else { pending.store(false, Ordering::Release); return attention() };
        drop(guard);
        let Ok(receipt) = wait_apply(&mut run, &flag) else { return writer_view("attention", "Variant may have been published; check status before retrying.", false) };
        pending.store(false, Ordering::Release);
        let mut lines = receipt.stdout.split(|b| *b == b'\n');
        let result: Option<serde_json::Value> = lines.next().and_then(|line| serde_json::from_slice(line).ok());
        let final_result: Option<serde_json::Value> = lines.next().and_then(|line| serde_json::from_slice(line).ok());
        if receipt.status.code() != Some(0)
            || result.as_ref().and_then(|v| v.get("schema_version")).and_then(serde_json::Value::as_u64) != Some(1)
            || result.as_ref().and_then(|v| v.get("record")).and_then(serde_json::Value::as_str) != Some("remedy_result")
            || result.as_ref().and_then(|v| v.get("result")).and_then(serde_json::Value::as_str) != Some("captured")
            || result.as_ref().and_then(|v| v.get("operation")).and_then(serde_json::Value::as_str) != Some("execute_host_variant")
            || result.as_ref().and_then(|v| v.pointer("/key/raw")).and_then(serde_json::Value::as_str) != Some(entry.key.raw.as_str())
            || final_result.as_ref().and_then(|v| v.get("outcome")).and_then(serde_json::Value::as_str) != Some("done")
            || final_result.as_ref().and_then(|v| v.get("record")).and_then(serde_json::Value::as_str) != Some("result")
            || final_result.as_ref().and_then(|v| v.get("exit_code")).and_then(serde_json::Value::as_i64) != Some(0)
            || final_result.as_ref().and_then(|v| v.get("error_kind")) != Some(&serde_json::Value::Null)
            || lines.next() != Some(&[][..]) || lines.next().is_some() {
            return writer_view("attention", "Variant may have been published; check status before retrying.", true);
        }
        writer_view("done", "Host variant created.", true)
    }).await;
    let mut inner = state.inner.lock().map_err(|_| "Status state unavailable")?;
    if !uncertain.load(Ordering::Acquire) {
        inner.active = None;
    }
    drop(inner);
    worker.map_err(|_| "Variant operation ended unexpectedly; check status before retrying.".into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    fn fixture()
    -> Result<(protocol::StatusContext, protocol::StatusEntry, Value), serde_json::Error> {
        let text = |raw| json!({"raw":raw,"shown":raw});
        let path = |raw| json!({"raw":raw,"shown":raw});
        let machine = json!({"host":text("box"),"user":text("u"),"os":text("linux"),
            "distro":text("arch"),"like":text(""),"arch":text("x86_64"),"profiles":[]});
        let context = serde_json::from_value(
            json!({"schema_version":1,"experimental":true,"command":"status",
            "home":path("/h"),"repo":path("/r"),"config":path("/c"),"repo_config":path("/r/.dotman.toml"),
            "state":path("/s/state.toml"),"identity":path("/i"),"age_key_file":null,
            "repo_id":text("repo"),"machine":machine}),
        )?;
        let tuple = json!({"state":"present","tuple":{"kind":"file","hash":"0123456789abcdef0123456789abcdef","mode":"0644"}});
        let entry = serde_json::from_value(
            json!({"status":"changed-locally","key":text("file"),"scope":"home",
            "path":null,"source":text("file"),"recorded_source":text("file"),"encrypted":false,"template":false,
            "system":false,"row":"changed-locally","row_number":9,"category":null,
            "r":tuple,"source_tuple":tuple,"h":tuple,"raw_h":tuple,"s":tuple,
            "identity_changed":false,"update_state":false}),
        )?;
        let occurrence =
            json!({"scope":"home","source":"file","reason":"host_variant","finding":null});
        let row = json!({"schema_version":1,"record":"remedy_review","operation":"review_host_variant","executable":true,
            "key":text("file"),"selected":text("file"),"host":text("box"),"target":text("file{host=box}"),
            "repo_id":text("repo"),"encrypted":false,"recipients":[],"home_mode":"0644",
            "home":path("/h"),"repo":path("/r"),"state":path("/s/state.toml"),
            "config":path("/c"),"repo_config":path("/r/.dotman.toml"),"identity":path("/i"),"age_key_file":null,
            "snapshot":{"occurrence":occurrence,"config_path":b"/r/.dotman.toml","review":{"config_path":"","key":"file","selected":"file","target":"file{host=box}",
                "identity":{"host":"box","user":"u","os":"linux","distro":"arch","like":"","arch":"x86_64","profiles":[]},
                "repo_id":"repo","encrypted":false,"recipients":[],"home_hash":"0123456789abcdef0123456789abcdef","home_mode":420}}});
        Ok((context, entry, row))
    }

    fn receipt(row: &Value) -> Vec<u8> {
        format!(
            "{row}\n{}\n",
            json!({"record":"result","outcome":"done","exit_code":0,"error_kind":null})
        )
        .into_bytes()
    }

    #[test]
    fn review_binds_identity_source_context_and_home() -> Result<(), Box<dyn std::error::Error>> {
        let (context, entry, mut row) = fixture()?;
        assert!(parse_review(&receipt(&row), Some(0), &context, &entry).is_ok());
        *row.pointer_mut("/snapshot/review/identity/user")
            .ok_or("missing user")? = json!("other");
        assert!(parse_review(&receipt(&row), Some(0), &context, &entry).is_err());
        *row.pointer_mut("/snapshot/review/identity/user")
            .ok_or("missing user")? = json!("u");
        *row.pointer_mut("/selected/raw").ok_or("missing source")? = json!("other");
        assert!(parse_review(&receipt(&row), Some(0), &context, &entry).is_err());
        Ok(())
    }

    #[test]
    fn review_rejects_recipients_and_invalid_framing() -> Result<(), Box<dyn std::error::Error>> {
        let (context, entry, mut row) = fixture()?;
        *row.get_mut("recipients").ok_or("missing recipients")? = json!(["age1bad"]);
        assert!(parse_review(&receipt(&row), Some(0), &context, &entry).is_err());
        let (_, _, row) = fixture()?;
        let mut bytes = receipt(&row);
        bytes.extend_from_slice(b"extra\n");
        assert!(parse_review(&bytes, Some(0), &context, &entry).is_err());
        assert!(parse_review(&receipt(&row), Some(1), &context, &entry).is_err());
        Ok(())
    }

    #[test]
    fn confirmation_paths_escape_non_utf8_bytes() {
        use std::os::unix::ffi::OsStringExt;
        let path = std::path::PathBuf::from(std::ffi::OsString::from_vec(b"/repo-\xff\n".to_vec()));
        assert_eq!(shown_path(&path), "/repo-\\xff\\x0a");
    }

    #[test]
    fn terminal_reasons_are_bound_and_do_not_echo_untrusted_text() {
        let receipt = |message: &str| {
            format!(
                "{}\n{}\n",
                json!({"schema_version":1,"record":"remedy_error","code":"terminal_only","message":message}),
                json!({"record":"result","outcome":"error","exit_code":1,"error_kind":"other"})
            )
        };
        assert_eq!(
            terminal_reason(
                receipt("mode settings require terminal review").as_bytes(),
                Some(1)
            ),
            Some("Mode settings require terminal review. No variant was created.")
        );
        assert_eq!(
            terminal_reason(
                receipt("recipient type requires terminal review").as_bytes(),
                Some(1)
            ),
            Some("The recipient type requires terminal review. No variant was created.")
        );
        assert_eq!(
            terminal_reason(receipt("private-marker").as_bytes(), Some(1)),
            None
        );
        assert_eq!(
            terminal_reason(
                receipt("mode settings require terminal review").as_bytes(),
                Some(3)
            ),
            None
        );
    }
}
