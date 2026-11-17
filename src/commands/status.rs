//! `dotman status`: each managed key's row in the three-way model.

use crate::cli::StatusArgs;
use crate::commands::add::{self, Hold};
use crate::commands::apply::{Classified, classify_key};
use crate::commands::sync;
use crate::context::{self, Context, is_under};
use crate::engine::status::{Row, TemplateError, Variants, Worktree};
use crate::engine::{Home, Obstacle, Tuple};
use crate::git::StatusEntry;
use crate::hooks::{self, Standing};
use crate::name;
use crate::output::json::{self, PathValue, Text};
use crate::output::{self, escape_line};
use crate::repo_config::RepoConfig;
use crate::secrets::{Finding, Place};
use crate::state::{Entry as StateEntry, IdentitySnapshot, State};
use crate::system::{self, Verdict};
use crate::variant::{Flag, Sources};
use anyhow::{Context as _, Result};
use serde::Serialize;
use std::collections::BTreeSet;
use std::io::Write;

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub(crate) enum Observation {
    Present { tuple: TupleValue },
    Missing,
    Blocked { reason: Text },
    Unavailable { reason: &'static str },
}

impl Observation {
    pub(crate) fn home(home: &Home) -> Self {
        match home {
            Home::Missing => Self::Missing,
            Home::Present(tuple) => Self::tuple(tuple),
            Home::Blocked(reason) => Self::Blocked {
                reason: Text::new(match reason {
                    Obstacle::Directory => "directory".to_owned(),
                    Obstacle::Special => "special".to_owned(),
                    Obstacle::Parent(parent) => format!("parent: {parent}"),
                }),
            },
        }
    }

    pub(crate) fn tuple(tuple: &Tuple) -> Self {
        if tuple.hash.len() != 32
            || !tuple
                .hash
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Self::Unavailable {
                reason: "invalid_recorded_hash",
            };
        }
        Self::Present {
            tuple: TupleValue::new(tuple),
        }
    }

    fn recorded(entry: Option<&StateEntry>) -> Result<Self> {
        entry
            .map(Tuple::from_entry)
            .transpose()
            .map(|tuple| tuple.as_ref().map_or(Self::Missing, Self::tuple))
    }
}

#[derive(Clone, Serialize)]
pub(crate) struct TupleValue {
    kind: &'static str,
    hash: String,
    mode: String,
}

impl TupleValue {
    fn new(tuple: &Tuple) -> Self {
        Self {
            kind: match tuple.kind {
                crate::state::Kind::File => "file",
                crate::state::Kind::Symlink => "symlink",
            },
            hash: tuple.hash.clone(),
            mode: format!("{:04o}", tuple.mode),
        }
    }
}

#[derive(Clone, Serialize)]
struct FindingValue {
    rule: &'static str,
    line: usize,
    place: &'static str,
}

impl From<Finding> for FindingValue {
    fn from(found: Finding) -> Self {
        Self {
            rule: found.rule,
            line: found.line,
            place: match found.place {
                Place::Content => "content",
                Place::Name => "name",
                Place::Target => "target",
            },
        }
    }
}

#[derive(Clone, Serialize)]
struct InvalidSource {
    source: Text,
    reason: Text,
}

// reason: independent status metadata, not a state machine.
#[allow(clippy::struct_excessive_bools)]
#[derive(Clone, Serialize)]
struct Entry {
    record: &'static str,
    status: &'static str,
    key: Text,
    scope: &'static str,
    path: Option<PathValue>,
    source: Option<Text>,
    recorded_source: Option<Text>,
    encrypted: bool,
    template: bool,
    system: bool,
    row: Option<&'static str>,
    row_number: Option<u8>,
    category: Option<&'static str>,
    r: Observation,
    source_tuple: Observation,
    h: Observation,
    raw_h: Observation,
    s: Observation,
    identity_changed: bool,
    update_state: bool,
    kept_mode: Option<String>,
    repo_mode: Option<String>,
    alternatives: Vec<Text>,
    invalid_sources: Vec<InvalidSource>,
    warnings: Vec<Text>,
    hold: Option<&'static str>,
    finding: Option<FindingValue>,
    remedies: Vec<Text>,
}

impl Entry {
    fn new(status: &'static str, key: &str, scope: &'static str) -> Self {
        Self {
            record: "entry",
            status,
            key: Text::new(key),
            scope,
            path: None,
            source: None,
            recorded_source: None,
            encrypted: false,
            template: false,
            system: scope == "system",
            row: None,
            row_number: None,
            category: None,
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
            s: Observation::Unavailable {
                reason: "not_observed",
            },
            identity_changed: false,
            update_state: false,
            kept_mode: None,
            repo_mode: None,
            alternatives: Vec::new(),
            invalid_sources: Vec::new(),
            warnings: Vec::new(),
            hold: None,
            finding: None,
            remedies: Vec::new(),
        }
    }

