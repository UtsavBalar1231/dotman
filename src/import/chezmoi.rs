//! The chezmoi importer.
//!
//! chezmoi evaluates `.chezmoiignore`, `.chezmoiremove` and `.chezmoiexternal.*` as templates on
//! every read of a source, runs the `modify_` script of a target it dumps, and runs the `[hooks]`
//! of the user's config. A hostile source must not cause any of that, so the default import never
//! lets chezmoi see the real source. It copies only the plain files and `symlink_` files into a
//! private directory and runs chezmoi there with a config of its own.
//!
//! The accounting unit is a regular file or a symlink of the source, found by our own walk of the
//! tree below `.chezmoiroot` and outside `.git`. Each one ends up in the repo or in the report.
//! Directories are not entries, and neither are chezmoi's own configuration files
//! (`.chezmoiignore`, `.chezmoidata*`, `.chezmoitemplates/`, `.chezmoiversion`, `.chezmoi.*`) nor
//! dot-files that chezmoi itself ignores; the notes say what became of those. The names of the
//! entries are decoded here, not by chezmoi, because chezmoi's own type filters do not agree with
//! each other (`managed --include=templates` omits a `symlink_*.tmpl`).

use super::gittree::{Staging, check_target, install_keeping_origin, shown};
use super::{
    EditedConfig, Item, Report, author_email, count, held, held_link, origin_hint, proceed,
    require_fresh,
};
use crate::cli::ImportArgs;
use crate::commands::Outcome;
use crate::commands::add::{MATCH, ensure_worktree_parents, record_mode};
use crate::context::{Context, is_real_dir};
use crate::engine::{parents, portable_link_target};
use crate::git::Git;
use crate::name;
use crate::output;
use crate::paths::{
    create_private_dir, find_collisions, is_reserved_root_name, private_temp_dir, validate_key,
    write_atomic,
};
use crate::proc;
use crate::repo_config::RepoConfig;
use crate::sys;
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use glob::Pattern;
use serde::Deserialize;
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::{OsStr, OsString};
use std::fs::{self, OpenOptions};
use std::io::{self, Read};
use std::os::unix::fs::{OpenOptionsExt, symlink};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use walkdir::WalkDir;

/// A source with more entries than this is refused, so a hostile tree cannot exhaust the disk.
const MAX_ENTRIES: usize = 100_000;
/// A source with more file content than this is refused, for the same reason.
const MAX_BYTES: u64 = 1 << 30;
const MAX_IGNORE_BYTES: u64 = 1 << 20;
/// Detection looks this deep, and at this many entries, for chezmoi's name encoding.
const DETECT_DEPTH: usize = 3;
const DETECT_ENTRIES: usize = 5_000;
const MAX_BRACES: usize = 8;

/// The umask goes into the config because the dumped permissions follow the process umask
/// otherwise, and an import must not depend on the shell it was started from.
const CONFIG: &str = "umask = 0o022\n";

const HINT: &str = "chezmoi is not installed, and reading a chezmoi source needs it. Install it with `pacman -S chezmoi`, `brew install chezmoi`, or see https://www.chezmoi.io/install/";

const TEMPLATE: &str = "templates, not run; `--render-templates` imports this host's rendering";
const SCRIPT: &str =
    "scripts, which dotman never runs: turn them into `[hooks]` or run them by hand";
const MODIFY: &str = "modify_ scripts, not run: dotman has no equivalent";
const ENCRYPTED: &str =
    "encrypted files: decrypt them with chezmoi, then store each with `{bin} add --encrypt`";
const EXTERNAL: &str = "externals, which dotman never fetches: install them separately";
const REMOVE: &str = "removals chezmoi performs, which dotman never does";
const IGNORED: &str =
    "ignored by .chezmoiignore on some or all machines; add it with `{bin} add` if you want it";
const SOURCE_LINK: &str = "symlinks inside the source, which chezmoi reads through and dotman does not follow; add the file they point to with `{bin} add`";
const ODD: &str = "entries that are not regular files or symlinks, or whose names are not UTF-8";
const NO_TARGET: &str = "entries chezmoi produced no file for";
const MISORDERED: &str = "entries whose attributes are out of chezmoi's order";
/// The attributes of a regular file, in the order chezmoi reads them.
const FILE_ATTRS: [&str; 5] = [
    "encrypted_",
    "private_",
    "readonly_",
    "empty_",
    "executable_",
];
const UNMANAGEABLE: &str = "paths dotman cannot manage";
const CLASH: &str = "paths that clash with another path of the import";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    File,
    Create,
    Symlink,
    Template,
    Script,
    Modify,
    Remove,
    Encrypted,
    External,
    /// A symlink in the source tree itself.
    Link,
    Odd,
}

impl Class {
    const fn reason(self) -> Option<&'static str> {
        match self {
            Self::File | Self::Create | Self::Symlink => None,
            Self::Template => Some(TEMPLATE),
            Self::Script => Some(SCRIPT),
            Self::Modify => Some(MODIFY),
            Self::Remove => Some(REMOVE),
            Self::Encrypted => Some(ENCRYPTED),
            Self::External => Some(EXTERNAL),
            Self::Link => Some(SOURCE_LINK),
            Self::Odd => Some(ODD),
        }
    }
}

#[derive(Debug, Default, Clone, Copy)]
// reason: independent source-name flags, not a state machine
#[allow(clippy::struct_excessive_bools)]
struct Attrs {
    private: bool,
    executable: bool,
    readonly: bool,
    symlink: bool,
}

impl Attrs {
    /// What chezmoi gives a file under umask 022.
    const fn file_perm(self) -> u32 {
        let mut perm = if self.executable { 0o755 } else { 0o644 };
        if self.private {
            perm &= !0o077;
        }
        if self.readonly {
            perm &= !0o222;
        }
        perm
    }

    const fn dir_perm(self) -> u32 {
        let mut perm = if self.private { 0o700 } else { 0o755 };
        if self.readonly {
            perm &= !0o222;
        }
        perm
    }
}

/// The target name, and whether chezmoi's `dot_` prefix made it a dot-file.
fn dot_name(rest: &str) -> (String, bool) {
    match (rest.strip_prefix("literal_"), rest.strip_prefix("dot_")) {
        (Some(literal), _) => (literal.to_owned(), false),
        (None, Some(dotted)) => (format!(".{dotted}"), true),
        (None, None) => (rest.to_owned(), false),
    }
}

struct Decoded {
    class: Class,
    name: String,
    attrs: Attrs,
    /// The name carried chezmoi's `dot_` prefix.
    dotted: bool,
}

fn take_prefix<'a>(name: &'a str, prefixes: &[&'static str]) -> Option<(&'static str, &'a str)> {
    prefixes
        .iter()
        .find_map(|prefix| name.strip_prefix(prefix).map(|rest| (*prefix, rest)))
}

