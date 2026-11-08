//! The legacy dotman (v2) importer.
//!
//! A legacy repo keeps one cumulative snapshot per commit in `commits/<id>.zst` and file contents
//! in `objects/<hash>.zst`, both zstd. `HEAD` and `refs/heads/<name>` hold plain ids. The history
//! is rebuilt as git commits through `git fast-import`. `remote-mappings.toml` is never read.
//!
//! The accounting unit is the distinct source key over the whole history, spelled as stored. A
//! key counts as converted when every commit that holds it kept it, and as needing action when any
//! commit dropped it.

use super::{
    EditedConfig, Item, Report, author_email, count, homes, proceed, require_clean, require_fresh,
};
use crate::cli::ImportArgs;
use crate::commands::Outcome;
use crate::commands::add::ensure_worktree_parents;
use crate::commands::sync::IDENTITY_FIX;
use crate::context::Context;
use crate::engine::{Node, read_node, recorded_mode};
use crate::output;
use crate::paths::{validate_key, write_atomic};
use crate::prompt;
use crate::repo_config::RepoConfig;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{File, OpenOptions, Permissions};
use std::io::{self, BufWriter, Read, Seek, Write};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_BYTES: u64 = 100 * 1024 * 1024;
const BINCODE_LIMIT: usize = 100 * 1024 * 1024;
const OVERLAY_MESSAGE: &str = "import: overlay from legacy dotman";

const UNSAFE_KEY: &str = "keys the path rules refuse";
const OUTSIDE_HOME: &str = "absolute keys outside any home directory";
const COLLISION: &str = "keys that land on the same path as another key";
const NO_OBJECT: &str = "keys whose content is missing from the legacy store";
const MISSING: &str = "files missing from the repo, left out";
const DIFFERS: &str = "files whose content differs from the repo, left as they are";
const MODE_DIFFERS: &str = "modes dotman does not change";
const BLOCKED: &str = "files the repo holds in another form";

type Refusal = (&'static str, String);

// reason: the legacy layout needs every field to decode, but not every field is read.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct Commit {
    id: String,
    parents: Vec<String>,
    message: String,
    author: String,
    timestamp: i64,
    tree_hash: String,
}

// reason: the legacy layout needs every field to decode, but not every field is read.
#[allow(dead_code)]
#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct SnapshotFile {
    hash: String,
    mode: u32,
    content_hash: String,
}

#[derive(Debug, Deserialize)]
#[cfg_attr(test, derive(serde::Serialize))]
struct Snapshot {
    commit: Commit,
    files: BTreeMap<PathBuf, SnapshotFile>,
}

impl Snapshot {
    /// In their stored order, without the null id and without repeats.
    fn parents(&self) -> Vec<&str> {
        let mut parents: Vec<&str> = Vec::new();
        for parent in &self.commit.parents {
            let parent = parent.as_str();
            if !is_null(parent) && !parents.contains(&parent) {
                parents.push(parent);
            }
        }
        parents
    }
}

/// The null commit id and the hash of an empty file are all zeros.
fn is_null(id: &str) -> bool {
    !id.is_empty() && id.bytes().all(|b| b == b'0')
}

/// Ids become file names, so anything but a short hex string is refused.
fn check_id(id: &str, what: &str) -> Result<()> {
    ensure!(
        !id.is_empty() && id.len() <= 64 && id.bytes().all(|b| b.is_ascii_hexdigit()),
        "{what} {id:?} is not a hex id"
    );
    Ok(())
}

pub(super) fn is_legacy(dir: &Path) -> bool {
    dir.join("index.bin").is_file() || dir.join("commits").is_dir()
}

fn read_capped(path: &Path, limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len()).is_ok_and(|n| n <= limit),
        "{} is larger than {limit} bytes",
        path.display()
    );
    Ok(bytes)
}

fn inflate(compressed: &[u8], limit: u64) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    zstd::Decoder::new(compressed)?
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len()).is_ok_and(|n| n <= limit),
        "it expands to more than {limit} bytes"
    );
    Ok(bytes)
}

fn decode<const LIMIT: usize>(bytes: &[u8]) -> Result<Snapshot> {
    let config = bincode::config::legacy().with_limit::<LIMIT>();
    let (snapshot, _) = bincode::serde::decode_from_slice(bytes, config)?;
    Ok(snapshot)
}

fn load_snapshot(root: &Path, id: &str) -> Result<Snapshot> {
    check_id(id, "commit id")?;
    let path = root.join("commits").join(format!("{id}.zst"));
    let compressed =
        read_capped(&path, MAX_BYTES).with_context(|| format!("cannot read legacy commit {id}"))?;
    let bytes = inflate(&compressed, MAX_BYTES)
        .with_context(|| format!("cannot decompress legacy commit {id}"))?;
    decode::<BINCODE_LIMIT>(&bytes).with_context(|| format!("cannot decode legacy commit {id}"))
}

fn object_path(root: &Path, hash: &str) -> PathBuf {
    root.join("objects").join(format!("{hash}.zst"))
}

fn read_object(root: &Path, hash: &str) -> Result<Vec<u8>> {
    if is_null(hash) {
        return Ok(Vec::new());
    }
    check_id(hash, "object id")?;
    let compressed = read_capped(&object_path(root, hash), MAX_BYTES)
        .with_context(|| format!("cannot read legacy object {hash}"))?;
    inflate(&compressed, MAX_BYTES)
        .with_context(|| format!("cannot decompress legacy object {hash}"))
}