    fn row(&mut self, row: Row) {
        use crate::engine::status::Category;
        self.row = Some(token(row));
        self.row_number = row.number();
        self.update_state = matches!(row, Row::Clean { update_state: true });
        self.category = row.category().map(|category| match category {
            Category::ForgottenEdited => "forgotten_edited",
            Category::Conflicts => "conflicts",
            Category::ExistingFiles => "existing_files",
            Category::LocalDeletions => "local_deletions",
            Category::TemplateDrift => "template_drift",
            Category::Blocked => "blocked",
        });
    }

    fn invalid(&mut self, problems: Vec<(String, &'static str)>) {
        self.invalid_sources = problems
            .into_iter()
            .map(|(source, reason)| InvalidSource {
                source: Text::new(source),
                reason: Text::new(reason),
            })
            .collect();
    }
}

#[derive(Serialize)]
struct IdentityValue {
    host: Text,
    user: Text,
    os: Text,
    distro: Text,
    like: Text,
    arch: Text,
    profiles: Vec<Text>,
}

impl From<&IdentitySnapshot> for IdentityValue {
    fn from(identity: &IdentitySnapshot) -> Self {
        Self {
            host: Text::new(&identity.host),
            user: Text::new(&identity.user),
            os: Text::new(&identity.os),
            distro: Text::new(&identity.distro),
            like: Text::new(&identity.like),
            arch: Text::new(&identity.arch),
            profiles: identity.profiles.iter().map(Text::new).collect(),
        }
    }
}

#[derive(Serialize)]
struct ContextRecord {
    record: &'static str,
    schema_version: u32,
    experimental: bool,
    command: &'static str,
    home: PathValue,
    repo: PathValue,
    config: PathValue,
    repo_config: PathValue,
    state: PathValue,
    identity: PathValue,
    age_key_file: Option<PathValue>,
    repo_id: Option<Text>,
    machine: IdentityValue,
}

pub(crate) fn write_context(
    ctx: &Context,
    command: &'static str,
    repo_id: Option<&str>,
) -> Result<()> {
    json::write(
        &mut std::io::stdout().lock(),
        &ContextRecord {
            record: "context",
            schema_version: 1,
            experimental: true,
            command,
            home: PathValue::new(&ctx.paths.home),
            repo: PathValue::new(&ctx.paths.repo),
            config: PathValue::new(&ctx.paths.config_file),
            repo_config: PathValue::new(&ctx.repo_config_file()),
            state: PathValue::new(&ctx.paths.state_file()),
            identity: PathValue::new(&ctx.paths.age_identity),
            age_key_file: ctx.paths.age_key_file.as_deref().map(PathValue::new),
            repo_id: repo_id.map(Text::new),
            machine: ctx.identity().into(),
        },
    )
}

/// The stable porcelain token for a row.
#[must_use]
pub const fn token(row: Row) -> &'static str {
    match row {
        Row::GoneEverywhere => "gone",
        Row::Forgotten => "forgotten",
        Row::ForgottenEdited => "forgotten-edited",
        Row::Clean { .. } => "clean",
        Row::NotApplied => "not-applied",
        Row::DeletedLocally => "deleted-locally",
        Row::DeletedChangedUpstream => "deleted-changed-upstream",
        Row::Existing => "existing",
        Row::ChangedLocally => "changed-locally",
        Row::ChangedInRepo => "changed-in-repo",
        Row::BothChanged => "both-changed",
        Row::TemplateDrift => "template-drift",
        Row::Blocked => "blocked",
        Row::SymlinkAtTarget => "symlink-at-target",
    }
}

const UNREADABLE: &str = "unreadable";

const UNNAMED_SHOWN: usize = 3;