fn strip_any<'a>(name: &'a str, prefixes: &[&'static str]) -> Option<&'a str> {
    take_prefix(name, prefixes).map(|(_, rest)| rest)
}

/// The class and target name of a source file name, as chezmoi's attribute grammar reads it.
fn decode_file(name: &str) -> Decoded {
    let mut rest = name;
    let mut attrs = Attrs::default();
    let mut class = Class::File;
    for (prefix, kind) in [
        ("create_", Class::Create),
        ("modify_", Class::Modify),
        ("remove_", Class::Remove),
        ("run_", Class::Script),
        ("symlink_", Class::Symlink),
    ] {
        if let Some(after) = rest.strip_prefix(prefix) {
            class = kind;
            rest = after;
            break;
        }
    }
    if class == Class::Script {
        rest = strip_any(rest, &["once_", "onchange_"]).unwrap_or(rest);
        rest = strip_any(rest, &["before_", "after_"]).unwrap_or(rest);
    }
    attrs.symlink = class == Class::Symlink;
    let mut encrypted = false;
    while let Some((prefix, after)) = take_prefix(rest, &FILE_ATTRS) {
        match prefix {
            "encrypted_" => encrypted = true,
            "private_" => attrs.private = true,
            "readonly_" => attrs.readonly = true,
            "executable_" => attrs.executable = true,
            _ => {}
        }
        rest = after;
    }
    let (name, dotted) = dot_name(rest);
    let (name, template) = match (name.strip_suffix(".literal"), name.strip_suffix(".tmpl")) {
        (Some(plain), _) => (plain.to_owned(), false),
        (None, Some(plain)) => (plain.to_owned(), true),
        (None, None) => (name, false),
    };
    if matches!(class, Class::File | Class::Create | Class::Symlink) {
        if encrypted {
            class = Class::Encrypted;
        } else if template {
            class = Class::Template;
        }
    }
    Decoded {
        class,
        name,
        attrs,
        dotted,
    }
}

struct DecodedDir {
    name: String,
    attrs: Attrs,
    exact: bool,
    dotted: bool,
}

fn decode_dir(name: &str) -> DecodedDir {
    let mut rest = name;
    let mut attrs = Attrs::default();
    let mut exact = false;
    while let Some((prefix, after)) =
        take_prefix(rest, &["exact_", "external_", "private_", "readonly_"])
    {
        match prefix {
            "exact_" => exact = true,
            "private_" => attrs.private = true,
            "readonly_" => attrs.readonly = true,
            _ => {}
        }
        rest = after;
    }
    let (name, dotted) = dot_name(rest);
    DecodedDir {
        name,
        attrs,
        exact,
        dotted,
    }
}

#[derive(Debug)]
struct Entry {
    /// Path in the source, relative to its root.
    rel: String,
    target: String,
    class: Class,
    attrs: Attrs,
}

impl Entry {
    fn detail(&self) -> String {
        match self.class {
            Class::File
            | Class::Create
            | Class::Symlink
            | Class::Template
            | Class::Encrypted
            | Class::Modify
                if self.rel != self.target =>
            {
                format!("{} (target {})", self.rel, self.target)
            }
            _ => self.rel.clone(),
        }
    }

    fn item(&self, reason: &'static str) -> Item {
        Item {
            reason,
            detail: self.detail(),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Zone {
    Plain,
    Scripts,
    Externals,
}

#[derive(Default)]
struct Tree {
    entries: Vec<Entry>,
    /// Target path of a directory to its attributes.
    dirs: BTreeMap<String, Attrs>,
    exact: Vec<String>,
    /// Target prefix and absolute path of each `.chezmoiignore`.
    ignores: Vec<(String, PathBuf)>,
    /// chezmoi's own configuration files and directories.
    own: Vec<String>,
    /// Dot-files that chezmoi itself ignores.
    hidden: usize,
    seen: usize,
    bytes: u64,
}

fn join(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_owned()
    } else {
        format!("{base}/{name}")
    }
}

/// Follows `.chezmoiroot`, which names a subdirectory of the checkout.
fn resolve_root(source: &Path) -> Result<PathBuf> {
    let file = source.join(".chezmoiroot");
    let regular = fs::symlink_metadata(&file).is_ok_and(|meta| meta.is_file());
    if !regular {
        return Ok(source.to_path_buf());
    }
    let text = fs::read_to_string(&file).context("cannot read .chezmoiroot")?;
    let name = text.trim();
    let relative = Path::new(name);
    ensure!(
        !name.is_empty()
            && relative
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_))),
        ".chezmoiroot must name a subdirectory of the source, got `{name}`"
    );
    let root = source.join(relative);
    let inside = fs::canonicalize(&root)
        .ok()
        .zip(fs::canonicalize(source).ok())
        .is_some_and(|(root, source)| root.starts_with(source));
    ensure!(
        is_real_dir(&root) && inside,
        ".chezmoiroot names `{name}`, which is not a directory inside the source"
    );
    Ok(root)
}

fn scan(root: &Path, rel: &str, prefix: &str, zone: Zone, tree: &mut Tree) -> Result<()> {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let mut names: Vec<OsString> = fs::read_dir(&dir)
        .with_context(|| format!("cannot read {}", dir.display()))?
        .map(|entry| entry.map(|entry| entry.file_name()))
        .collect::<io::Result<_>>()
        .with_context(|| format!("cannot read {}", dir.display()))?;
    names.sort();
    for os in names {
        tree.seen = tree.seen.saturating_add(1);
        ensure!(
            tree.seen <= MAX_ENTRIES,
            "the source has more than {MAX_ENTRIES} entries; `{} import` will not read a tree that large",
            name::bin()
        );
        visit(root, rel, prefix, zone, &os, tree)?;
    }
    Ok(())
}

fn visit(
    root: &Path,
    rel: &str,
    prefix: &str,
    zone: Zone,
    os: &OsStr,
    tree: &mut Tree,
) -> Result<()> {
    let path = root.join(rel).join(os);
    let meta =
        fs::symlink_metadata(&path).with_context(|| format!("cannot stat {}", path.display()))?;
    let lossy = os.to_string_lossy();
    let child = join(rel, &lossy);
    let Some(name) = os.to_str() else {
        tree.entries.push(Entry {
            target: child.clone(),
            rel: child,
            class: Class::Odd,
            attrs: Attrs::default(),
        });
        return Ok(());
    };
    if name == ".git" {
        return Ok(());
    }
    let class_here = if meta.file_type().is_symlink() {
        Class::Link
    } else if meta.is_file() {
        Class::File
    } else {
        Class::Odd
    };
    let leaf = |class: Class, target: String, attrs: Attrs| Entry {
        rel: child.clone(),
        target,
        class,
        attrs,
    };
    match zone {
        Zone::Scripts | Zone::Externals => {
            if meta.is_dir() {
                return scan(root, &child, "", zone, tree);
            }
            let class = match (class_here, zone) {
                (Class::Odd | Class::Link, _) => class_here,
                (_, Zone::Scripts) => Class::Script,
                _ => Class::External,
            };
            tree.entries
                .push(leaf(class, child.clone(), Attrs::default()));
        }
        Zone::Plain if name.starts_with('.') => {
            visit_dot(root, rel, name, &meta, &child, class_here, tree)?;
        }
        Zone::Plain if meta.is_dir() => {
            let decoded = decode_dir(name);
            let target = join(prefix, &decoded.name);
            tree.dirs.insert(target.clone(), decoded.attrs);
            if decoded.exact {
                tree.exact.push(target.clone());
            }
            scan(root, &child, &target, Zone::Plain, tree)?;
        }
        Zone::Plain => {
            let decoded = decode_file(name);
            let target = join(prefix, &decoded.name);
            let class = match class_here {
                Class::File => decoded.class,
                other => other,
            };
            if meta.is_file() {
                tree.bytes = tree.bytes.saturating_add(meta.len());
                ensure!(
                    tree.bytes <= MAX_BYTES,
                    "the source holds more than {} GiB of files; `{} import` will not read a tree that large",
                    MAX_BYTES >> 30,
                    name::bin()
                );
            }
            tree.entries.push(leaf(class, target, decoded.attrs));
        }
    }
    Ok(())
}