/// The commit `HEAD` names. The old writer added no newline, so both files are trimmed.
fn head(root: &Path) -> Result<String> {
    let text = |path: PathBuf| -> Result<String> {
        let bytes =
            read_capped(&path, 4096).with_context(|| format!("cannot read {}", path.display()))?;
        Ok(String::from_utf8_lossy(&bytes).trim().to_owned())
    };
    let head = text(root.join("HEAD"))?;
    let id = match head.strip_prefix("ref: refs/heads/") {
        Some(branch) => {
            ensure!(
                branch
                    .split('/')
                    .all(|part| !matches!(part, "" | "." | "..")),
                "HEAD names the branch {branch:?}, which is not a plain branch name"
            );
            let path = root.join("refs/heads").join(branch);
            if !path.is_file() {
                bail!("the legacy repo has no commits: branch {branch} does not exist");
            }
            text(path)?
        }
        None => head,
    };
    ensure!(!is_null(&id), "the legacy repo has no commits");
    check_id(&id, "HEAD")?;
    Ok(id)
}

/// Every commit reachable from `head`, parents first.
fn load_history(root: &Path, head: &str) -> Result<Vec<(String, Snapshot)>> {
    let mut loaded = BTreeMap::new();
    let mut pending = vec![head.to_owned()];
    while let Some(id) = pending.pop() {
        if loaded.contains_key(&id) {
            continue;
        }
        let snapshot = load_snapshot(root, &id)?;
        pending.extend(snapshot.parents().into_iter().map(str::to_owned));
        loaded.insert(id, snapshot);
    }
    topological(loaded)
}

/// Kahn's algorithm. Among the commits ready at once the oldest goes first, then the smaller id,
/// so the order does not depend on map iteration.
fn topological(mut loaded: BTreeMap<String, Snapshot>) -> Result<Vec<(String, Snapshot)>> {
    let mut waiting: BTreeMap<String, usize> = BTreeMap::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ready: BTreeSet<(i64, String)> = BTreeSet::new();
    for (id, snapshot) in &loaded {
        let parents = snapshot.parents();
        if parents.is_empty() {
            ready.insert((snapshot.commit.timestamp, id.clone()));
            continue;
        }
        waiting.insert(id.clone(), parents.len());
        for parent in parents {
            children
                .entry(parent.to_owned())
                .or_default()
                .push(id.clone());
        }
    }
    let total = loaded.len();
    let mut ordered = Vec::with_capacity(total);
    while let Some((_, id)) = ready.pop_first() {
        for child in children.remove(&id).unwrap_or_default() {
            let Some(count) = waiting.get_mut(&child) else {
                continue;
            };
            *count = count.saturating_sub(1);
            if *count == 0
                && let Some(snapshot) = loaded.get(&child)
            {
                ready.insert((snapshot.commit.timestamp, child));
            }
        }
        if let Some(snapshot) = loaded.remove(&id) {
            ordered.push((id, snapshot));
        }
    }
    ensure!(ordered.len() == total, "the legacy history has a cycle");
    Ok(ordered)
}

fn strip_home<'a>(source: &'a str, homes: &[&Path]) -> Option<&'a str> {
    let path = Path::new(source);
    if let Some(rest) = homes.iter().find_map(|home| path.strip_prefix(home).ok()) {
        return rest.to_str();
    }
    let rest = source
        .strip_prefix("/home/")
        .or_else(|| source.strip_prefix("/Users/"))?;
    let (user, below) = rest.split_once('/')?;
    (!user.is_empty()).then_some(below)
}

/// The repo key for a stored key: a home prefix is stripped, then the rules `dot add` applies.
/// A `.git` component is refused at any depth, because git cannot store one.
fn target_key(source: &str, homes: &[&Path]) -> Result<String, Refusal> {
    let key = if source.starts_with('/') {
        strip_home(source, homes).ok_or_else(|| {
            (
                OUTSIDE_HOME,
                format!("{source}: not under $HOME, /home/<user> or /Users/<user>"),
            )
        })?
    } else {
        source
    };
    validate_key(key).map_err(|e| (UNSAFE_KEY, format!("{source}: {e}")))?;
    if key.split('/').any(|part| part.eq_ignore_ascii_case(".git")) {
        return Err((UNSAFE_KEY, format!("{source}: has a `.git` component")));
    }
    Ok(key.to_owned())
}

fn object_problem(
    source: &str,
    file: &SnapshotFile,
    has_object: &dyn Fn(&str) -> bool,
) -> Option<Refusal> {
    let hash = file.content_hash.as_str();
    if is_null(hash) {
        return None;
    }
    if let Err(e) = check_id(hash, "object id") {
        return Some((NO_OBJECT, format!("{source}: {e}")));
    }
    (!has_object(hash)).then(|| {
        (
            NO_OBJECT,
            format!("{source}: object {hash} is not in the legacy store"),
        )
    })
}

/// A file of one snapshot that the import keeps.
struct Chosen {
    source: String,
    key: String,
    /// `mode & 0o777` as the old add path stored it.
    mode: u32,
    object: String,
}

struct Selection {
    chosen: Vec<Chosen>,
    dropped: Vec<(String, Refusal)>,
}