/// Runs `dotman status`. Takes no lock, writes nothing and always exits 0 when it can read the repo.
///
/// # Errors
///
/// Fails when the repo is not initialised or has a rebase or merge in progress, or the repo, the
/// state or an input cannot be read.
pub fn run(args: &StatusArgs) -> Result<()> {
    let ctx = Context::resolve("status")?;
    let repo_id = ctx.guard().and_then(|()| ctx.repo_id());
    if args.json {
        write_context(&ctx, "status", repo_id.as_deref().ok())?;
    }
    let repo_id = repo_id?;
    let repo_config = ctx.repo_config()?;
    let state = ctx.state(&repo_id)?;

    let (home_paths, system_filters) = system::split_inputs(&ctx.paths, &args.paths)?;
    let filters = if home_paths.is_empty() {
        Vec::new()
    } else {
        let cwd = std::env::current_dir().context("cannot read the current directory")?;
        home_paths
            .iter()
            .map(|p| ctx.input_key(p, &cwd))
            .collect::<Result<Vec<_>>>()?
    };
    let unfiltered = args.paths.is_empty();

    let sources = ctx.sources()?;
    let mut keys: BTreeSet<String> = sources.keys().map(str::to_owned).collect();
    keys.extend(state.files.keys().cloned());
    let scan = add::scan_dirs(&ctx, &repo_config, &keys, &state)?;
    let in_scope = |key: &str| unfiltered || filters.iter().any(|f| is_under(key, f));
    let worktree = Worktree::new(&ctx.paths.repo, &repo_config.modes);
    let variants = Variants::new(
        worktree,
        &sources,
        ctx.identity(),
        ctx.template_vars(),
        ctx.age(),
    );

    let mut entries = Vec::new();
    // Clean rows whose S lags, as after a run killed before it saved the state.
    let mut unrecorded = 0_usize;
    let mut in_home = Vec::new();
    for key in keys {
        if !in_scope(&key) {
            continue;
        }
        if let Some(entry) = home_entry(
            &ctx,
            &variants,
            &sources,
            &state,
            &key,
            &mut unrecorded,
            &mut in_home,
        )? {
            entries.push(entry);
        }
    }

    let wanted = |key: &str| unfiltered || system_filters.iter().any(|f| is_under(key, f));
    entries.extend(system_entries(&ctx, &repo_config, &state, wanted)?);
    let mut twins = BTreeSet::new();
    for (a, b) in ctx.index_collisions()? {
        output::warning(&format!(
            "{} and {} differ only in case or Unicode normalization; `{} apply` and `{} sync` refuse until one is renamed",
            escape_line(&a),
            escape_line(&b),
            name::bin(),
            name::bin()
        ));
        twins.extend([a, b]);
    }
    entries.extend(twins.into_iter().map(|key| {
        let scope = if key.starts_with('/') {
            "system"
        } else {
            "home"
        };
        Entry::new("collision", &key, scope)
    }));

    let new: Vec<&String> = scan.new.keys().filter(|k| in_scope(k)).collect();
    let held: Vec<(&String, Hold)> = scan
        .held
        .iter()
        .filter(|(k, _)| in_scope(k))
        .map(|(k, h)| (k, *h))
        .collect();
    let worktree_status = ctx.git.status()?;
    let mut in_repo = sync::held_entries(&ctx, &worktree_status);
    in_repo.retain(|(entry, _)| in_scope(&entry.path.to_string_lossy()));
    report_unnamed(&ctx, in_scope)?;

    Report {
        ctx: &ctx,
        config: &repo_config,
        state: &state,
        sources: &sources,
        scan: &scan,
        entries: &entries,
        new: &new,
        held: &held,
        in_home: &in_home,
        in_repo: &in_repo,
        unrecorded,
    }
    .render(args, in_scope)
}

struct Report<'a> {
    ctx: &'a Context,
    config: &'a RepoConfig,
    state: &'a State,
    sources: &'a Sources,
    scan: &'a add::Scan,
    entries: &'a [Entry],
    new: &'a [&'a String],
    held: &'a [(&'a String, Hold)],
    in_home: &'a [(String, Finding)],
    in_repo: &'a [(&'a StatusEntry, Finding)],
    unrecorded: usize,
}

#[derive(Serialize)]
struct HookEntry {
    pattern: Text,
    command: Text,
}

#[derive(Serialize)]
struct HookReview {
    standing: &'static str,
    canonical: Text,
    entries: Vec<HookEntry>,
    pending_keys: Vec<Text>,
    pending_commands: Vec<Text>,
}

impl HookReview {
    fn new(ctx: &Context, config: &RepoConfig, state: &State) -> Result<Self> {
        let review = hooks::review(ctx, &config.hooks, state)?;
        let standing = match review.standing {
            Some(Standing::Empty) => "empty",
            Some(Standing::Approved) => "approved",
            Some(Standing::Unapproved) => "unapproved",
            Some(Standing::Changed { old_format: false }) => "changed",
            Some(Standing::Changed { old_format: true }) => "old_format",
            None => "invalid",
        };
        Ok(Self {
            standing,
            canonical: Text::new(review.canonical),
            pending_keys: review.pending.into_iter().map(Text::new).collect(),
            pending_commands: review.commands.into_iter().map(Text::new).collect(),
            entries: config
                .hooks
                .iter()
                .map(|(pattern, command)| HookEntry {
                    pattern: Text::new(pattern),
                    command: Text::new(command),
                })
                .collect(),
        })
    }
}

#[derive(Serialize)]
struct DirectoryReview {
    path: Text,
    waiting: usize,
}

#[derive(Serialize)]
struct IgnoredEntry {
    key: Text,
    pattern: Text,
    directory: bool,
}

