use crate::context::Context;
use crate::engine::{Home, Tuple};
use crate::paths::validate_key;
use crate::state::{IdentitySnapshot, Kind};
use anyhow::{Context as _, Result, ensure};
use serde::Deserialize;
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
}

#[derive(Deserialize)]
#[serde(untagged, deny_unknown_fields)]
pub enum InputPath {
    Raw { raw: String },
    Bytes { bytes: Vec<u8> },
}

impl InputPath {
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
        let Some(source) = source else {
            return Ok(None);
        };
        ensure!(source == "-", "decisions must come from stdin");
        let mut bytes = Vec::new();
        std::io::stdin()
            .lock()
            .read_to_end(&mut bytes)
            .context("cannot read reviewed decisions")?;
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

    pub(crate) fn bind(self, ctx: &Context, sync: bool) -> Result<Reviewed> {
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

impl Reviewed {
    pub(crate) fn recheck(&self, ctx: &Context) -> Result<()> {
        if !self.input.membership(ctx)?
            || self.input.repo_id != ctx.repo_id()?
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
