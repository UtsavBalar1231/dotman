//! `dotman diff`: how `$HOME` differs from the repo, shown by `git diff --no-index` from the repo
//! to `$HOME`, so an edit made in `$HOME` shows as added lines.
//!
//! The comparison runs on copies in a private temp tree, so the user's pager, colours and external
//! diff tool never see a path inside `$HOME` or the repo.

use crate::cli::DiffArgs;
use crate::context::{Context, TEMP_PREFIX};
use crate::engine::status::{Content, Source, SourceEntry, Variants, Worktree};
use crate::engine::{Home, Node, Tuple, portable_link_target, read_node};
use crate::git::Git;
use crate::output;
use crate::paths::{create_private_dir, private_temp_dir};
use crate::proc;
use crate::secrets::Locked;
use crate::sys;
use crate::system::{self, Verdict};
use crate::variant::Selection;
use anyhow::{Context as _, Result, bail};
use libc::{SIGHUP, SIGINT, SIGQUIT, SIGTERM};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs::{self, Permissions};
use std::io::IsTerminal;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::os::unix::process::ExitStatusExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// How long `git config` or `git var` may take to name the pager.
const PAGER_PROBE: Duration = Duration::from_secs(5);

/// Runs `dotman diff`. Takes no lock and writes nothing outside its temp tree; differences are not an error.
///
/// # Errors
///
/// Fails when the repo is not initialised or has a rebase or merge in progress, an input cannot be
/// resolved to a key, or a file cannot be rendered for comparison.
pub fn run(args: &DiffArgs, no_pager: bool) -> Result<()> {
    let ctx = Context::resolve("diff")?;
    let repo_id = ctx.guard().and_then(|()| ctx.repo_id());
    if args.json {
        crate::commands::status::write_context(&ctx, "diff", repo_id.as_deref().ok())?;
    }
    let repo_id = repo_id?;
    if args.json {
        return json_diff(&ctx, args, &repo_id);
    }
    let repo_config = ctx.repo_config()?;
    let (home_paths, system_filters) = system::split_inputs(&ctx.paths, &args.paths)?;
    let sources = ctx.sources()?;
    let keys = if args.paths.is_empty() || !home_paths.is_empty() {
        ctx.select(&home_paths, sources.keys().map(str::to_owned).collect())?
    } else {
        BTreeSet::new()
    };
    let worktree = Worktree::new(&ctx.paths.repo, &repo_config.modes);
    let variants = Variants::new(
        worktree,
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );
    let mut entries = Vec::new();
    for key in keys {
        match variants.get(&key) {
            Ok(Selection::Winner(r)) => entries.push((key, r)),
            Ok(Selection::None) => {}
            Err(e) if e.is::<Locked>() => output::warning(&format!("{key}: locked, not compared")),
            Err(e) => output::warning(&format!("{key}: not compared: {e:#}")),
            Ok(Selection::Ambiguous(_)) => {
                output::warning(&format!("{key}: ambiguous, not compared"));
            }
            Ok(Selection::Invalid(_)) => {
                output::warning(&format!("{key}: has an invalid variant, not compared"));
            }
        }
    }
    let state = ctx.state(&ctx.repo_id()?)?;
    let mut system_entries = Vec::new();
    if args.paths.is_empty() || !system_filters.is_empty() {
        let found = system::evaluate(&ctx, &repo_config, &state)?;
        let all = found.iter().map(|f| f.key.clone()).collect();
        let wanted = system::select_keys(&system_filters, &all)?;
        for item in found.into_iter().filter(|f| wanted.contains(&f.key)) {
            match item.verdict {
                Verdict::Row { r: Some(r), .. } => system_entries.push((item.key, r)),
                Verdict::Row { r: None, .. } => {}
                Verdict::Unreadable(why) => {
                    output::warning(&format!("{}: not compared: {why}", item.key));
                }
                Verdict::Refused(_) | Verdict::Invalid(_) | Verdict::Ambiguous(_) => {
                    output::warning(&format!("{}: not valid here, not compared", item.key));
                }
            }
        }
    }
    let refs: Vec<(&str, &SourceEntry)> = entries.iter().map(|(k, r)| (k.as_str(), r)).collect();
    let system_refs: Vec<(&str, &SourceEntry)> = system_entries
        .iter()
        .map(|(k, r)| (k.as_str(), r))
        .collect();
    let no_pager = no_pager || std::env::var_os("NO_PAGER").is_some_and(|v| !v.is_empty());
    show_all(&ctx, &refs, &system_refs, no_pager, &DIFF)
}