#[derive(Serialize)]
struct WarningValue {
    key: Option<Text>,
    message: Text,
}

#[derive(Serialize)]
struct ReviewContext {
    record: &'static str,
    hooks: HookReview,
    unapproved_dirs: Vec<DirectoryReview>,
    waiting_keys: Vec<Text>,
    current_identity: IdentityValue,
    recorded_identity: Option<IdentityValue>,
    retained_ignore_patterns: Vec<Text>,
    ignored_entries: Vec<IgnoredEntry>,
    remote: RemotePosition,
    warnings: Vec<WarningValue>,
    unnamed_repo_paths: Vec<PathValue>,
}

#[derive(Serialize)]
struct Summary {
    record: &'static str,
    rows: usize,
    clean: usize,
    new: usize,
    held: usize,
    held_in_home: usize,
    held_in_repo: usize,
    unrecorded: usize,
}

impl Report<'_> {
    fn render(&self, args: &StatusArgs, in_scope: impl Fn(&str) -> bool) -> Result<()> {
        let mut out = std::io::stdout().lock();
        if args.json {
            return self.write(&mut out, in_scope);
        }
        let rows: Vec<_> = self
            .entries
            .iter()
            .map(|entry| (entry.status, entry.key.raw().to_owned()))
            .collect();
        for (label, key) in &rows {
            if args.porcelain {
                write!(out, "{label}\t{key}\0")
            } else if *label == "clean" {
                Ok(())
            } else {
                writeln!(out, "{label:<26}{}", escape_line(key))
            }
            .context("cannot write to stdout")?;
        }
        if args.porcelain {
            write_records(&mut out, self.new, self.held, self.in_home, self.in_repo)
                .context("cannot write to stdout")?;
        } else {
            list_new(&mut out, self.ctx, self.new, self.held).context("cannot write to stdout")?;
            list_held_in_home(&mut out, self.ctx, self.sources, self.in_home)
                .context("cannot write to stdout")?;
            list_still_ignored(&mut out, self.ctx, self.scan, in_scope)
                .context("cannot write to stdout")?;
            list_held_in_repo(&mut out, self.ctx, self.in_repo)
                .context("cannot write to stdout")?;
            for note in &self.scan.notes {
                output::warning(note);
            }
            summarize(
                &rows,
                self.unrecorded,
                [
                    self.new.len(),
                    self.held.len(),
                    self.in_home.len(),
                    self.in_repo.len(),
                ],
            );
            report_remote(self.ctx)?;
        }
        Ok(())
    }

    fn write(&self, out: &mut impl Write, in_scope: impl Fn(&str) -> bool) -> Result<()> {
        for entry in self.entries {
            json::write(out, entry)?;
        }
        for key in self.new {
            json::write(out, &Entry::new("new", key, "home"))?;
        }
        for (key, hold) in self.held {
            let mut entry = Entry::new("held", key, "home");
            entry.hold = Some(hold_token(*hold));
            if let Hold::Content(found) = hold {
                entry.finding = Some((*found).into());
            }
            entry.remedies.push(Text::new(
                hold.listed(&add::pasteable(&self.ctx.paths.home, key)),
            ));
            json::write(out, &entry)?;
        }
        for (key, found) in self.in_home {
            let mut entry = self
                .entries
                .iter()
                .find(|entry| entry.key.raw() == key)
                .cloned()
                .context("held home entry is missing from status report")?;
            entry.status = "held-in-home";
            entry.finding = Some((*found).into());
            let mut ways = found.ways(Some(&add::pasteable(&self.ctx.paths.home, key)));
            ways.push(add::forget_hint(&self.ctx.paths.home, self.sources, key));
            entry.remedies = ways.into_iter().map(Text::new).collect();
            json::write(out, &entry)?;
        }
        for (status, found) in self.in_repo {
            let mut entry = Entry::new(
                "held-in-repo",
                &status.path.to_string_lossy(),
                "repo_source",
            );
            entry.path = Some(PathValue::new(&status.path));
            entry.finding = Some((*found).into());
            entry
                .remedies
                .push(Text::new(sync::held_remedy(self.ctx, status)));
            json::write(out, &entry)?;
        }
        self.review(out, in_scope)?;
        json::write(
            out,
            &Summary {
                record: "summary",
                rows: self.entries.len(),
                clean: self
                    .entries
                    .iter()
                    .filter(|entry| entry.status == "clean")
                    .count(),
                new: self.new.len(),
                held: self.held.len(),
                held_in_home: self.in_home.len(),
                held_in_repo: self.in_repo.len(),
                unrecorded: self.unrecorded,
            },
        )
    }

    fn review(&self, out: &mut impl Write, in_scope: impl Fn(&str) -> bool) -> Result<()> {
        let warnings = self
            .ctx
            .warnings()
            .into_iter()
            .map(|(key, message)| WarningValue {
                key: Some(Text::new(
                    key.split_once('\0').map_or(key.as_str(), |(_, key)| key),
                )),
                message: Text::new(message),
            })
            .chain(self.scan.notes.iter().map(|note| WarningValue {
                key: None,
                message: Text::new(note),
            }))
            .collect();
        let ignored_entries = self
            .scan
            .still_ignored
            .iter()
            .filter(|(key, _)| in_scope(key))
            .map(|(key, entry)| IgnoredEntry {
                key: Text::new(key),
                pattern: Text::new(&entry.pattern),
                directory: entry.is_dir,
            })
            .collect();
        let retained_ignore_patterns = self
            .state
            .ignore_seen
            .iter()
            .filter(|pattern| !self.config.ignore.patterns.contains(pattern))
            .map(Text::new)
            .collect();
        let unnamed_repo_paths = context::unnamed_untracked(self.ctx)?
            .iter()
            .filter(|path| in_scope(&path.to_string_lossy()))
            .map(|path| PathValue::new(path))
            .collect();
        json::write(
            out,
            &ReviewContext {
                record: "review_context",
                hooks: HookReview::new(self.ctx, self.config, self.state)?,
                unapproved_dirs: self
                    .scan
                    .unapproved_counts()
                    .into_iter()
                    .map(|(dir, waiting)| DirectoryReview {
                        path: Text::new(dir),
                        waiting,
                    })
                    .collect(),
                waiting_keys: self.scan.waiting.keys().map(Text::new).collect(),
                current_identity: self.ctx.identity().into(),
                recorded_identity: self.state.identity.as_ref().map(IdentityValue::from),
                retained_ignore_patterns,
                ignored_entries,
                remote: remote_position(self.ctx)?,
                warnings,
                unnamed_repo_paths,
            },
        )
    }
}