fn visit_dot(
    root: &Path,
    rel: &str,
    name: &str,
    meta: &fs::Metadata,
    child: &str,
    class_here: Class,
    tree: &mut Tree,
) -> Result<()> {
    let at_root = rel.is_empty();
    let file_like = matches!(class_here, Class::File | Class::Link);
    let entry = |class: Class| Entry {
        rel: child.to_owned(),
        target: child.to_owned(),
        class,
        attrs: Attrs::default(),
    };
    if at_root && meta.is_dir() && name == ".chezmoiscripts" {
        return scan(root, child, "", Zone::Scripts, tree);
    }
    if at_root && meta.is_dir() && name == ".chezmoiexternals" {
        return scan(root, child, "", Zone::Externals, tree);
    }
    if name.starts_with(".chezmoiexternal") && file_like {
        tree.entries.push(entry(Class::External));
    } else if name.starts_with(".chezmoiremove") && file_like {
        tree.entries.push(entry(Class::Remove));
    } else if name == ".chezmoiignore" && class_here == Class::File {
        let prefix = rel.to_owned();
        tree.ignores.push((prefix, root.join(child)));
        tree.own.push(child.to_owned());
    } else if name.starts_with(".chezmoi") {
        tree.own.push(child.to_owned());
    } else {
        tree.hidden = tree.hidden.saturating_add(1);
    }
    Ok(())
}

/// Patterns of `.chezmoiignore` files, read as text.
#[derive(Default)]
struct Ignore {
    include: Vec<Pattern>,
    exclude: Vec<Pattern>,
    unusable: Vec<String>,
    templated: bool,
}

/// `text` without its `{{ ... }}` actions, which may span lines, and whether it had any.
fn strip_actions(text: &str) -> (String, bool) {
    let mut out = String::new();
    let mut rest = text;
    let mut any = false;
    while let Some(start) = rest.find("{{") {
        any = true;
        out.push_str(rest.get(..start).unwrap_or_default());
        let after = rest.get(start.saturating_add(2)..).unwrap_or_default();
        rest = after.find("}}").map_or("", |end| {
            after.get(end.saturating_add(2)..).unwrap_or_default()
        });
    }
    out.push_str(rest);
    (out, any)
}

/// `a/{b,c}` as `a/b` and `a/c`, which doublestar accepts and `glob` does not.
fn expand_braces(pattern: &str) -> Vec<String> {
    let Some(open) = pattern.find('{') else {
        return vec![pattern.to_owned()];
    };
    let Some(close) = pattern
        .get(open..)
        .and_then(|tail| tail.find('}'))
        .map(|at| at.saturating_add(open))
    else {
        return vec![pattern.to_owned()];
    };
    let head = pattern.get(..open).unwrap_or_default();
    let body = pattern
        .get(open.saturating_add(1)..close)
        .unwrap_or_default();
    let tail = pattern.get(close.saturating_add(1)..).unwrap_or_default();
    body.split(',')
        .flat_map(|alternative| expand_braces(&format!("{head}{alternative}{tail}")))
        .collect()
}

impl Ignore {
    /// `prefix` is the target path of the directory that holds the file, which its patterns are
    /// relative to.
    fn add_file(&mut self, text: &str, prefix: &str) {
        let (plain, templated) = strip_actions(text);
        self.templated |= templated;
        for line in plain.lines() {
            let line = line
                .split_once(" #")
                .map_or(line, |(before, _)| before)
                .trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (negated, body) = line
                .strip_prefix('!')
                .map_or((false, line), |body| (true, body.trim_start()));
            // A `!` inside a conditional may undo a pattern on one host only.
            if negated && templated {
                continue;
            }
            let body = body.trim_start_matches('/').trim_end_matches('/');
            if body.is_empty() || body.matches('{').count() > MAX_BRACES {
                self.unusable.push(line.to_owned());
                continue;
            }
            let full = join(prefix, body);
            for expanded in expand_braces(&full) {
                match Pattern::new(&expanded) {
                    Ok(pattern) if negated => self.exclude.push(pattern),
                    Ok(pattern) => self.include.push(pattern),
                    Err(_) => self.unusable.push(line.to_owned()),
                }
            }
        }
    }

    /// A pattern matches a path or any directory above it.
    fn ignores(&self, key: &str) -> bool {
        let hit = |set: &[Pattern]| {
            parents(key)
                .chain(std::iter::once(key))
                .any(|path| set.iter().any(|pattern| pattern.matches_with(path, MATCH)))
        };
        hit(&self.include) && !hit(&self.exclude)
    }
}

fn read_ignore(path: &Path) -> Result<String> {
    let mut text = String::new();
    fs::File::open(path)
        .and_then(|file| file.take(MAX_IGNORE_BYTES).read_to_string(&mut text))
        .with_context(|| format!("cannot read {}", path.display()))?;
    Ok(text)
}

fn key_problem(key: &str) -> Option<String> {
    let first = key.split('/').next().unwrap_or_default();
    if matches!(
        first.to_lowercase().as_str(),
        ".git" | ".dotman" | ".dotman.toml"
    ) {
        return Some("dotman keeps its own files under that name".to_owned());
    }
    // A reserved root name is kept in the repo as an unmanaged file; the other rules still hold.
    let probe = if is_reserved_root_name(first) {
        format!("_/{key}")
    } else {
        key.to_owned()
    };
    validate_key(&probe).err().map(|e| e.to_string())
}

/// What the entries that can become files turn into before chezmoi is asked about them.
struct Screened<'a> {
    wanted: Vec<&'a Entry>,
    items: Vec<Item>,
}

fn screen<'a>(tree: &'a Tree, ignore: Option<&Ignore>, render: bool) -> Screened<'a> {
    let mut screened = Screened {
        wanted: Vec::new(),
        items: Vec::new(),
    };
    for entry in &tree.entries {
        let wanted = match entry.class {
            Class::File | Class::Create | Class::Symlink => true,
            Class::Template => render,
            other => {
                if let Some(reason) = other.reason() {
                    screened.items.push(entry.item(reason));
                }
                continue;
            }
        };
        if !wanted {
            screened.items.push(entry.item(TEMPLATE));
        } else if ignore.is_some_and(|ignore| ignore.ignores(&entry.target)) {
            screened.items.push(entry.item(IGNORED));
        } else if let Some(problem) = key_problem(&entry.target) {
            screened.items.push(Item {
                reason: UNMANAGEABLE,
                detail: format!("{}: {problem}", entry.rel),
            });
        } else {
            screened.wanted.push(entry);
        }
    }
    screened
}

/// Copies what chezmoi may read into `dest`. Nothing is followed: a file that turned into a
/// symlink since the scan fails the open.
fn copy_wanted(root: &Path, dest: &Path, wanted: &[&Entry]) -> Result<()> {
    create_private_dir(dest, false).with_context(|| format!("cannot create {}", dest.display()))?;
    for entry in wanted {
        let to = dest.join(&entry.rel);
        if let Some(parent) = to.parent() {
            create_private_dir(parent, false)
                .with_context(|| format!("cannot create {}", parent.display()))?;
        }
        copy_file(&root.join(&entry.rel), &to)?;
    }
    if fs::symlink_metadata(root.join(".chezmoiversion")).is_ok_and(|meta| meta.is_file()) {
        copy_file(&root.join(".chezmoiversion"), &dest.join(".chezmoiversion"))?;
    }
    Ok(())
}

fn copy_file(from: &Path, to: &Path) -> Result<()> {
    let mut source = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(from)
        .with_context(|| format!("cannot read {}", from.display()))?;
    let mut copy = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(to)
        .with_context(|| format!("cannot create {}", to.display()))?;
    io::copy(&mut source, &mut copy).with_context(|| format!("cannot copy {}", from.display()))?;
    Ok(())
}

struct Runner {
    program: PathBuf,
    global: Vec<OsString>,
    cwd: PathBuf,
}