#[derive(Serialize)]
struct DiffRecord {
    record: &'static str,
    key: crate::output::json::Text,
    scope: &'static str,
    path: crate::output::json::PathValue,
    status: &'static str,
    system_off: bool,
    source: Option<crate::output::json::Text>,
    r: crate::commands::status::Observation,
    source_tuple: crate::commands::status::Observation,
    h: crate::commands::status::Observation,
    raw_h: crate::commands::status::Observation,
    payload: Option<crate::output::json::PathValue>,
}

#[derive(Serialize)]
struct DiffReport {
    record: &'static str,
    count: usize,
}

impl DiffRecord {
    fn new(ctx: &Context, key: &str, scope: &'static str, off: bool) -> Self {
        use crate::commands::status::Observation;
        let path = if scope == "home" {
            ctx.paths.home.join(key)
        } else {
            ctx.system_scope().disk(key)
        };
        Self {
            record: "diff",
            key: crate::output::json::Text::new(key),
            scope,
            path: crate::output::json::PathValue::new(&path),
            status: "unavailable",
            system_off: off,
            source: None,
            r: Observation::Unavailable {
                reason: "not_observed",
            },
            source_tuple: Observation::Unavailable {
                reason: "not_observed",
            },
            h: Observation::Unavailable {
                reason: "not_observed",
            },
            raw_h: Observation::Unavailable {
                reason: "not_observed",
            },
            payload: None,
        }
    }
}

fn json_diff(ctx: &Context, args: &DiffArgs, repo_id: &str) -> Result<()> {
    let config = ctx.repo_config()?;
    let state = ctx.state(repo_id)?;
    let sources = ctx.sources()?;
    let (home_paths, system_filters) = system::split_inputs(&ctx.paths, &args.paths)?;
    let mut all: BTreeSet<String> = sources.keys().map(str::to_owned).collect();
    let indexed = ctx.git.index_paths()?;
    let tracked =
        crate::variant::Sources::group(indexed.iter().map(String::as_str).filter(|path| {
            !crate::paths::is_reserved_root_name(path.split('/').next().unwrap_or(path))
                && ctx.check_key(path, true).is_ok()
        }));
    all.extend(tracked.keys().map(str::to_owned));
    all.extend(state.files.keys().cloned());
    let keys = if args.paths.is_empty() || !home_paths.is_empty() {
        ctx.select(&home_paths, all)?
    } else {
        BTreeSet::new()
    };
    let variants = Variants::new(
        Worktree::new(&ctx.paths.repo, &config.modes),
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );
    ctx.git.check_version()?;
    let tmp = private_temp_dir(&temp_prefix(), &private_base(ctx)?)?;
    anyhow::ensure!(
        !fs::canonicalize(tmp.path())?.starts_with(fs::canonicalize(&ctx.paths.repo)?),
        "the diff temporary directory is inside the repo"
    );
    let mut count = 0_usize;
    for key in keys {
        let record = home_diff(ctx, tmp.path(), &variants, &sources, &state, &key)?;
        emit_diff(&record, &mut count)?;
    }
    if args.paths.is_empty() || !system_filters.is_empty() {
        let found = system::evaluate_with_nodes(ctx, &config, &state)?;
        let indexed_system: Vec<&str> = indexed
            .iter()
            .filter_map(|path| path.strip_prefix(".dotman/system/"))
            .filter(|path| crate::paths::validate_key(path).is_ok())
            .collect();
        let grouped = crate::variant::Sources::group(indexed_system);
        let existing: BTreeSet<String> = found.iter().map(|(f, _)| f.key.clone()).collect();
        let missing: BTreeSet<String> = grouped
            .keys()
            .map(system::key_of)
            .filter(|key| !existing.contains(key))
            .collect();
        let all = existing.union(&missing).cloned().collect();
        let wanted = system::select_keys(&system_filters, &all)?;
        for (item, node) in found.into_iter().filter(|(f, _)| wanted.contains(&f.key)) {
            let record = system_diff(ctx, tmp.path(), item, node)?;
            emit_diff(&record, &mut count)?;
        }
        for key in missing.intersection(&wanted) {
            let record = missing_system_diff(ctx, tmp.path(), key)?;
            emit_diff(&record, &mut count)?;
        }
    }
    crate::output::json::write(
        &mut std::io::stdout().lock(),
        &DiffReport {
            record: "report",
            count,
        },
    )
}

