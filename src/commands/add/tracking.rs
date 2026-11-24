//! Reviewed changes to the set of Home paths managed by the desktop.

use super::{
    DirPlan, Hold, Leaf, Planner, Store, adopt_dirs, commit_stored, hold_by_name_and_size,
    node_hold, refuse_collisions, secret_patterns,
};
use crate::commands::forget;
use crate::context::{Context, is_under};
use crate::engine::{Node, parents, read_node};
use crate::paths::validate_key;
use crate::repo_config::RepoConfig;
use crate::state::{Kind, State};
use anyhow::{Context as _, Result, bail, ensure};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::os::unix::ffi::OsStrExt;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    schema_version: u32,
    operation: Action,
    paths: Vec<String>,
    repo_id: String,
    expected: Option<String>,
    #[serde(default)]
    excluded: BTreeSet<usize>,
    #[serde(default)]
    keep_folder_rule: bool,
}

#[derive(Copy, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Action {
    TrackFiles,
    TrackFolder,
    StopTracking,
}

#[derive(Default, Serialize)]
struct Inventory {
    // The token binds the file kind, mode and bytes. Only paths and reasons reach the UI.
    files: BTreeMap<String, (Kind, String)>,
    folders: BTreeSet<String>,
    held: BTreeMap<String, String>,
    existing: BTreeSet<String>,
    removal: BTreeMap<String, Vec<forget::reviewed::Source>>,
    parent_modes: BTreeMap<String, u32>,
}

pub(super) fn fingerprint(ctx: &Context, node: &Node) -> Result<String> {
    let (kind, mode, bytes) = match node {
        Node::File { bytes, mode, .. } => ("file", *mode, bytes.as_slice()),
        Node::Symlink(path) => ("link", 0, path.as_os_str().as_bytes()),
        Node::Missing => bail!("file disappeared during review"),
        _ => bail!("unsupported file type"),
    };
    Ok(format!(
        "{kind}-{mode}-{}",
        ctx.git.hash_object(bytes, false)?
    ))
}

#[cfg(target_os = "linux")]
pub(super) fn destination(
    repo: &std::path::Path,
    key: &str,
) -> Result<(std::fs::File, std::path::PathBuf)> {
    use std::ffi::CString;
    use std::fs::{File, OpenOptions};
    use std::os::fd::{AsFd, AsRawFd};
    use std::os::unix::fs::OpenOptionsExt;
    let mut parent = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
        .open(repo)?;
    let path = std::path::Path::new(key);
    for component in path.parent().context("missing parent")? {
        let name = CString::new(component.as_bytes())?;
        let opened = match crate::sys::openat_dir(parent.as_fd(), &name) {
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                match crate::sys::mkdirat(parent.as_fd(), &name, 0o755) {
                    Err(e) if e.kind() != io::ErrorKind::AlreadyExists => return Err(e.into()),
                    _ => {}
                }
                crate::sys::openat_dir(parent.as_fd(), &name)?
            }
            other => other?,
        };
        parent = File::from(opened);
    }
    let target = std::path::PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()))
        .join(path.file_name().context("missing filename")?);
    Ok((parent, target))
}

fn tracking_reason(hold: Hold) -> String {
    match hold {
        Hold::SecretName => String::from("The filename matches a secret-file pattern."),
        Hold::Large => String::from("This file is larger than 1 MiB."),
        Hold::Content(finding) => finding.to_string(),
        _ => hold.reason(),
    }
}