impl Runner {
    fn run<I, S>(&self, args: I) -> Result<Vec<u8>>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        let args: Vec<OsString> = args
            .into_iter()
            .map(|arg| arg.as_ref().to_owned())
            .collect();
        let name = args
            .first()
            .map(|arg| arg.to_string_lossy().into_owned())
            .unwrap_or_default();
        let child = proc::command(&self.program)
            .args(&self.global)
            .args(&args)
            .current_dir(&self.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .context("cannot run chezmoi")?;
        // The scratch directory holds a copy of the dotfiles, which may include secrets. Ignored
        // only after the spawn, because an ignored signal stays ignored across exec.
        let out = {
            let _ignored =
                sys::ignore_signals(&[libc::SIGINT, libc::SIGQUIT, libc::SIGHUP, libc::SIGTERM]);
            child.wait_with_output()
        }
        .context("cannot run chezmoi")?;
        if !out.status.success() {
            let stderr = String::from_utf8_lossy(&out.stderr);
            let message: String = stderr.trim().chars().take(500).collect();
            bail!("`chezmoi {name}` failed ({}): {message}", out.status);
        }
        Ok(out.stdout)
    }
}

#[derive(Deserialize)]
struct Dumped {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    contents: Option<String>,
    #[serde(default)]
    perm: Option<u32>,
    #[serde(default)]
    linkname: Option<String>,
}

enum Content {
    /// A file of the source tree, read when it is written.
    Path(PathBuf),
    Bytes(Vec<u8>),
    Link(String),
}

#[derive(Default)]
struct Fetched {
    items: BTreeMap<String, (Content, u32)>,
    dir_perms: BTreeMap<String, u32>,
    failed: BTreeMap<String, String>,
}

struct Session<'a> {
    runner: &'a Runner,
    home: &'a Path,
    tree: &'a Tree,
}

impl Session<'_> {
    fn absolute(&self, target: &str) -> PathBuf {
        self.home.join(target)
    }

    fn managed(&self) -> Result<BTreeSet<String>> {
        Ok(String::from_utf8_lossy(&self.runner.run(["managed"])?)
            .lines()
            .map(str::to_owned)
            .collect())
    }

    fn trimmed_link(bytes: &[u8]) -> String {
        String::from_utf8_lossy(bytes).trim().to_owned()
    }

    fn cat(&self, target: &str) -> Result<Vec<u8>> {
        self.runner
            .run([OsStr::new("cat"), self.absolute(target).as_os_str()])
    }

    /// With `dump`: one run that returns every target. `file_root` holds the plain files.
    fn dump(&self, wanted: &[&Entry], file_root: &Path, render: bool) -> Result<Fetched> {
        let mut args: Vec<OsString> = ["dump", "-f", "json"].map(OsString::from).into();
        if render {
            args.push("--exclude=scripts,encrypted,externals".into());
            args.push("--parent-dirs".into());
            args.extend(
                wanted
                    .iter()
                    .map(|entry| self.absolute(&entry.target).into_os_string()),
            );
        } else {
            args.push("--exclude=templates,scripts,encrypted,externals".into());
        }
        let out = self.runner.run(&args)?;
        let dumped: BTreeMap<String, Dumped> = serde_json::from_slice::<Option<_>>(&out)
            .context("cannot read the output of `chezmoi dump`")?
            .unwrap_or_default();
        let mut fetched = Fetched::default();
        for (key, entry) in &dumped {
            if entry.kind == "dir" {
                fetched
                    .dir_perms
                    .insert(key.clone(), entry.perm.unwrap_or(0o755) & 0o777);
            }
        }
        for entry in wanted {
            match self.resolve_dumped(entry, dumped.get(&entry.target), file_root) {
                Ok(found) => {
                    fetched.items.insert(entry.target.clone(), found);
                }
                Err(why) => {
                    fetched.failed.insert(entry.target.clone(), why);
                }
            }
        }
        Ok(fetched)
    }

    fn resolve_dumped(
        &self,
        entry: &Entry,
        dumped: Option<&Dumped>,
        file_root: &Path,
    ) -> Result<(Content, u32), String> {
        let dumped = dumped.ok_or_else(|| {
            "chezmoi has no file for it here (an empty file needs the empty_ prefix, or chezmoi ignores it)"
                .to_owned()
        })?;
        match dumped.kind.as_str() {
            "symlink" => {
                let link = dumped.linkname.clone().unwrap_or_default();
                Ok((Content::Link(link), 0o777))
            }
            "file" => {
                let perm = dumped
                    .perm
                    .map_or_else(|| entry.attrs.file_perm(), |perm| perm & 0o777);
                let text = dumped.contents.as_deref().unwrap_or_default();
                if entry.class == Class::Template {
                    let bytes = if text.contains('\u{FFFD}') {
                        self.cat(&entry.target).map_err(|e| e.to_string())?
                    } else {
                        text.as_bytes().to_vec()
                    };
                    return Ok((Content::Bytes(bytes), perm));
                }
                // The JSON holds text, so a binary file comes back damaged; the source file is
                // used, once it is known to be the file chezmoi means.
                let path = file_root.join(&entry.rel);
                let bytes = fs::read(&path).map_err(|e| format!("cannot read it: {e}"))?;
                if String::from_utf8_lossy(&bytes) == text {
                    Ok((Content::Path(path), perm))
                } else {
                    Err("chezmoi's content for it differs from the source file".to_owned())
                }
            }
            other => Err(format!("chezmoi made a {other} of it")),
        }
    }

    /// Without `dump` entry types: `managed` for the list, `cat` for each file.
    fn cat_all(&self, wanted: &[&Entry]) -> Result<Fetched> {
        let listed = self.managed()?;
        let mut fetched = Fetched::default();
        for (target, attrs) in &self.tree.dirs {
            fetched.dir_perms.insert(target.clone(), attrs.dir_perm());
        }
        for entry in wanted {
            let result = if listed.contains(&entry.target) {
                self.cat(&entry.target).map_err(|e| e.to_string())
            } else {
                Err("chezmoi managed does not list it".to_owned())
            };
            match result {
                Ok(bytes) if entry.attrs.symlink => {
                    fetched.items.insert(
                        entry.target.clone(),
                        (Content::Link(Self::trimmed_link(&bytes)), 0o777),
                    );
                }
                Ok(bytes) => {
                    fetched.items.insert(
                        entry.target.clone(),
                        (Content::Bytes(bytes), entry.attrs.file_perm()),
                    );
                }
                Err(why) => {
                    fetched.failed.insert(entry.target.clone(), why);
                }
            }
        }
        Ok(fetched)
    }
}

/// Why chezmoi produced nothing for `entry`. A name with its attributes out of order is the
/// likely cause, since chezmoi then reads the rest as part of the name and targets another file.
fn no_target(entry: &Entry, why: &str) -> Item {
    let name = entry.rel.rsplit('/').next().unwrap_or(&entry.rel);
    in_chezmoi_order(name).map_or_else(
        || Item {
            reason: NO_TARGET,
            detail: format!("{}: {why}", entry.detail()),
        },
        |fixed| Item {
            reason: MISORDERED,
            detail: format!(
                "{}: chezmoi reads {} only in that order and takes the rest as part of the name; rename it to {fixed}",
                entry.detail(),
                FILE_ATTRS.join(", ")
            ),
        },
    )
}