fn missing_system_diff(ctx: &Context, tmp: &Path, key: &str) -> Result<DiffRecord> {
    use crate::commands::status::Observation;
    let scope = ctx.system_scope();
    let standing = scope.standing(key);
    let mut record = DiffRecord::new(ctx, key, "system", standing == system::Standing::Off);
    if matches!(standing, system::Standing::Refused(_)) {
        record.status = "refused";
        return Ok(record);
    }
    record.r = Observation::Missing;
    record.source_tuple = Observation::Missing;
    match scope.node(key) {
        Ok(node) => {
            record.raw_h = Observation::home(&node.home());
            let rel = format!("system/{}", key.trim_start_matches('/'));
            compare_json(tmp, &rel, None, node, &mut record)?;
        }
        Err(e) => {
            record.status = "unreadable";
            record.h = Observation::Unavailable {
                reason: "unreadable",
            };
            record.raw_h = record.h.clone();
            output::warning(&format!("{key}: not compared: {e:#}"));
        }
    }
    Ok(record)
}

fn home_diff(
    ctx: &Context,
    tmp: &Path,
    variants: &Variants<'_>,
    sources: &crate::variant::Sources,
    state: &crate::state::State,
    key: &str,
) -> Result<DiffRecord> {
    use crate::commands::status::Observation;
    let mut record = DiffRecord::new(ctx, key, "home", false);
    record.source = sources
        .select(key, ctx.identity())
        .winner()
        .map(|source| crate::output::json::Text::new(*source));
    let mut r = match variants.get(key) {
        Ok(Selection::Winner(r)) => Some(r),
        Ok(Selection::None) => None,
        Ok(Selection::Ambiguous(_)) => {
            record.status = "ambiguous";
            return Ok(record);
        }
        Ok(Selection::Invalid(_)) => {
            record.status = "invalid";
            return Ok(record);
        }
        Err(e) => {
            let reason = if e.is::<Locked>() {
                "locked"
            } else if e.is::<crate::engine::status::TemplateError>() {
                "template_error"
            } else {
                "unreadable"
            };
            record.status = reason;
            record.r = Observation::Unavailable { reason };
            record.source_tuple = Observation::Unavailable { reason };
            output::warning(&format!("{key}: not compared: {e:#}"));
            return Ok(record);
        }
    };
    if let Some(entry) = &mut r {
        record.source = Some(crate::output::json::Text::new(&entry.source));
        record.source_tuple = Observation::tuple(&entry.tuple);
        if let Some(saved) = state.files.get(key) {
            let saved_tuple = Tuple::from_entry(saved)?;
            let against = saved
                .repo_mode
                .as_deref()
                .map(crate::engine::parse_mode)
                .transpose()?;
            if entry.tuple.kind == crate::state::Kind::File
                && against == Some(entry.tuple.mode)
                && !variants.mode_is_explicit(&entry.source)
            {
                entry.tuple.mode = saved_tuple.mode;
            }
        }
        record.r = Observation::tuple(&entry.tuple);
    } else {
        record.r = Observation::Missing;
        record.source_tuple = Observation::Missing;
    }
    match read_node(&ctx.paths.home, key) {
        Ok(node) => {
            record.raw_h = Observation::home(&node.home());
            let node = match node {
                Node::Symlink(target) => Node::Symlink(portable_link_target(
                    &target,
                    key,
                    &ctx.paths.home,
                    ctx.paths.home_real.as_deref(),
                )),
                node => node,
            };
            compare_json(tmp, key, r.as_ref(), node, &mut record)?;
        }
        Err(e) => {
            record.status = "unreadable";
            record.h = Observation::Unavailable {
                reason: "unreadable",
            };
            record.raw_h = record.h.clone();
            output::warning(&format!("{key}: not compared: {e:#}"));
        }
    }
    Ok(record)
}

