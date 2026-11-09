//! The importer for git trees laid out relative to `$HOME`: yadm, bare repos, vcsh and URLs.
//!
//! The source is cloned with its history, converted in a staging directory and moved into place
//! once the report is out and the user agreed. The staging clone sits beside the repo, so the
//! final step is a rename.
//!
//! The accounting unit is an entry of the source's HEAD tree: a file, a symlink or a gitlink.
//! Every entry is either in the repo afterwards (as it was, renamed, or as an unmanaged root
//! file) or reported. Entries that stay in the repo but need a look, because yadm and dotman pick
//! different files for their target, are reported too and not counted as converted.

use super::chezmoi;
use super::farm;
use super::yadm::{self, Alternate, Machine};
use super::{
    EditedConfig, Item, Report, Source, author_email, count, find_legacy, held, homes, legacy,
    proceed, require_clean, require_fresh,
};
use crate::bootstrap;
use crate::cli::ImportArgs;
use crate::commands::Outcome;
use crate::commands::add::ensure_worktree_parents;
use crate::config::{Allow, Config, TemplateConfig};
use crate::context::{Context, is_real_dir};
use crate::engine::{Node, read_node};
use crate::git::{Git, TreeEntry, redact_credentials};
use crate::identity;
use crate::name;
use crate::output;
use crate::paths::{
    create_private_dir, is_reserved_root_name, private_temp_dir, validate_key, write_atomic,
};
use crate::repo_config::RepoConfig;
use crate::secrets;
use crate::state::IdentitySnapshot;
use crate::template::{self, Vars};
use crate::variant::{Selection, Sources};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read};
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use tempfile::TempDir;

const SUBMODULE: &str = "submodules, which dotman never fetches: vendor or install them separately";
const UNMANAGEABLE: &str = "paths dotman cannot manage, left in the repo and never applied";
const CLASH: &str = "alternates that land on the name of another file";
const PRECEDENCE: &str =
    "targets where yadm and dotman pick different files here; both files are kept";
const MISSING: &str = "files missing from the repo, left out";
const DIFFERS: &str = "files whose content differs from the repo, left as they are";
const MODE_DIFFERS: &str = "files whose executable bit differs from the repo, left as it is";
const BLOCKED: &str = "files the repo holds in another form";

const MAX_PROBED: usize = 500;
const SHOWN: usize = 10;
const SHOWN_COMMITS: usize = 3;

/// A source `dotman import` found by itself.
pub(super) struct Candidate {
    pub(super) label: &'static str,
    pub(super) path: PathBuf,
}

pub(super) fn is_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

fn is_git_source(dir: &Path) -> bool {
    is_git_dir(dir) || dir.join(".git").exists()
}

fn git_at(dir: &Path) -> Git {
    Git::new(dir, "import")
}

/// Legacy, yadm, bare dot-directories of `$HOME`, vcsh, chezmoi, then symlink farms, in the order
/// `find` reports them.
pub(super) fn candidates(ctx: &Context) -> Result<Vec<Candidate>> {
    let home = &ctx.paths.home;
    let mut found = Vec::new();
    if let Some(path) = find_legacy(home) {
        found.push(Candidate {
            label: "legacy dotman repo",
            path,
        });
    }
    found.extend(yadm_repos(ctx).into_iter().map(|path| Candidate {
        label: "yadm repo",
        path,
    }));
    let mut bare = Vec::new();
    for entry in fs::read_dir(home).with_context(|| format!("cannot read {}", home.display()))? {
        let path = entry?.path();
        let dot_dir = path
            .file_name()
            .is_some_and(|name| name.as_bytes().starts_with(b"."));
        if dot_dir && is_real_dir(&path) && is_git_dir(&path) && !legacy::is_legacy(&path) {
            bare.push(path);
        }
    }
    bare.sort();
    found.extend(
        bare.into_iter()
            .filter(|path| works_on_home(ctx, path))
            .map(|path| Candidate {
                label: "bare repo",
                path,
            }),
    );
    found.extend(vcsh_repos(ctx).into_iter().map(|path| Candidate {
        label: "vcsh repo",
        path,
    }));
    found.extend(chezmoi::candidates(ctx).into_iter().map(|path| Candidate {
        label: "chezmoi source",
        path,
    }));
    found.extend(farm::candidates(ctx).into_iter().map(|path| Candidate {
        label: "symlink farm",
        path,
    }));
    Ok(found)
}

/// yadm's repo in `$XDG_DATA_HOME`, which is where yadm keeps it, in `~/.local/share`, and where
/// yadm before 3.0 kept it: `~/.yadm` and `$XDG_CONFIG_HOME/yadm`.
fn yadm_repos(ctx: &Context) -> Vec<PathBuf> {
    let mut repos: Vec<PathBuf> = Vec::new();
    let home = &ctx.paths.home;
    for dir in [
        ctx.paths.data_home.join("yadm/repo.git"),
        home.join(".local/share/yadm/repo.git"),
        home.join(".yadm/repo.git"),
        ctx.paths.config_home.join("yadm/repo.git"),
    ] {
        let real = fs::canonicalize(&dir).unwrap_or_else(|_| dir.clone());
        if is_git_dir(&dir)
            && !repos
                .iter()
                .any(|seen| fs::canonicalize(seen).unwrap_or_else(|_| seen.clone()) == real)
        {
            repos.push(dir);
        }
    }
    repos
}

fn vcsh_dir(ctx: &Context) -> PathBuf {
    ctx.paths.config_home.join("vcsh/repo.d")
}

fn vcsh_repos(ctx: &Context) -> Vec<PathBuf> {
    let Ok(entries) = fs::read_dir(vcsh_dir(ctx)) else {
        return Vec::new();
    };
    let mut repos: Vec<PathBuf> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| {
            path.extension() == Some(OsStr::new("git"))
                && is_git_dir(path)
                && git_at(path).head().ok().flatten().is_some()
        })
        .collect();
    repos.sort();
    repos
}

/// A bare repo is `$HOME`'s when `core.worktree` names `$HOME`, or, with no `core.worktree`, when
/// at least half of the files in its HEAD tree exist there (the `--git-dir=$HOME/.cfg
/// --work-tree=$HOME` alias never sets it). At most `MAX_PROBED` paths are checked.
fn works_on_home(ctx: &Context, dir: &Path) -> bool {
    let git = git_at(dir);
    if git.head().ok().flatten().is_none() {
        return false;
    }
    let Ok(worktree) = git.config_values(&dir.join("config"), "core.worktree") else {
        return false;
    };
    if let Some(value) = worktree.last() {
        let path = Path::new(value);
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            dir.join(path)
        };
        let Ok(real) = fs::canonicalize(absolute) else {
            return false;
        };
        return homes(ctx)
            .iter()
            .any(|home| fs::canonicalize(home).is_ok_and(|home| home == real));
    }
    let Ok(entries) = git.ls_tree("HEAD") else {
        return false;
    };
    let probed: Vec<&TreeEntry> = entries.iter().take(MAX_PROBED).collect();
    let present = probed
        .iter()
        .filter(|entry| fs::symlink_metadata(ctx.paths.home.join(&entry.path)).is_ok())
        .count();
    !probed.is_empty() && present.saturating_mul(2) >= probed.len()
}

/// The one source `dotman import` takes when none is given, or the exit code after listing several.
pub(super) fn find(ctx: &Context) -> Result<ControlFlow<Outcome, Source>> {
    let found = candidates(ctx)?;
    match found.as_slice() {
        [] => bail!(
            "no source given and none found: looked for a legacy dotman repo in {home}/.dotman, a yadm repo, bare repos directly under {home}, vcsh repos in {vcsh}, a chezmoi source, and links in {home} that resolve under one directory; run `{bin} import <source>` with a path or a git URL",
            home = ctx.paths.home.display(),
            vcsh = vcsh_dir(ctx).display(),
            bin = name::bin()
        ),
        [one] => Ok(ControlFlow::Continue(Source::Path(one.path.clone()))),
        many => {
            output::warning(&format!(
                "several sources found; `{} import` will not pick one for you:",
                name::bin()
            ));
            for candidate in many {
                output::warning(&format!(
                    "  {} import {}    # {}",
                    name::bin(),
                    shell_words::quote(&candidate.path.to_string_lossy()),
                    candidate.label
                ));
            }
            Ok(ControlFlow::Break(Outcome::Attention))
        }
    }
}