/// `name` with its file attributes in chezmoi's order, when they are in another order.
fn in_chezmoi_order(name: &str) -> Option<String> {
    if ["remove_", "run_", "symlink_"]
        .iter()
        .any(|class| name.starts_with(class))
    {
        return None;
    }
    let (class, mut rest) = take_prefix(name, &["create_", "modify_"]).unwrap_or(("", name));
    let mut seen: Vec<usize> = Vec::new();
    while let Some((prefix, after)) = take_prefix(rest, &FILE_ATTRS) {
        seen.extend(FILE_ATTRS.iter().position(|attr| *attr == prefix));
        rest = after;
    }
    if seen.is_sorted_by(|a, b| a < b) {
        return None;
    }
    seen.sort_unstable();
    seen.dedup();
    let attrs: String = seen
        .iter()
        .filter_map(|at| FILE_ATTRS.get(*at).copied())
        .collect();
    Some(format!("{class}{attrs}{rest}"))
}

/// An entry the repo gets.
struct Write {
    key: String,
    content: Content,
    perm: u32,
}

/// Every wanted entry is a write or an item; clashes between the writes become items.
fn settle(
    ctx: &Context,
    wanted: &[&Entry],
    mut fetched: Fetched,
    items: &mut Vec<Item>,
) -> (Vec<Write>, BTreeMap<String, u32>) {
    let mut writes = Vec::new();
    for entry in wanted {
        if let Some(why) = fetched.failed.remove(&entry.target) {
            items.push(no_target(entry, &why));
            continue;
        }
        let Some((content, perm)) = fetched.items.remove(&entry.target) else {
            continue;
        };
        let content = match content {
            Content::Link(link) if link.is_empty() => {
                items.push(Item {
                    reason: NO_TARGET,
                    detail: format!("{}: the symlink has no target", entry.detail()),
                });
                continue;
            }
            Content::Link(link) => Content::Link(
                portable_link_target(
                    Path::new(&link),
                    &entry.target,
                    &ctx.paths.home,
                    ctx.paths.home_real.as_deref(),
                )
                .to_string_lossy()
                .into_owned(),
            ),
            other => other,
        };
        // A read that fails here fails again when the file is written, and is reported there.
        let found = match &content {
            Content::Bytes(bytes) => held(bytes, &entry.target, &[]),
            Content::Path(path) => fs::read(path)
                .ok()
                .and_then(|bytes| held(&bytes, &entry.target, &[])),
            Content::Link(link) => held_link(link, &entry.target),
        };
        if let Some(item) = found {
            items.push(item);
            continue;
        }
        writes.push(Write {
            key: entry.target.clone(),
            content,
            perm,
        });
    }
    let keys: BTreeSet<&str> = writes.iter().map(|write| write.key.as_str()).collect();
    let mut clashing: BTreeMap<String, String> = find_collisions(keys.iter().copied())
        .into_iter()
        .map(|(first, other)| {
            (
                other,
                format!("differs from {first} only in case or Unicode form"),
            )
        })
        .collect();
    for key in &keys {
        if parents(key).any(|parent| keys.contains(parent)) {
            clashing.insert(
                (*key).to_owned(),
                "a parent of it is also a file".to_owned(),
            );
        }
    }
    let source_of: BTreeMap<&str, &Entry> = wanted
        .iter()
        .map(|entry| (entry.target.as_str(), *entry))
        .collect();
    writes.retain(|write| {
        let Some(why) = clashing.get(&write.key) else {
            return true;
        };
        let detail = source_of
            .get(write.key.as_str())
            .map_or_else(|| write.key.clone(), |entry| entry.detail());
        items.push(Item {
            reason: CLASH,
            detail: format!("{detail}: {why}"),
        });
        false
    });
    (writes, fetched.dir_perms)
}

const fn stored_mode(perm: u32) -> u32 {
    if perm & 0o111 == 0 { 0o644 } else { 0o755 }
}

/// `.dotman.toml` for the import: the defaults plus the modes that differ from what dotman
/// stores by itself.
fn repo_config(writes: &[Write], dir_perms: &BTreeMap<String, u32>) -> Result<RepoConfig> {
    let mut config = RepoConfig::default();
    let modes = &mut config.modes;
    for write in writes {
        if is_reserved_root_name(write.key.split('/').next().unwrap_or_default()) {
            continue;
        }
        if !matches!(write.content, Content::Link(_)) {
            record_mode(modes, &write.key, write.perm, stored_mode(write.perm))?;
        }
        for parent in parents(&write.key) {
            let perm = dir_perms.get(parent).copied().unwrap_or(0o755);
            record_mode(modes, &format!("{parent}/"), perm, 0o755)?;
        }
    }
    Ok(config)
}

fn write_files(repo: &Path, writes: &[Write]) -> Result<()> {
    for write in writes {
        ensure_worktree_parents(repo, &write.key)?;
        let path = repo.join(&write.key);
        match &write.content {
            Content::Link(link) => symlink(link, &path)
                .with_context(|| format!("cannot create the symlink {}", write.key))?,
            Content::Bytes(bytes) => write_atomic(&path, bytes, stored_mode(write.perm))?,
            Content::Path(source) => {
                let bytes = fs::read(source)
                    .with_context(|| format!("cannot read {}", source.display()))?;
                write_atomic(&path, &bytes, stored_mode(write.perm))?;
            }
        }
    }
    Ok(())
}

/// The first existing, readable `chezmoi` config's `sourceDir`.
fn config_source_dir(ctx: &Context) -> Option<PathBuf> {
    let dir = ctx.paths.config_home.join("chezmoi");
    [
        "chezmoi.toml",
        "chezmoi.json",
        "chezmoi.yaml",
        "chezmoi.yml",
    ]
    .iter()
    .find_map(|name| {
        let text = fs::read_to_string(dir.join(name)).ok()?;
        let value = source_dir_from(name, &text)?;
        let path = if value == "~" {
            ctx.paths.home.clone()
        } else if let Some(rest) = value.strip_prefix("~/") {
            ctx.paths.home.join(rest)
        } else {
            PathBuf::from(value)
        };
        path.is_absolute().then_some(path)
    })
}

/// `sourceDir` from the text of a chezmoi config; chezmoi reads its keys without regard to case.
fn source_dir_from(file: &str, text: &str) -> Option<String> {
    let from_toml = |text: &str| {
        let table = text.parse::<toml::Table>().ok()?;
        table.iter().find_map(|(key, value)| {
            key.eq_ignore_ascii_case("sourceDir")
                .then(|| value.as_str().map(str::to_owned))
                .flatten()
        })
    };
    let from_json = |text: &str| {
        let value: serde_json::Value = serde_json::from_str(text).ok()?;
        value.as_object()?.iter().find_map(|(key, value)| {
            key.eq_ignore_ascii_case("sourceDir")
                .then(|| value.as_str().map(str::to_owned))
                .flatten()
        })
    };
    // No YAML parser: a top-level `sourceDir:` line is all that is read.
    let from_yaml = |text: &str| {
        text.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            if !key.eq_ignore_ascii_case("sourceDir") {
                return None;
            }
            let value = value
                .split_once(" #")
                .map_or(value, |(before, _)| before)
                .trim();
            let unquoted = ['"', '\''].iter().find_map(|quote| {
                value
                    .strip_prefix(*quote)
                    .and_then(|inner| inner.strip_suffix(*quote))
            });
            Some(unquoted.unwrap_or(value).to_owned()).filter(|value| !value.is_empty())
        })
    };
    match file {
        "chezmoi.toml" => from_toml(text),
        "chezmoi.json" => from_json(text),
        _ => from_yaml(text),
    }
}