/// Splits a snapshot into the files that can be imported and the keys that cannot. Two keys that
/// reach one path keep the one stored relative, which is what `dot add` wrote. A file that is also
/// a directory in the same snapshot is dropped, because fast-import would silently replace it.
fn select(snapshot: &Snapshot, homes: &[&Path], has_object: &dyn Fn(&str) -> bool) -> Selection {
    let mut dropped = Vec::new();
    let mut by_key: BTreeMap<String, Vec<(&str, &SnapshotFile)>> = BTreeMap::new();
    for (path, file) in &snapshot.files {
        let Some(source) = path.to_str() else {
            let lossy = path.to_string_lossy().into_owned();
            let why = format!("{lossy}: not valid UTF-8");
            dropped.push((lossy, (UNSAFE_KEY, why)));
            continue;
        };
        match target_key(source, homes) {
            Err(refusal) => dropped.push((source.to_owned(), refusal)),
            Ok(key) => match object_problem(source, file, has_object) {
                Some(refusal) => dropped.push((source.to_owned(), refusal)),
                None => by_key.entry(key).or_default().push((source, file)),
            },
        }
    }
    let mut chosen: BTreeMap<String, (&str, &SnapshotFile)> = BTreeMap::new();
    for (key, mut candidates) in by_key {
        let at = candidates
            .iter()
            .position(|(source, _)| *source == key)
            .unwrap_or(0);
        let winner = candidates.swap_remove(at);
        for (loser, _) in candidates {
            dropped.push((
                loser.to_owned(),
                (COLLISION, format!("{loser}: same path as {}", winner.0)),
            ));
        }
        chosen.insert(key, winner);
    }
    let directories: BTreeSet<&str> = chosen
        .keys()
        .flat_map(|key| key.match_indices('/').filter_map(|(at, _)| key.get(..at)))
        .collect();
    let shadowing: Vec<String> = chosen
        .keys()
        .filter(|key| directories.contains(key.as_str()))
        .cloned()
        .collect();
    for key in shadowing {
        if let Some((source, _)) = chosen.remove(&key) {
            dropped.push((
                source.to_owned(),
                (
                    COLLISION,
                    format!("{source}: also a directory in the same commit"),
                ),
            ));
        }
    }
    let chosen = chosen
        .into_iter()
        .map(|(key, (source, file))| Chosen {
            source: source.to_owned(),
            key,
            mode: file.mode & 0o777,
            object: file.content_hash.clone(),
        })
        .collect();
    Selection { chosen, dropped }
}

/// The mode a file has with no `[modes]` entry: git keeps only the executable bit.
const fn default_mode(mode: u32) -> u32 {
    if mode & 0o111 == 0 { 0o644 } else { 0o755 }
}

struct Planned {
    /// Positions in the plan, always earlier ones.
    parents: Vec<usize>,
    author: String,
    timestamp: i64,
    message: String,
    files: Vec<Chosen>,
    config: String,
}

struct Plan {
    commits: Vec<Planned>,
    report: Report,
}

#[derive(Default)]
struct Tally {
    commits: usize,
    skipped: usize,
    why: Option<Refusal>,
    imported_as: Option<String>,
}

fn author_name(author: &str) -> String {
    let name: String = author
        .split('<')
        .next()
        .unwrap_or_default()
        .chars()
        .filter(|c| !matches!(c, '>' | '\n' | '\r' | '\0'))
        .collect();
    let name = name.trim();
    if name.is_empty() {
        "unknown".to_owned()
    } else {
        name.to_owned()
    }
}

fn plan(
    history: &[(String, Snapshot)],
    homes: &[&Path],
    has_object: &dyn Fn(&str) -> bool,
) -> Result<Plan> {
    let mut tallies: BTreeMap<String, Tally> = BTreeMap::new();
    let mut position: BTreeMap<&str, usize> = BTreeMap::new();
    let mut commits = Vec::with_capacity(history.len());
    for (id, snapshot) in history {
        let Selection { chosen, dropped } = select(snapshot, homes, has_object);
        let mut modes = BTreeMap::new();
        for file in &chosen {
            let tally = tallies.entry(file.source.clone()).or_default();
            tally.commits = tally.commits.saturating_add(1);
            tally.imported_as.get_or_insert_with(|| file.key.clone());
            if file.mode != default_mode(file.mode) {
                modes.insert(file.key.clone(), format!("{:04o}", file.mode));
            }
        }
        for (source, why) in dropped {
            let tally = tallies.entry(source).or_default();
            tally.commits = tally.commits.saturating_add(1);
            tally.skipped = tally.skipped.saturating_add(1);
            tally.why.get_or_insert(why);
        }
        let parents = snapshot
            .parents()
            .into_iter()
            .map(|parent| {
                position
                    .get(parent)
                    .copied()
                    .ok_or_else(|| anyhow!("commit {parent} comes after {id}"))
            })
            .collect::<Result<Vec<_>>>()?;
        position.insert(id, commits.len());
        let config = RepoConfig {
            modes,
            ..RepoConfig::default()
        };
        commits.push(Planned {
            parents,
            author: author_name(&snapshot.commit.author),
            timestamp: snapshot.commit.timestamp.max(0),
            message: snapshot.commit.message.clone(),
            files: chosen,
            config: toml::to_string_pretty(&config)?,
        });
    }
    Ok(Plan {
        commits,
        report: account(tallies),
    })
}

fn account(tallies: BTreeMap<String, Tally>) -> Report {
    let mut report = Report::default();
    let mut stripped = Vec::new();
    for (source, tally) in tallies {
        let Some((reason, detail)) = tally.why else {
            report.converted = report.converted.saturating_add(1);
            if let Some(key) = tally.imported_as.filter(|key| *key != source) {
                stripped.push((source, key));
            }
            continue;
        };
        let detail = if tally.commits > 1 {
            format!(
                "{detail} (in {} of {} commits)",
                tally.skipped, tally.commits
            )
        } else {
            detail
        };
        report.needs_action.push(Item { reason, detail });
    }
    if let Some((source, key)) = stripped.first() {
        let n = stripped.len();
        let (verb, pronoun) = if n == 1 {
            ("was", "its")
        } else {
            ("were", "their")
        };
        report.notes.push(format!(
            "{} {verb} imported without {pronoun} home prefix, for example {source} as {key}",
            count(n, "absolute key")
        ));
    }
    report
}