/// The source's clone, in a directory that is removed when this is dropped.
pub(super) struct Staging {
    _dir: TempDir,
    pub(super) repo: PathBuf,
    pub(super) git: Git,
}

impl Staging {
    /// Beside the dotman repo for an import, so that moving it into place is a rename, and in
    /// the system temp directory otherwise, so that nothing is created in the dotman repo's
    /// parent.
    pub(super) fn new(ctx: &Context, beside_repo: bool) -> Result<Self> {
        let base = if beside_repo {
            let parent =
                ctx.paths.repo.parent().ok_or_else(|| {
                    anyhow!("{} has no parent directory", ctx.paths.repo.display())
                })?;
            create_private_dir(parent, false)
                .with_context(|| format!("cannot create {}", parent.display()))?;
            parent.to_path_buf()
        } else {
            std::env::temp_dir()
        };
        let dir = private_temp_dir(".dotman-import-", &base)?;
        let repo = dir.path().join("repo");
        Ok(Self {
            git: git_at(&repo),
            _dir: dir,
            repo,
        })
    }

    fn clone_source(&self, source: &Source) -> Result<()> {
        self.git.clone_head_from(&source.git_arg()?)?;
        ensure!(
            self.git.head()?.is_some(),
            "{} has no commits",
            source.display()
        );
        Ok(())
    }
}

/// An entry that stays in the repo.
#[derive(Debug)]
struct Kept {
    from: String,
    to: String,
    mode: String,
    /// The rewritten body of a template.
    body: Option<String>,
    /// A root name dotman never applies; the file stays in the repo unmanaged.
    reserved: bool,
}

/// An entry that needs action.
#[derive(Debug)]
struct Left {
    reason: &'static str,
    detail: String,
    path: String,
    /// Taken out of the tree by the conversion commit.
    remove: bool,
}

#[derive(Debug, Default)]
struct Analysis {
    kept: Vec<Kept>,
    left: Vec<Left>,
    /// Entries with `##` in the name, whether or not they could be converted.
    alternates: Vec<String>,
    /// Targets of the converted alternates.
    targets: BTreeSet<String>,
    env: BTreeSet<String>,
    /// Converted templates that print a yadm variable whose case changes, and those variables.
    cased: BTreeMap<String, BTreeSet<&'static str>>,
    /// The source path of the entry that carries a precedence report, and the report.
    precedence: BTreeMap<String, String>,
}

struct Named {
    path: String,
    mode: String,
    alternate: Alternate,
    name: String,
}

/// Why `item`'s new name is not free, if it is not.
fn clash(
    item: &Named,
    claims: &BTreeMap<String, usize>,
    directories: &BTreeSet<String>,
    plain: &BTreeMap<String, String>,
) -> Option<String> {
    if claims.get(&item.name).copied().unwrap_or(0) > 1 {
        Some("another alternate maps to the same name".to_owned())
    } else if plain.contains_key(&item.name) {
        Some(format!(
            "the repo already has a plain {}, which yadm keeps over its alternates",
            item.name
        ))
    } else if directories.contains(&item.name) {
        Some("the name is also a directory in the repo".to_owned())
    } else if item
        .name
        .match_indices('/')
        .filter_map(|(at, _)| item.name.get(..at))
        .any(|parent| plain.contains_key(parent))
    {
        Some("a parent of the name is a file in the repo".to_owned())
    } else {
        None
    }
}

/// Names every entry and settles the clashes. A `##` file that cannot be converted, or whose new
/// name is taken, is left. The clash rules never cascade: dropping one file does not free its name.
fn settle(entries: &[(String, String)]) -> Analysis {
    let mut found = Analysis::default();
    let mut plain: BTreeMap<String, String> = BTreeMap::new();
    let mut named: Vec<Named> = Vec::new();
    for (path, mode) in entries {
        if mode == "160000" {
            found.left.push(Left {
                reason: SUBMODULE,
                detail: format!("{path}: a submodule"),
                path: path.clone(),
                remove: false,
            });
            continue;
        }
        let Some(parsed) = yadm::parse(path) else {
            plain.insert(path.clone(), mode.clone());
            continue;
        };
        found.alternates.push(path.clone());
        let converted =
            parsed.and_then(|alternate| alternate.dotman_path(path).map(|name| (alternate, name)));
        match converted {
            Ok((alternate, name)) => named.push(Named {
                path: path.clone(),
                mode: mode.clone(),
                alternate,
                name,
            }),
            Err((reason, detail)) => found.left.push(Left {
                reason,
                detail,
                path: path.clone(),
                remove: true,
            }),
        }
    }

    let mut claims: BTreeMap<String, usize> = BTreeMap::new();
    for item in &named {
        let seen = claims.entry(item.name.clone()).or_default();
        *seen = seen.saturating_add(1);
    }
    let directories: BTreeSet<String> = plain
        .keys()
        .chain(named.iter().map(|item| &item.name))
        .flat_map(|path| {
            path.match_indices('/')
                .filter_map(|(at, _)| path.get(..at).map(str::to_owned))
        })
        .collect();
    let mut survivors = Vec::new();
    for item in named {
        let clash = clash(&item, &claims, &directories, &plain);
        match clash {
            Some(why) => found.left.push(Left {
                reason: CLASH,
                detail: format!("{}: {why}", item.path),
                path: item.path,
                remove: true,
            }),
            None => survivors.push(item),
        }
    }

    for (path, mode) in plain {
        let first = path.split('/').next().unwrap_or_default();
        let reserved = is_reserved_root_name(first);
        if !reserved && let Err(e) = validate_key(&path) {
            found.left.push(Left {
                reason: UNMANAGEABLE,
                detail: format!("{path}: {e}"),
                path,
                remove: false,
            });
            continue;
        }
        found.kept.push(Kept {
            from: path.clone(),
            to: path,
            mode,
            body: None,
            reserved,
        });
    }
    for item in survivors {
        found.targets.insert(item.alternate.target.clone());
        found.kept.push(Kept {
            from: item.path,
            to: item.name,
            mode: item.mode,
            body: None,
            reserved: false,
        });
    }
    found.kept.sort_by(|a, b| a.from.cmp(&b.from));
    found
}

/// Whether `rewritten` renders: a safety net for the converter, with every variable defined.
fn renders(path: &str, rewritten: &yadm::Rewritten) -> Result<(), String> {
    let sample = IdentitySnapshot {
        host: "host".to_owned(),
        user: "user".to_owned(),
        os: "linux".to_owned(),
        distro: "distro".to_owned(),
        like: "like".to_owned(),
        arch: "arch".to_owned(),
        profiles: vec!["profile".to_owned()],
    };
    let machine = Config {
        template: TemplateConfig {
            env: rewritten.env.iter().cloned().collect(),
        },
        ..Config::default()
    };
    let vars = Vars::new(&sample, &machine, Path::new("/home/user"), |_| {
        Some(String::new())
    });
    template::render(path, &rewritten.text, &vars)
        .map(drop)
        .map_err(|e| format!("the converted template does not render: {e}"))
}

/// Rewrites the templates, then finds the targets where yadm and dotman disagree.
fn analyze(git: &Git, tree: &[TreeEntry], identity: &IdentitySnapshot) -> Analysis {
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut unreadable = Vec::new();
    for entry in tree {
        match entry.path.to_str() {
            Some(path) => entries.push((path.to_owned(), entry.mode.clone())),
            None => unreadable.push(entry.path.to_string_lossy().into_owned()),
        }
    }
    entries.sort();
    let mut found = settle(&entries);
    for path in unreadable {
        found.left.push(Left {
            reason: UNMANAGEABLE,
            detail: format!("{path}: not valid UTF-8"),
            path,
            remove: false,
        });
    }

    let mut dropped: Vec<Left> = Vec::new();
    for kept in &mut found.kept {
        let Some(Ok(alternate)) = yadm::parse(&kept.from) else {
            continue;
        };
        if !alternate.is_template() {
            continue;
        }
        let outcome = (|| -> Result<String, String> {
            if kept.mode == "120000" {
                return Err("a symlink cannot be a template".to_owned());
            }
            let bytes = git
                .show_blob("HEAD", &kept.from)
                .map_err(|e| e.to_string())?;
            let text = String::from_utf8(bytes).map_err(|_| "is not UTF-8".to_owned())?;
            let rewritten = yadm::rewrite_template(&text)?;
            renders(&kept.from, &rewritten)?;
            found.env.extend(rewritten.env);
            if !rewritten.cased.is_empty() {
                found.cased.insert(kept.to.clone(), rewritten.cased);
            }
            Ok(rewritten.text)
        })();
        match outcome {
            Ok(body) => kept.body = Some(body),
            Err(why) => dropped.push(Left {
                reason: yadm::TEMPLATE,
                detail: format!("{}: {why}", kept.from),
                path: kept.from.clone(),
                remove: true,
            }),
        }
    }
    if !dropped.is_empty() {
        let gone: BTreeSet<&str> = dropped.iter().map(|left| left.path.as_str()).collect();
        found.kept.retain(|kept| !gone.contains(kept.from.as_str()));
        found.left.extend(dropped);
    }
    found.targets = found
        .kept
        .iter()
        .filter_map(|kept| yadm::parse(&kept.from)?.ok())
        .map(|alternate| alternate.target)
        .collect();

    found.precedence = precedence(&found, identity);
    found.left.sort_by(|a, b| a.path.cmp(&b.path));
    found
}