const fn hold_token(hold: Hold) -> &'static str {
    match hold {
        Hold::Large => "large",
        Hold::Binary => "binary",
        Hold::SecretName => "secret_name",
        Hold::SecretBinary => "secret_binary",
        Hold::Content(_) => "content",
        Hold::Special => "special",
        Hold::Unreadable => "unreadable",
        Hold::Collision => "collision",
        Hold::Unapproved => "unapproved",
    }
}

fn write_records(
    out: &mut impl Write,
    new: &[&String],
    held: &[(&String, Hold)],
    in_home: &[(String, Finding)],
    in_repo: &[(&StatusEntry, Finding)],
) -> std::io::Result<()> {
    for key in new {
        write!(out, "new\t{key}\0")?;
    }
    for (key, _) in held {
        write!(out, "held\t{key}\0")?;
    }
    for (key, _) in in_home {
        write!(out, "held-in-home\t{key}\0")?;
    }
    for (entry, _) in in_repo {
        write!(out, "held-in-repo\t{}\0", entry.path.to_string_lossy())?;
    }
    Ok(())
}

/// The untracked repo files that sync never commits because their names are not UTF-8, and that
/// no `[ignore]` pattern silences. Past one, a single warning counts them and names the first
/// [`UNNAMED_SHOWN`], so thousands of them do not bury the rest of the output.
fn report_unnamed(ctx: &Context, in_scope: impl Fn(&str) -> bool) -> Result<()> {
    let names: Vec<String> = context::unnamed_untracked(ctx)?
        .iter()
        .map(|path| path.to_string_lossy().into_owned())
        .filter(|shown| in_scope(shown))
        .collect();
    let repo = ctx.paths.repo.display();
    match names.as_slice() {
        [] => {}
        [one] => output::warning(&format!(
            "{}: in the repo, never committed: its name is not valid UTF-8, which no key can have; rename or remove it in {repo}, or keep it out with an [ignore] pattern",
            escape_line(one)
        )),
        many => {
            output::warning(&format!(
                "{} files in the repo are never committed: their names are not valid UTF-8, which no key can have; rename or remove them in {repo}, or keep them out with an [ignore] pattern:",
                many.len()
            ));
            for name in many.iter().take(UNNAMED_SHOWN) {
                output::warning(&format!("  {}", escape_line(name)));
            }
            let more = many.len().saturating_sub(UNNAMED_SHOWN);
            if more > 0 {
                output::warning(&format!("  and {more} more"));
            }
        }
    }
    Ok(())
}

#[derive(Serialize)]
struct RemotePosition {
    basis: &'static str,
    branch: Option<Text>,
    ahead: Option<usize>,
    behind: Option<usize>,
    #[serde(skip)]
    no_upstream: bool,
}