impl Inventory {
    fn add(ctx: &Context, req: &Request, config: &RepoConfig, state: &State) -> Result<Self> {
        let inputs = req
            .paths
            .iter()
            .map(|p| ctx.paths.home.join(p))
            .collect::<Vec<_>>();
        let mut result = Self::default();
        let mut planner = Planner::new(ctx, config, state, ctx.paths.home.clone())?;
        for (key, input) in req.paths.iter().zip(&inputs) {
            if let Err(error) = planner.input(input) {
                if req.operation != Action::TrackFiles {
                    return Err(error);
                }
                let reason = if crate::secrets::held_name(key).is_some() {
                    "Protected filename"
                } else {
                    "Cannot track this selection as a regular file or symbolic link"
                };
                result.held.insert(key.clone(), reason.to_owned());
            }
        }
        let plan = planner.out;
        ensure!(
            match req.operation {
                Action::TrackFolder => plan.dirs.len() == 1 && plan.files.is_empty(),
                Action::TrackFiles => plan.dirs.is_empty(),
                Action::StopTracking => false,
            },
            "selection changed kind"
        );
        let sources = ctx.sources()?;
        refuse_collisions(&sources, &plan)?;
        let patterns = secret_patterns(config)?;
        for dir in &plan.dirs {
            if !dir.discovery_ignored.contains_key(&dir.key) {
                result.folders.insert(dir.key.clone());
            }
            for (key, reason) in &dir.discovery_ignored {
                result.held.insert(key.clone(), (*reason).to_owned());
            }
            for (key, hold) in &dir.held {
                result.held.insert(key.clone(), tracking_reason(*hold));
            }
            for key in &dir.dangling {
                result
                    .held
                    .insert(key.clone(), String::from("Dangling symbolic link"));
            }
            for (key, entry) in &dir.still_ignored {
                result.held.insert(
                    key.clone(),
                    format!("Retained ignore rule {}", entry.pattern),
                );
            }
            for note in &dir.notes {
                result
                    .held
                    .insert(note.clone(), String::from("Unsupported entry"));
            }
        }
        for (key, leaf) in plan.items().0 {
            if sources.contains(&key) || state.files.contains_key(&key) {
                result.existing.insert(key);
                continue;
            }
            let size = match leaf {
                Leaf::File { size, .. } => size,
                Leaf::Link => 0,
            };
            if let Some(hold) = hold_by_name_and_size(&patterns, &key, size) {
                result.held.insert(key, tracking_reason(hold));
                continue;
            }
            if matches!(leaf, Leaf::File { binary: true, .. })
                && req.operation == Action::TrackFolder
            {
                result.held.insert(key, String::from("Binary file"));
                continue;
            }
            ctx.check_key(&key, true)?;
            ensure!(
                matches!(read_node(&ctx.paths.repo, &key)?, Node::Missing),
                "destination is occupied"
            );
            let node = read_node(&ctx.paths.home, &key)?;
            if let Some(finding) = node_hold(ctx, &node, &key, &key, false) {
                result
                    .held
                    .insert(key, format!("Protected content ({})", finding.rule));
                continue;
            }
            for parent in parents(&key) {
                super::dir_mode(&mut result.parent_modes, &ctx.paths.home, parent)?;
            }
            result
                .files
                .insert(key, (leaf.kind(), fingerprint(ctx, &node)?));
        }
        Ok(result)
    }

    fn receipt(&self, token: &str, execute: bool) -> serde_json::Value {
        let display = |text: &str| {
            if crate::secrets::held_name(text).is_some() {
                "[protected filename]".to_owned()
            } else {
                crate::output::escape_line(text).into_owned()
            }
        };
        let held = self
            .held
            .iter()
            .enumerate()
            .map(|(index, (path, reason))| {
                let name = if crate::secrets::held_name(path).is_some() {
                    // A numbered absolute label cannot collide with a Home-relative managed key.
                    format!("/[protected filename {}]", index.saturating_add(1))
                } else {
                    display(path)
                };
                (name, display(reason))
            })
            .collect::<BTreeMap<_, _>>();
        serde_json::json!({
            "record": if execute { "tracking_done" } else { "tracking_review" },
            "token": token,
            "files": self.files.keys().map(|key| display(key)).collect::<Vec<_>>(),
            "folders": self.folders.iter().map(|key| display(key)).collect::<Vec<_>>(),
            "held": held,
            "existing": self.existing.iter().map(|key| display(key)).collect::<Vec<_>>()
        })
    }

    fn restrict(&mut self, req: &Request) -> Result<Vec<String>> {
        ensure!(
            !req.keep_folder_rule
                || (req.operation == Action::TrackFolder && !req.excluded.is_empty()),
            "invalid folder selection"
        );
        let mut omitted = Vec::new();
        if !req.excluded.is_empty() {
            ensure!(
                req.expected.is_some()
                    && req.operation != Action::StopTracking
                    && req.excluded.len() < self.files.len()
                    && req
                        .excluded
                        .last()
                        .is_some_and(|index| *index < self.files.len()),
                "invalid file selection"
            );
            omitted = self
                .files
                .keys()
                .enumerate()
                .filter(|(index, _)| req.excluded.contains(index))
                .map(|(_, key)| key.clone())
                .collect();
            for key in &omitted {
                self.files.remove(key);
            }
            if !req.keep_folder_rule {
                self.folders.clear();
            }
        }
        Ok(omitted)
    }