/// For each target whose alternates were all converted: where yadm's weights and dotman's
/// selection pick different files on this machine.
fn precedence(found: &Analysis, identity: &IdentitySnapshot) -> BTreeMap<String, String> {
    let machine = Machine::from_identity(identity);
    let blocked: BTreeSet<&str> = found
        .left
        .iter()
        .filter_map(|left| left.path.split_once("##").map(|(target, _)| target))
        .collect();
    let mut reports = BTreeMap::new();
    for target in &found.targets {
        if blocked.contains(target.as_str()) {
            continue;
        }
        let members: Vec<(&Kept, Alternate)> = found
            .kept
            .iter()
            .filter_map(|kept| {
                let alternate = yadm::parse(&kept.from)?.ok()?;
                (alternate.target == *target).then_some((kept, alternate))
            })
            .collect();
        let plain_exists = found
            .kept
            .iter()
            .any(|kept| kept.from == *target && kept.to == *target);
        let alternates: Vec<&Alternate> = members.iter().map(|(_, a)| a).collect();
        let yadm_pick = yadm::winner(&alternates, &machine).and_then(|at| members.get(at));
        // yadm skips an alternate when a plain file sits at the target, unless it is a template.
        let yadm_file: Option<&str> = match yadm_pick {
            Some((kept, alternate)) if alternate.is_template() || !plain_exists => {
                Some(kept.from.as_str())
            }
            _ if plain_exists => Some(target.as_str()),
            _ => None,
        };

        let mut paths: Vec<&str> = members.iter().map(|(kept, _)| kept.to.as_str()).collect();
        if plain_exists {
            paths.push(target.as_str());
        }
        let group = Sources::group(paths.iter().copied());
        let (dotman_file, dotman_shown): (Option<&str>, String) =
            match group.select(target, identity) {
                Selection::Winner(path) => (Some(path), path.to_owned()),
                Selection::None => (None, "nothing".to_owned()),
                Selection::Ambiguous(tied) => (
                    None,
                    format!("neither, {} are equally specific", tied.join(" and ")),
                ),
                Selection::Invalid(_) => (None, "neither, a name is invalid".to_owned()),
            };
        let yadm_in_dotman = yadm_file.map(|file| {
            members
                .iter()
                .find(|(kept, _)| kept.from == file)
                .map_or(file, |(kept, _)| kept.to.as_str())
        });
        if yadm_in_dotman == dotman_file && !dotman_shown.starts_with("neither") {
            continue;
        }
        let carrier = yadm_file
            .filter(|file| members.iter().any(|(kept, _)| kept.from == *file))
            .or_else(|| {
                dotman_file.and_then(|path| {
                    members
                        .iter()
                        .find(|(kept, _)| kept.to == path)
                        .map(|(kept, _)| kept.from.as_str())
                })
            })
            .or_else(|| members.first().map(|(kept, _)| kept.from.as_str()));
        if let Some(carrier) = carrier {
            reports.insert(
                carrier.to_owned(),
                format!(
                    "{target}: yadm applies {}, dotman applies {dotman_shown}",
                    yadm_file.unwrap_or("nothing")
                ),
            );
        }
    }
    reports
}

pub(super) fn shown(items: &[String]) -> String {
    let mut text = items
        .iter()
        .take(SHOWN)
        .cloned()
        .collect::<Vec<_>>()
        .join(", ");
    if items.len() > SHOWN {
        let _ = write!(text, " (+{} more)", items.len().saturating_sub(SHOWN));
    }
    text
}

/// `recover` adds the way to read a file the conversion removed from the tree.
fn tally(found: &Analysis, recover: bool) -> Report {
    let mut report = Report::default();
    for left in &found.left {
        let detail = if left.remove && recover {
            format!(
                "{}; left out of the repo, `{} git show {}` prints it",
                left.detail,
                name::bin(),
                shell_words::quote(&format!("HEAD~1:{}", left.path))
            )
        } else {
            left.detail.clone()
        };
        report.needs_action.push(Item {
            reason: left.reason,
            detail,
        });
    }
    let mut reserved = Vec::new();
    for kept in &found.kept {
        if let Some(detail) = found.precedence.get(&kept.from) {
            report.needs_action.push(Item {
                reason: PRECEDENCE,
                detail: detail.clone(),
            });
        } else {
            report.converted = report.converted.saturating_add(1);
        }
        if kept.reserved {
            reserved.push(kept.to.clone());
        }
    }
    if !reserved.is_empty() {
        report.notes.push(format!(
            "{} {} in the repo and never applied: {}",
            count(reserved.len(), "root file"),
            if reserved.len() == 1 { "stays" } else { "stay" },
            shown(&reserved)
        ));
    }
    report
}

/// Takes each file the content guard holds out of the conversion, so the conversion commit removes
/// it from the tree; the history the source brings still has it. Root files dotman never applies
/// are not dotfiles and stay.
fn guard_content(repo: &Path, found: &mut Analysis, allow: &[Allow]) {
    let mut left = Vec::new();
    found.kept.retain(|kept| {
        if kept.reserved || kept.mode == "120000" {
            return true;
        }
        let item = kept.body.as_ref().map_or_else(
            || match read_node(repo, &kept.from) {
                Ok(Node::File { bytes, .. }) => held(&bytes, &kept.to, allow),
                _ => None,
            },
            |body| held(body.as_bytes(), &kept.to, allow),
        );
        let Some(item) = item else {
            return true;
        };
        left.push(Left {
            reason: item.reason,
            detail: format!("{}; the source's history still has it", item.detail),
            path: kept.from.clone(),
            remove: true,
        });
        false
    });
    if !left.is_empty() {
        found.left.extend(left);
        found.left.sort_by(|a, b| a.path.cmp(&b.path));
    }
}