fn system_diff(
    ctx: &Context,
    tmp: &Path,
    item: system::Evaluated,
    node: Option<Node>,
) -> Result<DiffRecord> {
    use crate::commands::status::Observation;
    let mut record = DiffRecord::new(ctx, &item.key, "system", item.off);
    match item.verdict {
        Verdict::Row { r, .. } => {
            if let Some(entry) = &r {
                record.source = Some(crate::output::json::Text::new(&entry.source));
                record.r = Observation::tuple(&entry.tuple);
                record.source_tuple = record.r.clone();
            } else {
                record.r = Observation::Missing;
                record.source_tuple = Observation::Missing;
            }
            let node = node.context("system comparison snapshot is missing")?;
            record.raw_h = Observation::home(&node.home());
            let rel = format!("system/{}", item.key.trim_start_matches('/'));
            compare_json(tmp, &rel, r.as_ref(), node, &mut record)?;
        }
        Verdict::Unreadable(why) => {
            record.status = "unreadable";
            output::warning(&format!("{}: not compared: {why}", item.key));
        }
        Verdict::Refused(_) => record.status = "refused",
        Verdict::Invalid(_) => record.status = "invalid",
        Verdict::Ambiguous(_) => record.status = "ambiguous",
    }
    Ok(record)
}

fn emit_diff(record: &DiffRecord, count: &mut usize) -> Result<()> {
    crate::output::json::write(&mut std::io::stdout().lock(), record)?;
    *count = count.saturating_add(1);
    Ok(())
}

fn compare_json(
    tmp: &Path,
    rel: &str,
    r: Option<&SourceEntry>,
    node: Node,
    record: &mut DiffRecord,
) -> Result<()> {
    use crate::commands::status::Observation;
    use std::os::unix::ffi::OsStrExt;
    let h = node.home();
    record.h = Observation::home(&h);
    if matches!(h, Home::Blocked(_)) {
        record.status = "blocked";
        return Ok(());
    }
    let equal = match (&h, r) {
        (Home::Missing, None) => true,
        (Home::Present(h), Some(r)) => h == &r.tuple,
        _ => false,
    };
    if equal {
        record.status = "equal";
        return Ok(());
    }
    let tree = private_temp_dir(&temp_prefix(), tmp)?;
    let home = tree.path().join("home");
    let repo = tree.path().join("repo");
    let path = Path::new(rel);
    if let Some(r) = r {
        place(&repo.join(path), &r.content, r.tuple.mode)?;
    }
    match node {
        Node::File { bytes, mode, .. } => place(&home.join(path), &Content::Bytes(bytes), mode)?,
        Node::Symlink(target) => place(&home.join(path), &Content::Link(target), 0)?,
        Node::Missing => {}
        Node::BadParent(_) | Node::Dir | Node::Special => bail!("blocked comparison"),
    }
    let bytes = capture_diff(tree.path())?;
    record.payload = Some(crate::output::json::PathValue::new(Path::new(
        std::ffi::OsStr::from_bytes(&bytes),
    )));
    record.status = "different";
    Ok(())
}

fn capture_diff(tmp: &Path) -> Result<Vec<u8>> {
    for side in ["repo", "home"] {
        create_private_dir(&tmp.join(side), false)?;
    }
    let git = Git::new(tmp, "diff");
    let mut cmd = git.command();
    cmd.args([
        "--no-pager",
        "diff",
        "--no-index",
        "--no-ext-diff",
        "--no-textconv",
        "--color=never",
        "--no-prefix",
        "--",
        "repo",
        "home",
    ]);
    cmd.stdout(std::process::Stdio::piped());
    let child = cmd.spawn().context("failed to run git diff")?;
    let result = {
        let _ignored = sys::ignore_signals(&[SIGINT, SIGQUIT, SIGHUP, SIGTERM]);
        child.wait_with_output()
    }
    .context("failed to run git diff")?;
    match result.status.code() {
        Some(0 | 1) => Ok(result.stdout),
        _ => bail!("git diff --no-index failed ({})", result.status),
    }
}

/// The names of the two trees `git diff` compares, which label its paths.
struct Sides {
    home: &'static str,
    repo: &'static str,
    /// The repo's tree is the old side, as in `dotman diff`, so an edit made in `$HOME` shows as
    /// added lines; otherwise `$HOME`'s is, as before a write.
    repo_first: bool,
}

const DIFF: Sides = Sides {
    home: "home",
    repo: "repo",
    repo_first: true,
};

/// What a write of R does to the file there now.
const WRITE: Sides = Sides {
    home: "now",
    repo: "written",
    repo_first: false,
};