/// A directory is a chezmoi source when it has `.chezmoiroot`, or a name chezmoi's grammar
/// writes with `dot_` or `.chezmoi`, near the top. A bare git directory has no files to read.
pub(super) fn is_source(dir: &Path) -> bool {
    if !is_real_dir(dir) {
        return false;
    }
    if dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir() {
        return false;
    }
    if fs::symlink_metadata(dir.join(".chezmoiroot")).is_ok_and(|meta| meta.is_file()) {
        return true;
    }
    WalkDir::new(dir)
        .follow_links(false)
        .max_depth(DETECT_DEPTH)
        .into_iter()
        .filter_entry(|entry| entry.file_name() != ".git")
        .take(DETECT_ENTRIES)
        .filter_map(Result::ok)
        .filter(|entry| entry.depth() > 0)
        .any(|entry| {
            let Some(name) = entry.file_name().to_str() else {
                return false;
            };
            name.starts_with(".chezmoi")
                || if entry.file_type().is_dir() {
                    decode_dir(name).dotted
                } else {
                    decode_file(name).dotted
                }
        })
}

/// chezmoi's usual source directories and the `sourceDir` of its config.
pub(super) fn candidates(ctx: &Context) -> Vec<PathBuf> {
    let mut found: Vec<PathBuf> = Vec::new();
    let mut real: Vec<PathBuf> = Vec::new();
    let listed = [
        ctx.paths.data_home.join("chezmoi"),
        ctx.paths.home.join(".local/share/chezmoi"),
    ]
    .into_iter()
    .chain(config_source_dir(ctx));
    for dir in listed {
        let resolved = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if is_source(&dir) && !real.contains(&resolved) {
            real.push(resolved);
            found.push(dir);
        }
    }
    found
}

fn warn_before_rendering(tree: &Tree) {
    output::warning(
        "--render-templates runs chezmoi's templates with its full function set (`output`, password managers and so on), using your chezmoi config and its hooks; chezmoi also evaluates .chezmoiignore, .chezmoiremove and .chezmoiexternal* as templates",
    );
    let templates: Vec<&Entry> = tree
        .entries
        .iter()
        .filter(|entry| entry.class == Class::Template)
        .collect();
    output::warning(&format!(
        "{} to be rendered:",
        count(templates.len(), "template")
    ));
    for entry in templates {
        output::warning(&format!("  {}", entry.detail()));
    }
}

fn notes(
    source: &Path,
    tree: &Tree,
    ignore: Option<&Ignore>,
    render: bool,
    plan: &Plan,
) -> Vec<String> {
    let mut notes = Vec::new();
    let origin = Git::new(source, "import")
        .remote_url("origin")
        .ok()
        .flatten();
    notes.push(format!(
        "imported from {}; chezmoi's git history is not imported{}",
        source.display(),
        origin.map_or_else(String::new, |url| {
            let (shown, set) = origin_hint(&url);
            format!(", and the source's own origin is {shown} ({set})")
        })
    ));
    notes.push(format!(
        "the new repo has no remote, as a local path is no use as one; add one with `{} init --remote <url>`",
        name::bin()
    ));
    if !tree.ignores.is_empty() {
        let unusable = ignore.map_or(0, |ignore| ignore.unusable.len());
        notes.push(format!(
            ".chezmoiignore was read as text and never run; chezmoi evaluates it as a template per machine, so a file it lists inside a condition may apply on another host only, and the files it matches are reported rather than imported{}",
            if unusable == 0 {
                String::new()
            } else {
                format!(" ({unusable} patterns could not be read and were skipped)")
            }
        ));
    }
    let own: Vec<String> = tree
        .own
        .iter()
        .filter(|name| !name.ends_with(".chezmoiignore"))
        .cloned()
        .collect();
    if !own.is_empty() {
        notes.push(format!(
            "chezmoi's own files are not carried over, and dotman does not read chezmoi data or templates: {}",
            shown(&own)
        ));
    }
    if !tree.exact.is_empty() {
        notes.push(format!(
            "exact_ directories ({}) make chezmoi delete files it does not manage there; dotman never deletes files",
            shown(&tree.exact)
        ));
    }
    let created = plan.created;
    if created > 0 {
        notes.push(format!(
            "{} imported as managed files; chezmoi only created them when the target was missing, and dotman keeps them in step",
            count(created, "create_ file")
        ));
    }
    if tree.hidden > 0 {
        notes.push(format!(
            "{} left out; chezmoi ignores dot-files that are not its own",
            count(tree.hidden, "dot-file")
        ));
    }
    if plan.reserved > 0 {
        notes.push(format!(
            "{} kept in the repo and never applied",
            count(plan.reserved, "root file")
        ));
    }
    if render {
        notes.extend(plan.rendered.iter().map(|target| {
            format!(
                "{target}: a template, imported as this host's rendering; it is a plain file now"
            )
        }));
    }
    notes
}

/// What the import will do, before anything is written.
struct Plan {
    writes: Vec<Write>,
    config: RepoConfig,
    report: Report,
    created: usize,
    reserved: usize,
    rendered: Vec<String>,
}

/// The scratch directory and the chezmoi it is set up for; dropping it removes the copy.
struct Prepared {
    _scratch: tempfile::TempDir,
    runner: Runner,
    copy: PathBuf,
    entry_types: bool,
}

/// Writes the config, asks which flags this chezmoi knows and builds the invocation. The render
/// run keeps the user's config and source and only moves the state and cache aside.
fn prepare(
    ctx: &Context,
    args: &ImportArgs,
    program: PathBuf,
    source: &Path,
    render: bool,
) -> Result<Prepared> {
    let base = if args.dry_run {
        std::env::temp_dir()
    } else {
        create_private_dir(&ctx.paths.state_dir, false)
            .with_context(|| format!("cannot create {}", ctx.paths.state_dir.display()))?;
        ctx.paths.state_dir.clone()
    };
    let scratch = private_temp_dir(".dotman-chezmoi-", &base)?;
    let dir = scratch.path();
    let config = dir.join("config.toml");
    fs::write(&config, CONFIG).context("cannot write the chezmoi config")?;
    let help = Runner {
        program: program.clone(),
        global: vec!["--config".into(), config.clone().into()],
        cwd: dir.to_path_buf(),
    };
    let help_text = String::from_utf8_lossy(&help.run(["help", "dump"])?).into_owned();
    let copy = dir.join("source");
    let mut global: Vec<OsString> = Vec::new();
    if !render {
        global.extend(["--config".into(), config.into()]);
    }
    global.extend([
        "--persistent-state".into(),
        dir.join("state.boltdb").into(),
        "--cache".into(),
        dir.join("cache").into(),
        "--source".into(),
        if render {
            source.into()
        } else {
            copy.clone().into()
        },
        "--destination".into(),
        ctx.paths.home.clone().into(),
        "--no-tty".into(),
    ]);
    if help_text.contains("--no-pager") {
        global.push("--no-pager".into());
    }
    let runner = Runner {
        program,
        global,
        cwd: dir.to_path_buf(),
    };
    Ok(Prepared {
        runner,
        copy,
        entry_types: help_text.contains("--exclude"),
        _scratch: scratch,
    })
}

/// Asks chezmoi for the entries in `wanted`, after copying them in the default run. In the render
/// run the entries this host does not manage are dropped from `wanted` and reported.
fn fetch(
    ctx: &Context,
    prepared: &Prepared,
    tree: &Tree,
    root: &Path,
    wanted: &mut Vec<&Entry>,
    items: &mut Vec<Item>,
    render: bool,
) -> Result<Fetched> {
    let session = Session {
        runner: &prepared.runner,
        home: &ctx.paths.home,
        tree,
    };
    if render {
        let listed = session.managed()?;
        wanted.retain(|entry| {
            let kept = listed.contains(&entry.target);
            if !kept {
                items.push(no_target(
                    entry,
                    "chezmoi does not manage it on this host (.chezmoiignore, or an empty file)",
                ));
            }
            kept
        });
    } else {
        copy_wanted(root, &prepared.copy, wanted)?;
    }
    if prepared.entry_types {
        let file_root = if render {
            root
        } else {
            prepared.copy.as_path()
        };
        session.dump(wanted, file_root, render)
    } else {
        session.cat_all(wanted)
    }
}