/// A place in the source's history whose content the guard holds, with the commits that add it
/// or hold it.
struct Tainted {
    place: Place,
    rule: &'static str,
    commits: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Place {
    /// A blob, at a path that names it.
    File(String),
    /// A part of a commit object: its message, or the header named, such as `commit author`.
    Commit(&'static str),
}

/// What the guard holds in one object: the rule, and for a commit the part that holds it, as
/// [`commit_field`] names it; empty for a blob.
#[derive(Debug, Clone, Copy)]
struct Hit {
    rule: &'static str,
    field: &'static str,
}

/// Stdout of `git <args>` in `git`'s repo, with `input` as stdin when given. An error unless git
/// exits 0.
fn git_stdout<S: AsRef<OsStr>>(git: &Git, args: &[S], input: Option<&Path>) -> Result<Vec<u8>> {
    let mut cmd = git.command();
    cmd.args(args);
    match input {
        Some(path) => {
            cmd.stdin(File::open(path).with_context(|| format!("cannot read {}", path.display()))?)
        }
        None => cmd.stdin(Stdio::null()),
    };
    let out = cmd.output().context("failed to run git")?;
    let shown: Vec<String> = args
        .iter()
        .map(|arg| arg.as_ref().to_string_lossy().into_owned())
        .collect();
    ensure!(
        out.status.success(),
        "`git {}` failed: {}",
        shown.join(" "),
        String::from_utf8_lossy(&out.stderr).trim()
    );
    Ok(out.stdout)
}

fn is_oid(text: &str) -> bool {
    matches!(text.len(), 40 | 64) && text.bytes().all(|b| b.is_ascii_hexdigit())
}

/// Every blob and commit message reachable from the clone's refs, which are all the import takes,
/// that the content guard holds, by path and rule. Each object is read once: `rev-list --objects`
/// lists them, `cat-file --batch-check` keeps the commits and the blobs that are not empty, and
/// `cat-file --batch` streams them, where a binary blob is skipped after its first bytes. The
/// lists go to git through files, which a large history cannot deadlock.
fn tainted_history(staging: &Staging, allow: &[Allow]) -> Result<Vec<Tainted>> {
    let git = &staging.git;
    let scratch = staging
        .repo
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", staging.repo.display()))?;
    let listed = git_stdout(git, &["rev-list", "--all", "--objects"], None)?;
    let mut paths: BTreeMap<String, String> = BTreeMap::new();
    // Newest first; `rev-list` prints a commit with no path, and a tree or blob with one.
    let mut commits = Vec::new();
    for line in String::from_utf8_lossy(&listed).lines() {
        let (oid, path) = line.split_once(' ').unwrap_or((line, ""));
        if !is_oid(oid) {
            continue;
        }
        if !line.contains(' ') {
            commits.push(oid.to_owned());
        }
        paths
            .entry(oid.to_owned())
            .or_insert_with(|| path.to_owned());
    }
    let objects = scratch.join("objects.txt");
    write_lines(&objects, paths.keys())?;
    let checked = git_stdout(
        git,
        &[
            "cat-file",
            "--batch-check=%(objectname) %(objecttype) %(objectsize)",
        ],
        Some(&objects),
    )?;
    let checked = String::from_utf8_lossy(&checked);
    let scanned = checked.lines().filter_map(|line| {
        let mut fields = line.split(' ');
        match (fields.next(), fields.next(), fields.next()) {
            (Some(oid), Some("blob"), Some(size)) if size != "0" => Some(oid),
            (Some(oid), Some("commit"), Some(_)) => Some(oid),
            _ => None,
        }
    });
    let list = scratch.join("blobs.txt");
    write_lines(&list, scanned)?;
    let mut cmd = git.command();
    cmd.args(["cat-file", "--batch"])
        .stdin(File::open(&list).with_context(|| format!("cannot read {}", list.display()))?)
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = cmd.spawn().context("failed to run git")?;
    let scanned = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("git cat-file gave no output"))
        .and_then(|stdout| scan_batch(&mut BufReader::new(stdout), &paths, allow));
    let status = child.wait().context("failed to run git")?;
    let mut held = scanned?;
    ensure!(status.success(), "`git cat-file --batch` failed ({status})");
    let mut in_commits: BTreeMap<(&'static str, &'static str), Vec<String>> = BTreeMap::new();
    for commit in commits {
        if let Some(hit) = held.remove(&commit) {
            in_commits
                .entry((hit.field, hit.rule))
                .or_default()
                .push(commit);
        }
    }
    let mut grouped: BTreeMap<(String, &'static str), Vec<String>> = BTreeMap::new();
    if !held.is_empty() {
        let adding = adding_commits(git, &held)?;
        for (oid, hit) in &held {
            let path = paths.get(oid).cloned().unwrap_or_default();
            let entry = grouped.entry((path, hit.rule)).or_default();
            for commit in adding.get(oid).into_iter().flatten() {
                if !entry.contains(commit) {
                    entry.push(commit.clone());
                }
            }
        }
    }
    let files = grouped
        .into_iter()
        .map(|((path, rule), commits)| (Place::File(path), rule, commits));
    let in_commits = in_commits
        .into_iter()
        .map(|((field, rule), commits)| (Place::Commit(field), rule, commits));
    Ok(files
        .chain(in_commits)
        .map(|(place, rule, commits)| Tainted {
            place,
            rule,
            commits,
        })
        .collect())
}

/// The part of a commit object that holds its 1-based `line`: the message after the first empty
/// line, or the header before it that the line belongs to, as a header's continuation lines start
/// with a space.
fn commit_field(bytes: &[u8], line: usize) -> &'static str {
    let wanted = line.checked_sub(1);
    let mut field = "commit header";
    for (index, text) in bytes.split(|b| *b == b'\n').enumerate() {
        if text.is_empty() {
            break;
        }
        if !text.starts_with(b" ") {
            field = match text.split(|b| *b == b' ').next() {
                Some(b"author") => "commit author",
                Some(b"committer") => "commit committer",
                Some(b"mergetag") => "merged tag",
                Some(b"gpgsig" | b"gpgsig-sha256") => "commit signature",
                _ => "commit header",
            };
        }
        if Some(index) == wanted {
            return field;
        }
    }
    "commit message"
}

fn write_lines(path: &Path, lines: impl Iterator<Item = impl AsRef<str>>) -> Result<()> {
    let mut text = String::new();
    for line in lines {
        text.push_str(line.as_ref());
        text.push('\n');
    }
    fs::write(path, text).with_context(|| format!("cannot write {}", path.display()))
}

/// Reads `cat-file --batch` output: per object a `<oid> <type> <size>` line, the content and a
/// newline. Returns what the guard holds each blob or commit for, keyed by oid.
fn scan_batch(
    reader: &mut impl BufRead,
    paths: &BTreeMap<String, String>,
    allow: &[Allow],
) -> Result<BTreeMap<String, Hit>> {
    let sniff = u64::try_from(secrets::SNIFF_BYTES).unwrap_or(u64::MAX);
    let mut held = BTreeMap::new();
    let mut header = String::new();
    loop {
        header.clear();
        if reader.read_line(&mut header)? == 0 {
            return Ok(held);
        }
        let mut fields = header.split_whitespace();
        // `<oid> missing` has no size and no content.
        let (Some(oid), Some(kind), Some(size)) = (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        let size: u64 = size
            .parse()
            .with_context(|| format!("git cat-file printed a bad size for {oid}"))?;
        let mut bytes = Vec::new();
        if kind == "commit" {
            // The whole commit, headers too: a merged tag's message travels in a header. No
            // allow entry names a commit, and git refuses a NUL in a message, so none is skipped
            // as binary.
            reader.by_ref().take(size).read_to_end(&mut bytes)?;
            if let Some(found) = secrets::held(&bytes, "", &[]) {
                let field = commit_field(&bytes, found.line);
                let hit = Hit {
                    rule: found.rule,
                    field,
                };
                held.insert(oid.to_owned(), hit);
            }
            let mut newline = [0u8; 1];
            reader.read_exact(&mut newline)?;
            continue;
        }
        reader
            .by_ref()
            .take(size.min(sniff))
            .read_to_end(&mut bytes)?;
        let rest = size.saturating_sub(size.min(sniff));
        if secrets::looks_binary(&bytes) {
            io::copy(&mut reader.by_ref().take(rest), &mut io::sink())?;
        } else {
            reader.by_ref().take(rest).read_to_end(&mut bytes)?;
            let path = paths.get(oid).map_or("", String::as_str);
            if let Some(found) = secrets::held(&bytes, path, allow) {
                let hit = Hit {
                    rule: found.rule,
                    field: "",
                };
                held.insert(oid.to_owned(), hit);
            }
        }
        let mut newline = [0u8; 1];
        reader.read_exact(&mut newline)?;
    }
}

/// The commits that put each blob in `held` at some path, newest first, from one walk of the
/// history. A blob that only a merge brings has none.
fn adding_commits(
    git: &Git,
    held: &BTreeMap<String, Hit>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let log = git_stdout(
        git,
        &[
            "log",
            "--all",
            "--root",
            "--no-renames",
            "--no-abbrev",
            "--raw",
            "--format=commit %H",
        ],
        None,
    )?;
    let mut commits: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut current = String::new();
    for line in String::from_utf8_lossy(&log).lines() {
        if let Some(commit) = line.strip_prefix("commit ") {
            commit.clone_into(&mut current);
        } else if line.starts_with(':')
            && let Some(blob) = line.split_whitespace().nth(3)
            && held.contains_key(blob)
        {
            commits
                .entry(blob.to_owned())
                .or_default()
                .push(current.clone());
        }
    }
    Ok(commits)
}

fn warn_tainted(tainted: &[Tainted], squash: bool, commits: &str) {
    if tainted.is_empty() {
        return;
    }
    output::warning("the source's history holds credentials or age secret keys:");
    for item in tainted {
        let short: Vec<&str> = item
            .commits
            .iter()
            .take(SHOWN_COMMITS)
            .map(|commit| commit.get(..12).unwrap_or(commit))
            .collect();
        let more = item.commits.len().saturating_sub(SHOWN_COMMITS);
        let (place, verb) = match &item.place {
            Place::File(path) => (path.as_str(), "added in"),
            Place::Commit(field) => (*field, "in"),
        };
        let named = if short.is_empty() {
            "in a merge".to_owned()
        } else {
            format!("{verb} {}", short.join(", "))
        };
        let more = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        output::warning(&output::escape_line(&format!(
            "  {place}: {}, {named}{more}",
            item.rule
        )));
    }
    if squash {
        output::warning(&format!(
            "so the import is one commit of the current tree, and the history is left out; `--keep-history` imports all {commits} instead"
        ));
    } else {
        output::warning(&format!(
            "--keep-history imports all {commits}, so the repo keeps these, and so does every remote it is pushed to"
        ));
    }
}

/// Replaces the clone's history with one commit of its current tree, in a `.git` of its own, so
/// that no object of the old history reaches the repo, not even an unreachable one.
fn squash_history(staging: &Staging) -> Result<()> {
    let git = &staging.git;
    let branch = format!(
        "refs/heads/{}",
        git.branch()?.unwrap_or_else(|| "main".to_owned())
    );
    let text = |bytes: Vec<u8>| String::from_utf8_lossy(&bytes).trim().to_owned();
    // `HEAD:` names HEAD's root tree.
    let tree = text(git_stdout(git, &["rev-parse", "--verify", "HEAD:"], None)?);
    let commit = text(git_stdout(
        git,
        &[
            "commit-tree",
            &tree,
            "-m",
            "import: the current tree; the source's history holds credentials",
        ],
        None,
    )?);
    git_stdout(git, &["update-ref", &branch, &commit], None)?;
    let history = staging
        .repo
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", staging.repo.display()))?
        .join("history.git");
    fs::rename(staging.repo.join(".git"), &history)
        .with_context(|| format!("cannot move {}", history.display()))?;
    git.init()?;
    git_stdout(
        git,
        &[
            OsStr::new("fetch"),
            OsStr::new("-q"),
            OsStr::new("--no-tags"),
            history.as_os_str(),
            OsStr::new(&branch),
        ],
        None,
    )?;
    git_stdout(git, &["update-ref", &branch, "FETCH_HEAD"], None)?;
    git_stdout(git, &["symbolic-ref", "HEAD", &branch], None)?;
    git_stdout(git, &["reset", "-q"], None)?;
    fs::remove_dir_all(&history).with_context(|| format!("cannot remove {}", history.display()))
}

/// Modes git cannot carry: a file in `$HOME` with the same bytes as its blob and no group or other
/// access, which the import would otherwise widen to git's 0644 or 0755 on the next apply.
fn home_modes(ctx: &Context, git: &Git, found: &Analysis) -> BTreeMap<String, u32> {
    let home = &ctx.paths.home;
    let mut modes = BTreeMap::new();
    for kept in &found.kept {
        if kept.reserved || kept.from != kept.to || kept.body.is_some() {
            continue;
        }
        let default = match kept.mode.as_str() {
            "100644" => 0o644,
            "100755" => 0o755,
            _ => continue,
        };
        // The bytes are read only for a file whose mode alone already qualifies.
        let stricter = fs::symlink_metadata(home.join(&kept.to)).is_ok_and(|meta| {
            let mode = meta.mode() & 0o777;
            let owner_only = mode | 0o700 == 0o700;
            meta.is_file() && owner_only && mode != default
        });
        if !stricter {
            continue;
        }
        if let Ok(Node::File { bytes, mode, .. }) = read_node(home, &kept.to)
            && git
                .show_blob("HEAD", &kept.from)
                .is_ok_and(|blob| blob == bytes)
        {
            modes.insert(kept.to.clone(), mode & 0o777);
        }
    }
    modes
}

/// Where the stricter modes go. A source's own `.dotman.toml` is never rewritten, since saving it
/// would drop its comments, so the `[modes]` lines to add are reported instead.
#[derive(Clone, Copy, PartialEq, Eq)]
enum SourceConfig {
    None,
    Parses,
    Broken,
}

fn home_modes_notes(modes: &BTreeMap<String, u32>, config: SourceConfig) -> Vec<String> {
    if modes.is_empty() {
        return Vec::new();
    }
    let found = format!(
        "{} in $HOME {} the same bytes as the source and a stricter mode than git keeps",
        count(modes.len(), "file"),
        if modes.len() == 1 { "has" } else { "have" }
    );
    if config == SourceConfig::None {
        let shown_modes: Vec<String> = modes
            .iter()
            .map(|(key, mode)| format!("{key} {mode:04o}"))
            .collect();
        return vec![format!(
            "{found}; [modes] records it: {}",
            shown(&shown_modes)
        )];
    }
    let broken = if config == SourceConfig::Broken {
        " (it does not parse yet)"
    } else {
        ""
    };
    let table: BTreeMap<&str, String> = modes
        .iter()
        .map(|(key, mode)| (key.as_str(), format!("{mode:04o}")))
        .collect();
    let lines = toml::to_string(&table).unwrap_or_default();
    let mut notes = vec![format!(
        "{found}; the source's own .dotman.toml{broken} is kept as it is, so add these lines to its [modes] to keep them:"
    )];
    notes.extend(lines.lines().map(|line| format!("  {line}")));
    notes
}

/// The subject of the conversion commit; `None` when the tree needs none.
fn conversion(found: &Analysis) -> Option<&'static str> {
    if !found.alternates.is_empty() {
        Some("import: convert yadm alternates")
    } else if !found.kept.iter().any(|kept| kept.to == ".dotman.toml") {
        Some("import: add .dotman.toml")
    } else if found.left.iter().any(|left| left.remove) {
        Some("import: leave out files that hold a credential")
    } else {
        None
    }
}

/// Writes the conversion into the worktree of `repo` and commits it.
fn convert(git: &Git, repo: &Path, found: &Analysis, modes: &BTreeMap<String, u32>) -> Result<()> {
    for left in found.left.iter().filter(|left| left.remove) {
        fs::remove_file(repo.join(&left.path))
            .with_context(|| format!("cannot remove {}", left.path))?;
    }
    for kept in found.kept.iter().filter(|kept| kept.from != kept.to) {
        ensure_worktree_parents(repo, &kept.to)?;
        let (from, to) = (repo.join(&kept.from), repo.join(&kept.to));
        match &kept.body {
            Some(body) => {
                let mode = if kept.mode == "100755" { 0o755 } else { 0o644 };
                write_atomic(&to, body.as_bytes(), mode)?;
                fs::remove_file(&from)?;
            }
            None => fs::rename(&from, &to)
                .with_context(|| format!("cannot rename {} to {}", kept.from, kept.to))?,
        }
    }
    let has_config = found.kept.iter().any(|kept| kept.to == ".dotman.toml");
    if !has_config {
        let mut config = RepoConfig::default();
        config.modes.extend(
            modes
                .iter()
                .map(|(key, mode)| (key.clone(), format!("{mode:04o}"))),
        );
        config.save(&repo.join(".dotman.toml"), false)?;
    }
    let Some(message) = conversion(found) else {
        return Ok(());
    };
    git.stage_all()?;
    git.commit(message)?;
    Ok(())
}

/// The repo directory can be replaced by the staging clone when it holds nothing but a `.git`
/// without commits, a `.dotman.toml` and an unedited `install.sh`.
pub(super) fn check_target(repo: &Path) -> Result<()> {
    let entries = match fs::read_dir(repo) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", repo.display())),
    };
    ensure!(
        is_real_dir(repo),
        "{} exists and is not a directory",
        repo.display()
    );
    for entry in entries {
        let name = entry?.file_name();
        let generated = name == bootstrap::FILE
            && matches!(bootstrap::inspect(repo)?, bootstrap::Found::Unedited { .. });
        ensure!(
            generated || name == ".git" || name == ".dotman.toml",
            "{} holds {}, which `{} import` would not keep; move it away and run the import again",
            repo.display(),
            name.to_string_lossy(),
            name::bin()
        );
    }
    Ok(())
}