/// One `git diff`, without a pager, from `repo/<key>` (R) to `home/<key>` (H) over the keys whose
/// H differs from R; system keys are read through the system scope.
pub(crate) fn show(
    ctx: &Context,
    entries: &[(&str, &SourceEntry)],
    system_entries: &[(&str, &SourceEntry)],
) -> Result<()> {
    show_all(ctx, entries, system_entries, true, &DIFF)
}

/// What writing R over each system key changes: one `git diff`, without a pager, from the file
/// there now, `now/system/<key>`, to what is written, `written/system/<key>`.
pub(crate) fn show_writes(ctx: &Context, system_entries: &[(&str, &SourceEntry)]) -> Result<()> {
    show_all(ctx, &[], system_entries, true, &WRITE)
}

// System keys go under `system/` in both trees, so one can never land on a home key.
fn show_all(
    ctx: &Context,
    entries: &[(&str, &SourceEntry)],
    system_entries: &[(&str, &SourceEntry)],
    no_pager: bool,
    sides: &Sides,
) -> Result<()> {
    ctx.git.check_version()?;
    // Decrypted content goes here, so it is removed on every return, a failed git diff included.
    let tmp = private_temp_dir(&temp_prefix(), &private_base(ctx)?)?;
    let (home_side, repo_side) = (tmp.path().join(sides.home), tmp.path().join(sides.repo));
    let (home, home_real) = (&ctx.paths.home, ctx.paths.home_real.as_deref());
    let mut differs = false;
    for (key, r) in entries {
        let node = match read_node(home, key) {
            Ok(node) => node,
            Err(e) => {
                output::warning(&format!("{key}: not compared: {e:#}"));
                continue;
            }
        };
        let h = match &node {
            Node::Symlink(target) => Home::Present(Tuple::of_link(&portable_link_target(
                target, key, home, home_real,
            ))),
            _ => node.home(),
        };
        differs |= stage(&[&home_side, &repo_side], Path::new(key), (key, r), h, node)?;
    }
    let scope = ctx.system_scope();
    for (key, r) in system_entries {
        let node = match scope.node(key) {
            Ok(node) => node,
            Err(e) => {
                output::warning(&format!("{key}: not compared: {e:#}"));
                continue;
            }
        };
        let rel = Path::new("system").join(key.trim_start_matches('/'));
        let h = node.home();
        differs |= stage(&[&home_side, &repo_side], &rel, (key, r), h, node)?;
    }
    if !differs {
        output::info("no differences between the repo and $HOME");
        return Ok(());
    }
    let order = if sides.repo_first {
        [sides.repo, sides.home]
    } else {
        [sides.home, sides.repo]
    };
    run_diff(tmp.path(), order, no_pager, ctx.json)
}

/// One version of a path for [`show_versions`]: its content and its git mode.
pub(crate) struct Version {
    pub content: Content,
    pub mode: u32,
}

/// One `git diff`, without a pager, from origin's version of each path, `origin/<path>`, to this
/// machine's, `here/<path>`, so this machine's edits show as added lines. A missing version shows
/// as an added or removed file. Decrypted content goes only into a private temp dir, removed
/// before this returns.
pub(crate) fn show_versions(
    ctx: &Context,
    versions: &[(PathBuf, [Option<Version>; 2])],
) -> Result<()> {
    ctx.git.check_version()?;
    let tmp = private_temp_dir(&temp_prefix(), &private_base(ctx)?)?;
    let (here, origin) = (tmp.path().join("here"), tmp.path().join("origin"));
    for (path, [mine, theirs]) in versions {
        if !path
            .components()
            .all(|part| matches!(part, std::path::Component::Normal(_)))
        {
            bail!("{} is not a path inside the repo", path.display());
        }
        for (side, version) in [(&here, mine), (&origin, theirs)] {
            if let Some(version) = version {
                place(&side.join(path), &version.content, version.mode)?;
            }
        }
    }
    run_diff(tmp.path(), ["origin", "here"], true, ctx.json)
}