fn remote_position(ctx: &Context) -> Result<RemotePosition> {
    let mut position = RemotePosition {
        basis: "unknown",
        branch: None,
        ahead: None,
        behind: None,
        no_upstream: false,
    };
    let git = &ctx.git;
    if git.remote_url("origin")?.is_none() || git.head()?.is_none() {
        return Ok(position);
    }
    let Some(branch) = git.branch()? else {
        return Ok(position);
    };
    position.branch = Some(Text::new(&branch));
    let upstream = format!("refs/remotes/origin/{branch}");
    // No branch was ever pushed, as when every push after `init --remote` failed.
    if git.resolve_commit(&upstream)?.is_none() {
        // With no `origin` ref at all, nothing was fetched, and the count would take in the
        // commits origin already has.
        let fetched = git
            .command()
            .args([
                "for-each-ref",
                "--count=1",
                "--format=%(refname)",
                "refs/remotes/origin/",
            ])
            .output()
            .context("failed to run git")?;
        if fetched.status.success() && fetched.stdout.is_empty() {
            position.basis = "not_fetched";
            return Ok(position);
        }
        let out = git
            .command()
            .args([
                "rev-list",
                "--count",
                "HEAD",
                "--not",
                "--remotes=origin",
                "--",
            ])
            .output()
            .context("failed to run git")?;
        let count = String::from_utf8_lossy(&out.stdout).trim().parse::<usize>();
        if let (Ok(n @ 1..), true) = (count, out.status.success()) {
            position.basis = "last_fetched";
            position.ahead = Some(n);
            position.no_upstream = true;
        }
        return Ok(position);
    }
    let out = git
        .command()
        .args(["rev-list", "--left-right", "--count"])
        .arg(format!("HEAD...{upstream}"))
        .arg("--")
        .output()
        .context("failed to run git")?;
    let text = String::from_utf8_lossy(&out.stdout);
    let counts: Vec<usize> = text
        .split_whitespace()
        .filter_map(|n| n.parse().ok())
        .collect();
    let (&[ahead, behind], true) = (counts.as_slice(), out.status.success()) else {
        return Ok(position);
    };
    position.basis = "last_fetched";
    position.ahead = Some(ahead);
    position.behind = Some(behind);
    Ok(position)
}

/// How HEAD stands against `origin/<branch>` as it was last fetched; nothing is fetched.
fn report_remote(ctx: &Context) -> Result<()> {
    let remote = remote_position(ctx)?;
    let commits = |n: usize| if n == 1 { "commit" } else { "commits" };
    if remote.basis == "not_fetched" {
        output::info(&format!(
            "origin has not been fetched yet; `{} sync` fetches and pushes",
            name::bin()
        ));
        return Ok(());
    }
    if remote.no_upstream {
        if let Some(n) = remote.ahead {
            output::info(&format!(
                "{n} {} not pushed yet: run `{} sync`",
                commits(n),
                name::bin()
            ));
        }
        return Ok(());
    }
    let (Some(ahead), Some(behind)) = (remote.ahead, remote.behind) else {
        return Ok(());
    };
    let position = match (ahead, behind) {
        (0, 0) => return Ok(()),
        (ahead, 0) => format!("{ahead} {} to push", commits(ahead)),
        (0, behind) => format!("{behind} {} to pull", commits(behind)),
        (ahead, behind) => format!("{ahead} {} to push, {behind} to pull", commits(ahead)),
    };
    output::info(&format!("{position}: run `{} sync`", name::bin()));
    Ok(())
}