/// Moves the staging clone to the repo path. An empty repo that `dotman init` made is replaced; its
/// `origin` survives when the import brings none.
fn install(
    ctx: &Context,
    staging: &Staging,
    edited: Option<&EditedConfig>,
) -> Result<Option<String>> {
    let repo = &ctx.paths.repo;
    check_target(repo)?;
    let mut prior_origin = None;
    if repo.exists() {
        if repo.join(".git").exists() {
            prior_origin = ctx.git.remote_url("origin")?;
        }
        if let Some(edited) = edited {
            edited.back_up()?;
        }
        for name in [".dotman.toml", bootstrap::FILE] {
            match fs::remove_file(repo.join(name)) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::NotFound => {}
                Err(e) => return Err(e).with_context(|| format!("cannot remove the old {name}")),
            }
        }
        let git_dir = repo.join(".git");
        if is_real_dir(&git_dir) {
            fs::remove_dir_all(&git_dir)
                .with_context(|| format!("cannot remove {}", git_dir.display()))?;
        }
        fs::remove_dir(repo).with_context(|| format!("cannot replace {}", repo.display()))?;
    }
    fs::rename(&staging.repo, repo)
        .with_context(|| format!("cannot move the import into {}", repo.display()))?;
    Ok(prior_origin)
}

/// [`install`], then the `origin` that `dotman init` had set when the import brings none.
pub(super) fn install_keeping_origin(
    ctx: &Context,
    staging: &Staging,
    edited: Option<&EditedConfig>,
) -> Result<()> {
    let prior_origin = install(ctx, staging, edited)?;
    if let Some(url) = prior_origin
        && ctx.git.remote_url("origin")?.is_none()
    {
        ctx.git.set_remote_url("origin", &url)?;
        output::info(&format!(
            "origin is {} again, as `{} init` set it",
            redact_credentials(&url),
            name::bin()
        ));
    }
    Ok(())
}