fn run_diff(tmp: &Path, [old, new]: [&str; 2], no_pager: bool, json: bool) -> Result<()> {
    for side in [old, new] {
        let side = tmp.join(side);
        create_private_dir(&side, false)
            .with_context(|| format!("cannot create {}", side.display()))?;
    }
    let git = Git::new(tmp, "diff");
    let mut cmd = git.command();
    if json {
        cmd.stdout(crate::output::json::child_stdout()?);
    }
    if no_pager || (std::io::stdout().is_terminal() && pager_missing(&git)) {
        cmd.arg("--no-pager");
    }
    // `--no-index` keeps each side's directory in the path, so `--no-prefix` with sides named
    // `repo` and `home` gives `repo/<key>` and `home/<key>`; `--src-prefix=repo/` would double it.
    cmd.args(["diff", "--no-index", color(), "--no-prefix", "--", old, new]);
    let mut child = cmd.spawn().context("failed to run git diff")?;
    // Ctrl-C reaches the whole foreground group. Ignored here only after the spawn, so git and the
    // pager keep their default actions, and dotman outlives them to remove the temp dir; only SIGKILL
    // can still leave it behind.
    let status = {
        let _ignored = sys::ignore_signals(&[SIGINT, SIGQUIT, SIGHUP, SIGTERM]);
        child.wait()
    }
    .context("failed to run git diff")?;
    match status.code() {
        Some(0 | 1) => Ok(()),
        None if status.signal() == Some(libc::SIGPIPE) => Ok(()),
        _ => bail!("git diff --no-index failed ({status})"),
    }
}

// Whether the pager git would start for `diff` cannot be found. git then dies before it shows
// anything ("cannot run less", on a system without less), so the diff goes without one. A pager
// that starts and then fails keeps git's behaviour.
fn pager_missing(git: &Git) -> bool {
    let ask = |args: &[&str]| {
        let mut cmd = git.command();
        cmd.args(args);
        proc::output_within(&mut cmd, PAGER_PROBE)
            .ok()
            .flatten()
            .filter(|out| out.status.success())
            .map(|out| String::from_utf8_lossy(&out.stdout).trim().to_owned())
    };
    // `git var` does not know `pager.diff`, which git reads first.
    if ask(&["config", "--get", "pager.diff"]).is_some() {
        return false;
    }
    let Some(pager) = ask(&["var", "GIT_PAGER"]).filter(|pager| pager != "cat") else {
        return false;
    };
    let words = shell_words::split(&pager).unwrap_or_default();
    // Leading `NAME=value` words set the pager's environment, as in `LESS=FRX less`.
    words
        .iter()
        .find(|word| !word.contains('='))
        .is_some_and(|program| proc::find_program(program).is_none())
}

// git ignores NO_COLOR, and an inherited GIT_PAGER_IN_USE makes its `auto` colour a pipe.
fn color() -> &'static str {
    let no_color = std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty());
    if no_color || !std::io::stdout().is_terminal() {
        "--color=never"
    } else {
        "--color=auto"
    }
}

// Places both sides of one key under `sides` at `rel`; whether they differ.
fn stage(
    sides: &[&PathBuf; 2],
    rel: &Path,
    (key, r): (&str, &SourceEntry),
    h: Home,
    node: Node,
) -> Result<bool> {
    let [home_side, repo_side] = sides;
    match h {
        Home::Present(h) if h == r.tuple => return Ok(false),
        Home::Blocked(_) => {
            output::warning(&format!("{key}: blocked, not compared"));
            return Ok(false);
        }
        Home::Present(_) => match node {
            Node::File { bytes, mode, .. } => {
                place(&home_side.join(rel), &Content::Bytes(bytes), mode)?;
            }
            Node::Symlink(target) => place(&home_side.join(rel), &Content::Link(target), 0)?,
            _ => return Ok(false),
        },
        Home::Missing => {}
    }
    place(&repo_side.join(rel), &r.content, r.tuple.mode)?;
    Ok(true)
}

/// Named as every dotman temp file is, with this run's PID, so the sweep removes the decrypted
/// copies of a killed run once that PID is gone.
fn temp_prefix() -> String {
    format!("{TEMP_PREFIX}{}-", std::process::id())
}

// `$XDG_RUNTIME_DIR` is private to the user and usually memory-backed; the state dir is private too.
pub(crate) fn private_base(ctx: &Context) -> Result<PathBuf> {
    if let Some(dir) = crate::context::runtime_dir() {
        return Ok(dir);
    }
    let state = &ctx.paths.state_dir;
    create_private_dir(state, false)
        .with_context(|| format!("cannot create {}", state.display()))?;
    Ok(state.clone())
}