/// Always quoted, so a name with a quote, a backslash or a newline stays one path.
fn quote(key: &str) -> String {
    use std::fmt::Write as _;
    let mut out = String::with_capacity(key.len().saturating_add(2));
    out.push('"');
    for c in key.chars() {
        match c {
            '"' | '\\' => {
                out.push('\\');
                out.push(c);
            }
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            c if c.is_ascii_control() => {
                let _ = write!(out, "\\{:03o}", u32::from(c));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn write_data(out: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
    writeln!(out, "data {}", bytes.len())?;
    out.write_all(bytes)?;
    out.write_all(b"\n")
}

/// The `git fast-import` stream for the whole plan. Each commit lists its full tree after
/// `deleteall`, because legacy snapshots are cumulative. Contents are emitted once per hash. A
/// commit with no parent resets the branch first, since a later root commit would otherwise
/// continue the branch.
fn write_stream(
    plan: &Plan,
    email: &str,
    read: &mut dyn FnMut(&str) -> Result<Vec<u8>>,
    out: &mut dyn Write,
) -> Result<()> {
    let mut next: usize = 0;
    let mut blobs: BTreeMap<&str, usize> = BTreeMap::new();
    let mut marks: Vec<usize> = Vec::with_capacity(plan.commits.len());
    for commit in &plan.commits {
        for file in &commit.files {
            if blobs.contains_key(file.object.as_str()) {
                continue;
            }
            let bytes =
                read(&file.object).with_context(|| format!("while reading {}", file.source))?;
            next = next.saturating_add(1);
            writeln!(out, "blob\nmark :{next}")?;
            write_data(out, &bytes)?;
            blobs.insert(file.object.as_str(), next);
        }
        next = next.saturating_add(1);
        let who = format!("{} <{email}> {} +0000", commit.author, commit.timestamp);
        if commit.parents.is_empty() {
            writeln!(out, "reset refs/heads/main")?;
        }
        writeln!(
            out,
            "commit refs/heads/main\nmark :{next}\nauthor {who}\ncommitter {who}"
        )?;
        let mut message = commit.message.clone();
        if !message.ends_with('\n') {
            message.push('\n');
        }
        write_data(out, message.as_bytes())?;
        for (n, parent) in commit.parents.iter().enumerate() {
            let mark = marks
                .get(*parent)
                .ok_or_else(|| anyhow!("a parent was not written before its child"))?;
            writeln!(out, "{} :{mark}", if n == 0 { "from" } else { "merge" })?;
        }
        writeln!(out, "deleteall")?;
        for file in &commit.files {
            let mode = if file.mode & 0o111 == 0 {
                "100644"
            } else {
                "100755"
            };
            let blob = blobs
                .get(file.object.as_str())
                .ok_or_else(|| anyhow!("no content was written for {}", file.key))?;
            writeln!(out, "M {mode} :{blob} {}", quote(&file.key))?;
        }
        writeln!(out, "M 100644 inline {}", quote(".dotman.toml"))?;
        write_data(out, commit.config.as_bytes())?;
        writeln!(out)?;
        marks.push(next);
    }
    writeln!(out, "done")?;
    Ok(())
}

/// Converts the legacy history at `root` into the repo's `main` branch.
pub(super) fn import(ctx: &Context, root: &Path, args: &ImportArgs) -> Result<Outcome> {
    require_fresh(ctx)?;
    let email = author_email(ctx)?;
    let history = load_history(root, &head(root)?)?;
    let mut plan = plan(&history, &homes(ctx), &|hash| {
        object_path(root, hash).is_file()
    })?;
    if root.join("remote-mappings.toml").is_file() {
        plan.report.notes.push(
            "remote-mappings.toml is ignored: the commits are rebuilt without a mapping to any remote"
                .to_owned(),
        );
    }
    let edited = EditedConfig::find(ctx)?;
    if let Some(edited) = &edited {
        plan.report.notes.push(edited.note(args.dry_run));
    }

    // Built before anything is written, so an unreadable object stops the import with the repo
    // untouched; a dry run builds it too, to find the same problems.
    let mut stream = tempfile::tempfile().context("cannot create a temp file for the import")?;
    let mut out = BufWriter::new(&mut stream);
    write_stream(&plan, &email, &mut |hash| read_object(root, hash), &mut out)?;
    out.flush()?;
    drop(out);
    stream.rewind()?;

    plan.report.print();
    let commits = count(plan.commits.len(), "commit");
    let repo = &ctx.paths.repo;
    if args.dry_run {
        output::info(&format!("would import {commits} into {}", repo.display()));
        return Ok(plan.report.outcome());
    }
    let question = format!("import {commits} into {}? [y/N]", repo.display());
    if !proceed(args, &question)? {
        output::info("nothing imported");
        return Ok(Outcome::Done);
    }
    ctx.git.init()?;
    if let Some(edited) = &edited {
        edited.back_up()?;
    }
    ctx.git.fast_import(stream)?;
    ctx.git.checkout_head()?;
    output::success(&format!("imported {commits} into {}", repo.display()));
    output::info("next: run `dot status`, then `dot apply`");
    Ok(plan.report.outcome())
}

enum ModeChange {
    Keep,
    Set(u32),
    /// The legacy mode has an executable bit the repo file lacks.
    Executable(u32),
    Differs(String),
}

/// An existing `[modes]` entry is never overridden. Without one, an executable bit the repo file
/// lacks is restored, because dotman records that bit as the git file mode and not in `[modes]`.
/// A repo file that is executable where the legacy mode is not keeps its bit and is reported.
/// Other differences go into `[modes]`.
fn mode_change(
    modes: &BTreeMap<String, String>,
    key: &str,
    on_disk: u32,
    legacy: u32,
) -> Result<ModeChange> {
    if let Some(recorded) = recorded_mode(modes, key)? {
        return Ok(if recorded == legacy {
            ModeChange::Keep
        } else {
            ModeChange::Differs(format!(
                "{key}: [modes] has {recorded:04o}, the legacy mode is {legacy:04o}"
            ))
        });
    }
    let repo_executable = on_disk & 0o100 != 0;
    let legacy_executable = legacy & 0o111 != 0;
    let default = if repo_executable { 0o755 } else { 0o644 };
    Ok(if legacy == default {
        ModeChange::Keep
    } else if legacy_executable && !repo_executable {
        ModeChange::Executable(legacy)
    } else if repo_executable && !legacy_executable {
        ModeChange::Differs(format!(
            "{key}: the repo file is executable and the legacy mode {legacy:04o} is not; the bit is left as it is"
        ))
    } else {
        ModeChange::Set(legacy)
    })
}

struct Addition {
    key: String,
    mode: u32,
    bytes: Vec<u8>,
}

/// A mode the overlay restores: the executable bit first if `executable`, then the entry in
/// `[modes]` if `record`.
struct ModeFix {
    key: String,
    executable: bool,
    record: Option<u32>,
}

struct Overlay {
    report: Report,
    fixes: Vec<ModeFix>,
    files: Vec<Addition>,
    /// Keys the repo lacks and `compare` left out.
    missing: Vec<String>,
}

/// Compares the legacy `HEAD` snapshot with the worktree. Content is never changed.
fn compare(ctx: &Context, root: &Path, include_missing: bool) -> Result<Overlay> {
    let snapshot = load_snapshot(root, &head(root)?)?;
    let Selection { chosen, dropped } = select(&snapshot, &homes(ctx), &|hash| {
        object_path(root, hash).is_file()
    });
    let repo = &ctx.paths.repo;
    let config = ctx.repo_config()?;
    let sources = ctx.sources()?;
    let mut overlay = Overlay {
        report: Report::default(),
        fixes: Vec::new(),
        files: Vec::new(),
        missing: Vec::new(),
    };
    let mut needs = |reason: &'static str, detail: String| {
        overlay.report.needs_action.push(Item { reason, detail });
    };
    let mut converted: usize = 0;
    for (_, (reason, detail)) in dropped {
        needs(reason, detail);
    }
    for entry in chosen {
        let key = entry.key.as_str();
        match read_node(repo, key)? {
            Node::Missing if sources.contains(key) => needs(
                BLOCKED,
                format!(
                    "{key}: the repo keeps it as a variant, template or encrypted file; not compared"
                ),
            ),
            Node::Missing if include_missing => {
                overlay.files.push(Addition {
                    key: entry.key.clone(),
                    mode: entry.mode,
                    bytes: read_object(root, &entry.object)?,
                });
                converted = converted.saturating_add(1);
            }
            Node::Missing => {
                overlay.missing.push(entry.key.clone());
                needs(
                    MISSING,
                    format!("{key}: not in the repo; `--include-missing` adds it"),
                );
            }
            Node::File { bytes, mode, .. } => {
                let change = mode_change(&config.modes, key, mode, entry.mode)?;
                match &change {
                    ModeChange::Set(mode) => overlay.fixes.push(ModeFix {
                        key: entry.key.clone(),
                        executable: false,
                        record: Some(*mode),
                    }),
                    ModeChange::Executable(mode) => overlay.fixes.push(ModeFix {
                        key: entry.key.clone(),
                        executable: true,
                        record: (*mode != 0o755).then_some(*mode),
                    }),
                    ModeChange::Keep | ModeChange::Differs(_) => {}
                }
                if bytes != read_object(root, &entry.object)? {
                    needs(DIFFERS, key.to_owned());
                } else if let ModeChange::Differs(detail) = change {
                    needs(MODE_DIFFERS, detail);
                } else {
                    converted = converted.saturating_add(1);
                }
            }
            Node::Symlink(_) | Node::Dir | Node::Special | Node::BadParent(_) => needs(
                BLOCKED,
                format!(
                    "{key}: the repo has a symlink, a directory or another kind of entry there"
                ),
            ),
        }
    }
    overlay.report.converted = converted;
    if root.join("remote-mappings.toml").is_file() {
        overlay
            .report
            .notes
            .push("remote-mappings.toml is ignored".to_owned());
    }
    Ok(overlay)
}

/// Whether to add the files the repo lacks: yes with `--include-missing`, which `compare` has
/// already applied, or when an interactive run is asked once and says yes. `--yes`, `--dry-run`
/// and a non-interactive run never ask, so the files stay out.
pub(super) fn add_missing(args: &ImportArgs, missing: &[String]) -> Result<bool> {
    if missing.is_empty() || args.yes || args.dry_run || !prompt::interactive() {
        return Ok(false);
    }
    output::warning("the repo lacks these files:");
    for key in missing {
        output::warning(&format!("  {key}"));
    }
    prompt::confirm(&format!(
        "add {}? [y/N]",
        count(missing.len(), "missing file")
    ))
}

/// Opens without following a symlink, so a link swapped in meanwhile cannot redirect the chmod,
/// and sets the mode through the handle.
fn make_executable(path: &Path) -> Result<()> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("cannot open {}", path.display()))?;
    ensure!(
        file.metadata()?.is_file(),
        "{} is no longer a regular file",
        path.display()
    );
    file.set_permissions(Permissions::from_mode(0o755))?;
    Ok(())
}

/// Adds the legacy `HEAD`'s modes, and with `--include-missing` its missing files, to the repo as
/// one commit.
pub(super) fn overlay(ctx: &Context, root: &Path, args: &ImportArgs) -> Result<Outcome> {
    ctx.repo_id()?;
    require_clean(ctx)?;
    let mut found = compare(ctx, root, args.include_missing)?;
    if add_missing(args, &found.missing)? {
        found = compare(ctx, root, true)?;
    }
    let Overlay {
        report,
        fixes,
        files,
        ..
    } = found;
    let changed = !fixes.is_empty() || !files.is_empty();
    if changed && !ctx.git.identity_configured()? {
        bail!(IDENTITY_FIX);
    }
    report.print();
    let summary = format!(
        "{}, {}",
        count(fixes.len(), "mode"),
        count(files.len(), "file")
    );
    if !changed {
        output::info("nothing to overlay; no commit made");
        return Ok(report.outcome());
    }
    if args.dry_run {
        output::info(&format!("would commit: {OVERLAY_MESSAGE} ({summary})"));
        return Ok(report.outcome());
    }
    if !proceed(args, &format!("commit the overlay ({summary})? [y/N]"))? {
        output::info("nothing written");
        return Ok(Outcome::Done);
    }
    let repo = &ctx.paths.repo;
    let mut config = ctx.repo_config()?;
    for fix in fixes {
        if fix.executable {
            ensure_worktree_parents(repo, &fix.key)?;
            make_executable(&repo.join(&fix.key))?;
        }
        if let Some(mode) = fix.record {
            config.modes.insert(fix.key, format!("{mode:04o}"));
        }
    }
    for file in files {
        ensure_worktree_parents(repo, &file.key)?;
        let stored = default_mode(file.mode);
        write_atomic(&repo.join(&file.key), &file.bytes, stored)?;
        if file.mode != stored {
            config.modes.insert(file.key, format!("{:04o}", file.mode));
        }
    }
    config.save(&ctx.repo_config_file(), false)?;
    ctx.git.stage_all()?;
    if ctx.git.commit(OVERLAY_MESSAGE)?.is_some() {
        output::success(&format!("committed: {OVERLAY_MESSAGE} ({summary})"));
        output::info("next: run `dot status`, then `dot apply`");
    } else {
        output::info("nothing to overlay; no commit made");
    }
    Ok(report.outcome())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(mode: u32, hash: &str) -> SnapshotFile {
        SnapshotFile {
            hash: hash.to_owned(),
            mode,
            content_hash: hash.to_owned(),
        }
    }

    fn snapshot(id: &str, parents: &[&str], files: &[(&str, u32)]) -> (String, Snapshot) {
        let snapshot = Snapshot {
            commit: Commit {
                id: id.to_owned(),
                parents: parents.iter().map(|p| (*p).to_owned()).collect(),
                message: format!("message of {id}"),
                author: "Old Name <old@mail.invalid>".to_owned(),
                timestamp: 1_700_000_000,
                tree_hash: String::new(),
            },
            files: files
                .iter()
                .map(|(key, mode)| (PathBuf::from(key), file(*mode, "aa")))
                .collect(),
        };
        (id.to_owned(), snapshot)
    }

    fn planned(history: &[(String, Snapshot)], missing: &'static [&str]) -> Plan {
        let home = Path::new("/h/me");
        plan(history, &[home], &|hash| !missing.contains(&hash)).unwrap()
    }

    fn kept_keys(plan: &Plan) -> Vec<&str> {
        let first = plan.commits.first().unwrap();
        first.files.iter().map(|f| f.key.as_str()).collect()
    }

    fn stream_of(plan: &Plan) -> String {
        let mut out = Vec::new();
        write_stream(
            plan,
            "me@mail.invalid",
            &mut |_| Ok(b"x".to_vec()),
            &mut out,
        )
        .unwrap();
        String::from_utf8(out).unwrap()
    }

    #[test]
    fn keys_the_path_rules_refuse_are_reported_once_and_never_reach_the_stream() {
        let keys = [
            (".bashrc", 0o644),
            ("../evil", 0o644),
            (".git/config", 0o644),
            ("a/.GIT/x", 0o644),
            ("README.md", 0o644),
            (".dotman.toml", 0o644),
            ("/etc/passwd", 0o644),
            ("/home/u/", 0o644),
        ];
        let mut second: Vec<_> = keys.to_vec();
        second.push(("later", 0o644));
        let history = [snapshot("c1", &[], &keys), snapshot("c2", &["c1"], &second)];
        let plan = planned(&history, &[]);
        let report = &plan.report;
        assert_eq!(report.total(), 9, "distinct keys over the whole history");
        assert_eq!(report.converted, 2);
        assert_eq!(report.needs_action.len(), 7);
        for item in &report.needs_action {
            assert!(item.detail.contains("(in 2 of 2 commits)"), "{item:?}");
        }
        let text = stream_of(&plan);
        for bad in ["evil", "\".git/config\"", ".GIT", "README", "passwd", "\"/"] {
            assert!(!text.contains(bad), "{bad} reached the stream:\n{text}");
        }
        assert!(text.contains("\"later\""));
        assert_eq!(text.matches("M 100644 inline \".dotman.toml\"").count(), 2);
    }

    #[test]
    fn absolute_keys_lose_a_home_prefix_and_the_others_are_reported() {
        let history = [snapshot(
            "c1",
            &[],
            &[
                ("/h/me/.a", 0o644),
                ("/home/other/.b", 0o644),
                ("/Users/other/c/d", 0o644),
                ("/srv/e", 0o644),
            ],
        )];
        let plan = planned(&history, &[]);
        let kept = kept_keys(&plan);
        assert_eq!(kept, [".a", ".b", "c/d"]);
        assert_eq!(plan.report.converted, 3);
        let reasons: Vec<_> = plan.report.needs_action.iter().map(|i| i.reason).collect();
        assert_eq!(reasons, [OUTSIDE_HOME]);
        let notes = &plan.report.notes;
        assert!(
            notes.iter().any(|n| n.starts_with("3 absolute keys")),
            "{notes:?}"
        );
    }

    #[test]
    fn two_keys_for_one_path_keep_the_relative_one() {
        let history = [snapshot(
            "c1",
            &[],
            &[
                (".bashrc", 0o644),
                ("/home/u/.bashrc", 0o644),
                ("a", 0o644),
                ("a/b", 0o644),
            ],
        )];
        let plan = planned(&history, &[]);
        let kept = kept_keys(&plan);
        assert_eq!(kept, [".bashrc", "a/b"]);
        assert_eq!(plan.report.converted, 2);
        let reasons: Vec<_> = plan.report.needs_action.iter().map(|i| i.reason).collect();
        assert_eq!(reasons, [COLLISION, COLLISION]);
        assert_eq!(plan.report.total(), 4);
    }

    #[test]
    fn a_missing_object_is_reported_and_the_zero_hash_is_an_empty_file() {
        let (id, mut snap) = snapshot("c1", &[], &[("kept", 0o644), ("gone", 0o644)]);
        snap.files.insert(PathBuf::from("gone"), file(0o644, "bb"));
        snap.files.insert(
            PathBuf::from("empty"),
            file(0o644, "00000000000000000000000000000000"),
        );
        snap.files.insert(PathBuf::from("bad"), file(0o644, "../x"));
        let plan = planned(&[(id, snap)], &["bb"]);
        assert_eq!(plan.report.converted, 2);
        assert_eq!(plan.report.needs_action.len(), 2);
        assert!(
            plan.report
                .needs_action
                .iter()
                .all(|i| i.reason == NO_OBJECT)
        );
        let mut reads = Vec::new();
        let mut out = Vec::new();
        write_stream(
            &plan,
            "me@mail.invalid",
            &mut |hash| {
                reads.push(hash.to_owned());
                Ok(Vec::new())
            },
            &mut out,
        )
        .unwrap();
        assert_eq!(
            reads,
            ["00000000000000000000000000000000", "aa"],
            "in key order: empty, then kept"
        );
    }

    #[test]
    fn commits_come_parents_first_and_a_merge_keeps_its_parent_order() {
        let history = [
            snapshot("d", &["b", "c"], &[("f", 0o644)]),
            snapshot(
                "c",
                &["a", "00000000000000000000000000000000"],
                &[("f", 0o644)],
            ),
            snapshot("b", &["a", "a"], &[("f", 0o644)]),
            snapshot("a", &[], &[("f", 0o644)]),
        ];
        let loaded: BTreeMap<_, _> = history.into_iter().collect();
        let ordered = topological(loaded).unwrap();
        let ids: Vec<&str> = ordered.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids, ["a", "b", "c", "d"]);
        let plan = planned(&ordered, &[]);
        let parents: Vec<_> = plan.commits.iter().map(|c| c.parents.clone()).collect();
        assert_eq!(parents, [vec![], vec![0], vec![0], vec![1, 2]]);
        let text = stream_of(&plan);
        assert!(text.contains("from :3\nmerge :4\n"), "{text}");
        assert_eq!(text.matches("\nmerge :").count(), 1);
        assert_eq!(text.matches("reset refs/heads/main").count(), 1);
        assert!(text.ends_with("done\n"));
    }

    #[test]
    fn a_cycle_is_an_error() {
        let loaded: BTreeMap<_, _> = [snapshot("a", &["b"], &[]), snapshot("b", &["a"], &[])]
            .into_iter()
            .collect();
        let err = topological(loaded).unwrap_err().to_string();
        assert!(err.contains("cycle"), "{err}");
    }

    #[test]
    fn modes_become_git_modes_and_non_default_entries() {
        let history = [snapshot(
            "c1",
            &[],
            &[
                ("plain", 0o644),
                ("private", 0o600),
                ("script", 0o755),
                ("odd", 0o700),
                ("setuid", 0o4755),
            ],
        )];
        let plan = planned(&history, &[]);
        let commit = plan.commits.first().unwrap();
        let config = RepoConfig::parse(&commit.config).unwrap();
        let expected: BTreeMap<String, String> = [("odd", "0700"), ("private", "0600")]
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        assert_eq!(config.modes, expected);
        assert_eq!(
            RepoConfig {
                modes: BTreeMap::new(),
                ..config
            },
            RepoConfig::default()
        );
        let text = stream_of(&plan);
        for (mode, key) in [
            ("100644", "plain"),
            ("100644", "private"),
            ("100755", "script"),
            ("100755", "odd"),
            ("100755", "setuid"),
        ] {
            assert!(
                text.contains(&format!("M {mode} :1 \"{key}\"")),
                "{key}:\n{text}"
            );
        }
    }

    #[test]
    fn the_stream_names_the_importing_email_and_only_the_old_name() {
        let history = [snapshot("c1", &[], &[("a\"b\nc", 0o644)])];
        let text = stream_of(&planned(&history, &[]));
        let who = "Old Name <me@mail.invalid> 1700000000 +0000";
        assert!(
            text.contains(&format!("author {who}\ncommitter {who}\n")),
            "{text}"
        );
        assert!(!text.contains("old@mail.invalid"));
        assert!(text.contains("M 100644 :1 \"a\\\"b\\nc\"\n"), "{text}");
        assert!(text.contains("data 14\nmessage of c1\n"), "{text}");
        assert_eq!(author_name("<a@b>"), "unknown");
        assert_eq!(author_name("  Ann  <a@b>"), "Ann");
        assert_eq!(author_name("solo"), "solo");
    }

    #[test]
    fn oversized_input_is_refused_by_the_zstd_and_bincode_limits() {
        let packed = zstd::encode_all(&[0_u8; 4096][..], 1).unwrap();
        assert_eq!(inflate(&packed, 4096).unwrap().len(), 4096);
        let err = inflate(&packed, 4095).unwrap_err().to_string();
        assert!(err.contains("expands to more than 4095"), "{err}");

        let (_, snap) = snapshot("c1", &[], &[("f", 0o644)]);
        let bytes = bincode::serde::encode_to_vec(&snap, bincode::config::legacy()).unwrap();
        assert_eq!(decode::<BINCODE_LIMIT>(&bytes).unwrap().commit.id, "c1");
        assert!(decode::<16>(&bytes).is_err());

        // A string that claims 200 MB must fail before anything is allocated.
        let mut claim = 200_000_000_u64.to_le_bytes().to_vec();
        claim.extend_from_slice(b"short");
        assert!(decode::<BINCODE_LIMIT>(&claim).is_err());
    }

    #[test]
    fn ids_that_could_leave_the_store_are_refused() {
        for bad in ["", "../x", "a/b", "g1", &"a".repeat(65)] {
            assert!(check_id(bad, "id").is_err(), "{bad}");
        }
        assert!(check_id("c1c1", "id").is_ok());
        assert!(is_null("0000") && !is_null("") && !is_null("00a0"));
    }

    #[test]
    fn the_fixture_history_loads_in_order_with_its_merge() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/legacy-v2/repo");
        let head = head(&root).unwrap();
        assert_eq!(head, "c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3c3");
        let history = load_history(&root, &head).unwrap();
        let ids: Vec<&str> = history.iter().map(|(id, _)| id.as_str()).collect();
        assert_eq!(ids.len(), 4);
        assert_eq!(ids.last(), Some(&head.as_str()));
        assert!(ids.first().is_some_and(|id| id.starts_with("c1c1")));
        let merge = history.last().unwrap();
        assert_eq!(
            merge.1.parents(),
            [
                "c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2c2",
                "5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d5d"
            ]
        );
        let plan = plan(&history, &[Path::new("/h/me")], &|h| {
            object_path(&root, h).is_file()
        })
        .unwrap();
        assert_eq!((plan.report.total(), plan.report.converted), (6, 6));
        assert_eq!(
            read_object(&root, "00000000000000000000000000000000").unwrap(),
            b""
        );
    }

    fn change(modes: &[(&str, &str)], on_disk: u32, legacy: u32) -> ModeChange {
        let modes = modes
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        mode_change(&modes, "f", on_disk, legacy).unwrap()
    }

    #[test]
    fn a_lost_exec_bit_is_restored_and_an_extra_one_is_reported() {
        assert!(matches!(change(&[], 0o644, 0o644), ModeChange::Keep));
        assert!(matches!(change(&[], 0o755, 0o755), ModeChange::Keep));
        assert!(matches!(
            change(&[], 0o644, 0o755),
            ModeChange::Executable(0o755)
        ));
        assert!(matches!(
            change(&[], 0o644, 0o700),
            ModeChange::Executable(0o700)
        ));
        assert!(matches!(change(&[], 0o644, 0o600), ModeChange::Set(0o600)));
        assert!(matches!(change(&[], 0o755, 0o700), ModeChange::Set(0o700)));
        let ModeChange::Differs(why) = change(&[], 0o755, 0o644) else {
            unreachable!("an extra exec bit is reported");
        };
        assert!(why.contains("the repo file is executable"), "{why}");
        assert!(matches!(
            change(&[("f", "0640")], 0o644, 0o640),
            ModeChange::Keep
        ));
        assert!(matches!(
            change(&[("f", "0640")], 0o644, 0o755),
            ModeChange::Differs(_)
        ));
    }

    #[test]
    fn only_an_interactive_run_without_yes_or_dry_run_is_asked_about_missing_files() {
        let args = |yes, dry_run| ImportArgs {
            source: None,
            overlay: None,
            include_missing: false,
            render_templates: false,
            swap: false,
            yes,
            no_bootstrap: false,
            dry_run,
        };
        let missing = ["a".to_owned()];
        assert!(!add_missing(&args(false, false), &[]).unwrap());
        assert!(!add_missing(&args(true, false), &missing).unwrap());
        assert!(!add_missing(&args(false, true), &missing).unwrap());
    }

    #[test]
    fn one_absolute_key_reads_in_the_singular() {
        let one = planned(&[snapshot("c1", &[], &[("/home/o/.a", 0o644)])], &[]);
        assert_eq!(
            one.report.notes,
            ["1 absolute key was imported without its home prefix, for example /home/o/.a as .a"]
        );
        let two = planned(
            &[snapshot(
                "c1",
                &[],
                &[("/home/o/.a", 0o644), ("/home/o/.b", 0o644)],
            )],
            &[],
        );
        assert!(
            two.report
                .notes
                .iter()
                .any(|n| n.starts_with("2 absolute keys were imported without their home prefix")),
            "{:?}",
            two.report.notes
        );
    }
}