/// The machine config with yadm's `local.*` settings in it, and what was left alone.
fn yadm_machine(ctx: &Context, file: &Path) -> Result<(Config, Vec<String>)> {
    let mut machine = ctx.machine.clone();
    let mut notes = Vec::new();
    let git = &ctx.git;
    for class in git.config_values(file, "local.class")? {
        if !machine.profiles.contains(&class) {
            machine.profiles.push(class);
        }
    }
    let last = |key: &str| -> Result<Option<String>> {
        Ok(git
            .config_values(file, key)?
            .pop()
            .filter(|v| !v.is_empty()))
    };
    let os = last("local.os")?;
    let mapped = os.as_deref().and_then(yadm::dotman_os).map(str::to_owned);
    if let Some(os) = os.as_ref().filter(|_| mapped.is_none()) {
        notes.push(format!(
            "yadm's local.os is {os}, which is not Linux, Darwin or WSL; it is not copied"
        ));
    }
    let identity = &mut machine.identity;
    let settings = [
        (&mut identity.host, "hostname", last("local.hostname")?),
        (&mut identity.user, "user", last("local.user")?),
        (&mut identity.distro, "distro", last("local.distro")?),
        (&mut identity.arch, "arch", last("local.arch")?),
        (&mut identity.os, "os", mapped),
    ];
    for (slot, name, value) in settings {
        let Some(value) = value else { continue };
        match slot {
            Some(existing) if !existing.eq_ignore_ascii_case(&value) => notes.push(format!(
                "yadm's local.{name} is {value}, but the machine config sets {existing}; the machine config is kept"
            )),
            Some(_) => {}
            None => *slot = Some(value),
        }
    }
    Ok((machine, notes))
}

/// What yadm leaves outside the tree that dotman does not take over.
fn yadm_files(ctx: &Context, found: &Analysis) -> Vec<String> {
    let mut notes = Vec::new();
    let yadm_dir = ctx.paths.config_home.join("yadm");
    let in_tree = |name: &str| found.kept.iter().any(|kept| kept.to == name);
    let encrypt = yadm_dir.join("encrypt");
    let archive = ctx.paths.data_home.join("yadm/archive");
    let patterns = fs::read_to_string(&encrypt).ok().map(|text| {
        text.lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with('#'))
            .map(str::to_owned)
            .collect::<Vec<_>>()
    });
    if patterns.is_some() || archive.exists() || in_tree(".config/yadm/encrypt") {
        let which = patterns.as_ref().map_or_else(
            || "yadm's encrypted files".to_owned(),
            |patterns| {
                format!(
                    "yadm's encrypt patterns ({}): {}",
                    patterns.len(),
                    shown(patterns)
                )
            },
        );
        notes.push(format!(
            "{which}. dotman does not read yadm archives: decrypt them with `yadm decrypt`, then store each file with `{} add --encrypt <path>`",
            name::bin()
        ));
    }
    let bootstrap = yadm_dir.join("bootstrap");
    if bootstrap.exists() || in_tree(".config/yadm/bootstrap") {
        notes.push(
            "a yadm bootstrap script exists; dotman has no run-once scripts, so run it by hand or turn parts of it into `[hooks]`"
                .to_owned(),
        );
    }
    notes
}

/// What yadm left in `$HOME`, listed and never changed.
fn home_notes(ctx: &Context, found: &Analysis) -> Vec<String> {
    let home = &ctx.paths.home;
    let links: Vec<String> = found
        .targets
        .iter()
        .filter_map(|target| {
            let dest = fs::read_link(home.join(target)).ok()?;
            dest.file_name()?
                .to_string_lossy()
                .contains("##")
                .then(|| format!("{target} -> {}", dest.display()))
        })
        .collect();
    let files: Vec<String> = found
        .alternates
        .iter()
        .filter(|path| fs::symlink_metadata(home.join(path)).is_ok())
        .cloned()
        .collect();
    let mut notes = Vec::new();
    if !links.is_empty() {
        notes.push(format!(
            "yadm's alternate links in $HOME ({}) block the converted files until `{} import --swap` replaces them: {}",
            links.len(),
            name::bin(),
            shown(&links)
        ));
    }
    if !files.is_empty() {
        notes.push(format!(
            "{} of the source are in $HOME; they can be removed once yadm is gone: {}",
            count(files.len(), "`##` file"),
            shown(&files)
        ));
    }
    notes
}

/// The other vcsh repos beside `source`, as `--overlay` commands.
fn vcsh_siblings(ctx: &Context, source: &Path) -> Option<String> {
    let dir = vcsh_dir(ctx);
    let same = |a: &Path, b: &Path| match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    };
    if !same(source.parent()?, &dir) {
        return None;
    }
    let others: Vec<String> = vcsh_repos(ctx)
        .into_iter()
        .filter(|path| !same(path, source))
        .map(|path| {
            format!(
                "{} import --overlay {}",
                name::bin(),
                shell_words::quote(&path.to_string_lossy())
            )
        })
        .collect();
    (!others.is_empty()).then(|| {
        format!(
            "{} more in {}; add their files with: {}",
            count(others.len(), "vcsh repo"),
            dir.display(),
            others.join("; ")
        )
    })
}

fn remote_notes(
    source: &Source,
    source_origin: Option<&str>,
    found: &Analysis,
    squash: bool,
) -> Vec<String> {
    let mut notes = vec![format!("imported from {}", source.display())];
    match source {
        // The squashed commit shares nothing with the source's history, so sync would refuse to
        // pull from it, and pushing there would keep the credentials its history holds.
        _ if squash => notes.push(format!(
            "the new repo has no remote, and the source cannot be one: the source still holds the history this import left out, credentials included, and shares no commit with it. Set a new, empty repo as the remote with `{} init --remote <url>`",
            name::bin()
        )),
        Source::Path(_) => {
            let advice = source_origin.map_or_else(
                || format!("add one with `{} init --remote <url>`", name::bin()),
                |url| {
                    let url = redact_credentials(url);
                    format!(
                        "the source's own origin is {url}; `{} init --remote {url}` sets it here",
                        name::bin()
                    )
                },
            );
            notes.push(format!(
                "the new repo has no remote, as a local path is no use as one; {advice}"
            ));
        }
        Source::Url(_) => notes.push(format!(
            "origin is {}; the conversion commit is pushed by the next `{} sync`",
            source.display(),
            name::bin()
        )),
    }
    if !found.alternates.is_empty() {
        notes.push(
            "pushing renamed alternates to a remote that other yadm machines still pull from breaks their alternates until they migrate too"
                .to_owned(),
        );
    }
    notes
}

