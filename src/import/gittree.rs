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

use super::yadm::{self, Alternate, Machine};
use super::{
    EditedConfig, Item, Report, Source, author_email, count, find_legacy, homes, legacy, proceed,
    require_fresh,
};
use crate::cli::ImportArgs;
use crate::commands::Outcome;
use crate::commands::add::ensure_worktree_parents;
use crate::config::{Config, TemplateConfig};
use crate::context::{Context, is_real_dir};
use crate::engine::{Node, read_node};
use crate::git::{Git, TreeEntry, redact_credentials};
use crate::identity;
use crate::output;
use crate::paths::{
    create_private_dir, is_reserved_root_name, private_temp_dir, validate_key, write_atomic,
};
use crate::repo_config::RepoConfig;
use crate::state::IdentitySnapshot;
use crate::template::{self, Vars};
use crate::variant::{Selection, Sources};
use anyhow::{Context as _, Result, anyhow, bail, ensure};
use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::fs;
use std::io;
use std::ops::ControlFlow;
use std::os::unix::ffi::OsStrExt;
use std::path::{Path, PathBuf};
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

/// A source `dot import` found by itself.
pub(super) struct Candidate {
    pub(super) label: &'static str,
    pub(super) path: PathBuf,
}

fn is_git_dir(dir: &Path) -> bool {
    dir.join("HEAD").is_file() && dir.join("objects").is_dir() && dir.join("refs").is_dir()
}

fn is_git_source(dir: &Path) -> bool {
    is_git_dir(dir) || dir.join(".git").exists()
}

fn git_at(dir: &Path) -> Git {
    Git::new(dir, "import")
}

/// Legacy, yadm, bare dot-directories of `$HOME`, then vcsh, in the order `find` reports them.
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
    Ok(found)
}

