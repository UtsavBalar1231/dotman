use serde::{Deserialize, Serialize};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FinalResult {
    record: String,
    outcome: String,
    exit_code: i32,
    error_kind: Option<String>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Snapshot {
    action: String,
    occurrence: Option<serde_json::Value>,
    key: String,
    repo_id: String,
    reason: String,
    hash: String,
    content_oid: String,
    mode: u32,
    prepare_mode: bool,
    config_oid: String,
    config_mode: u32,
    context: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Tuple {
    kind: String,
    hash: String,
    mode: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Review {
    schema_version: u32,
    record: String,
    operation: String,
    executable: bool,
    reason: String,
    repo_id: String,
    key: String,
    home: Tuple,
    snapshot: Option<Snapshot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PlainReview {
    schema_version: u32,
    record: String,
    operation: String,
    executable: bool,
    reason: String,
    key: String,
    occurrence: serde_json::Value,
    plaintext_may_be_published: bool,
    prepare_mode: bool,
    snapshot: Option<Snapshot>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Executed {
    schema_version: u32,
    record: String,
    operation: String,
    result: String,
    key: String,
}

/// An executable CLI review retained exclusively by native code.
pub struct HeldReview {
    snapshot: Snapshot,
}

fn records(bytes: &[u8], status: Option<i32>) -> Result<(&[u8], &[u8]), &'static str> {
    if status != Some(0) || bytes.len() > 65_536 {
        return Err("Remedy receipt could not be verified");
    }
    let mut lines = bytes.split(|byte| *byte == b'\n');
    let first = lines.next().ok_or("Missing remedy receipt")?;
    let last = lines.next().ok_or("Missing final result")?;
    if lines.next() != Some(&[][..]) || lines.next().is_some() {
        return Err("Invalid remedy receipt");
    }
    let result: FinalResult = serde_json::from_slice(last).map_err(|_| "Invalid final result")?;
    if result.record != "result"
        || result.outcome != "done"
        || result.exit_code != 0
        || result.error_kind.is_some()
    {
        return Err("Remedy failed");
    }
    Ok((first, last))
}

/// Parse and bind an add review to the selected inventory occurrence.
///
/// # Errors
/// Refuses malformed, stale, unsupported, or non-executable reviews.
pub fn review(
    bytes: &[u8],
    status: Option<i32>,
    key: &str,
    repo_id: &str,
    reason: &str,
) -> Result<HeldReview, &'static str> {
    let (first, _) = records(bytes, status)?;
    let review: Review = serde_json::from_slice(first).map_err(|_| "Invalid remedy review")?;
    let snapshot = review
        .snapshot
        .ok_or("This file cannot be added without changing metadata")?;
    if review.schema_version != 1
        || review.record != "remedy_review"
        || review.operation != "review"
        || !review.executable
        || review.key != key
        || review.repo_id != repo_id
        || review.reason != reason
        || snapshot.action != "held"
        || snapshot.occurrence.is_some()
        || snapshot.key != key
        || snapshot.repo_id != repo_id
        || snapshot.reason != reason
        || snapshot.prepare_mode
        || review.home.kind != "file"
        || snapshot.hash != review.home.hash
        || format!("{:04o}", snapshot.mode) != review.home.mode
        || snapshot.hash.len() != 32
        || !snapshot
            .hash
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
        || !matches!(snapshot.mode, 0o600 | 0o644)
    {
        return Err("Remedy review differs from the selected occurrence");
    }
    Ok(HeldReview { snapshot })
}

/// Validate a name-only plaintext review.
///
/// # Errors
/// Refuses nonmatching or nonexecutable reviews.
pub fn review_plain(
    bytes: &[u8],
    status: Option<i32>,
    key: &str,
    repo_id: &str,
) -> Result<HeldReview, &'static str> {
    let (first, _) = records(bytes, status)?;
    let review: PlainReview =
        serde_json::from_slice(first).map_err(|_| "Invalid plaintext review")?;
    let snapshot = review
        .snapshot
        .ok_or("Plaintext review is not executable")?;
    let occurrence =
        serde_json::json!({"scope":"new","source":null,"reason":"secret_name","finding":null});
    if review.schema_version != 1
        || review.record != "remedy_review"
        || review.operation != "review_plain"
        || !review.executable
        || !review.plaintext_may_be_published
        || review.reason != "secret_name"
        || review.key != key
        || review.occurrence != occurrence
        || snapshot.occurrence.as_ref() != Some(&occurrence)
        || snapshot.action != "allow_plain"
        || snapshot.key != key
        || snapshot.repo_id != repo_id
        || snapshot.reason != "secret_name"
        || snapshot.prepare_mode != review.prepare_mode
        || !matches!(snapshot.mode, 0o600 | 0o644)
        || snapshot.hash.len() != 32
        || !snapshot
            .hash
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("Plaintext review differs from the selected occurrence");
    }
    Ok(HeldReview { snapshot })
}

impl HeldReview {
    /// File mode reviewed by the CLI.
    #[must_use]
    pub const fn mode(&self) -> u32 {
        self.snapshot.mode
    }

    /// Whether the review includes an explicit mode setting.
    #[must_use]
    pub const fn prepare_mode(&self) -> bool {
        self.snapshot.prepare_mode
    }

    /// Bind any observed Home tuple to the review.
    #[must_use]
    pub fn matches_home(&self, observation: &crate::protocol::StatusObservation) -> bool {
        match observation {
            crate::protocol::StatusObservation::Unavailable { .. } => true,
            crate::protocol::StatusObservation::Present { tuple } => {
                tuple.kind == "file"
                    && tuple.hash == self.snapshot.hash
                    && tuple.mode == format!("{:04o}", self.snapshot.mode)
            }
            _ => false,
        }
    }

    /// Encode the fixed execute operation with the retained snapshot.
    ///
    /// # Errors
    /// Returns an error if the request cannot be encoded.
    pub fn execute_request(&self) -> Result<Vec<u8>, serde_json::Error> {
        #[derive(Serialize)]
        struct Request<'a> {
            schema_version: u32,
            operation: &'static str,
            key: &'a str,
            snapshot: &'a Snapshot,
            #[serde(skip_serializing_if = "Option::is_none")]
            occurrence: Option<&'a serde_json::Value>,
        }

        let operation = if self.snapshot.action == "held" {
            "execute"
        } else {
            "execute_plain"
        };
        serde_json::to_vec(&Request {
            schema_version: 1,
            operation,
            key: &self.snapshot.key,
            snapshot: &self.snapshot,
            occurrence: self.snapshot.occurrence.as_ref(),
        })
    }
}