/// What yadm's own files add to an import.
struct YadmSetup {
    is_yadm: bool,
    /// The machine config with yadm's `local.*` settings, when that differs from the current one.
    machine: Option<Config>,
    notes: Vec<String>,
}

fn yadm_setup(ctx: &Context, source: &Source, tree: &[TreeEntry]) -> Result<YadmSetup> {
    let is_yadm_repo = yadm_repos(ctx).iter().any(|path| same_dir(path, source));
    let is_yadm = is_yadm_repo
        || tree.iter().any(|entry| {
            entry.path.starts_with(".config/yadm")
                || entry
                    .path
                    .as_os_str()
                    .as_bytes()
                    .windows(2)
                    .any(|w| w == b"##")
        });
    if !is_yadm {
        return Ok(YadmSetup {
            is_yadm,
            machine: None,
            notes: Vec::new(),
        });
    }
    let (machine, notes) = yadm_machine(ctx, &ctx.paths.config_home.join("yadm/config"))?;
    Ok(YadmSetup {
        is_yadm,
        machine: Some(machine).filter(|machine| *machine != ctx.machine),
        notes,
    })
}

impl YadmSetup {
    fn notes(&self, ctx: &Context, found: &Analysis, dry_run: bool) -> Vec<String> {
        let mut notes = Vec::new();
        if let Some(machine) = &self.machine {
            let before = &ctx.machine;
            let added: Vec<&str> = machine
                .profiles
                .iter()
                .filter(|profile| !before.profiles.contains(profile))
                .map(String::as_str)
                .collect();
            let mut changes = Vec::new();
            if !added.is_empty() {
                changes.push(format!("profiles {}", added.join(", ")));
            }
            if machine.identity != before.identity {
                changes.push("[identity] overrides".to_owned());
            }
            notes.push(format!(
                "yadm's local settings ({}) {} the machine config {}",
                changes.join(" and "),
                if dry_run { "would go into" } else { "go into" },
                ctx.paths.config_file.display()
            ));
        }
        notes.extend(self.notes.iter().cloned());
        if !found.env.is_empty() {
            notes.push(format!(
                "converted templates read the environment variables {}; add them to `[template] env` in {}",
                found.env.iter().cloned().collect::<Vec<_>>().join(", "),
                ctx.paths.config_file.display()
            ));
        }
        if !found.cased.is_empty() {
            let printed: BTreeSet<&str> = found.cased.values().flatten().copied().collect();
            let files: Vec<String> = found.cased.keys().cloned().collect();
            notes.push(format!(
                "converted templates print yadm.{}, which dotman spells in lowercase (yadm prints Linux, dotman linux), so their rendered text changes case: {}",
                printed.into_iter().collect::<Vec<_>>().join(" and yadm."),
                shown(&files)
            ));
        }
        if self.is_yadm {
            notes.extend(yadm_files(ctx, found));
            notes.extend(home_notes(ctx, found));
        }
        notes
    }
}

/// Clones `source` into the dotman repo and converts it.
pub(super) fn import(ctx: &Context, source: &Source, args: &ImportArgs) -> Result<Outcome> {
    check_source(source)?;
    require_fresh(ctx)?;
    author_email(ctx)?;
    check_target(&ctx.paths.repo)?;
    let edited = EditedConfig::find(ctx)?;
    let staging = Staging::new(ctx, !args.dry_run)?;
    let source_origin = match source {
        Source::Path(path) => git_at(path).remote_url("origin")?,
        Source::Url(_) => None,
    };
    staging.clone_source(source)?;
    if matches!(source, Source::Path(_)) {
        staging.git.remove_remote("origin")?;
    }
    let tree = staging.git.ls_tree("HEAD")?;

    let setup = yadm_setup(ctx, source, &tree)?;
    let identity = identity::current(setup.machine.as_ref().unwrap_or(&ctx.machine));
    let mut found = analyze(&staging.git, &tree, &identity);
    let has_own = found.kept.iter().any(|kept| kept.to == ".dotman.toml");
    let own = has_own.then(|| RepoConfig::load(&staging.repo.join(".dotman.toml")));
    let allow = &ctx.machine.guard.allow;
    guard_content(&staging.repo, &mut found, allow);
    let tainted = tainted_history(&staging, allow)?;
    let squash = !tainted.is_empty() && !args.keep_history;
    let mut modes = home_modes(ctx, &staging.git, &found);
    let config = match own {
        None => SourceConfig::None,
        Some(Ok(own)) => {
            modes.retain(|key, _| !own.modes.contains_key(key));
            SourceConfig::Parses
        }
        Some(Err(_)) => SourceConfig::Broken,
    };
    let mut report = tally(&found, !squash);
    report.notes.extend(home_modes_notes(&modes, config));
    report.notes.extend(remote_notes(
        source,
        source_origin.as_deref(),
        &found,
        squash,
    ));
    report.notes.extend(setup.notes(ctx, &found, args.dry_run));
    if let Source::Path(path) = source {
        report.notes.extend(vcsh_siblings(ctx, path));
    }
    if let Some(note) = edited.as_ref().map(|edited| edited.note(args.dry_run)) {
        report.notes.push(note);
    }
    report.print();

    let commits = count(staging.git.log(&[], None)?.len(), "commit");
    warn_tainted(&tainted, squash, &commits);
    let repo = &ctx.paths.repo;
    // The same count in every line, the conversion commit apart from the source's.
    let (what, added, to_add) = if squash {
        (
            format!("the current tree of {commits} as one commit"),
            "",
            "",
        )
    } else if conversion(&found).is_some() {
        (
            commits,
            " and added one conversion commit",
            " and add one conversion commit",
        )
    } else {
        (commits, "", "")
    };
    if args.dry_run {
        output::info(&format!(
            "would import {what}{to_add} into {}",
            repo.display()
        ));
        return Ok(report.outcome());
    }
    let question = format!("import {what}{to_add} into {}? [y/N]", repo.display());
    if !proceed(args, &question)? {
        output::info("nothing imported");
        return Ok(Outcome::Done);
    }
    convert(&staging.git, &staging.repo, &found, &modes)?;
    if squash {
        squash_history(&staging)?;
    }
    install_keeping_origin(ctx, &staging, edited.as_ref())?;
    if let Some(machine) = setup.machine {
        machine.save(&ctx.paths.config_file, false)?;
    }
    output::success(&format!("imported {what}{added} into {}", repo.display()));
    super::print_next_steps();
    Ok(report.outcome())
}