pub(super) fn import(ctx: &Context, source: &Path, args: &ImportArgs) -> Result<Outcome> {
    let program = proc::find_program("chezmoi").ok_or_else(|| anyhow!(HINT))?;
    require_fresh(ctx)?;
    author_email(ctx)?;
    check_target(&ctx.paths.repo)?;
    let edited = EditedConfig::find(ctx)?;
    let root = resolve_root(source)?;
    let mut tree = Tree::default();
    scan(&root, "", "", Zone::Plain, &mut tree)?;

    // A dry run never renders: rendering runs the user's commands.
    let render = args.render_templates && !args.dry_run;
    let mut ignore = Ignore::default();
    for (prefix, path) in &tree.ignores {
        ignore.add_file(&read_ignore(path)?, &decode_prefix(prefix));
    }
    let ignore = (!tree.ignores.is_empty()).then_some(ignore);
    // The render run asks chezmoi, which evaluates the ignore file for this host.
    let screened = screen(&tree, ignore.as_ref().filter(|_| !render), render);
    if render {
        warn_before_rendering(&tree);
        if !proceed(args, "run these templates? [y/N]")? {
            output::info("nothing imported");
            return Ok(Outcome::Done);
        }
    }

    let prepared = prepare(ctx, args, program, source, render)?;
    let (mut wanted, mut items) = (screened.wanted, screened.items);
    let fetched = fetch(
        ctx,
        &prepared,
        &tree,
        &root,
        &mut wanted,
        &mut items,
        render,
    )?;
    let mut plan = build_plan(ctx, &wanted, fetched, items)?;
    plan.report.notes = notes(source, &tree, ignore.as_ref(), render, &plan);
    if args.render_templates && args.dry_run {
        plan.report.notes.push(
            "--render-templates runs nothing in a dry run; this is the report without it"
                .to_owned(),
        );
    }
    if let Some(edited) = &edited {
        plan.report.notes.push(edited.note(args.dry_run));
    }
    plan.report.print();

    let repo = &ctx.paths.repo;
    let files = count(plan.writes.len(), "file");
    if args.dry_run {
        output::info(&format!(
            "would import {files} into {} as one commit",
            repo.display()
        ));
        return Ok(plan.report.outcome());
    }
    if !proceed(
        args,
        &format!("import {files} into {}? [y/N]", repo.display()),
    )? {
        output::info("nothing imported");
        return Ok(Outcome::Done);
    }
    let staging = Staging::new(ctx, true)?;
    staging.git.init()?;
    write_files(&staging.repo, &plan.writes)?;
    plan.config
        .save(&staging.repo.join(".dotman.toml"), false)?;
    staging.git.stage_all()?;
    staging
        .git
        .commit(&format!("import: chezmoi {}", source.display()))?;
    install_keeping_origin(ctx, &staging, edited.as_ref())?;
    output::success(&format!("imported {files} into {}", repo.display()));
    super::print_next_steps();
    Ok(plan.report.outcome())
}

/// A `.chezmoiignore` in a subdirectory is relative to that directory's target path.
fn decode_prefix(rel_dir: &str) -> String {
    rel_dir
        .split('/')
        .filter(|part| !part.is_empty())
        .fold(String::new(), |prefix, part| {
            join(&prefix, &decode_dir(part).name)
        })
}