fn place(path: &Path, content: &Content, mode: u32) -> Result<()> {
    if let Some(dir) = path.parent() {
        create_private_dir(dir, false)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    match content {
        Content::Bytes(bytes) => {
            fs::write(path, bytes).with_context(|| format!("cannot write {}", path.display()))?;
            // git shows only the execute bit; the 0700 temp dir keeps the content private.
            let shown = if mode & 0o111 == 0 { 0o600 } else { 0o700 };
            fs::set_permissions(path, Permissions::from_mode(shown))?;
        }
        Content::Link(target) => {
            symlink(target, path).with_context(|| format!("cannot create {}", path.display()))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_uses_the_observed_node_after_the_file_changes() -> Result<()> {
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("input");
        fs::create_dir(&root)?;
        fs::write(root.join("file"), b"before")?;
        fs::set_permissions(root.join("file"), Permissions::from_mode(0o644))?;
        let node = read_node(&root, "file")?;
        let h = node.home();
        fs::write(root.join("file"), b"after")?;

        let repo = SourceEntry {
            source: "file".into(),
            tuple: Tuple::of_file(b"repo", 0o644),
            content: Content::Bytes(b"repo".to_vec()),
            template: false,
            encrypted: false,
        };
        let home_side = tmp.path().join("home");
        let repo_side = tmp.path().join("repo");
        assert!(stage(
            &[&home_side, &repo_side],
            Path::new("file"),
            ("file", &repo),
            h,
            node,
        )?);
        assert_eq!(fs::read(home_side.join("file"))?, b"before");

        fs::write(root.join("file"), b"repo")?;
        let node = read_node(&root, "file")?;
        let h = node.home();
        fs::write(root.join("file"), b"after")?;
        assert!(!stage(
            &[&home_side, &repo_side],
            Path::new("file"),
            ("file", &repo),
            h,
            node,
        )?);
        Ok(())
    }

    #[test]
    fn json_payload_and_tuple_share_the_owned_node() -> Result<()> {
        use crate::commands::status::Observation;
        use crate::output::json::{PathValue, Text};
        let tmp = tempfile::tempdir()?;
        let root = tmp.path().join("input");
        fs::create_dir(&root)?;
        fs::write(root.join("file"), b"before\n")?;
        let node = read_node(&root, "file")?;
        let observed = Observation::home(&node.home());
        fs::write(root.join("file"), b"after\n")?;
        let repo = SourceEntry {
            source: "file".into(),
            tuple: Tuple::of_file(b"repo\n", 0o644),
            content: Content::Bytes(b"repo\n".to_vec()),
            template: false,
            encrypted: false,
        };
        let mut record = DiffRecord {
            record: "diff",
            key: Text::new("file"),
            scope: "home",
            path: PathValue::new(&root.join("file")),
            status: "unavailable",
            system_off: false,
            source: None,
            r: Observation::tuple(&repo.tuple),
            source_tuple: Observation::tuple(&repo.tuple),
            h: observed.clone(),
            raw_h: observed,
            payload: None,
        };
        compare_json(tmp.path(), "file", Some(&repo), node, &mut record)?;
        let json = serde_json::to_value(record)?;
        assert_eq!(
            json.pointer("/h/tuple/hash").and_then(|v| v.as_str()),
            Some(crate::engine::hash_hex(b"before\n").as_str())
        );
        assert!(
            json.pointer("/payload/raw")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.contains("+before\n"))
        );
        assert!(
            !json
                .pointer("/payload/raw")
                .and_then(|v| v.as_str())
                .is_some_and(|s| s.contains("+after\n"))
        );
        fs::write(root.join("file"), b"repo\n")?;
        fs::set_permissions(root.join("file"), Permissions::from_mode(0o644))?;
        let node = read_node(&root, "file")?;
        fs::write(root.join("file"), b"after\n")?;
        let mut record = DiffRecord {
            record: "diff",
            key: Text::new("file"),
            scope: "home",
            path: PathValue::new(&root.join("file")),
            status: "unavailable",
            system_off: false,
            source: None,
            r: Observation::tuple(&repo.tuple),
            source_tuple: Observation::tuple(&repo.tuple),
            h: Observation::home(&node.home()),
            raw_h: Observation::home(&node.home()),
            payload: None,
        };
        compare_json(tmp.path(), "file", Some(&repo), node, &mut record)?;
        assert_eq!(record.status, "equal");
        assert!(record.payload.is_none());
        Ok(())
    }
}