/// The token for a home key; `None` when it is not managed. A clean key whose S lags counts in
/// `unrecorded`.
fn home_entry(
    ctx: &Context,
    variants: &Variants<'_>,
    sources: &Sources,
    state: &State,
    key: &str,
    unrecorded: &mut usize,
    in_home: &mut Vec<(String, Finding)>,
) -> Result<Option<Entry>> {
    let mut entry = Entry::new(UNREADABLE, key, "home");
    entry.alternatives = sources.sources_of(key).map(Text::new).collect();
    if let Some(source) = sources.select(key, ctx.identity()).winner() {
        entry.source = Some(Text::new(*source));
        entry.encrypted = sources.flag(key, source) == Flag::Age;
        entry.template = sources.flag(key, source) == Flag::Tmpl;
    }
    entry.s = Observation::recorded(state.files.get(key))?;
    entry.recorded_source = state.files.get(key).map(|s| Text::new(&s.source));
    entry.status = match classify_key(ctx, variants, state, key) {
        Ok(Classified::Unmanaged) => return Ok(None),
        Ok(Classified::Item(item)) => {
            if let Some(found) = item.capture_hold(ctx) {
                in_home.push((key.to_owned(), found));
            }
            if item.token() == "clean" && item.row() == (Row::Clean { update_state: true }) {
                *unrecorded = unrecorded.saturating_add(1);
            }
            if let Some((kept, repo)) = item.kept() {
                output::info(&format!(
                    "{key}: kept at {kept:04o} here; the repo says {repo:04o}"
                ));
                entry.kept_mode = Some(format!("{kept:04o}"));
                entry.repo_mode = Some(format!("{repo:04o}"));
            }
            entry.row(item.row());
            entry.identity_changed = item.identity_changed();
            let (h, raw) = item.observations();
            entry.h = Observation::home(h);
            entry.raw_h = Observation::home(raw);
            entry.r = Observation::Missing;
            entry.source_tuple = Observation::Missing;
            if let Some((source, tuple, encrypted, template)) = item.source_metadata() {
                entry.source = Some(Text::new(source));
                entry.encrypted = encrypted;
                entry.template = template;
                entry.r = Observation::tuple(tuple);
                let source_tuple = Tuple {
                    mode: item.kept().map_or(tuple.mode, |(_, repo)| repo),
                    ..tuple.clone()
                };
                entry.source_tuple = Observation::tuple(&source_tuple);
            }
            item.token()
        }
        Ok(Classified::Ambiguous(tied)) => {
            entry.alternatives = tied.into_iter().map(Text::new).collect();
            "ambiguous"
        }
        Ok(Classified::Invalid(problems)) => {
            entry.invalid(problems);
            "invalid"
        }
        Ok(Classified::Locked(_)) => {
            entry.r = Observation::Unavailable { reason: "locked" };
            "locked"
        }
        Err(e) => {
            output::warning(&format!("{key}: {e:#}"));
            if e.is::<TemplateError>() {
                entry.r = Observation::Unavailable {
                    reason: "template_error",
                };
                entry.warnings.push(Text::new("template-error"));
                "template-error"
            } else {
                entry.warnings.push(Text::new("cannot observe this key"));
                UNREADABLE
            }
        }
    };
    Ok(Some(entry))
}

/// New files are under `[dirs]`, which sync captures, so they count as managed. Held files are
/// not managed, as `list` shows, but they count as to look at.
fn summarize(
    rows: &[(&str, String)],
    unrecorded: usize,
    [new, held, held_in_home, held_in_repo]: [usize; 4],
) {
    let clean = rows.iter().filter(|(label, _)| *label == "clean").count();
    let managed = rows.len().saturating_add(new);
    let breakdown = if new > 0 || held > 0 {
        format!(" ({new} new, {held} held back)")
    } else {
        String::new()
    };
    let in_home = if held_in_home > 0 {
        format!(", {held_in_home} held in $HOME")
    } else {
        String::new()
    };
    let in_repo = if held_in_repo > 0 {
        format!(", {held_in_repo} held in the repo")
    } else {
        String::new()
    };
    output::info(&format!(
        "{managed} managed, {clean} clean, {} to look at{breakdown}{in_home}{in_repo}",
        managed.saturating_sub(clean).saturating_add(held)
    ));
    if unrecorded > 0 {
        output::info(&format!(
            "run `{} apply` to record {unrecorded} {} that already {} the repo",
            name::bin(),
            if unrecorded == 1 { "path" } else { "paths" },
            if unrecorded == 1 { "matches" } else { "match" }
        ));
    }
}

/// The header's note on a list of held files. An `[ignore]` pattern is no answer for a file held
/// only because its `[dirs]` entry is not approved here; its own line names the one that is.
pub(crate) fn held_note(holds: impl IntoIterator<Item = Hold>) -> &'static str {
    let holds: Vec<Hold> = holds.into_iter().collect();
    if holds.iter().all(|hold| *hold == Hold::Unapproved) {
        ""
    } else if holds.contains(&Hold::Unapproved) {
        "; an [ignore] pattern in .dotman.toml silences one not under an unapproved [dirs] entry"
    } else {
        "; an [ignore] pattern in .dotman.toml silences one"
    }
}

fn list_new(
    out: &mut impl Write,
    ctx: &Context,
    new: &[&String],
    held: &[(&String, Hold)],
) -> std::io::Result<()> {
    if !new.is_empty() {
        writeln!(out, "\nnew files, which `{} sync` captures:", name::bin())?;
        for key in new {
            writeln!(out, "  {}", escape_line(key))?;
        }
    }
    if !held.is_empty() {
        writeln!(
            out,
            "\nheld back{}:",
            held_note(held.iter().map(|(_, hold)| *hold))
        )?;
        for (key, hold) in held {
            let path = add::pasteable(&ctx.paths.home, key);
            writeln!(
                out,
                "  {} ({})",
                escape_line(key),
                escape_line(&hold.listed(&path))
            )?;
        }
    }
    Ok(())
}