fn build_plan(
    ctx: &Context,
    wanted: &[&Entry],
    fetched: Fetched,
    mut items: Vec<Item>,
) -> Result<Plan> {
    let (writes, dir_perms) = settle(ctx, wanted, fetched, &mut items);
    let by_target: BTreeMap<&str, &Entry> = wanted
        .iter()
        .map(|entry| (entry.target.as_str(), *entry))
        .collect();
    let class_of = |write: &Write| by_target.get(write.key.as_str()).map(|entry| entry.class);
    let created = writes
        .iter()
        .filter(|write| class_of(write) == Some(Class::Create))
        .count();
    let rendered = writes
        .iter()
        .filter(|write| class_of(write) == Some(Class::Template))
        .map(|write| write.key.clone())
        .collect();
    let reserved = writes
        .iter()
        .filter(|write| is_reserved_root_name(write.key.split('/').next().unwrap_or_default()))
        .count();
    let config = repo_config(&writes, &dir_perms)?;
    let report = Report {
        converted: writes.len(),
        needs_action: items,
        notes: Vec::new(),
    };
    Ok(Plan {
        writes,
        config,
        report,
        created,
        reserved,
        rendered,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decoded(name: &str) -> (Class, String) {
        let d = decode_file(name);
        (d.class, d.name)
    }

    #[test]
    fn attributes_out_of_chezmoi_s_order_are_named_with_the_fix() {
        assert_eq!(
            in_chezmoi_order("executable_private_x").as_deref(),
            Some("private_executable_x")
        );
        assert_eq!(
            in_chezmoi_order("create_executable_encrypted_dot_k").as_deref(),
            Some("create_encrypted_executable_dot_k")
        );
        assert_eq!(
            in_chezmoi_order("private_private_x").as_deref(),
            Some("private_x")
        );
        for fine in [
            "private_executable_x",
            "dot_bashrc",
            "encrypted_private_readonly_empty_executable_x",
            "symlink_dot_x",
            "run_once_x.sh",
        ] {
            assert_eq!(in_chezmoi_order(fine), None, "{fine}");
        }
        let entry = Entry {
            rel: "dot_config/executable_private_x".to_owned(),
            target: ".config/x".to_owned(),
            class: Class::File,
            attrs: Attrs::default(),
        };
        let item = no_target(&entry, "chezmoi has no file for it here");
        assert_eq!(item.reason, MISORDERED);
        assert!(
            item.detail.contains("rename it to private_executable_x"),
            "{}",
            item.detail
        );
    }

    #[test]
    fn file_names_decode_by_chezmoi_s_grammar() {
        assert_eq!(decoded("dot_bashrc"), (Class::File, ".bashrc".to_owned()));
        assert_eq!(decoded("executable_x"), (Class::File, "x".to_owned()));
        assert!(decode_file("executable_x").attrs.executable);
        assert!(decode_file("private_executable_dot_k").attrs.private);
        assert_eq!(decoded("dot_gitconfig.tmpl").0, Class::Template);
        assert_eq!(decoded("run_once_x.sh").0, Class::Script);
        assert_eq!(decoded("run_onchange_before_x.sh.tmpl").0, Class::Script);
        assert_eq!(decoded("modify_dot_x").0, Class::Modify);
        assert_eq!(decoded("modify_dot_x.tmpl").0, Class::Modify);
        assert_eq!(decoded("remove_dot_x").0, Class::Remove);
        assert_eq!(decoded("create_dot_x").0, Class::Create);
        assert_eq!(decoded("symlink_dot_x").0, Class::Symlink);
        assert_eq!(decoded("symlink_dot_x.tmpl").0, Class::Template);
        assert_eq!(decoded("encrypted_private_dot_k.age").0, Class::Encrypted);
        assert_eq!(decoded("literal_dot_x"), (Class::File, "dot_x".to_owned()));
        assert_eq!(
            decoded("dot_a.tmpl.literal"),
            (Class::File, ".a.tmpl".to_owned())
        );
        assert_eq!(
            decoded("private_modify_x"),
            (Class::File, "modify_x".to_owned())
        );
    }

    #[test]
    fn modify_and_run_only_count_as_the_first_prefix() {
        assert_eq!(decoded("empty_modify_x").0, Class::File);
        assert_eq!(decoded("dot_run_x").0, Class::File);
    }

    #[test]
    fn permissions_follow_the_prefixes_under_umask_022() {
        let perm = |name: &str| decode_file(name).attrs.file_perm();
        assert_eq!(perm("dot_a"), 0o644);
        assert_eq!(perm("executable_a"), 0o755);
        assert_eq!(perm("private_a"), 0o600);
        assert_eq!(perm("private_executable_a"), 0o700);
        assert_eq!(perm("readonly_a"), 0o444);
        assert_eq!(decode_dir("private_dot_ssh").attrs.dir_perm(), 0o700);
        assert_eq!(decode_dir("dot_config").attrs.dir_perm(), 0o755);
    }

    #[test]
    fn directories_decode_with_their_attributes() {
        let dir = decode_dir("exact_private_dot_ssh");
        assert_eq!(dir.name, ".ssh");
        assert!(dir.exact && dir.attrs.private && dir.dotted);
    }

    #[test]
    fn actions_are_stripped_across_lines() {
        let (text, any) = strip_actions("a\n{{- if eq .x \"y\" -}}\nb\n{{ end }}\nc {{ x");
        assert!(any);
        assert_eq!(text.split_whitespace().collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(strip_actions("plain"), ("plain".to_owned(), false));
    }

    #[test]
    fn braces_expand() {
        assert_eq!(expand_braces("a/{b,c}/d"), ["a/b/d", "a/c/d"]);
        assert_eq!(expand_braces("{a,b}{1,2}"), ["a1", "a2", "b1", "b2"]);
        assert_eq!(expand_braces("a{b"), ["a{b"]);
    }

    fn ignores(text: &str, key: &str) -> bool {
        let mut ignore = Ignore::default();
        ignore.add_file(text, "");
        ignore.ignores(key)
    }

    #[test]
    fn ignore_patterns_match_like_doublestar() {
        let text = "README.md\n**/*.bak\n# comment\n\nLibrary/**\n.cache\n";
        assert!(ignores(text, "README.md"));
        assert!(ignores(text, "a.bak"));
        assert!(ignores(text, ".config/x/y.bak"));
        assert!(ignores(text, "Library/a/b"));
        assert!(ignores(text, ".cache/deep/file"));
        assert!(!ignores(text, "docs/README.md"));
        assert!(!ignores(text, ".bashrc"));
    }

    #[test]
    fn a_conditional_pattern_is_honored_and_a_conditional_negation_is_not() {
        let text = "{{ if eq .chezmoi.os \"darwin\" }}Library/**{{ end }}\n";
        assert!(ignores(text, "Library/Fonts/x"));
        let negated = "{{ if .x }}\n.config/**\n!.config/keep\n{{ end }}\n";
        assert!(ignores(negated, ".config/keep"));
        assert!(!ignores(".config/**\n!.config/keep\n", ".config/keep"));
        assert!(ignores(".config/**\n!.config/keep\n", ".config/other"));
    }

    #[test]
    fn braces_and_nested_ignore_files_work() {
        assert!(ignores("{a,b}/c\n", "b/c/d"));
        let mut ignore = Ignore::default();
        ignore.add_file("*.log\n", ".config/app");
        assert!(ignore.ignores(".config/app/x.log"));
        assert!(!ignore.ignores(".config/other/x.log"));
        assert!(!ignore.ignores("x.log"));
    }

    #[test]
    fn source_dir_is_read_from_each_config_format() {
        assert_eq!(
            source_dir_from("chezmoi.toml", "sourceDir = \"/a/b\"\n[data]\nx = 1\n").as_deref(),
            Some("/a/b")
        );
        assert_eq!(
            source_dir_from("chezmoi.json", "{\"sourcedir\": \"~/c\"}").as_deref(),
            Some("~/c")
        );
        assert_eq!(
            source_dir_from(
                "chezmoi.yaml",
                "data:\n  sourceDir: no\nsourceDir: '/d'  # here\n"
            )
            .as_deref(),
            Some("/d")
        );
        assert_eq!(source_dir_from("chezmoi.toml", "x = 1"), None);
        assert_eq!(source_dir_from("chezmoi.toml", "not toml ["), None);
    }

    #[test]
    fn keys_that_dotman_cannot_hold_are_refused() {
        assert!(key_problem(".bashrc").is_none());
        assert!(key_problem("README.md").is_none());
        assert!(key_problem(".dotman.toml").is_some());
        assert!(key_problem(".Git/config").is_some());
        assert!(key_problem("a/../b").is_some());
        assert!(key_problem("README.md/../x").is_some());
    }

    #[test]
    fn a_tree_is_scanned_into_entries_and_notes() {
        let tmp = tempfile::TempDir::new().unwrap();
        let root = tmp.path();
        for (path, text) in [
            ("dot_bashrc", "x"),
            ("private_dot_ssh/private_config", "x"),
            ("dot_gitconfig.tmpl", "x"),
            ("run_once_x.sh", "x"),
            (".chezmoiscripts/run_y.sh", "x"),
            (".chezmoiignore", "x"),
            (".chezmoidata.toml", "x"),
            (".chezmoiexternal.toml.tmpl", "x"),
            (".gitignore", "x"),
            (".git/config", "x"),
        ] {
            let path = root.join(path);
            fs::create_dir_all(path.parent().unwrap()).unwrap();
            fs::write(path, text).unwrap();
        }
        symlink("dot_bashrc", root.join("dot_link")).unwrap();
        let mut tree = Tree::default();
        scan(root, "", "", Zone::Plain, &mut tree).unwrap();
        let seen: Vec<(&str, Class)> = tree
            .entries
            .iter()
            .map(|e| (e.target.as_str(), e.class))
            .collect();
        assert_eq!(
            seen,
            [
                (".chezmoiexternal.toml.tmpl", Class::External),
                (".chezmoiscripts/run_y.sh", Class::Script),
                (".bashrc", Class::File),
                (".gitconfig", Class::Template),
                (".link", Class::Link),
                (".ssh/config", Class::File),
                ("x.sh", Class::Script),
            ]
        );
        assert_eq!(tree.hidden, 1);
        assert_eq!(tree.own.len(), 2);
        assert_eq!(tree.dirs.get(".ssh").map(|a| a.dir_perm()), Some(0o700));
    }

    #[test]
    fn chezmoi_names_make_a_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let dir = tmp.path().join("plain");
        fs::create_dir_all(dir.join(".config")).unwrap();
        fs::write(dir.join(".config/run_backup.sh"), "x").unwrap();
        assert!(!is_source(&dir));
        let chezmoi = tmp.path().join("cz");
        fs::create_dir_all(chezmoi.join("dot_config")).unwrap();
        assert!(is_source(&chezmoi));
        let rooted = tmp.path().join("rooted");
        fs::create_dir_all(&rooted).unwrap();
        fs::write(rooted.join(".chezmoiroot"), "home\n").unwrap();
        assert!(is_source(&rooted));
        assert!(!is_source(&tmp.path().join("missing")));
    }

    #[test]
    fn chezmoiroot_must_stay_inside_the_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let source = tmp.path().join("src");
        fs::create_dir_all(source.join("home")).unwrap();
        fs::write(source.join(".chezmoiroot"), "home\n").unwrap();
        assert_eq!(resolve_root(&source).unwrap(), source.join("home"));
        for bad in ["..", "/etc", "a/../..", ""] {
            fs::write(source.join(".chezmoiroot"), bad).unwrap();
            assert!(resolve_root(&source).is_err(), "{bad}");
        }
        fs::write(source.join(".chezmoiroot"), "out").unwrap();
        symlink(tmp.path(), source.join("out")).unwrap();
        assert!(resolve_root(&source).is_err());
    }
}
