//! Post-apply hooks: `[hooks]` in `.dotman.toml` maps a glob over keys to a shell command, run
//! once after a command changed a matching path. A machine runs a table only after approving it.

use crate::commands::add::MATCH;
use crate::context::Context;
use crate::output;
use crate::proc;
use crate::prompt;
use crate::state::State;
use anyhow::{Context as _, Result, bail};
use glob::Pattern;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::process::{ExitStatus, Stdio};

// reason: independent switches, not a state machine
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone, Copy)]
pub struct Policy {
    pub approve: bool,
    pub no_hooks: bool,
    pub dry_run: bool,
    /// `dotman init <url>` never prompts: nobody has read the cloned table yet.
    pub never_ask: bool,
}

const RISK: &str = "Approval covers these command lines, not the scripts they call: a managed script that a later sync changes runs in its new version.";

/// Runs the hooks whose glob matches a key in `changed`, if this machine approves them.
/// Fails after running every hook when any of them failed.
pub fn run(ctx: &Context, changed: &BTreeSet<String>, policy: Policy) -> Result<()> {
    if policy.no_hooks {
        return Ok(());
    }
    let table = ctx.repo_config()?.hooks;
    if table.is_empty() {
        return Ok(());
    }
    validate(&table)?;
    let due = due(&table, changed)?;
    let hash = table_hash(&table)?;
    let mut state = ctx.state(&ctx.repo_id()?)?;
    let recorded = state.hooks.approved.as_deref() == Some(hash.as_str());
    let mut approved = policy.approve || recorded;
    if policy.dry_run {
        let would_ask = !approved && !policy.never_ask && prompt::interactive();
        for command in due {
            output::info(&if approved {
                format!("would run hook: {command}")
            } else if would_ask {
                format!("would ask before running hook: {command}")
            } else {
                format!("hook not approved on this machine, would be skipped: {command}")
            });
        }
        return Ok(());
    }

    let asked = !approved && !due.is_empty() && !policy.never_ask && prompt::interactive();
    if asked {
        approved = ask(&table)?;
    }
    if approved && !recorded {
        state.hooks.approved = Some(hash);
        state.save(&ctx.paths.state_file(), false)?;
        if asked {
            output::context("approved the [hooks] table above on this machine");
        } else {
            show_table("approved this [hooks] table on this machine:", &table);
        }
    }
    if due.is_empty() {
        return Ok(());
    }
    if !approved {
        output::warning(&format!(
            "{} not run: the [hooks] table in .dotman.toml is not approved on this machine. Read it, then run again with --approve-hooks:",
            count(due.len())
        ));
        for command in due {
            output::warning(&format!("  {command}"));
        }
        return Ok(());
    }

    let mut failed = 0_usize;
    for command in due {
        output::action("hook", command);
        match spawn(ctx, command) {
            Ok(status) if status.success() => {}
            Ok(status) => {
                failed = failed.saturating_add(1);
                output::warning(&format!("hook `{command}` failed: {status}"));
            }
            Err(e) => {
                failed = failed.saturating_add(1);
                output::warning(&format!("hook `{command}` could not start: {e}"));
            }
        }
    }
    if failed > 0 {
        bail!("{} failed", count(failed));
    }
    Ok(())
}

fn count(n: usize) -> String {
    format!("{n} {}", if n == 1 { "hook" } else { "hooks" })
}

/// The commands to run, each once, in the table's key order.
fn due<'a>(
    table: &'a BTreeMap<String, String>,
    changed: &BTreeSet<String>,
) -> Result<Vec<&'a str>> {
    let mut commands: Vec<&str> = Vec::new();
    for (glob, command) in table {
        let pattern = Pattern::new(glob)
            .with_context(|| format!("bad [hooks] glob `{glob}` in .dotman.toml"))?;
        if !commands.contains(&command.as_str())
            && changed.iter().any(|key| pattern.matches_with(key, MATCH))
        {
            commands.push(command);
        }
    }
    Ok(commands)
}

/// Fails, naming the entry, when a glob or command holds a control character other than tab. A
/// carriage return or an escape sequence could make the table read as other commands than the
/// ones that run.
pub fn validate(table: &BTreeMap<String, String>) -> Result<()> {
    let control = |text: &str| text.chars().any(|c| c.is_control() && c != '\t');
    if let Some((glob, command)) = table
        .iter()
        .find(|(glob, command)| control(glob) || control(command))
    {
        bail!(
            "the [hooks] entry `{}` = `{}` in .dotman.toml holds a control character, so no hook runs and none is approved; write the command on one line, or move it into a script",
            output::escape(glob),
            output::escape(command)
        );
    }
    Ok(())
}

/// SHA-256 of the table as JSON. The parsed map is hashed, so TOML spacing and entry order do not
/// change it; JSON quotes every glob and command, so no two tables share their bytes.
fn table_hash(table: &BTreeMap<String, String>) -> Result<String> {
    let bytes = serde_json::to_vec(table).context("cannot encode the [hooks] table")?;
    Ok(format!("{:x}", Sha256::digest(&bytes)))
}

/// Where this machine stands on the repo's `[hooks]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Empty,
    Approved,
    Unapproved,
    /// This machine approved another table, or this one under the hash an older dotman used.
    Changed,
}