fn list_still_ignored(
    out: &mut impl Write,
    ctx: &Context,
    scan: &add::Scan,
    in_scope: impl Fn(&str) -> bool,
) -> std::io::Result<()> {
    let found: Vec<_> = scan
        .still_ignored
        .iter()
        .filter(|(key, _)| in_scope(key))
        .collect();
    if found.is_empty() {
        return Ok(());
    }
    writeln!(out, "\n{}", add::still_ignored_header())?;
    for (key, entry) in found {
        let path = add::pasteable(&ctx.paths.home, key);
        writeln!(
            out,
            "  {}{} ({})",
            escape_line(key),
            if entry.is_dir { "/" } else { "" },
            escape_line(&entry.listed(&path))
        )?;
    }
    Ok(())
}

/// The managed files whose edit in `$HOME` the guard would hold, which keep their
/// `changed-locally` row: `sync` captures none of them.
fn list_held_in_home(
    out: &mut impl Write,
    ctx: &Context,
    sources: &Sources,
    held: &[(String, Finding)],
) -> std::io::Result<()> {
    if held.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "\nheld in $HOME; `{} sync` captures none of it:",
        name::bin()
    )?;
    for (key, found) in held {
        writeln!(out, "  {}: {found}", escape_line(key))?;
        let path = add::pasteable(&ctx.paths.home, key);
        let mut ways = found.ways(Some(&path));
        ways.push(add::forget_hint(&ctx.paths.home, sources, key));
        for (i, way) in ways.iter().enumerate() {
            let or = if i == 0 { "" } else { "or " };
            writeln!(out, "    {or}{}", escape_line(way))?;
        }
    }
    Ok(())
}

/// The repo edits that sync holds, which a clean row cannot show: `dotman edit` leaves the key
/// clean while its uncommitted copy holds a credential.
fn list_held_in_repo(
    out: &mut impl Write,
    ctx: &Context,
    held: &[(&StatusEntry, Finding)],
) -> std::io::Result<()> {
    if held.is_empty() {
        return Ok(());
    }
    writeln!(
        out,
        "\nheld in the repo; `{} sync` commits none of it, and with a remote stops before the pull:",
        name::bin()
    )?;
    for (entry, found) in held {
        writeln!(
            out,
            "  {}: {found}\n    {}",
            escape_line(&entry.path.to_string_lossy()),
            escape_line(&sync::held_remedy(ctx, entry))
        )?;
    }
    Ok(())
}

fn system_entries(
    ctx: &Context,
    repo_config: &RepoConfig,
    state: &State,
    wanted: impl Fn(&str) -> bool,
) -> Result<Vec<Entry>> {
    let scope = ctx.system_scope();
    let mut rows = Vec::new();
    for found in system::evaluate(ctx, repo_config, state)? {
        if !wanted(&found.key) {
            continue;
        }
        let key = found.key;
        let mut entry = Entry::new(UNREADABLE, &key, "system");
        entry.s = Observation::recorded(state.system.get(&key))?;
        entry.recorded_source = state.system.get(&key).map(|s| Text::new(&s.source));
        if let Verdict::Row { row, r, h } = &found.verdict {
            entry.row(*row);
            entry.h = Observation::home(h);
            entry.raw_h = Observation::home(h);
            entry.r = r
                .as_ref()
                .map_or(Observation::Missing, |r| Observation::tuple(&r.tuple));
            entry.source_tuple = r
                .as_ref()
                .map_or(Observation::Missing, |r| Observation::tuple(&r.tuple));
            entry.source = r.as_ref().map(|r| Text::new(&r.source));
        }
        let label = match found.verdict {
            Verdict::Refused(why) => {
                output::warning(&format!("{key}: {why}"));
                entry.warnings.push(Text::new(why));
                "invalid"
            }
            Verdict::Invalid(problems) => {
                for (source, reason) in &problems {
                    output::warning(&format!("{key}: skipped, {source}: {reason}"));
                }
                entry.invalid(problems);
                "invalid"
            }
            Verdict::Ambiguous(tied) => {
                entry.alternatives = tied.into_iter().map(Text::new).collect();
                "ambiguous"
            }
            _ if found.off => "system-off",
            Verdict::Unreadable(why) => {
                output::warning(&format!("{key}: {why}"));
                entry
                    .warnings
                    .push(Text::new("cannot observe this system key"));
                UNREADABLE
            }
            Verdict::Row { row, .. } => {
                let siblings = scope.siblings(&key);
                if !siblings.is_empty() {
                    output::warning(&system::sibling_note(&key, &siblings));
                    entry
                        .warnings
                        .push(Text::new(system::sibling_note(&key, &siblings)));
                }
                token(row)
            }
        };
        entry.status = label;
        rows.push(entry);
    }
    Ok(rows)
}
