use crate::context::Context;
use crate::engine::{Home, Tuple};
use crate::paths::validate_key;
use crate::state::{IdentitySnapshot, Kind};
use anyhow::{Context as _, Result, ensure};
use serde::{Deserialize, Serialize};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read as _;
use std::os::unix::ffi::{OsStrExt as _, OsStringExt as _};
use std::path::{Component, Path, PathBuf};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    schema_version: u32,
    pub repo_id: String,
    pub home: InputPath,
    pub repo: InputPath,
    pub machine: Machine,
    pub files: Vec<FileDecision>,
    pub sync: Option<SyncReview>,
    pub approvals: Approvals,
    #[serde(default)]
    pub trust: Option<TrustReview>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustReview {
    pub action: String,
    pub paths: TrustPaths,
    pub lock_dev: u64,
    pub lock_ino: u64,
    pub inventory_hash: String,
    pub directories: Vec<String>,
    pub retained: Vec<String>,
    pub current: Vec<String>,
    pub state: TrustControl,
    pub config: TrustControl,
    pub machine_config: TrustControl,
    pub candidates: Vec<TrustCandidate>,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustPaths {
    pub state: InputPath,
    pub machine_config: InputPath,
    pub repo_config: InputPath,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustTuple {
    pub kind: String,
    pub hash: String,
    pub mode: u32,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustControl {
    pub kind: String,
    pub hash: String,
    pub mode: u32,
    pub dev: u64,
    pub ino: u64,
    pub uid: u32,
    pub gid: u32,
    pub nlink: u64,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TrustCandidate {
    pub key: String,
    pub disposition: String,
    pub tuple: TrustTuple,
}

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum InputPath {
    Raw { raw: String },
    Bytes { bytes: Vec<u8> },
}

impl InputPath {
    #[cfg(target_os = "linux")]
    fn from_path(path: &Path) -> Self {
        path.to_str().map_or_else(
            || Self::Bytes {
                bytes: path.as_os_str().as_bytes().to_vec(),
            },
            |raw| Self::Raw {
                raw: raw.to_owned(),
            },
        )
    }
    pub(crate) fn path(&self) -> Result<PathBuf> {
        let path = match self {
            Self::Raw { raw } => PathBuf::from(raw),
            Self::Bytes { bytes } => PathBuf::from(std::ffi::OsString::from_vec(bytes.clone())),
        };
        ensure!(
            !path.as_os_str().as_bytes().contains(&0),
            "a path contains NUL"
        );
        Ok(path)
    }

    fn relative(&self) -> Result<PathBuf> {
        let path = self.path()?;
        ensure!(
            !path.as_os_str().is_empty()
                && path
                    .as_os_str()
                    .as_bytes()
                    .split(|byte| *byte == b'/')
                    .all(|part| { !part.is_empty() && part != b"." && part != b".." })
                && path.components().all(|part| {
                    let Component::Normal(part) = part else {
                        return false;
                    };
                    part.to_str()
                        .is_none_or(|part| validate_key(&format!("entry/{part}")).is_ok())
                }),
            "outgoing path is not a permitted repository-relative path"
        );
        Ok(path)
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Machine {
    host: String,
    user: String,
    os: String,
    distro: String,
    like: String,
    arch: String,
    profiles: Vec<String>,
}

impl Machine {
    pub(crate) fn matches(&self, actual: &IdentitySnapshot) -> bool {
        self.host == actual.host
            && self.user == actual.user
            && self.os == actual.os
            && self.distro == actual.distro
            && self.like == actual.like
            && self.arch == actual.arch
            && self.profiles == actual.profiles
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TupleInput {
    kind: Kind,
    hash: String,
    mode: String,
}

impl TupleInput {
    fn tuple(&self) -> Result<Tuple> {
        ensure!(
            self.hash.len() == 32
                && self
                    .hash
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
            "a tuple hash must be 32 lowercase hexadecimal digits"
        );
        ensure!(
            self.mode.len() == 4 && self.mode.bytes().all(|byte| (b'0'..=b'7').contains(&byte)),
            "a tuple mode must be four octal digits"
        );
        let mode = u32::from_str_radix(&self.mode, 8)?;
        ensure!(
            self.kind != Kind::Symlink || mode == 0o777,
            "a symlink tuple mode must be 0777"
        );
        Ok(Tuple {
            kind: self.kind,
            hash: self.hash.clone(),
            mode,
        })
    }
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum Observed {
    Present { tuple: TupleInput },
    Missing,
    Unavailable { reason: String },
    Blocked { reason: String },
}

impl Observed {
    pub(crate) fn hypothetical(&self) -> bool {
        matches!(self, Self::Unavailable { reason } if reason == "hypothetical_ciphertext")
    }
    fn check(&self) -> Result<()> {
        match self {
            Self::Present { tuple } => {
                tuple.tuple()?;
            }
            Self::Unavailable { reason } | Self::Blocked { reason } => {
                ensure!(
                    !reason.is_empty(),
                    "an unavailable observation needs a reason"
                );
            }
            Self::Missing => {}
        }
        Ok(())
    }

    pub(crate) fn matches(&self, tuple: Option<&Tuple>) -> bool {
        match (self, tuple) {
            (Self::Missing, None) => true,
            (Self::Present { tuple: expected }, Some(actual)) => {
                expected.tuple().is_ok_and(|tuple| tuple == *actual)
            }
            _ => false,
        }
    }

    pub(crate) fn home(&self, home: &Home) -> bool {
        match home {
            Home::Missing => self.matches(None),
            Home::Present(tuple) => self.matches(Some(tuple)),
            Home::Blocked(_) => false,
        }
    }

    pub(crate) fn stored(&self, stored: &crate::commands::review::Stored) -> bool {
        use crate::commands::review::Stored;
        match stored {
            Stored::File { bytes, mode } => self.matches(Some(&Tuple::of_file(bytes, *mode))),
            Stored::Link(target) => self.matches(Some(&Tuple::of_link(target))),
            Stored::Missing => self.matches(None),
            Stored::Ciphertext => false,
        }
    }
}

#[derive(Clone, Copy, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    TakeRepo,
    KeepLocal,
    Forget,
    Restore,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FileDecision {
    pub key: String,
    #[serde(deserialize_with = "nullable")]
    pub source: Option<String>,
    pub r: Observed,
    pub source_tuple: Observed,
    pub h: Observed,
    pub raw_h: Observed,
    pub action: Action,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capture {
    pub key: String,
    pub source: String,
    pub logical_r: Observed,
    pub raw_h: Observed,
    pub stored_before: Observed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outgoing {
    pub path: InputPath,
    pub stored: Observed,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SyncReview {
    #[serde(deserialize_with = "nullable")]
    pub head: Option<String>,
    #[serde(deserialize_with = "nullable")]
    pub tracking_ref: Option<String>,
    pub captures: Vec<Capture>,
    pub outgoing: Vec<Outgoing>,
    #[serde(default, deserialize_with = "non_null")]
    pub selected_lines: Option<BTreeMap<String, Vec<u32>>>,
    #[serde(default, deserialize_with = "non_null")]
    pub selected_target: Option<String>,
}

fn non_null<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    de: D,
) -> Result<Option<T>, D::Error> {
    T::deserialize(de).map(Some)
}

fn nullable<'de, D: serde::Deserializer<'de>, T: Deserialize<'de>>(
    de: D,
) -> Result<Option<T>, D::Error> {
    Option::deserialize(de)
}

#[derive(Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Approvals {
    pub identity: Option<IdentityApproval>,
    pub hooks: Option<HookApproval>,
    pub directories: Option<Vec<String>>,
    pub ignores: Option<IgnoreApproval>,
    pub mass_delete: Option<Vec<InputPath>>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityApproval {
    #[serde(deserialize_with = "nullable")]
    pub recorded: Option<Machine>,
    pub current: Machine,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HookApproval {
    pub canonical: String,
    pub pending_keys: Vec<String>,
    pub commands: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IgnoreApproval {
    pub retained: Vec<String>,
    pub current: Vec<String>,
}

pub struct Reviewed {
    pub input: Input,
    pub expected_head: RefCell<Option<String>>,
    pub attention: Cell<bool>,
    pub captured: RefCell<BTreeMap<PathBuf, crate::commands::review::Stored>>,
    pub reencrypted: RefCell<Option<crate::commands::review::Reencrypted>>,
    pub blocked: RefCell<BTreeSet<PathBuf>>,
    pub capture_settings: Cell<bool>,
    pub stale_files: RefCell<BTreeSet<String>>,
}

fn unique<T: Ord>(items: impl IntoIterator<Item = T>) -> Result<()> {
    let mut seen = BTreeSet::new();
    for item in items {
        ensure!(
            seen.insert(item),
            "duplicate reviewed key, path or approval"
        );
    }
    Ok(())
}

impl Input {
    pub(crate) fn stdin(source: Option<&str>) -> Result<Option<Self>> {
        Self::read_stdin(source, false)
    }

    pub(crate) fn trust_stdin(source: Option<&str>) -> Result<Option<Self>> {
        Self::read_stdin(source, true).map_err(|_| anyhow::anyhow!("trust review refused"))
    }

    fn read_stdin(source: Option<&str>, bounded: bool) -> Result<Option<Self>> {
        let Some(source) = source else {
            return Ok(None);
        };
        ensure!(source == "-", "decisions must come from stdin");
        let mut bytes = Vec::new();
        if bounded {
            std::io::stdin()
                .lock()
                .take(1024 * 1024 + 1)
                .read_to_end(&mut bytes)?;
            ensure!(bytes.len() <= 1024 * 1024, "trust input too large");
        } else {
            std::io::stdin()
                .lock()
                .read_to_end(&mut bytes)
                .context("cannot read reviewed decisions")?;
        }
        let input: Self = serde_json::from_slice(&bytes).context("invalid reviewed decisions")?;
        input.syntax()?;
        crate::prompt::reviewed();
        Ok(Some(input))
    }

    fn syntax(&self) -> Result<()> {
        ensure!(
            self.schema_version == 1,
            "unsupported decisions schema version"
        );
        ensure!(
            self.home.path()?.is_absolute() && self.repo.path()?.is_absolute(),
            "reviewed context paths must be absolute"
        );
        unique(self.files.iter().map(|file| &file.key))?;
        for file in &self.files {
            validate_key(&file.key)?;
            if let Some(source) = &file.source {
                validate_key(source)?;
            }
            for observation in [&file.r, &file.source_tuple, &file.h, &file.raw_h] {
                observation.check()?;
            }
        }
        if let Some(sync) = &self.sync {
            self.selected_syntax(sync)?;
            ensure!(
                sync.head
                    .as_ref()
                    .is_none_or(|head| matches!(head.len(), 40 | 64)
                        && head.bytes().all(|b| b.is_ascii_hexdigit())),
                "invalid reviewed HEAD"
            );
            unique(sync.captures.iter().map(|capture| &capture.key))?;
            unique(sync.captures.iter().map(|capture| &capture.source))?;
            unique(
                sync.outgoing
                    .iter()
                    .map(|entry| entry.path.relative())
                    .collect::<Result<Vec<_>>>()?,
            )?;
            for capture in &sync.captures {
                validate_key(&capture.key)?;
                validate_key(&capture.source)?;
                for observed in [&capture.logical_r, &capture.raw_h, &capture.stored_before] {
                    observed.check()?;
                }
            }
            for entry in &sync.outgoing {
                entry.stored.check()?;
            }
        }
        unique(&self.machine.profiles)?;
        if let Some(identity) = &self.approvals.identity {
            unique(&identity.current.profiles)?;
            if let Some(recorded) = &identity.recorded {
                unique(&recorded.profiles)?;
            }
        }
        if let Some(dirs) = &self.approvals.directories {
            unique(dirs)?;
            for dir in dirs {
                validate_key(dir)?;
            }
        }
        if let Some(ignores) = &self.approvals.ignores {
            unique(&ignores.retained)?;
            unique(&ignores.current)?;
        }
        if let Some(paths) = &self.approvals.mass_delete {
            unique(
                paths
                    .iter()
                    .map(InputPath::relative)
                    .collect::<Result<Vec<_>>>()?,
            )?;
        }
        if let Some(hooks) = &self.approvals.hooks {
            unique(&hooks.pending_keys)?;
            unique(&hooks.commands)?;
            for key in &hooks.pending_keys {
                validate_key(key)?;
            }
        }
        Ok(())
    }

    fn selected_syntax(&self, sync: &SyncReview) -> Result<()> {
        let Some(selected) = &sync.selected_lines else {
            ensure!(
                sync.selected_target.is_none(),
                "selected target requires selected lines"
            );
            return Ok(());
        };
        ensure!(!selected.is_empty(), "selected lines cannot be empty");
        ensure!(
            sync.outgoing.is_empty(),
            "selected lines require empty outgoing"
        );
        ensure!(
            self.files.is_empty()
                && self.approvals.identity.is_none()
                && self.approvals.hooks.is_none()
                && self.approvals.directories.is_none()
                && self.approvals.ignores.is_none()
                && self.approvals.mass_delete.is_none(),
            "selected lines cannot be mixed with other decisions"
        );
        ensure!(
            sync.captures.len() == selected.len(),
            "selected lines require exactly one capture per key"
        );
        for (key, ids) in selected {
            validate_key(key)?;
            ensure!(
                !ids.is_empty() && ids.iter().all(|id| *id != 0),
                "invalid selected line IDs"
            );
            unique(ids)?;
            ensure!(
                sync.captures.iter().any(|capture| capture.key == *key),
                "selected key has no capture"
            );
        }
        Ok(())
    }

    pub(crate) fn bind(self, ctx: &Context, sync: bool) -> Result<Reviewed> {
        ensure!(!sync || self.trust.is_none(), "trust review requires apply");
        if self.trust.is_some() {
            ensure!(
                self.files.is_empty()
                    && self.approvals.hooks.is_none()
                    && self.approvals.identity.is_none()
                    && self.approvals.directories.is_none()
                    && self.approvals.ignores.is_none()
                    && self.approvals.mass_delete.is_none(),
                "trust review cannot be mixed with other decisions"
            );
        }
        ensure!(
            self.sync.is_some() == sync,
            "sync review is required only for sync"
        );
        ensure!(
            sync || (self.approvals.directories.is_none()
                && self.approvals.ignores.is_none()
                && self.approvals.mass_delete.is_none()),
            "capture and mass-delete approvals require sync"
        );
        ensure!(self.membership(ctx)?, "a reviewed key is not managed");
        let head = self.sync.as_ref().and_then(|review| review.head.clone());
        let review = Reviewed {
            input: self,
            expected_head: RefCell::new(head),
            attention: Cell::new(false),
            captured: RefCell::new(BTreeMap::new()),
            reencrypted: RefCell::new(None),
            blocked: RefCell::new(BTreeSet::new()),
            capture_settings: Cell::new(true),
            stale_files: RefCell::new(BTreeSet::new()),
        };
        review.recheck(ctx)?;
        Ok(review)
    }

    fn membership(&self, ctx: &Context) -> Result<bool> {
        if self.trust.is_some() {
            return Ok(true);
        }
        let sources = ctx.sources()?;
        let mut state = ctx.state(&ctx.repo_id()?)?;
        for key in self.files.iter().map(|file| &file.key).chain(
            self.approvals
                .hooks
                .iter()
                .flat_map(|hooks| &hooks.pending_keys),
        ) {
            if ctx.check_key(key, true).is_err()
                || (!sources.contains(key) && !state.files.contains_key(key))
            {
                return Ok(false);
            }
        }
        if let Some(sync) = &self.sync {
            let config = ctx.repo_config()?;
            let keys = sources
                .keys()
                .map(str::to_owned)
                .chain(state.files.keys().cloned())
                .collect();
            // This scan establishes directory scope, not capture permission. Actual capture still
            // requires matching approvals, observations and content guards.
            state.approve_dirs(&config.dirs.paths);
            state
                .ignore_seen
                .retain(|pattern| config.ignore.patterns.contains(pattern));
            let scan = crate::commands::add::scan_dirs(ctx, &config, &keys, &state)?;
            for capture in &sync.captures {
                if ctx.check_key(&capture.key, true).is_err()
                    || (!sources.contains(&capture.key)
                        && !scan.new.contains_key(&capture.key)
                        && !scan.held.contains_key(&capture.key))
                {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}

#[cfg(target_os = "linux")]
pub fn trust_context() -> Result<Context> {
    let paths =
        crate::paths::Paths::resolve(&crate::paths::Env::from_process(), crate::sys::geteuid())?;
    let (observed, bytes) = trust_read(&paths.config_file, false)?;
    let config = if observed.kind == "missing" {
        crate::config::Config::default()
    } else {
        crate::config::Config::parse(std::str::from_utf8(&bytes)?)?.0
    };
    let paths = paths.apply_config(&config)?;
    let ctx = Context::new(paths, config, "apply");
    trust_display_context(&ctx)?;
    Ok(ctx)
}

#[cfg(not(target_os = "linux"))]
pub fn trust_context() -> Result<Context> {
    anyhow::bail!("trust review requires Linux")
}

impl Reviewed {
    pub(crate) fn recheck(&self, ctx: &Context) -> Result<()> {
        if !self.input.membership(ctx)?
            || self.input.repo_id
                != if self.input.trust.is_some() {
                    trust_repo_id(ctx)?
                } else {
                    ctx.repo_id()?
                }
            || self.input.home.path()?.as_os_str() != ctx.paths.home.as_os_str()
            || self.input.repo.path()?.as_os_str() != ctx.paths.repo.as_os_str()
            || !self.input.machine.matches(ctx.identity())
        {
            return Err(crate::context::ReviewChanged.into());
        }
        self.check_head(ctx)?;
        self.check_files(ctx)
    }

    fn check_files(&self, ctx: &Context) -> Result<()> {
        use crate::commands::apply::{Classified, classify_key};
        use crate::engine::status::{Variants, Worktree};
        if self.input.trust.is_some() {
            return Ok(());
        }
        let sources = ctx.sources()?;
        let state = ctx.state(&ctx.repo_id()?)?;
        let config = ctx.repo_config()?;
        let variants = Variants::new(
            Worktree::new(&ctx.paths.repo, &config.modes),
            &sources,
            ctx.identity(),
            ctx.template_vars(),
            ctx.age(),
        );
        for answer in &self.input.files {
            if !matches!(classify_key(ctx, &variants, &state, &answer.key)?,
                Classified::Item(item) if item.matches_review(answer))
            {
                self.stale_files.borrow_mut().insert(answer.key.clone());
                self.attention.set(true);
            }
        }
        Ok(())
    }

    pub(crate) fn check_head(&self, ctx: &Context) -> Result<()> {
        if let Some(sync) = &self.input.sync {
            let tracking = ctx
                .git
                .branch()?
                .map(|branch| format!("refs/remotes/origin/{branch}"));
            if *self.expected_head.borrow() != ctx.git.head()? || sync.tracking_ref != tracking {
                return Err(crate::context::ReviewChanged.into());
            }
        }
        Ok(())
    }

    pub(crate) fn accept_tip(&self, ctx: &Context, receipt: &crate::git::TipReceipt) -> Result<()> {
        let reference = self.branch().map(|branch| format!("refs/heads/{branch}"));
        if reference.as_deref() != Some(receipt.reference.as_str())
            || *self.expected_head.borrow() != receipt.old
        {
            return Err(crate::context::ReviewChanged.into());
        }
        self.expected_head.borrow_mut().clone_from(&receipt.new);
        self.check_head(ctx)
    }

    pub(crate) fn branch(&self) -> Option<&str> {
        self.input
            .sync
            .as_ref()?
            .tracking_ref
            .as_deref()?
            .strip_prefix("refs/remotes/origin/")
    }

    pub(crate) fn stale(&self, ctx: &Context, key: Option<&str>) -> Result<()> {
        self.attention.set(true);
        crate::output::warning(&format!(
            "{}: review is missing or stale; review again",
            key.unwrap_or("decision")
        ));
        ctx.attention("review_stale", key, "home", true)
    }

    pub(crate) fn file(&self, key: &str) -> Option<&FileDecision> {
        self.input.files.iter().find(|file| file.key == key)
    }

    pub(crate) fn capture(&self, key: &str) -> Option<&Capture> {
        self.input
            .sync
            .as_ref()?
            .captures
            .iter()
            .find(|capture| capture.key == key)
    }

    pub(crate) fn outgoing(&self, path: &Path) -> Option<&Observed> {
        self.input
            .sync
            .as_ref()?
            .outgoing
            .iter()
            .find(|entry| entry.path.path().is_ok_and(|entry| entry == path))
            .map(|entry| &entry.stored)
    }
}

#[cfg(target_os = "linux")]
fn trust_parent(path: &Path) -> Result<std::fs::File> {
    use std::os::fd::AsFd as _;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("invalid trust path"))?;
    ensure!(parent.is_absolute(), "invalid trust path");
    let mut dir = std::fs::File::open("/")?;
    for part in parent.components() {
        match part {
            Component::RootDir => {}
            Component::Normal(name) => {
                let name = std::ffi::CString::new(name.as_bytes())?;
                dir = std::fs::File::from(crate::sys::openat_dir(dir.as_fd(), &name)?);
            }
            _ => anyhow::bail!("invalid trust path"),
        }
    }
    Ok(dir)
}

#[cfg(target_os = "linux")]
fn trust_read(path: &Path, private: bool) -> Result<(TrustControl, Vec<u8>)> {
    trust_read_limit(path, private, 8 * 1024 * 1024)
}

#[cfg(target_os = "linux")]
fn trust_read_limit(path: &Path, private: bool, max_bytes: u64) -> Result<(TrustControl, Vec<u8>)> {
    use std::os::fd::{AsFd as _, AsRawFd as _};
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let dir = match trust_parent(path) {
        Ok(dir) => dir,
        Err(error)
            if error
                .downcast_ref::<std::io::Error>()
                .is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) =>
        {
            return Ok((missing_trust_control(), Vec::new()));
        }
        Err(error) => return Err(error),
    };
    let leaf = path
        .file_name()
        .ok_or_else(|| anyhow::anyhow!("invalid trust path"))?;
    let anchored = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd())).join(leaf);
    let mut file = match std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(anchored)
    {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((missing_trust_control(), Vec::new()));
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "trust input is not a regular file");
    if private {
        ensure!(
            !crate::sys::has_xattrs(file.as_fd())?,
            "state has extended metadata"
        );
    }
    ensure!(
        metadata.len() <= max_bytes,
        "trust input exceeds size limit"
    );
    let mut bytes = Vec::new();
    file.by_ref()
        .take(max_bytes.saturating_add(1))
        .read_to_end(&mut bytes)?;
    ensure!(
        u64::try_from(bytes.len())? <= max_bytes,
        "trust input exceeds size limit"
    );
    let after = file.metadata()?;
    if private {
        ensure!(
            !crate::sys::has_xattrs(file.as_fd())?,
            "state has extended metadata"
        );
    }
    ensure!(
        (
            metadata.dev(),
            metadata.ino(),
            metadata.mode(),
            metadata.len(),
            metadata.mtime(),
            metadata.mtime_nsec(),
            metadata.ctime(),
            metadata.ctime_nsec()
        ) == (
            after.dev(),
            after.ino(),
            after.mode(),
            after.len(),
            after.mtime(),
            after.mtime_nsec(),
            after.ctime(),
            after.ctime_nsec()
        ),
        "trust input changed while reading"
    );
    let observation = TrustControl {
        kind: "file".to_owned(),
        hash: crate::engine::hash_hex(&bytes),
        mode: metadata.mode() & 0o7777,
        dev: metadata.dev(),
        ino: metadata.ino(),
        uid: metadata.uid(),
        gid: metadata.gid(),
        nlink: metadata.nlink(),
    };
    Ok((observation, bytes))
}

#[cfg(target_os = "linux")]
fn trust_file(path: &Path) -> Result<TrustControl> {
    Ok(trust_read(path, false)?.0)
}

#[cfg(target_os = "linux")]
pub fn trust_repo_id(ctx: &Context) -> Result<String> {
    let (observation, bytes) =
        trust_read_limit(&ctx.paths.repo.join(".git/dotman-id"), false, 128)?;
    ensure!(
        observation.kind == "file" && bytes.len() == 32,
        "invalid repository identity"
    );
    let id = std::str::from_utf8(&bytes)?;
    ensure!(
        id.len() == 32
            && id
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "invalid repository identity"
    );
    Ok(id.to_owned())
}

#[cfg(not(target_os = "linux"))]
pub fn trust_repo_id(_ctx: &Context) -> Result<String> {
    anyhow::bail!("trust review requires Linux")
}

#[cfg(target_os = "linux")]
pub fn trust_git(
    ctx: &Context,
    args: &[&str],
    limit: u64,
) -> Result<(std::process::ExitStatus, Vec<u8>)> {
    use std::process::Stdio;
    let mut child = ctx
        .git
        .command()
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()?;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow::anyhow!("git output unavailable"))?;
    let mut bytes = Vec::new();
    let result = stdout
        .by_ref()
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes);
    drop(stdout);
    if result.is_err() || u64::try_from(bytes.len())? > limit {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("git output exceeds trust limit or cannot be read");
    }
    Ok((child.wait()?, bytes))
}

#[cfg(target_os = "linux")]
pub fn trust_lost_config(ctx: &Context, head: bool) -> Result<()> {
    let (current, bytes) = trust_read(&ctx.repo_config_file(), false)?;
    if current.kind != "missing" && !crate::context::holds_no_settings(&bytes) {
        return Ok(());
    }
    if !head {
        return Ok(());
    }
    let (status, tree) = trust_git(ctx, &["ls-tree", "-z", "HEAD", "--", ".dotman.toml"], 4096)?;
    ensure!(status.success(), "cannot inspect committed settings");
    if tree.is_empty() {
        return Ok(());
    }
    let (status, committed) = trust_git(
        ctx,
        &["cat-file", "blob", "HEAD:.dotman.toml"],
        8 * 1024 * 1024,
    )?;
    ensure!(status.success(), "cannot inspect committed settings");
    ensure!(
        crate::context::holds_no_settings(&committed),
        "repository settings would be lost"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn missing_trust_control() -> TrustControl {
    TrustControl {
        kind: "missing".to_owned(),
        hash: String::new(),
        mode: 0,
        dev: 0,
        ino: 0,
        uid: 0,
        gid: 0,
        nlink: 0,
    }
}

#[cfg(target_os = "linux")]
fn trust_display_value(bytes: &[u8]) -> Result<()> {
    let text = String::from_utf8_lossy(bytes);
    ensure!(
        !text
            .chars()
            .any(|c| c.is_control() || crate::output::is_hidden_format(c))
            && crate::secrets::held_name(&text).is_none()
            && crate::secrets::held(bytes, &text, &[]).is_none(),
        "trust review contains unsafe display values; review refused"
    );
    Ok(())
}

#[cfg(target_os = "linux")]
fn trust_display(ctx: &Context, review: &TrustReview) -> Result<()> {
    for value in review
        .directories
        .iter()
        .chain(&review.retained)
        .chain(&review.current)
        .chain(review.candidates.iter().map(|candidate| &candidate.key))
    {
        trust_display_value(value.as_bytes())?;
    }
    trust_display_context(ctx)
}

#[cfg(target_os = "linux")]
fn trust_display_context(ctx: &Context) -> Result<()> {
    for path in [
        ctx.paths.home.as_path(),
        ctx.paths.repo.as_path(),
        ctx.paths.config_file.as_path(),
        ctx.paths.age_identity.as_path(),
        ctx.repo_config_file().as_path(),
        ctx.paths.state_file().as_path(),
    ] {
        trust_display_value(path.as_os_str().as_bytes())?;
    }
    if let Some(path) = &ctx.paths.age_key_file {
        trust_display_value(path.as_os_str().as_bytes())?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
struct TrustInputs {
    state_file: TrustControl,
    repo_config_file: TrustControl,
    machine_config_file: TrustControl,
    config: crate::repo_config::RepoConfig,
    state: crate::state::State,
}

#[cfg(target_os = "linux")]
fn trust_inputs(ctx: &Context) -> Result<TrustInputs> {
    let (state_file, state_bytes) = trust_read(&ctx.paths.state_file(), true)?;
    ensure!(
        state_file.kind == "missing"
            || (state_file.kind == "file"
                && state_file.mode == 0o600
                && state_file.uid == crate::sys::geteuid()
                && state_file.nlink == 1),
        "state is not private"
    );
    let (repo_config_file, repo_bytes) = trust_read(&ctx.repo_config_file(), false)?;
    let (machine_config_file, machine_bytes) = trust_read(&ctx.paths.config_file, false)?;
    let config = if repo_config_file.kind == "missing" {
        crate::repo_config::RepoConfig::default()
    } else {
        crate::repo_config::RepoConfig::parse(std::str::from_utf8(&repo_bytes)?)?
    };
    let machine = if machine_config_file.kind == "missing" {
        crate::config::Config::default()
    } else {
        crate::config::Config::parse(std::str::from_utf8(&machine_bytes)?)?.0
    };
    ensure!(
        machine == ctx.machine,
        "machine config changed; review again"
    );
    ensure!(
        config
            .dirs
            .paths
            .iter()
            .chain(&config.ignore.patterns)
            .all(|value| { crate::secrets::held(value.as_bytes(), value, &[]).is_none() }),
        "trust policy contains sensitive material; review refused"
    );
    ensure!(
        config
            .dirs
            .paths
            .iter()
            .all(|dir| ctx.check_key(dir, false).is_ok()),
        "trust policy contains a forbidden directory; review refused"
    );
    let mut state: crate::state::State = if state_file.kind == "missing" {
        crate::state::State {
            repo_id: trust_repo_id(ctx)?,
            capture_dirs: Some(BTreeSet::new()),
            ..crate::state::State::default()
        }
    } else {
        toml::from_str(std::str::from_utf8(&state_bytes)?)?
    };
    ensure!(
        state.version == crate::state::VERSION,
        "unsupported state version"
    );
    ensure!(
        state.repo_id == trust_repo_id(ctx)?,
        "state belongs to another repository"
    );
    if state.capture_dirs.is_none() {
        state.grandfather_dirs(&config.dirs.paths);
    }
    state
        .ignore_seen
        .extend(config.ignore.patterns.iter().cloned());
    Ok(TrustInputs {
        state_file,
        repo_config_file,
        machine_config_file,
        config,
        state,
    })
}

#[cfg(target_os = "linux")]
type RepoObservation = (String, u64, u64, u32, i64, i64);
#[cfg(target_os = "linux")]
type ParentObservation = (String, u64, u64, u32, i64, i64, i64, i64);

#[cfg(target_os = "linux")]
fn trust_known(
    ctx: &Context,
    state: &crate::state::State,
) -> Result<(BTreeSet<String>, Vec<RepoObservation>)> {
    use std::os::unix::fs::MetadataExt as _;
    ensure!(state.files.len() <= 4096, "trust inventory too large");
    let sources: BTreeSet<_> = state
        .files
        .values()
        .map(|entry| entry.source.as_str())
        .collect();
    let mut found = Vec::new();
    let mut paths = Vec::new();
    let mut walk = walkdir::WalkDir::new(&ctx.paths.repo)
        .follow_links(false)
        .into_iter();
    while let Some(entry) = walk.next() {
        let entry = entry?;
        if entry.depth() == 0 {
            continue;
        }
        let rel = entry.path().strip_prefix(&ctx.paths.repo)?;
        let key = rel
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("unsupported repository name"))?;
        ensure!(
            entry.depth() <= 32 && key.len() <= 4096,
            "trust inventory too large"
        );
        let root = key.split('/').next().unwrap_or(key);
        if crate::paths::is_reserved_root_name(root)
            || entry.file_name().as_bytes().starts_with(b".dotman-tmp-")
        {
            if entry.file_type().is_dir() {
                walk.skip_current_dir();
            }
            continue;
        }
        ensure!(found.len() < 4096, "trust inventory too large");
        let meta = entry.metadata()?;
        found.push((
            key.to_owned(),
            meta.dev(),
            meta.ino(),
            meta.mode(),
            meta.mtime(),
            meta.mtime_nsec(),
        ));
        if !meta.is_dir() {
            ensure!(
                (meta.is_file() || meta.file_type().is_symlink()) && sources.contains(key),
                "repository inventory cannot be bound"
            );
            paths.push(key.to_owned());
        }
    }
    found.sort();
    let grouped = crate::variant::Sources::group(paths.iter().map(String::as_str));
    let keys = grouped
        .keys()
        .map(str::to_owned)
        .chain(state.files.keys().cloned())
        .collect();
    Ok((keys, found))
}

#[cfg(target_os = "linux")]
fn trust_parents(ctx: &Context, roots: &[String]) -> Result<Vec<ParentObservation>> {
    use std::os::unix::fs::MetadataExt as _;
    let mut parents = BTreeSet::new();
    for root in roots {
        let mut path = Path::new(root).parent();
        while let Some(part) = path {
            if !part.as_os_str().is_empty() {
                parents.insert(
                    part.to_str()
                        .ok_or_else(|| anyhow::anyhow!("invalid trust path"))?
                        .to_owned(),
                );
            }
            path = part.parent();
        }
    }
    ensure!(parents.len() <= 4096, "trust inventory too large");
    parents
        .into_iter()
        .map(|key| {
            let metadata = trust_parent(&ctx.paths.home.join(&key).join("sentinel"))?.metadata()?;
            Ok((
                key,
                metadata.dev(),
                metadata.ino(),
                metadata.mode(),
                metadata.mtime(),
                metadata.mtime_nsec(),
                metadata.ctime(),
                metadata.ctime_nsec(),
            ))
        })
        .collect()
}

#[cfg(target_os = "linux")]
fn trust_snapshot(ctx: &Context, action: &str, lock: &crate::lock::Lock) -> Result<TrustReview> {
    use crate::commands::add::trust_scan::scan;
    let TrustInputs {
        state_file,
        repo_config_file,
        machine_config_file,
        config,
        state,
    } = trust_inputs(ctx)?;
    let (keys, repo_inventory) = trust_known(ctx, &state)?;
    let parents = trust_parents(ctx, &config.dirs.paths)?;
    let before = scan(ctx, &config, &keys, &state, &config.dirs.paths)?;
    let unapproved: Vec<_> = config
        .dirs
        .paths
        .iter()
        .filter(|dir| !state.dirs_approved().contains(*dir))
        .cloned()
        .collect();
    let retained = state.unlisted_ignores(&config.ignore.patterns);
    for value in &retained {
        trust_display_value(value.as_bytes())?;
    }
    let mut projected = state;
    match action {
        "directories" => projected.approve_dirs(&unapproved),
        "ignores" => projected.ignore_seen = config.ignore.patterns.iter().cloned().collect(),
        _ => anyhow::bail!("unsupported trust action"),
    }
    let after = scan(ctx, &config, &keys, &projected, &config.dirs.paths)?;
    let mut candidates = trust_candidates(&after);
    for (key, _, _, mode, _, _, _, _) in &parents {
        candidates.push(TrustCandidate {
            key: key.clone(),
            disposition: "directory".to_owned(),
            tuple: TrustTuple {
                kind: "directory".to_owned(),
                hash: String::new(),
                mode: mode & 0o7777,
            },
        });
    }
    candidates.sort_by(|a, b| a.key.cmp(&b.key));
    candidates.dedup_by(|a, b| a.key == b.key);
    let (_, repo_after) = trust_known(ctx, &projected)?;
    ensure!(
        repo_inventory == repo_after && parents == trust_parents(ctx, &config.dirs.paths)?,
        "repository changed while reviewing"
    );
    let inventory = serde_json::to_vec(&(repo_inventory, parents, &before, &after))?;
    ensure!(
        inventory.len() <= 32 * 1024 * 1024,
        "trust inventory too large"
    );
    let (lock_dev, lock_ino) = lock.review_identity(&ctx.paths.lock_file())?;
    let review = TrustReview {
        action: action.to_owned(),
        paths: TrustPaths {
            state: InputPath::from_path(&ctx.paths.state_file()),
            machine_config: InputPath::from_path(&ctx.paths.config_file),
            repo_config: InputPath::from_path(&ctx.repo_config_file()),
        },
        lock_dev,
        lock_ino,
        inventory_hash: crate::engine::hash_hex(&inventory),
        directories: unapproved,
        retained,
        current: config.ignore.patterns,
        state: state_file,
        config: repo_config_file,
        machine_config: machine_config_file,
        candidates,
    };
    trust_display(ctx, &review)?;
    ensure!(
        serde_json::to_vec(&review)?.len() <= 1024 * 1024,
        "trust review too large"
    );
    ensure!(
        trust_read(&ctx.paths.state_file(), true)?.0 == review.state
            && trust_file(&ctx.repo_config_file())? == review.config
            && trust_file(&ctx.paths.config_file)? == review.machine_config,
        "trust input changed while reviewing"
    );
    Ok(review)
}

#[cfg(target_os = "linux")]
fn trust_candidates(after: &crate::commands::add::trust_scan::TrustScan) -> Vec<TrustCandidate> {
    use crate::commands::add::trust_scan::Eligibility;
    let mut candidates = BTreeMap::new();
    for (key, leaf) in &after.leaves {
        let disposition = match leaf.eligibility {
            Eligibility::Eligible => "new",
            Eligibility::Unapproved => "waiting",
            _ => continue,
        };
        candidates.insert(
            key.clone(),
            TrustCandidate {
                key: key.clone(),
                disposition: disposition.to_owned(),
                tuple: TrustTuple {
                    kind: "file".to_owned(),
                    hash: leaf.hash.clone(),
                    mode: leaf.mode & 0o7777,
                },
            },
        );
    }
    for (key, dir) in &after.directories {
        let tuple = TrustTuple {
            kind: "directory".to_owned(),
            hash: String::new(),
            mode: dir.mode & 0o7777,
        };
        candidates.insert(
            key.clone(),
            TrustCandidate {
                key: key.clone(),
                disposition: "directory".to_owned(),
                tuple,
            },
        );
    }
    candidates.into_values().collect()
}

#[cfg(target_os = "linux")]
fn trust_write(ctx: &Context, lock: &crate::lock::Lock, snapshot: &TrustReview) -> Result<()> {
    use std::io::Write as _;
    use std::os::fd::{AsFd as _, AsRawFd as _};
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let path = ctx.paths.state_file();
    let (observed, bytes) = trust_read(&path, true)?;
    ensure!(observed == snapshot.state, "state changed; review again");
    let repo_id = trust_repo_id(ctx)?;
    let initial = if observed.kind == "missing" {
        toml::to_string(&crate::state::State {
            repo_id: repo_id.clone(),
            capture_dirs: Some(BTreeSet::new()),
            ..crate::state::State::default()
        })?
        .into_bytes()
    } else {
        bytes
    };
    let edited = edit_trust_state(&initial, &repo_id, snapshot)?;
    let dir = trust_parent(&path)?;
    let metadata = dir.metadata()?;
    ensure!(
        metadata.uid() == crate::sys::geteuid() && metadata.mode().trailing_zeros() >= 6,
        "state directory is not private"
    );
    let anchored = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
    let mut temp = tempfile::Builder::new()
        .prefix(".dotman-tmp-trust-")
        .tempfile_in(&anchored)?;
    ensure!(
        observed.kind == "missing" || temp.as_file().metadata()?.gid() == observed.gid,
        "state group cannot be preserved"
    );
    temp.write_all(edited.as_bytes())?;
    temp.as_file()
        .set_permissions(std::fs::Permissions::from_mode(0o600))?;
    ensure!(
        !crate::sys::has_xattrs(temp.as_file().as_fd())?,
        "state replacement has extended metadata"
    );
    temp.as_file().sync_all()?;
    let current_dir = trust_parent(&path)?.metadata()?;
    ensure!(
        (metadata.dev(), metadata.ino()) == (current_dir.dev(), current_dir.ino()),
        "state directory changed; review again"
    );
    ensure!(
        trust_read(&path, true)?.0 == snapshot.state,
        "state changed; review again"
    );
    ensure!(
        trust_file(&ctx.repo_config_file())? == snapshot.config
            && trust_file(&ctx.paths.config_file)? == snapshot.machine_config,
        "configuration changed; review again"
    );
    ensure!(
        lock.review_identity(&ctx.paths.lock_file())? == (snapshot.lock_dev, snapshot.lock_ino),
        "lock changed; review again"
    );
    if observed.kind == "missing" {
        temp.persist_noclobber(anchored.join("state.toml"))
            .map_err(|error| error.error)?;
    } else {
        temp.persist(anchored.join("state.toml"))
            .map_err(|error| error.error)?;
    }
    dir.sync_all()?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn edit_trust_state(bytes: &[u8], repo_id: &str, snapshot: &TrustReview) -> Result<String> {
    let mut state: crate::state::State = toml::from_str(std::str::from_utf8(bytes)?)?;
    ensure!(
        state.version == crate::state::VERSION && state.repo_id == repo_id,
        "state belongs to another repository"
    );
    let mut document = std::str::from_utf8(bytes)?.parse::<toml_edit::DocumentMut>()?;
    let (key, values) = match snapshot.action.as_str() {
        "directories" => {
            state.approve_dirs(&snapshot.directories);
            ("capture_dirs", state.dirs_approved())
        }
        "ignores" => ("ignore_seen", snapshot.current.iter().cloned().collect()),
        _ => anyhow::bail!("unsupported trust action"),
    };
    let mut array = toml_edit::Array::new();
    for value in values {
        array.push(value);
    }
    if key == "ignore_seen" && array.is_empty() {
        document.as_table_mut().remove(key);
    } else {
        document.as_table_mut().insert(key, toml_edit::value(array));
    }
    Ok(document.to_string())
}

/// Reviews or grants one local capture trust without capturing files.
#[cfg(target_os = "linux")]
pub fn trust_action(
    ctx: &Context,
    action: &str,
    dry_run: bool,
    lock: &crate::lock::Lock,
) -> Result<crate::commands::Outcome> {
    #[derive(Serialize)]
    struct TrustRecord<'a> {
        record: &'static str,
        display_policy: &'static str,
        review: &'a TrustReview,
    }
    let snapshot =
        trust_snapshot(ctx, action, lock).map_err(|_| anyhow::anyhow!("trust review refused"))?;
    ctx.emit(&TrustRecord {
        record: "trust_review",
        display_policy: "all_values_scanned_v1",
        review: &snapshot,
    })?;
    let Some(reviewed) = &ctx.reviewed else {
        return Ok(crate::commands::Outcome::Done);
    };
    if reviewed.input.trust.as_ref() != Some(&snapshot) {
        reviewed.stale(ctx, None)?;
        return Ok(crate::commands::Outcome::Attention);
    }
    if !dry_run {
        ensure!(
            (action == "directories" && !snapshot.directories.is_empty())
                || (action == "ignores" && !snapshot.retained.is_empty()),
            "no pending trust to grant"
        );
        if trust_snapshot(ctx, action, lock).map_err(|_| anyhow::anyhow!("trust review refused"))?
            != snapshot
        {
            reviewed.stale(ctx, None)?;
            return Ok(crate::commands::Outcome::Attention);
        }
        trust_write(ctx, lock, &snapshot)?;
    }
    Ok(crate::commands::Outcome::Done)
}

#[cfg(not(target_os = "linux"))]
pub fn trust_action(
    _ctx: &Context,
    _action: &str,
    _dry_run: bool,
    _lock: &crate::lock::Lock,
) -> Result<crate::commands::Outcome> {
    anyhow::bail!("trust review requires Linux")
}

#[cfg(all(test, target_os = "linux"))]
mod trust_tests {
    use super::*;

    fn review(action: &str) -> TrustReview {
        let path = InputPath::Raw {
            raw: "/test".to_owned(),
        };
        TrustReview {
            action: action.to_owned(),
            paths: TrustPaths {
                state: path.clone(),
                machine_config: path.clone(),
                repo_config: path,
            },
            lock_dev: 1,
            lock_ino: 1,
            inventory_hash: String::new(),
            directories: vec![".config/new".to_owned()],
            retained: vec![".old".to_owned()],
            current: vec![".new".to_owned()],
            state: missing_trust_control(),
            config: missing_trust_control(),
            machine_config: missing_trust_control(),
            candidates: Vec::new(),
        }
    }

    #[test]
    fn raw_state_edit_preserves_unrelated_legacy_fields() {
        let original = b"version = 1\nrepo_id = 'same'\nignore_seen = ['.old']\nunknown = 'preserved' # keep\n\n[hooks]\npending = ['.a']\n";
        let updated = edit_trust_state(original, "same", &review("ignores")).unwrap();
        assert!(updated.contains("unknown = 'preserved' # keep"));
        let before: crate::state::State =
            toml::from_str(std::str::from_utf8(original).unwrap()).unwrap();
        let after: crate::state::State = toml::from_str(&updated).unwrap();
        assert_eq!(after.capture_dirs, None);
        assert_eq!(after.hooks, before.hooks);
        assert_eq!(after.files, before.files);
        assert_eq!(after.ignore_seen, BTreeSet::from([".new".to_owned()]));
        assert!(edit_trust_state(original, "foreign", &review("ignores")).is_err());
        let directory = edit_trust_state(original, "same", &review("directories")).unwrap();
        let state: crate::state::State = toml::from_str(&directory).unwrap();
        assert_eq!(
            state.dirs_approved(),
            BTreeSet::from([".config/new".to_owned()])
        );
        assert_eq!(state.ignore_seen, before.ignore_seen);
    }

    #[test]
    fn display_refusal_is_redacted() {
        let value = concat!("ghp_", "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa");
        let error = trust_display_value(value.as_bytes())
            .unwrap_err()
            .to_string();
        assert!(!error.contains(value));
        assert!(trust_display_value(b".config/normal").is_ok());
        assert!(trust_display_value(b".config/\x1b").is_err());
    }

    #[test]
    fn trust_control_rejects_symlinks_and_oversized_files() {
        use std::os::unix::fs::symlink;
        let temp = tempfile::tempdir().unwrap();
        let file = temp.path().join("file");
        std::fs::write(&file, b"first").unwrap();
        let first = trust_file(&file).unwrap();
        std::fs::write(&file, b"changed").unwrap();
        assert!(first != trust_file(&file).unwrap());
        let link = temp.path().join("link");
        symlink(&file, &link).unwrap();
        assert!(trust_file(&link).is_err());
        let large = std::fs::File::create(&file).unwrap();
        large.set_len(8 * 1024 * 1024 + 1).unwrap();
        assert!(trust_file(&file).is_err());
    }
}