/// Compares `table` with the approval that `state` holds; fails as [`validate`] does.
pub fn standing(table: &BTreeMap<String, String>, state: &State) -> Result<Standing> {
    if table.is_empty() {
        return Ok(Standing::Empty);
    }
    validate(table)?;
    let hash = table_hash(table)?;
    Ok(match state.hooks.approved.as_deref() {
        Some(approved) if approved == hash => Standing::Approved,
        Some(_) => Standing::Changed,
        None => Standing::Unapproved,
    })
}

fn show_table(header: &str, table: &BTreeMap<String, String>) {
    output::context(header);
    for (glob, command) in table {
        output::context(&format!("  {glob} -> {command}"));
    }
}

fn ask(table: &BTreeMap<String, String>) -> Result<bool> {
    show_table(&format!("{} in .dotman.toml:", count(table.len())), table);
    output::context(RISK);
    prompt::confirm("run the matching hooks and approve this table on this machine? [y/N]")
}

/// The one place a hook process starts.
fn spawn(ctx: &Context, command: &str) -> io::Result<ExitStatus> {
    proc::command("/bin/sh")
        .arg("-c")
        .arg(command)
        .current_dir(&ctx.paths.home)
        .stdin(if prompt::interactive() {
            Stdio::inherit()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::from(io::stderr()))
        .status()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table(entries: &[(&str, &str)]) -> BTreeMap<String, String> {
        entries
            .iter()
            .map(|(g, c)| ((*g).to_owned(), (*c).to_owned()))
            .collect()
    }

    fn keys(keys: &[&str]) -> BTreeSet<String> {
        keys.iter().map(|k| (*k).to_owned()).collect()
    }

    #[test]
    fn a_command_matched_by_several_globs_and_paths_is_due_once() {
        let table = table(&[
            ("a/*", "reload"),
            ("a/**", "reload"),
            ("b", "other"),
            ("z", "never"),
        ]);
        let due = due(&table, &keys(&["a/x", "a/y/z", "b"])).unwrap_or_default();
        assert_eq!(due, ["reload", "other"]);
    }

    #[test]
    fn a_bad_glob_is_named() {
        let err = due(&table(&[("a[", "x")]), &keys(&[]))
            .err()
            .map(|e| format!("{e:#}"))
            .unwrap_or_default();
        assert!(err.contains("`a[`"), "{err}");
    }

    fn hash(entries: &[(&str, &str)]) -> String {
        table_hash(&table(entries)).unwrap_or_default()
    }

    #[test]
    fn the_hash_follows_the_entries_and_nothing_else() {
        let one = hash(&[("a", "x"), ("b", "y")]);
        assert_eq!(one.len(), 64, "{one}");
        assert_eq!(one, hash(&[("b", "y"), ("a", "x")]));
        assert_ne!(one, hash(&[("a", "x"), ("b", "z")]));
    }

    #[test]
    fn tables_that_shared_the_old_nul_joined_bytes_hash_apart() {
        let one = [(".config/app/**", "true\0z*\0touch /tmp/PWNED")];
        let two = [(".config/app/**", "true"), ("z*", "touch /tmp/PWNED")];
        let joined = |entries: &[(&str, &str)]| {
            entries
                .iter()
                .flat_map(|(glob, command)| [*glob, *command])
                .fold(String::new(), |all, part| all + part + "\0")
        };
        assert_eq!(joined(&one), joined(&two), "the pair the old hash confused");
        assert_ne!(hash(&one), hash(&two));
    }

    #[test]
    fn control_characters_are_refused_and_tabs_are_not() {
        assert!(validate(&table(&[("a/*", "make\t-C x")])).is_ok());
        for (glob, command) in [
            ("a", "true\0touch x"),
            ("a", "touch x #\r\u{1b}[2K  a -> echo ok"),
            ("a", "one\ntwo"),
            ("a\u{1b}]0;t\u{7}", "true"),
            ("a", "x\u{9b}2K"),
        ] {
            let err = validate(&table(&[("b", "true"), (glob, command)]))
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default();
            assert!(err.contains("control character"), "{glob:?}: {err}");
            let named = format!("`{}` = `{}`", output::escape(glob), output::escape(command));
            assert!(err.contains(&named), "the entry is named: {err}");
        }
    }

    #[test]
    fn the_standing_tells_an_old_or_other_approval_from_none() {
        let entries = table(&[("a", "x")]);
        let mut state = State::default();
        assert_eq!(
            standing(&BTreeMap::new(), &state).ok(),
            Some(Standing::Empty)
        );
        assert_eq!(standing(&entries, &state).ok(), Some(Standing::Unapproved));
        // The xxh3-128 hex that the NUL-joined hash gave.
        state.hooks.approved = Some("55c7a64d8992c4767b4d28afab1811bd".to_owned());
        assert_eq!(standing(&entries, &state).ok(), Some(Standing::Changed));
        state.hooks.approved = Some(hash(&[("a", "x")]));
        assert_eq!(standing(&entries, &state).ok(), Some(Standing::Approved));
        assert!(standing(&table(&[("a", "x\ry")]), &state).is_err());
    }
}