/// yadm's repo in `$XDG_DATA_HOME`, which is where yadm keeps it, and in `~/.local/share`.
fn yadm_repos(ctx: &Context) -> Vec<PathBuf> {
    let mut repos: Vec<PathBuf> = Vec::new();
    for base in [&ctx.paths.data_home, &ctx.paths.home.join(".local/share")] {
        let dir = base.join("yadm/repo.git");
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

/// The one source `dot import` takes when none is given, or the exit code after listing several.
pub(super) fn find(ctx: &Context) -> Result<ControlFlow<Outcome, Source>> {
    let found = candidates(ctx)?;
    match found.as_slice() {
        [] => bail!(
            "no source given and none found: looked for a legacy dotman repo in {home}/.dotman, a yadm repo, bare repos directly under {home} and vcsh repos in {vcsh}; run `dot import <source>` with a path or a git URL",
            home = ctx.paths.home.display(),
            vcsh = vcsh_dir(ctx).display()
        ),
        [one] => Ok(ControlFlow::Continue(Source::Path(one.path.clone()))),
        many => {
            output::warning("several sources found; `dot import` will not pick one for you:");
            for candidate in many {
                output::warning(&format!(
                    "  dot import {}    # {}",
                    shell_words::quote(&candidate.path.to_string_lossy()),
                    candidate.label
                ));
            }
            Ok(ControlFlow::Break(Outcome::Attention))
        }
    }
}

/// The source's clone, in a directory that is removed when this is dropped.
struct Staging {
    _dir: TempDir,
    repo: PathBuf,
    git: Git,
}

impl Staging {
    /// Beside the dotman repo for an import, so that moving it into place is a rename, and in
    /// the system temp directory otherwise, so that nothing is created in the dotman repo's
    /// parent.
    fn new(ctx: &Context, beside_repo: bool) -> Result<Self> {
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
        self.git.clone_from(&source.git_arg()?)?;
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

fn shown(items: &[String]) -> String {
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
                "{}; left out of the repo, `dot git show {}` prints it",
                left.detail,
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

/// Writes the conversion into the worktree of `repo` and commits it.
fn convert(git: &Git, repo: &Path, found: &Analysis) -> Result<()> {
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
        RepoConfig::default().save(&repo.join(".dotman.toml"), false)?;
    }
    let message = if !found.alternates.is_empty() {
        "import: convert yadm alternates"
    } else if !has_config {
        "import: add .dotman.toml"
    } else {
        return Ok(());
    };
    git.stage_all()?;
    git.commit(message)?;
    Ok(())
}

/// The repo directory can be replaced by the staging clone when it holds nothing but a `.git`
/// without commits and a `.dotman.toml`.
fn check_target(repo: &Path) -> Result<()> {
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
        ensure!(
            name == ".git" || name == ".dotman.toml",
            "{} holds {}, which `dot import` would not keep; move it away and run the import again",
            repo.display(),
            name.to_string_lossy()
        );
    }
    Ok(())
}

/// Moves the staging clone to the repo path. An empty repo that `dot init` made is replaced; its
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
        match fs::remove_file(repo.join(".dotman.toml")) {
            Ok(()) => {}
            Err(e) if e.kind() == io::ErrorKind::NotFound => {}
            Err(e) => return Err(e).context("cannot remove the old .dotman.toml"),
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
            "{which}. dotman does not read yadm archives: decrypt them with `yadm decrypt`, then store each file with `dot add --encrypt <path>`"
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
            "yadm's alternate links in $HOME ({}) block the converted files until `dot import --swap`, coming in the same release, replaces them: {}",
            links.len(),
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
                "dot import --overlay {}",
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

fn remote_notes(source: &Source, source_origin: Option<&str>, found: &Analysis) -> Vec<String> {
    let mut notes = vec![format!("imported from {}", source.display())];
    match source {
        Source::Path(_) => {
            let advice = source_origin.map_or_else(
                || "add one with `dot init --remote <url>`".to_owned(),
                |url| {
                    let url = redact_credentials(url);
                    format!(
                        "the source's own origin is {url}; `dot init --remote {url}` sets it here"
                    )
                },
            );
            notes.push(format!(
                "the new repo has no remote, as a local path is no use as one; {advice}"
            ));
        }
        Source::Url(_) => notes.push(format!(
            "origin is {}; the conversion commit is pushed by the next `dot sync`",
            source.display()
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
    let found = analyze(&staging.git, &tree, &identity);
    let mut report = tally(&found, true);
    report
        .notes
        .extend(remote_notes(source, source_origin.as_deref(), &found));
    report.notes.extend(setup.notes(ctx, &found, args.dry_run));
    if let Source::Path(path) = source {
        report.notes.extend(vcsh_siblings(ctx, path));
    }
    if let Some(note) = edited.as_ref().map(|edited| edited.note(args.dry_run)) {
        report.notes.push(note);
    }
    report.print();

    let commits = count(staging.git.log(&[], None)?.len(), "commit");
    let repo = &ctx.paths.repo;
    if args.dry_run {
        output::info(&format!(
            "would import {commits} and add one conversion commit into {}",
            repo.display()
        ));
        return Ok(report.outcome());
    }
    let question = format!("import {commits} into {}? [y/N]", repo.display());
    if !proceed(args, &question)? {
        output::info("nothing imported");
        return Ok(Outcome::Done);
    }
    convert(&staging.git, &staging.repo, &found)?;
    let prior_origin = install(ctx, &staging, edited.as_ref())?;
    if let Some(url) = prior_origin
        && ctx.git.remote_url("origin")?.is_none()
    {
        ctx.git.set_remote_url("origin", &url)?;
        output::info(&format!(
            "origin is {} again, as `dot init` set it",
            redact_credentials(&url)
        ));
    }
    if let Some(machine) = setup.machine {
        machine.save(&ctx.paths.config_file, false)?;
    }
    let total = count(ctx.git.log(&[], None)?.len(), "commit");
    output::success(&format!("imported {total} into {}", repo.display()));
    output::info("next: run `dot status`, then `dot apply`");
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
            "{} is not a source `dot import` understands; a legacy dotman repo holds index.bin or commits/, and a git source is a repo, a bare repo or a URL",
            path.display()
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
                overlay.files.push(Addition {
                    path: kept.to.clone(),
                    symlink,
                    executable,
                    bytes: content(git, kept)?,
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
    ensure!(
        ctx.git.status()?.is_empty(),
        "the repo at {} has uncommitted changes; run `dot sync` first, so the overlay commit holds only the overlay",
        ctx.paths.repo.display()
    );
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
        bail!(crate::commands::sync::IDENTITY_FIX);
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
        output::info("next: run `dot status`, then `dot apply`");
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
            detail.contains("dot git show 'HEAD~1:c##h.my.host'"),
            "{detail}"
        );
    }

    #[test]
    fn the_listing_is_cut_after_ten() {
        let items: Vec<String> = (0..12).map(|n| n.to_string()).collect();
        assert!(shown(&items).ends_with("9 (+2 more)"));
        assert_eq!(shown(items.get(..2).expect("two")), "0, 1");
    }
}
