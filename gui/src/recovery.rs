use crate::protocol::{RecoveryArgument, RecoveryStep};

const LIMIT: usize = 1024 * 1024;
const MAX_ITEMS: usize = 4096;

/// One display-only step; render strings as text, and use shell text only in a user-controlled terminal.
#[derive(Debug, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RecoveryRow {
    /// Manual instruction.
    Instruction { text: String },
    /// One command, separate from other steps and alternatives.
    Command {
        /// Escaped condition, never part of the command.
        condition: Option<String>,
        /// POSIX shell text, not an execution or clipboard request.
        shell: String,
    },
}

fn add(out: &mut String, value: &str) -> Result<(), &'static str> {
    if value.len() > LIMIT.saturating_sub(out.len()) {
        return Err("recovery display exceeds size limit");
    }
    out.push_str(value);
    Ok(())
}

fn display(value: &str) -> Result<String, &'static str> {
    let mut out = String::new();
    for c in value.chars() {
        if c.is_control()
            || matches!(c, '\u{ad}' | '\u{61c}' | '\u{180e}' | '\u{200b}' | '\u{200e}' | '\u{200f}' | '\u{2028}'..='\u{202e}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{206f}' | '\u{feff}' | '\u{fff9}'..='\u{fffb}' | '\u{e0001}' | '\u{e0020}'..='\u{e007f}')
        {
            add(&mut out, &c.escape_unicode().to_string())?;
        } else {
            let mut buf = [0; 4];
            add(&mut out, c.encode_utf8(&mut buf))?;
        }
    }
    Ok(out)
}

fn bytes(arg: &RecoveryArgument) -> &[u8] {
    match arg {
        RecoveryArgument::Text(text) => text.as_bytes(),
        RecoveryArgument::Bytes { bytes } => bytes,
    }
}

fn command(argv: &[RecoveryArgument]) -> Result<String, &'static str> {
    if argv.len() > MAX_ITEMS || argv.first().is_none_or(|arg| bytes(arg).is_empty()) {
        return Err("invalid recovery command");
    }
    let mut setup = String::new();
    let mut words = String::new();
    for (index, arg) in argv.iter().enumerate() {
        let arg = bytes(arg);
        if arg.contains(&0) || arg.len() > LIMIT {
            return Err("invalid recovery argument");
        }
        if index != 0 {
            add(&mut words, " ")?;
        }
        if arg.iter().all(|b| (0x20..=0x7e).contains(b)) {
            add(&mut words, "'")?;
            for &b in arg {
                if b == b'\'' {
                    add(&mut words, "'\\''")?;
                } else {
                    add(&mut words, &(b as char).to_string())?;
                }
            }
            add(&mut words, "'")?;
        } else {
            add(&mut setup, &format!("arg_{index}=$(printf '%b.' '"))?;
            for b in arg {
                add(&mut setup, &format!("\\0{b:03o}"))?;
            }
            add(&mut setup, "') || exit; ")?;
            add(&mut words, &format!("\"${{arg_{index}%.}}\""))?;
        }
    }
    if setup.is_empty() {
        return Ok(words);
    }
    let mut shell = String::new();
    add(&mut shell, "( ")?;
    add(&mut shell, &setup)?;
    add(&mut shell, &words)?;
    add(&mut shell, " )")?;
    Ok(shell)
}