    fn track(
        &self,
        ctx: &Context,
        config: &mut RepoConfig,
        state: &mut State,
        omitted: &[String],
    ) -> Result<()> {
        let mut modes = BTreeMap::new();
        for (key, (kind, identity)) in &self.files {
            let entry = commit_stored(
                ctx,
                config,
                &mut modes,
                key,
                key,
                *kind,
                Store::ReviewedNew(identity),
            )?;
            state.files.insert(key.clone(), entry);
        }
        let dirs = self
            .folders
            .iter()
            .map(|key| DirPlan {
                key: key.clone(),
                ..DirPlan::default()
            })
            .collect::<Vec<_>>();
        adopt_dirs(ctx, config, state, &dirs)?;
        forget::exclude_from_capture(config, omitted.iter().map(String::as_str))?;
        state.identity.get_or_insert_with(|| ctx.identity().clone());
        config.save(&ctx.repo_config_file(), false)?;
        state.save(&ctx.paths.state_file(), false)?;
        Ok(())
    }

    fn forget(ctx: &Context, req: &Request, config: &RepoConfig, state: &State) -> Result<Self> {
        let key = req.paths.first().context("choose a path")?;
        let sources = ctx.sources()?;
        let mut result = Self::default();
        result.folders.extend(
            config
                .dirs
                .paths
                .iter()
                .filter(|d| is_under(d, key))
                .cloned(),
        );
        for path in sources
            .keys()
            .chain(state.files.keys().map(String::as_str))
            .filter(|p| is_under(p, key))
        {
            if !result.existing.insert(path.to_owned()) {
                continue;
            }
            for source in sources.sources_of(path) {
                let node = read_node(&ctx.paths.repo, source)?;
                let tuple = match &node {
                    Node::File { bytes, mode, .. } => crate::engine::Tuple::of_file(bytes, *mode),
                    Node::Symlink(target) => crate::engine::Tuple::of_link(target),
                    _ => bail!("source unavailable"),
                };
                result
                    .files
                    .insert(source.to_owned(), (tuple.kind, fingerprint(ctx, &node)?));
                result
                    .removal
                    .entry(path.to_owned())
                    .or_default()
                    .push(forget::reviewed::Source {
                        path: source.to_owned(),
                        kind: tuple.kind,
                        hash: tuple.hash,
                        mode: tuple.mode,
                    });
            }
        }
        ensure!(
            !result.existing.is_empty() || !result.folders.is_empty(),
            "path is not tracked"
        );
        Ok(result)
    }
}

pub fn run(req: &Request) -> Result<()> {
    ensure!(
        req.schema_version == 1
            && !req.paths.is_empty()
            && (req.operation == Action::TrackFiles || req.paths.len() == 1),
        "invalid tracking request"
    );
    let ctx = Context::resolve("remedy")?;
    let lock = ctx.lock_for_review(req.expected.is_some())?;
    let repo_id = ctx.repo_id()?;
    ensure!(repo_id == req.repo_id, "repository changed");
    for path in &req.paths {
        validate_key(path)?;
        ctx.check_key(path, req.operation == Action::TrackFiles)?;
    }
    let mut config = ctx.repo_config()?;
    let mut state = ctx.state(&repo_id)?;
    let mut inventory = match req.operation {
        Action::TrackFiles | Action::TrackFolder => Inventory::add(&ctx, req, &config, &state)?,
        Action::StopTracking => Inventory::forget(&ctx, req, &config, &state)?,
    };
    let binding = (
        &repo_id,
        &ctx.paths.home,
        &ctx.paths.repo,
        &ctx.paths.state_dir,
        &ctx.paths.config_file,
        &ctx.machine,
        &config,
        &state,
        ctx.git.head()?,
        req.operation,
        &req.paths,
        &inventory,
    );
    let token = ctx.git.hash_object(&serde_json::to_vec(&binding)?, false)?;
    let execute = req.expected.is_some();
    if let Some(expected) = &req.expected {
        ensure!(lock.is_held(), "repository lock is unavailable");
        ensure!(expected == &token, crate::context::ReviewChanged);
    }
    let omitted = inventory.restrict(req)?;
    let bytes = serde_json::to_vec(&inventory.receipt(&token, execute))?;
    ensure!(
        bytes.len() < 65_400,
        "the review is too large; select a smaller folder or fewer files"
    );
    if execute {
        ensure!(
            !inventory.files.is_empty()
                || !inventory.folders.is_empty()
                || (req.operation == Action::StopTracking && !inventory.existing.is_empty()),
            "nothing to change"
        );
        match req.operation {
            Action::StopTracking => forget::stop_tracking(
                &ctx,
                &mut config,
                &mut state,
                &req.paths,
                &inventory.existing,
                &inventory.folders,
                &inventory.removal,
            )?,
            Action::TrackFiles | Action::TrackFolder => {
                inventory.track(&ctx, &mut config, &mut state, &omitted)?;
            }
        }
    }
    let mut out = io::stdout().lock();
    out.write_all(&bytes)?;
    writeln!(out)?;
    Ok(())
}