/// Validate the successful fixed execute receipt.
///
/// # Errors
/// Refuses invalid or uncertain receipts.
pub fn executed(
    bytes: &[u8],
    status: Option<i32>,
    key: &str,
    plain: bool,
) -> Result<(), &'static str> {
    let (first, _) = records(bytes, status)?;
    let result: Executed = serde_json::from_slice(first).map_err(|_| "Invalid remedy result")?;
    if result.schema_version != 1
        || result.record != "remedy_result"
        || result.operation != if plain { "execute_plain" } else { "execute" }
        || result.result != "captured"
        || result.key != key
    {
        return Err("Remedy result differs from the request");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    const DONE: &str = r#"{"record":"result","outcome":"done","exit_code":0,"error_kind":null}"#;
    fn input() -> String {
        format!(
            "{}\n{DONE}\n",
            serde_json::json!({"schema_version":1,"record":"remedy_review","operation":"review","executable":true,"reason":"binary","repo_id":"id","key":"data","home":{"kind":"file","hash":"00000000000000000000000000000000","mode":"0644"},"snapshot":{"action":"held","occurrence":null,"key":"data","repo_id":"id","reason":"binary","hash":"00000000000000000000000000000000","content_oid":"oid","mode":420,"prepare_mode":false,"config_oid":"oid","config_mode":420,"context":"context"}})
        )
    }
    #[test]
    fn review_binds_occurrence_and_rejects_invalid_receipts() {
        let input = input();
        assert!(review(input.as_bytes(), Some(0), "data", "id", "binary").is_ok());
        assert!(review(input.as_bytes(), Some(0), "other", "id", "binary").is_err());
        assert!(review(input.as_bytes(), Some(0), "data", "id", "large").is_err());
        assert!(review(input.as_bytes(), Some(1), "data", "id", "binary").is_err());
        assert!(
            review(
                format!("{input}{DONE}\n").as_bytes(),
                Some(0),
                "data",
                "id",
                "binary"
            )
            .is_err()
        );
    }

    #[test]
    fn plaintext_review_requires_exact_occurrence_and_warning() -> Result<(), &'static str> {
        let occurrence =
            serde_json::json!({"scope":"new","source":null,"reason":"secret_name","finding":null});
        let mut record = serde_json::json!({"schema_version":1,"record":"remedy_review","operation":"review_plain","executable":true,"reason":"secret_name","key":".env","occurrence":occurrence,"plaintext_may_be_published":true,"prepare_mode":false,
            "snapshot":{"action":"allow_plain","occurrence":occurrence,"key":".env","repo_id":"id","reason":"secret_name","hash":"00000000000000000000000000000000","content_oid":"oid","mode":420,"prepare_mode":false,"config_oid":"oid","config_mode":420,"context":"context"}});
        let encode = |value: &serde_json::Value| format!("{value}\n{DONE}\n");
        assert!(review_plain(encode(&record).as_bytes(), Some(0), ".env", "id").is_ok());
        *record
            .get_mut("plaintext_may_be_published")
            .ok_or("missing warning")? = false.into();
        assert!(review_plain(encode(&record).as_bytes(), Some(0), ".env", "id").is_err());
        *record
            .get_mut("plaintext_may_be_published")
            .ok_or("missing warning")? = true.into();
        *record
            .pointer_mut("/occurrence/finding")
            .ok_or("missing finding")? =
            serde_json::json!({"rule":"age-secret-key","place":"content","line":1});
        assert!(review_plain(encode(&record).as_bytes(), Some(0), ".env", "id").is_err());
        Ok(())
    }

    #[test]
    #[ignore = "requires DOTMAN_TEST_CLI pointing to the built CLI"]
    fn cli_review_and_execute_interoperate() -> Result<(), Box<dyn std::error::Error>> {
        use std::io::Write;
        use std::os::unix::fs::PermissionsExt;
        use std::process::{Command, Stdio};
        struct Root(std::path::PathBuf);
        impl Drop for Root {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let root =
            Root(std::env::temp_dir().join(format!("dotman-held-test-{}", std::process::id())));
        std::fs::create_dir(&root.0)?;
        for dir in ["home", "config", "data", "state", "cache"] {
            std::fs::create_dir(root.0.join(dir))?;
        }
        std::fs::write(
            root.0.join("home/.gitconfig"),
            "[user]\nname = Test\nemail = test@example.invalid\n",
        )?;
        let cli = std::env::var_os("DOTMAN_TEST_CLI").ok_or("missing DOTMAN_TEST_CLI")?;
        let run = |args: &[&str],
                   input: Option<&[u8]>|
         -> Result<std::process::Output, Box<dyn std::error::Error>> {
            let mut command = Command::new(&cli);
            command
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", root.0.join("home"))
                .env("XDG_CONFIG_HOME", root.0.join("config"))
                .env("XDG_DATA_HOME", root.0.join("data"))
                .env("XDG_STATE_HOME", root.0.join("state"))
                .env("XDG_CACHE_HOME", root.0.join("cache"))
                .env("DOTMAN_NON_INTERACTIVE", "1")
                .current_dir(root.0.join("home"))
                .args(args)
                .stdin(if input.is_some() {
                    Stdio::piped()
                } else {
                    Stdio::null()
                })
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            let mut child = command.spawn()?;
            if let Some(input) = input {
                child
                    .stdin
                    .take()
                    .ok_or("missing stdin")?
                    .write_all(input)?;
            }
            Ok(child.wait_with_output()?)
        };
        assert!(run(&["init"], None)?.status.success());
        let dir = root.0.join("home/.config/app");
        std::fs::create_dir_all(&dir)?;
        std::fs::write(dir.join("seed"), b"seed")?;
        assert!(run(&["add", ".config/app"], None)?.status.success());
        std::fs::write(dir.join("data"), b"binary\0data")?;
        std::fs::set_permissions(dir.join("data"), std::fs::Permissions::from_mode(0o644))?;
        let status = run(&["status", "--json"], None)?;
        let crate::protocol::StatusReply::Report(status) =
            crate::protocol::parse_status(&status.stdout, status.status.code())?
        else {
            return Err("status failed".into());
        };
        let id = status.context.repo_id.ok_or("missing repo id")?.raw;
        let input = serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"operation":"review","key":".config/app/data"}),
        )?;
        let output = run(&["remedy", "--from", "-", "--json"], Some(&input))?;
        let held = review(
            &output.stdout,
            output.status.code(),
            ".config/app/data",
            &id,
            "binary",
        )?;
        assert!(!root.0.join("data/dotman/.config/app/data").exists());
        let request = held.execute_request()?;
        let output = run(&["remedy", "--from", "-", "--json"], Some(&request))?;
        executed(
            &output.stdout,
            output.status.code(),
            ".config/app/data",
            false,
        )?;
        assert_eq!(
            std::fs::read(root.0.join("data/dotman/.config/app/data"))?,
            b"binary\0data"
        );
        let repeated = run(&["remedy", "--from", "-", "--json"], Some(&request))?;
        assert!(!repeated.status.success());
        verify_plain(&run, &root.0, &id)?;
        verify_encrypt(&run, &root.0, &id)?;
        verify_managed(&run, &root.0)?;
        Ok(())
    }

    fn verify_plain(
        run: &impl Fn(
            &[&str],
            Option<&[u8]>,
        ) -> Result<std::process::Output, Box<dyn std::error::Error>>,
        root: &std::path::Path,
        id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let key = ".config/app/.env";
        let path = root.join("home").join(key);
        std::fs::write(&path, b"API_URL=https://example.invalid\n")?;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644))?;
        let occurrence =
            serde_json::json!({"scope":"new","source":null,"reason":"secret_name","finding":null});
        let input = serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"operation":"review_plain","key":key,"occurrence":occurrence}),
        )?;
        let output = run(&["remedy", "--from", "-", "--json"], Some(&input))?;
        let plain = review_plain(&output.stdout, output.status.code(), key, id)?;
        let request = plain.execute_request()?;
        let output = run(&["remedy", "--from", "-", "--json"], Some(&request))?;
        executed(&output.stdout, output.status.code(), key, true)?;
        assert_eq!(
            std::fs::read(root.join("data/dotman").join(key))?,
            b"API_URL=https://example.invalid\n"
        );
        Ok(())
    }

    fn verify_encrypt(
        run: &impl Fn(
            &[&str],
            Option<&[u8]>,
        ) -> Result<std::process::Output, Box<dyn std::error::Error>>,
        root: &std::path::Path,
        repo_id: &str,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        assert!(run(&["key", "init"], None)?.status.success());
        let key = ".config/app/token";
        let path = root.join("home").join(key);
        let credential = format!(
            "token={}\n",
            concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
        );
        std::fs::write(&path, &credential)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let occurrence = crate::held_encrypt::Occurrence {
            scope: "new".into(),
            source: None,
            reason: "content".into(),
            finding: Some(crate::held_encrypt::Finding {
                rule: "github-token".into(),
                place: "content".into(),
                line: 1,
            }),
        };
        let request = serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"operation":"review_encrypt","key":key,"occurrence":occurrence}),
        )?;
        let output = run(&["remedy", "--from", "-", "--json"], Some(&request))?;
        let review = crate::held_encrypt::HeldEncryption::parse(
            &output.stdout,
            output.status.code(),
            key,
            repo_id,
            &occurrence,
        )?;
        assert!(review.creates_record());
        assert!(!root.join("data/dotman").join(review.destination()).exists());
        let request = review.execute_request()?;
        std::fs::write(&path, b"changed\n")?;
        assert!(
            !run(&["remedy", "--from", "-", "--json"], Some(&request))?
                .status
                .success()
        );
        assert!(!root.join("data/dotman").join(review.destination()).exists());
        std::fs::write(&path, credential)?;
        let output = run(&["remedy", "--from", "-", "--json"], Some(&request))?;
        crate::held_encrypt::executed(&output.stdout, output.status.code(), key)?;
        assert!(
            root.join("data/dotman")
                .join(review.destination())
                .is_file()
        );
        assert!(
            !run(&["remedy", "--from", "-", "--json"], Some(&request))?
                .status
                .success()
        );
        Ok(())
    }

    fn verify_managed(
        run: &impl Fn(
            &[&str],
            Option<&[u8]>,
        ) -> Result<std::process::Output, Box<dyn std::error::Error>>,
        root: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        use std::os::unix::fs::PermissionsExt;
        let key = ".profile";
        let home = root.join("home").join(key);
        std::fs::write(&home, b"safe base\n")?;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o644))?;
        assert!(run(&["add", key], None)?.status.success());
        let repo = root.join("data/dotman");
        for args in [
            vec!["add", "--", key],
            vec!["commit", "-m", "managed fixture"],
        ] {
            let status = std::process::Command::new("/usr/bin/git")
                .env_clear()
                .env("PATH", "/usr/bin:/bin")
                .env("HOME", root.join("home"))
                .args(["-C"])
                .arg(&repo)
                .args(args)
                .status()?;
            assert!(status.success());
        }
        let secret = format!(
            "token={}\n",
            concat!("gh", "p_", "a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2a1B2")
        );
        std::fs::write(&home, &secret)?;
        std::fs::set_permissions(&home, std::fs::Permissions::from_mode(0o600))?;
        let status = run(&["status", "--json"], None)?;
        let crate::protocol::StatusReply::Report(status) =
            crate::protocol::parse_status(&status.stdout, status.status.code())?
        else {
            return Err("status failed".into());
        };
        let entry = status
            .entries
            .iter()
            .find(|entry| entry.status == "held-in-home" && entry.key.raw == key)
            .ok_or("missing held occurrence")?;
        let occurrence = crate::held_encrypt::Occurrence {
            scope: "home".into(),
            source: Some(key.into()),
            reason: "content".into(),
            finding: Some(crate::held_encrypt::Finding {
                rule: "github-token".into(),
                place: "content".into(),
                line: 1,
            }),
        };
        let request = serde_json::to_vec(
            &serde_json::json!({"schema_version":1,"operation":"review_encrypt","key":key,"occurrence":occurrence}),
        )?;
        let output = run(&["remedy", "--from", "-", "--json"], Some(&request))?;
        assert!(
            output.status.success(),
            "managed review failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        verify_bad_managed_reviews(&output, &status.context, entry, &occurrence)?;
        let review = crate::held_encrypt::HeldEncryption::parse_managed(
            &output.stdout,
            output.status.code(),
            &status.context,
            entry,
            &occurrence,
        )?;
        assert!(review.managed_effects().is_some());
        verify_managed_receipts(&review)?;
        let request = review.execute_request()?;
        std::fs::write(&home, b"changed\n")?;
        assert!(
            !run(&["remedy", "--from", "-", "--json"], Some(&request))?
                .status
                .success()
        );
        std::fs::write(&home, &secret)?;
        let output = run(&["remedy", "--from", "-", "--json"], Some(&request))?;
        assert!(matches!(
            review.execution(&output.stdout, output.status.code()),
            Ok(crate::held_encrypt::Execution::Complete { .. })
        ));
        assert_eq!(std::fs::read(&home)?, secret.as_bytes());
        assert!(
            root.join("data/dotman")
                .join(review.destination())
                .is_file()
        );
        assert!(!root.join("data/dotman").join(key).exists());
        Ok(())
    }

    fn verify_bad_managed_reviews(
        output: &std::process::Output,
        context: &crate::protocol::StatusContext,
        entry: &crate::protocol::StatusEntry,
        occurrence: &crate::held_encrypt::Occurrence,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let mut lines = output.stdout.split(|byte| *byte == b'\n');
        let original: serde_json::Value =
            serde_json::from_slice(lines.next().ok_or("missing review")?)?;
        let final_line = lines.next().ok_or("missing result")?;
        for change in ["missing", "source", "backup"] {
            let mut tampered = original.clone();
            let effects = tampered
                .get_mut("managed_consequences")
                .and_then(serde_json::Value::as_object_mut)
                .ok_or("missing effects")?;
            match change {
                "missing" => {
                    effects.remove("mode_change").ok_or("missing mode change")?;
                }
                "source" => {
                    *effects
                        .get_mut("selected_source")
                        .ok_or("missing source effect")? = "other".into();
                }
                _ => {
                    let effect = effects
                        .get_mut("creates_backup_directory")
                        .ok_or("missing backup effect")?;
                    *effect = (!effect.as_bool().ok_or("invalid backup effect")?).into();
                }
            }
            let mut bytes = serde_json::to_vec(&tampered)?;
            bytes.push(b'\n');
            bytes.extend_from_slice(final_line);
            bytes.push(b'\n');
            assert!(
                crate::held_encrypt::HeldEncryption::parse_managed(
                    &bytes,
                    output.status.code(),
                    context,
                    entry,
                    occurrence
                )
                .is_err()
            );
        }
        Ok(())
    }

    fn verify_managed_receipts(
        review: &crate::held_encrypt::HeldEncryption,
    ) -> Result<(), &'static str> {
        let mut partial = serde_json::json!({"schema_version":1,"record":"remedy_error","code":"partial",
            "message":"encryption partially completed; refresh status",
            "effects":{"record_created":false,"ciphertext_created":true,"state_needs_review":true,"backup_dir_created":false,"recovery_unavailable":false},
            "private_recovery":{"state_backup_run":"safe-123","file":"source"}});
        let finish = serde_json::json!({"record":"result","outcome":"error","exit_code":1,"error_kind":"other"});
        let encode = |first: &serde_json::Value| format!("{first}\n{finish}\n");
        let Ok(crate::held_encrypt::Execution::Partial(effects)) =
            review.execution(encode(&partial).as_bytes(), Some(1))
        else {
            unreachable!("verified partial");
        };
        assert!(effects.ciphertext_created && effects.state_needs_review);
        assert!(!effects.record_created && !effects.backup_dir_created);
        assert_eq!(
            effects
                .recovery
                .as_ref()
                .map(crate::held_encrypt::RecoveryHandle::as_str),
            Some("safe-123")
        );
        *partial
            .pointer_mut("/private_recovery/state_backup_run")
            .ok_or("missing backup run")? = "../escape".into();
        assert!(
            review
                .execution(encode(&partial).as_bytes(), Some(1))
                .is_err()
        );
        Ok(())
    }
}