/// Projects ordered CLI recovery data without executing it.
///
/// # Errors
/// Rejects missing, invalid or oversized data without producing partial rows.
pub fn project(steps: &[RecoveryStep]) -> Result<Vec<RecoveryRow>, &'static str> {
    if steps.is_empty() || steps.len() > MAX_ITEMS {
        return Err("invalid recovery steps");
    }
    let mut size = 0usize;
    let mut input_size = 0usize;
    let mut rows = Vec::new();
    for step in steps {
        let mut charge = |n: usize| -> Result<(), &'static str> {
            input_size = input_size
                .checked_add(n)
                .ok_or("recovery input exceeds size limit")?;
            if input_size > LIMIT {
                return Err("recovery input exceeds size limit");
            }
            Ok(())
        };
        let row = match step {
            RecoveryStep::Instruction { text } => {
                charge(text.raw.len())?;
                charge(text.shown.len())?;
                RecoveryRow::Instruction {
                    text: display(&text.raw)?,
                }
            }
            RecoveryStep::Command { argv, shown, when } => {
                if argv.len() != shown.len() || argv.len() > MAX_ITEMS {
                    return Err("invalid recovery command");
                }
                for arg in argv {
                    charge(bytes(arg).len())?;
                }
                for value in shown {
                    charge(value.len())?;
                }
                if let Some(condition) = when {
                    charge(condition.len())?;
                }
                RecoveryRow::Command {
                    condition: when.as_deref().map(display).transpose()?,
                    shell: command(argv)?,
                }
            }
        };
        let n = match &row {
            RecoveryRow::Instruction { text } => text.len(),
            RecoveryRow::Command { condition, shell } => shell
                .len()
                .saturating_add(condition.as_ref().map_or(0, String::len)),
        };
        size = size
            .checked_add(n)
            .ok_or("recovery display exceeds size limit")?;
        if size > LIMIT {
            return Err("recovery display exceeds size limit");
        }
        rows.push(row);
    }
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::{LIMIT, RecoveryRow, project};
    use crate::protocol::{RecoveryArgument as Arg, RecoveryStep as Step, RecoveryText};
    use std::os::unix::ffi::{OsStrExt, OsStringExt};
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct TestDir(PathBuf);

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn root() -> std::io::Result<TestDir> {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "dotman-recovery-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir(&path)?;
        let root = TestDir(path);
        std::fs::set_permissions(&root.0, std::fs::Permissions::from_mode(0o700))?;
        Ok(root)
    }

    fn step(argv: Vec<Arg>, when: Option<&str>) -> Step {
        Step::Command {
            shown: argv.iter().map(|_| "untrusted".to_owned()).collect(),
            argv,
            when: when.map(str::to_owned),
        }
    }

    #[test]
    fn shell_preserves_exact_arguments() -> Result<(), Box<dyn std::error::Error>> {
        let root = root()?;
        let helper = root
            .0
            .join(std::ffi::OsString::from_vec(b"no-op\xff\n".to_vec()));
        std::fs::write(&helper, b"#!/bin/sh\nprintf '%s\\0' \"$@\"\n")?;
        std::fs::set_permissions(&helper, std::fs::Permissions::from_mode(0o700))?;
        let expected_args: Vec<Vec<u8>> = vec![
            b"apostrophe' and \\backslash".to_vec(),
            b"$(echo injected); `echo x` %".to_vec(),
            b"--".to_vec(),
            b":(literal)-*[x]".to_vec(),
            Vec::new(),
            b"\xff\n\n".to_vec(),
            b"line\ninside".to_vec(),
        ];
        let mut argv = vec![Arg::Bytes {
            bytes: helper.as_os_str().as_bytes().to_vec(),
        }];
        argv.extend(
            expected_args
                .iter()
                .cloned()
                .map(|bytes| Arg::Bytes { bytes }),
        );
        let rows = project(&[step(argv, None)])?;
        let Some(RecoveryRow::Command { shell, .. }) = rows.first() else {
            return Err("missing command".into());
        };
        let output = Command::new("/bin/sh").arg("-c").arg(shell).output()?;
        assert!(output.status.success());
        let expected: Vec<u8> = expected_args
            .into_iter()
            .flat_map(|mut arg| {
                arg.push(0);
                arg
            })
            .collect();
        assert_eq!(output.stdout, expected);
        Ok(())
    }

    #[test]
    fn preserves_order_and_escapes_text_without_interpolation() -> Result<(), &'static str> {
        let steps = vec![
            Step::Instruction {
                text: RecoveryText {
                    raw: "<b>\u{202e}\n".into(),
                    shown: "wrong".into(),
                },
            },
            step(
                vec![Arg::Text("dotman".into()), Arg::Text("sync".into())],
                Some("after $(x)\u{1b}"),
            ),
            step(
                vec![Arg::Text("dotman".into()), Arg::Text("--abort".into())],
                Some("instead"),
            ),
        ];
        let rows = project(&steps)?;
        assert!(
            matches!(rows.first(), Some(RecoveryRow::Instruction { text }) if text == "<b>\\u{202e}\\u{a}")
        );
        assert!(
            matches!(rows.get(1), Some(RecoveryRow::Command { condition: Some(condition), shell }) if condition == "after $(x)\\u{1b}" && shell == "'dotman' 'sync'")
        );
        assert!(
            matches!(rows.get(2), Some(RecoveryRow::Command { condition: Some(condition), shell }) if condition == "instead" && shell == "'dotman' '--abort'")
        );
        Ok(())
    }

    #[test]
    fn rejects_invalid_and_oversized_data() {
        assert!(project(&[]).is_err());
        assert!(project(&[step(vec![], None)]).is_err());
        assert!(project(&[step(vec![Arg::Text(String::new())], None)]).is_err());
        assert!(project(&[step(vec![Arg::Text("x\0y".into())], None)]).is_err());
        assert!(project(&[step(vec![Arg::Text("x".repeat(LIMIT))], None)]).is_err());
        assert!(
            project(&[step(
                vec![
                    Arg::Text("x".into()),
                    Arg::Bytes {
                        bytes: vec![255; LIMIT / 4]
                    }
                ],
                None
            )])
            .is_err()
        );
        let mismatched = Step::Command {
            argv: vec![Arg::Text("x".into())],
            shown: vec![],
            when: None,
        };
        assert!(project(&[mismatched]).is_err());
    }
}
