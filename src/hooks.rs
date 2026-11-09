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

/// Runs the hooks whose glob matches a key in `changed`, if this machine approves them. An
/// approved table also runs, once, the hooks for the keys an unapproved one held back.
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
    let json = canonical(&table)?;
    let mut state = ctx.state(&ctx.repo_id()?)?;
    let dropped = drop_unmanaged(ctx, &mut state.hooks.pending)?;
    let standing = approval(&json, &state);
    let recorded = standing == Standing::Approved;
    let mut approved = policy.approve || recorded;
    let due_now = due(&table, changed)?;
    let with_pending = due(
        &table,
        &changed.union(&state.hooks.pending).cloned().collect(),
    )?;
    // Approving would run this table for the pending keys, though it is not the one shown with
    // them.
    let swapped = !approved
        && state.hooks.pending_table.as_deref() != Some(json.as_str())
        && !due(&table, &state.hooks.pending)?.is_empty();
    if policy.dry_run {
        let would_ask = !approved && !policy.never_ask && prompt::interactive();
        let due = if approved { with_pending } else { due_now };
        for command in due {
            let command = output::escape_line(command);
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

    let asked = !approved && !due_now.is_empty() && !policy.never_ask && prompt::interactive();
    if asked {
        approved = ask(&table)?;
    }
    let file = ctx.paths.state_file();
    if approved && !recorded {
        state.hooks.approved = Some(json.clone());
        state.save(&file, false)?;
        if asked {
            output::context("approved the [hooks] table above on this machine");
        } else {
            show_table("approved this [hooks] table on this machine:", &table);
        }
    }
    if !approved {
        if due_now.is_empty() && !swapped {
            if dropped {
                state.save(&file, false)?;
            }
            return Ok(());
        }
        let held = matched(&table, changed)?;
        if dropped || swapped || !held.is_subset(&state.hooks.pending) {
            state.hooks.pending.extend(held);
            state.hooks.pending_table = Some(json);
            state.save(&file, false)?;
        }
        let why = if swapped {
            "the [hooks] table in .dotman.toml changed while hooks for paths changed earlier waited for approval, and the new table, below, is not approved on this machine. Read it; then"
        } else if standing == (Standing::Changed { old_format: true }) {
            "the approval format changed, so the approval an older dotman saved for the [hooks] table in .dotman.toml no longer counts. Read the table; then"
        } else {
            "the [hooks] table in .dotman.toml is not approved on this machine. Read it; then"
        };
        output::warning(&format!(
            "{} not run: {why} `{} apply --approve-hooks` approves it and runs {} once:",
            count(with_pending.len()),
            crate::name::bin(),
            if with_pending.len() == 1 {
                "it"
            } else {
                "them"
            }
        ));
        for command in &with_pending {
            output::warning(&format!("  {}", output::escape_line(command)));
        }
        if swapped {
            show_table("the new [hooks] table:", &table);
        }
        return Ok(());
    }

    let ran = run_all(ctx, &with_pending);
    if dropped || !state.hooks.pending.is_empty() || state.hooks.pending_table.is_some() {
        state.hooks.pending.clear();
        state.hooks.pending_table = None;
        state.save(&file, false)?;
    }
    ran
}

/// Drops the pending keys no longer managed, as one that upstream forgot, which no hook is run
/// for any more. Returns whether it dropped any.
fn drop_unmanaged(ctx: &Context, pending: &mut BTreeSet<String>) -> Result<bool> {
    if pending.is_empty() {
        return Ok(false);
    }
    let (home, system) = (ctx.sources()?, ctx.system_sources()?);
    let before = pending.len();
    pending.retain(|key| {
        key.strip_prefix('/')
            .map_or_else(|| home.contains(key), |rel| system.contains(rel))
    });
    Ok(pending.len() != before)
}

/// Runs each of `due`, and fails after the last when any of them failed.
fn run_all(ctx: &Context, due: &[&str]) -> Result<()> {
    let mut failed = 0_usize;
    for command in due {
        output::action("hook", command);
        let shown = output::escape_line(command);
        match spawn(ctx, command) {
            Ok(status) if status.success() => {}
            Ok(status) => {
                failed = failed.saturating_add(1);
                output::warning(&format!("hook `{shown}` failed: {status}"));
            }
            Err(e) => {
                failed = failed.saturating_add(1);
                output::warning(&format!("hook `{shown}` could not start: {e}"));
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
        let pattern = pattern(glob)?;
        if !commands.contains(&command.as_str())
            && changed.iter().any(|key| pattern.matches_with(key, MATCH))
        {
            commands.push(command);
        }
    }
    Ok(commands)
}

/// The keys of `changed` that a glob in `table` matches.
fn matched(
    table: &BTreeMap<String, String>,
    changed: &BTreeSet<String>,
) -> Result<BTreeSet<String>> {
    let patterns = table
        .keys()
        .map(|glob| pattern(glob))
        .collect::<Result<Vec<_>>>()?;
    Ok(changed
        .iter()
        .filter(|key| patterns.iter().any(|p| p.matches_with(key, MATCH)))
        .cloned()
        .collect())
}

fn pattern(glob: &str) -> Result<Pattern> {
    Pattern::new(glob).with_context(|| format!("bad [hooks] glob `{glob}` in .dotman.toml"))
}

/// Fails, naming the entry and the character, when a glob holds a control character other than
/// tab, or a command one other than tab and newline. A carriage return or an escape sequence could
/// make the table read as other commands than the ones that run; `sh` runs a multi-line command,
/// and the table shows its newlines escaped.
pub fn validate(table: &BTreeMap<String, String>) -> Result<()> {
    for (glob, command) in table {
        let refused = glob
            .chars()
            .find(|c| c.is_control() && *c != '\t')
            .or_else(|| {
                command
                    .chars()
                    .find(|c| c.is_control() && !matches!(c, '\t' | '\n'))
            });
        if let Some(c) = refused {
            bail!(
                "the [hooks] entry `{}` = `{}` in .dotman.toml holds {}, so no hook runs and none is approved; remove it, or move the command into a script",
                output::escape_line(glob),
                output::escape_line(command),
                char_name(c)
            );
        }
    }
    Ok(())
}

/// Such as `U+000D carriage return`.
fn char_name(c: char) -> String {
    let name = match c {
        '\0' => "null",
        '\u{7}' => "bell",
        '\u{8}' => "backspace",
        '\t' => "tab",
        '\n' => "line feed",
        '\u{b}' => "vertical tab",
        '\u{c}' => "form feed",
        '\r' => "carriage return",
        '\u{1b}' => "escape",
        '\u{7f}' => "delete",
        '\u{85}' => "next line",
        '\u{9b}' => "control sequence introducer",
        _ => "control character",
    };
    format!("U+{:04X} {name}", u32::from(c))
}

/// The table as compact JSON of the ordered map, which `state.toml` keeps as the approval and
/// compares byte for byte. TOML spacing and entry order do not change it, and JSON quotes every
/// glob and command, so two tables never share it.
fn canonical(table: &BTreeMap<String, String>) -> Result<String> {
    serde_json::to_string(table).context("cannot encode the [hooks] table")
}

/// Where this machine stands on the repo's `[hooks]` table.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Standing {
    Empty,
    Approved,
    Unapproved,
    /// This machine approved another table; `old_format` when the approval is a hash, the form an
    /// older dotman saved.
    Changed {
        old_format: bool,
    },
}

/// Compares `table` with the approval that `state` holds; fails as [`validate`] does.
pub fn standing(table: &BTreeMap<String, String>, state: &State) -> Result<Standing> {
    if table.is_empty() {
        return Ok(Standing::Empty);
    }
    validate(table)?;
    Ok(approval(&canonical(table)?, state))
}

fn approval(json: &str, state: &State) -> Standing {
    match state.hooks.approved.as_deref() {
        Some(approved) if approved == json => Standing::Approved,
        // A canonical table is a JSON object, and an older dotman saved a hex hash.
        Some(approved) => Standing::Changed {
            old_format: !approved.starts_with('{'),
        },
        None => Standing::Unapproved,
    }
}

fn show_table(header: &str, table: &BTreeMap<String, String>) {
    output::context(header);
    for (glob, command) in table {
        output::context(&format!(
            "  {} -> {}",
            output::escape_line(glob),
            output::escape_line(command)
        ));
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

    fn json(entries: &[(&str, &str)]) -> String {
        canonical(&table(entries)).unwrap_or_default()
    }

    #[test]
    fn the_approval_is_the_ordered_table_and_nothing_else() {
        let one = json(&[("a", "x"), ("b", "y")]);
        assert_eq!(one, r#"{"a":"x","b":"y"}"#);
        assert_eq!(one, json(&[("b", "y"), ("a", "x")]));
        assert_ne!(one, json(&[("a", "x"), ("b", "z")]));
    }

    #[test]
    fn tables_that_shared_the_old_nul_joined_bytes_are_told_apart() {
        let one = [(".config/app/**", "true\0z*\0touch /tmp/PWNED")];
        let two = [(".config/app/**", "true"), ("z*", "touch /tmp/PWNED")];
        let joined = |entries: &[(&str, &str)]| {
            entries
                .iter()
                .flat_map(|(glob, command)| [*glob, *command])
                .fold(String::new(), |all, part| all + part + "\0")
        };
        assert_eq!(joined(&one), joined(&two), "the pair the old hash confused");
        assert_ne!(json(&one), json(&two));
    }

    #[test]
    fn control_characters_are_refused_by_name_and_tabs_and_command_newlines_are_not() {
        assert!(validate(&table(&[("a/*", "make\t-C x\nmake install")])).is_ok());
        for (glob, command, named) in [
            ("a", "true\0touch x", "U+0000 null"),
            (
                "a",
                "touch x #\r\u{1b}[2K  a -> echo ok",
                "U+000D carriage return",
            ),
            ("a\nb", "true", "U+000A line feed"),
            ("a\u{1b}]0;t\u{7}", "true", "U+001B escape"),
            ("a", "x\u{9b}2K", "U+009B control sequence introducer"),
            ("a", "x\u{7f}", "U+007F delete"),
            ("a", "x\u{1}", "U+0001 control character"),
        ] {
            let err = validate(&table(&[("b", "true"), (glob, command)]))
                .err()
                .map(|e| format!("{e:#}"))
                .unwrap_or_default();
            assert!(err.contains(&format!("holds {named},")), "{glob:?}: {err}");
            let entry = format!(
                "`{}` = `{}`",
                output::escape_line(glob),
                output::escape_line(command)
            );
            assert!(err.contains(&entry), "the entry is named: {err}");
            assert!(!err.contains('\n'), "on one line: {err:?}");
        }
    }

    #[test]
    fn only_the_exact_table_is_approved_and_an_old_hash_reads_as_changed() {
        let entries = table(&[("a", "x")]);
        let mut state = State::default();
        assert_eq!(
            standing(&BTreeMap::new(), &state).ok(),
            Some(Standing::Empty)
        );
        assert_eq!(standing(&entries, &state).ok(), Some(Standing::Unapproved));
        // The xxh3-128 hex that the NUL-joined hash gave.
        state.hooks.approved = Some("55c7a64d8992c4767b4d28afab1811bd".to_owned());
        let old = Standing::Changed { old_format: true };
        assert_eq!(standing(&entries, &state).ok(), Some(old));
        state.hooks.approved = Some(json(&[("a", "x "), ("b", "y")]));
        let other = Standing::Changed { old_format: false };
        assert_eq!(standing(&entries, &state).ok(), Some(other));
        state.hooks.approved = Some(json(&[("a", "x")]));
        assert_eq!(standing(&entries, &state).ok(), Some(Standing::Approved));
        assert!(standing(&table(&[("a", "x\ry")]), &state).is_err());
    }
}
