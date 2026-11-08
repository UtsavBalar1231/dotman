//! Post-apply hooks: `[hooks]` in `.dotman.toml` maps a glob over keys to a shell command, run
//! once after a command changed a matching path. A machine runs a table only after approving it.

use crate::commands::add::MATCH;
use crate::context::Context;
use crate::engine::hash_hex;
use crate::output;
use crate::proc;
use crate::prompt;
use anyhow::{Context as _, Result, bail};
use glob::Pattern;
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::process::{ExitStatus, Stdio};

// reason: independent switches, not a state machine
#[allow(clippy::struct_excessive_bools)]
#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct Policy {
    pub approve: bool,
    pub no_hooks: bool,
    pub dry_run: bool,
    /// `dotman init <url>` never prompts: nobody has read the cloned table yet.
    pub never_ask: bool,
}

const RISK: &str = "Approval covers these command lines, not the scripts they call: a managed script that a later sync changes runs in its new version.";

/// Runs the hooks whose glob matches a key in `changed`, if this machine approves them.
/// Fails after running every hook when any of them failed.
pub(crate) fn run(ctx: &Context, changed: &BTreeSet<String>, policy: Policy) -> Result<()> {
    if policy.no_hooks {
        return Ok(());
    }
    let table = ctx.repo_config()?.hooks;
    if table.is_empty() {
        return Ok(());
    }
    let due = due(&table, changed)?;
    let hash = table_hash(&table);
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

    if !approved && !due.is_empty() && !policy.never_ask && prompt::interactive() {
        approved = ask(&table)?;
    }
    if approved && !recorded {
        state.hooks.approved = Some(hash);
        state.save(&ctx.paths.state_file(), false)?;
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

/// Hashes the parsed table, so TOML spacing and entry order do not change it.
fn table_hash(table: &BTreeMap<String, String>) -> String {
    let mut bytes = Vec::new();
    for (glob, command) in table {
        bytes.extend_from_slice(glob.as_bytes());
        bytes.push(0);
        bytes.extend_from_slice(command.as_bytes());
        bytes.push(0);
    }
    hash_hex(&bytes)
}

fn ask(table: &BTreeMap<String, String>) -> Result<bool> {
    output::info(&format!("{} in .dotman.toml:", count(table.len())));
    for (glob, command) in table {
        output::info(&format!("  {glob} -> {command}"));
    }
    output::info(RISK);
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

    #[test]
    fn the_hash_follows_the_entries_and_nothing_else() {
        let one = table_hash(&table(&[("a", "x"), ("b", "y")]));
        assert_eq!(one, table_hash(&table(&[("b", "y"), ("a", "x")])));
        assert_ne!(one, table_hash(&table(&[("a", "x"), ("b", "z")])));
        assert_ne!(
            table_hash(&table(&[("a", "xb\0y")])),
            table_hash(&table(&[("a", "x"), ("b", "y")]))
        );
    }
}