fn same_dir(a: &Path, source: &Source) -> bool {
    let Source::Path(b) = source else {
        return false;
    };
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

fn check_source(source: &Source) -> Result<()> {
    if let Source::Path(path) = source {
        ensure!(
            is_git_source(path),
            "{} is not a source `{} import` understands; a legacy dotman repo holds index.bin, or HEAD and commits/, and a git source is a repo, a bare repo or a URL",
            path.display(),
            name::bin()
        );
    }
    Ok(())
}

struct Addition {
    path: String,
    symlink: bool,
    executable: bool,
    bytes: Vec<u8>,
}

struct Overlay {
    report: Report,
    files: Vec<Addition>,
    missing: Vec<String>,
}

fn content(git: &Git, kept: &Kept) -> Result<Vec<u8>> {
    kept.body.as_ref().map_or_else(
        || git.show_blob("HEAD", &kept.from),
        |body| Ok(body.clone().into_bytes()),
    )
}

/// Compares the converted source with the repo worktree. Nothing in the repo is changed.
fn compare(ctx: &Context, git: &Git, found: &Analysis, include_missing: bool) -> Result<Overlay> {
    let allow = &ctx.machine.guard.allow;
    let mut overlay = Overlay {
        report: tally(
            &Analysis {
                kept: Vec::new(),
                left: found
                    .left
                    .iter()
                    .map(|left| Left {
                        reason: left.reason,
                        detail: left.detail.clone(),
                        path: left.path.clone(),
                        remove: false,
                    })
                    .collect(),
                ..Analysis::default()
            },
            false,
        ),
        files: Vec::new(),
        missing: Vec::new(),
    };
    let mut converted: usize = 0;
    let mut reserved = Vec::new();
    for kept in &found.kept {
        if kept.reserved {
            converted = converted.saturating_add(1);
            reserved.push(kept.to.clone());
            continue;
        }
        let key = kept.to.as_str();
        let symlink = kept.mode == "120000";
        let executable = kept.mode == "100755";
        let mut needs = |reason: &'static str, detail: String| {
            overlay.report.needs_action.push(Item { reason, detail });
        };
        match read_node(&ctx.paths.repo, key)? {
            Node::Missing if include_missing => {
                let bytes = content(git, kept)?;
                if !symlink && let Some(item) = held(&bytes, key, allow) {
                    needs(item.reason, item.detail);
                    continue;
                }
                overlay.files.push(Addition {
                    path: kept.to.clone(),
                    symlink,
                    executable,
                    bytes,
                });
                converted = converted.saturating_add(1);
            }
            Node::Missing => {
                overlay.missing.push(kept.to.clone());
                needs(
                    MISSING,
                    format!("{key}: not in the repo; `--include-missing` adds it"),
                );
            }
            Node::File { bytes, mode, .. } if !symlink => {
                if bytes != content(git, kept)? {
                    needs(DIFFERS, key.to_owned());
                } else if (mode & 0o100 != 0) == executable {
                    converted = converted.saturating_add(1);
                } else {
                    needs(MODE_DIFFERS, key.to_owned());
                }
            }
            Node::Symlink(target) if symlink => {
                if target.as_os_str().as_bytes() == content(git, kept)?.as_slice() {
                    converted = converted.saturating_add(1);
                } else {
                    needs(DIFFERS, key.to_owned());
                }
            }
            Node::File { .. }
            | Node::Symlink(_)
            | Node::Dir
            | Node::Special
            | Node::BadParent(_) => {
                needs(
                    BLOCKED,
                    format!("{key}: the repo has a different kind of entry there"),
                );
            }
        }
    }
    overlay.report.converted = converted;
    if !reserved.is_empty() {
        overlay.report.notes.push(format!(
            "{} are never applied and were not added: {}",
            count(reserved.len(), "root file"),
            shown(&reserved)
        ));
    }
    Ok(overlay)
}

fn overlay_message(ctx: &Context, source: &Source) -> String {
    let name = match source {
        Source::Path(path) => path.strip_prefix(&ctx.paths.home).map_or_else(
            |_| path.display().to_string(),
            |rest| format!("~/{}", rest.display()),
        ),
        Source::Url(_) => source.display(),
    };
    format!("import: overlay from {name}")
}

/// Adds the files of `source` that the repo lacks as one commit, and reports the ones that differ.
pub(super) fn overlay(ctx: &Context, source: &Source, args: &ImportArgs) -> Result<Outcome> {
    check_source(source)?;
    ctx.repo_id()?;
    require_clean(ctx)?;
    let staging = Staging::new(ctx, false)?;
    staging.clone_source(source)?;
    let tree = staging.git.ls_tree("HEAD")?;
    let found = analyze(&staging.git, &tree, ctx.identity());
    let mut compared = compare(ctx, &staging.git, &found, args.include_missing)?;
    if legacy::add_missing(args, &compared.missing)? {
        compared = compare(ctx, &staging.git, &found, true)?;
    }
    let Overlay { report, files, .. } = compared;
    if !files.is_empty() && !ctx.git.identity_configured()? {
        bail!(crate::commands::sync::identity_fix());
    }
    report.print();
    let message = overlay_message(ctx, source);
    let summary = count(files.len(), "file");
    if files.is_empty() {
        output::info("nothing to overlay; no commit made");
        return Ok(report.outcome());
    }
    if args.dry_run {
        output::info(&format!("would commit: {message} ({summary})"));
        return Ok(report.outcome());
    }
    if !proceed(args, &format!("commit the overlay ({summary})? [y/N]"))? {
        output::info("nothing written");
        return Ok(Outcome::Done);
    }
    let repo = &ctx.paths.repo;
    for file in files {
        ensure_worktree_parents(repo, &file.path)?;
        let path = repo.join(&file.path);
        if file.symlink {
            std::os::unix::fs::symlink(OsStr::from_bytes(&file.bytes), &path)
                .with_context(|| format!("cannot create the symlink {}", file.path))?;
        } else {
            let mode = if file.executable { 0o755 } else { 0o644 };
            write_atomic(&path, &file.bytes, mode)?;
        }
    }
    ctx.git.stage_all()?;
    if ctx.git.commit(&message)?.is_some() {
        output::success(&format!("committed: {message} ({summary})"));
        super::print_next_steps();
    } else {
        output::info("nothing to overlay; no commit made");
    }
    Ok(report.outcome())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(paths: &[(&str, &str)]) -> Vec<(String, String)> {
        paths
            .iter()
            .map(|(p, m)| ((*p).to_owned(), (*m).to_owned()))
            .collect()
    }

    fn left_paths(found: &Analysis) -> Vec<&str> {
        found.left.iter().map(|l| l.path.as_str()).collect()
    }

    #[test]
    fn clashing_names_are_all_left_and_a_default_gives_way_to_the_plain_file() {
        let found = settle(&entries(&[
            ("a##o.Linux", "100644"),
            ("a##os.linux", "100644"),
            ("b", "100644"),
            ("b##default", "100644"),
            ("c##o.Darwin", "100644"),
            ("d/e", "100644"),
            ("d##default", "100644"),
        ]));
        let mut left = left_paths(&found);
        left.sort_unstable();
        assert_eq!(
            left,
            ["a##o.Linux", "a##os.linux", "b##default", "d##default"]
        );
        assert!(found.left.iter().all(|l| l.remove));
        let kept: Vec<(&str, &str)> = found
            .kept
            .iter()
            .map(|k| (k.from.as_str(), k.to.as_str()))
            .collect();
        assert_eq!(
            kept,
            [("b", "b"), ("c##o.Darwin", "c{os=macos}"), ("d/e", "d/e")]
        );
    }

    #[test]
    fn root_names_that_dotman_never_applies_stay_as_unmanaged_files() {
        let found = settle(&entries(&[
            ("README.md", "100644"),
            ("install.sh", "100755"),
            (".dotman.toml", "100644"),
            (".zshrc", "100644"),
            ("sub", "160000"),
        ]));
        let reserved: Vec<&str> = found
            .kept
            .iter()
            .filter(|k| k.reserved)
            .map(|k| k.from.as_str())
            .collect();
        assert_eq!(reserved, [".dotman.toml", "README.md", "install.sh"]);
        assert_eq!(left_paths(&found), ["sub"]);
        assert!(!found.left.iter().any(|l| l.remove));
    }

    #[test]
    fn the_report_counts_each_entry_once() {
        let found = settle(&entries(&[
            ("a", "100644"),
            ("b##o.Linux", "100644"),
            ("c##h.my.host", "100644"),
            ("README", "100644"),
        ]));
        let report = tally(&found, true);
        assert_eq!(report.total(), 4);
        assert_eq!(report.converted, 3);
        let detail = &report.needs_action.first().expect("an item").detail;
        assert!(
            detail.contains("dotman git show 'HEAD~1:c##h.my.host'"),
            "{detail}"
        );
    }

    #[test]
    fn the_listing_is_cut_after_ten() {
        let items: Vec<String> = (0..12).map(|n| n.to_string()).collect();
        assert!(shown(&items).ends_with("9 (+2 more)"));
        assert_eq!(shown(items.get(..2).expect("two")), "0, 1");
    }

    #[test]
    fn a_commit_line_is_named_by_the_header_or_message_that_holds_it() {
        let commit = b"tree 1\nparent 2\nauthor a <a@x> 1 +0000\ncommitter c <c@x> 1 +0000\nmergetag object 3\n type commit\n \n tag body\ngpgsig -----BEGIN-----\n sig\n\nsubject\n\nbody\n";
        let named = [
            (1, "commit header"),
            (3, "commit author"),
            (4, "commit committer"),
            (5, "merged tag"),
            (7, "merged tag"),
            (8, "merged tag"),
            (10, "commit signature"),
            (11, "commit message"),
            (12, "commit message"),
            (14, "commit message"),
        ];
        for (line, field) in named {
            assert_eq!(commit_field(commit, line), field, "line {line}");
        }
    }
}
