//! A pull that conflicts with this machine's commits. Each stopped path is described from the
//! merge base, this machine's `HEAD` and the fetched commit; what needs no answer is settled, the
//! rest is answered by `--keep-local` or `--take-repo`, and the pull is replayed onto the fetched
//! commit. Without an answer, or for a shape dotman does not model, nothing changes and sync
//! exits 4 with a report.

use crate::commands::add;
use crate::commands::diff::private_base;
use crate::commands::sync::{REMOTE, print_steps, quoted};
use crate::context::{Context, TEMP_PREFIX, holds_no_settings};
use crate::engine::write::{back_up_bytes, backup_run_dir};
use crate::git::{Decision, Entry, Refusal, Replayed, Side, Stopped};
use crate::name;
use crate::output;
use crate::paths::{private_temp_dir, validate_key};
use crate::repo_config::{self, RepoConfig, Settled, Tie, Winner};
use crate::secrets;
use crate::system;
use crate::template;
use crate::variant::{Flag, Sources};
use anyhow::{Context as _, Result, anyhow, bail};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::time::SystemTime;
use tempfile::TempDir;

const SETTINGS: &str = ".dotman.toml";

/// Generated from the rest of the repo, so origin's version is taken with no question.
const GENERATED: [&str; 2] = [".dotman/age-recipients", "install.sh"];

/// The modes dotman manages; any other, such as a gitlink's, is a shape it does not settle.
const MODES: [&str; 3] = ["100644", "100755", "120000"];

const SYMLINK: &str = "120000";

/// The answer `--keep-local` or `--take-repo` gives every question of a pull.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Mine,
    Theirs,
}

impl Answer {
    pub const fn from_flags(keep_local: bool, take_repo: bool) -> Option<Self> {
        if keep_local {
            Some(Self::Mine)
        } else if take_repo {
            Some(Self::Theirs)
        } else {
            None
        }
    }

    const fn winner(self) -> Winner {
        match self {
            Self::Mine => Winner::Mine,
            Self::Theirs => Winner::Theirs,
        }
    }

    const fn flag(self) -> &'static str {
        match self {
            Self::Mine => "--keep-local",
            Self::Theirs => "--take-repo",
        }
    }
}

/// What a path needs before the replay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// Plain or `{tmpl}` text: each clash takes one side, and both sides' other edits are kept.
    Text { template: bool },
    /// `{age}`, binary, a symlink, or a file against a symlink: one side whole.
    Whole,
    /// Edited here, removed in origin.
    RemovedThere,
    /// Removed here, edited in origin.
    RemovedHere,
}

#[derive(Debug)]
enum Class {
    /// Settled with no question; a winner when the `[modes]` entry follows a side.
    Settled(Content, Option<Winner>),
    Question(Kind),
    /// `.dotman.toml` with settings on every side, merged by structure.
    Settings,
    /// Why dotman cannot settle it.
    Unmodelled(String),
}

/// A path's final content before its blob is written.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Content {
    Entry(Option<Entry>),
    Bytes { bytes: Vec<u8>, mode: String },
}

/// One side of `.dotman.toml`.
enum Config {
    Absent,
    /// Empty, or comments only: the plain-text rules apply.
    NoSettings,
    Parsed(RepoConfig),
    Unparsable(String),
}

/// The three parsed sides of `.dotman.toml`, with the merge and the keys it left as ties.
struct Sides {
    merged: RepoConfig,
    mine: RepoConfig,
    theirs: RepoConfig,
    ties: Vec<Tie>,
}

/// The merged `.dotman.toml` and how many settings it took from origin.
struct Merged {
    content: Content,
    from_origin: usize,
}

/// Settles the conflicting pull of `branch` onto `onto`, the commit the pull fetched, starting
/// from the paths of its first stop. True when the pull was replayed; false when it was reported,
/// with nothing changed. A replay that cannot be undone fails with
/// [`crate::context::InProgress`].
pub fn resolve(
    ctx: &Context,
    branch: &str,
    onto: &str,
    stopped: Vec<Stopped>,
    answer: Option<Answer>,
) -> Result<bool> {
    let mut pull = Pull::new(ctx, branch, onto, answer)?;
    let mut stopped = stopped;
    loop {
        if !pull.decide(&stopped)? {
            return Ok(false);
        }
        let host = match ctx.identity().host.as_str() {
            "" => "unknown",
            host => host,
        };
        let replayed = ctx
            .git
            .replay(REMOTE, branch, onto, &pull.decisions, host)
            .map_err(|e| refused(ctx, e))?;
        match replayed {
            Replayed::Done => break,
            Replayed::Stopped(more) => {
                if more.is_empty() || more.iter().any(|s| pull.decisions.contains_key(&s.path)) {
                    bail!("the replay of the pull stopped on paths it had already settled");
                }
                stopped = more;
            }
        }
    }
    if let Some(n) = pull.from_origin.filter(|n| *n > 0) {
        output::info(&format!(
            "merged {SETTINGS}: {} from {REMOTE}/{branch}",
            output::count(n, "setting")
        ));
    }
    Ok(true)
}

/// `main` exits 4 on [`crate::context::InProgress`], which carries the steps that put the repo
/// back; the failure that led there is printed first.
fn refused(ctx: &Context, err: anyhow::Error) -> anyhow::Error {
    let failed = err.to_string();
    match err.downcast::<Refusal>() {
        Ok(refusal) => {
            if failed != refusal.to_string() {
                output::warning(&failed);
            }
            ctx.refused_pull(refusal).into()
        }
        Err(err) => err,
    }
}

struct Pull<'a> {
    ctx: &'a Context,
    branch: &'a str,
    answer: Option<Answer>,
    base: BTreeMap<PathBuf, Entry>,
    mine: BTreeMap<PathBuf, Entry>,
    theirs: BTreeMap<PathBuf, Entry>,
    decisions: BTreeMap<PathBuf, Decision>,
    winners: BTreeMap<String, Settled>,
    /// `.dotman.toml` stopped with settings on every side.
    settings: bool,
    /// Every stopped path so far, for the git steps.
    seen: Vec<PathBuf>,
    saved: BTreeSet<PathBuf>,
    backups: Option<PathBuf>,
    temp: Option<TempDir>,
    from_origin: Option<usize>,
}

impl<'a> Pull<'a> {
    fn new(ctx: &'a Context, branch: &'a str, onto: &str, answer: Option<Answer>) -> Result<Self> {
        let git = &ctx.git;
        let base = git
            .merge_base("HEAD", onto)?
            .ok_or_else(|| anyhow!("HEAD and {onto} have no commit in common"))?;
        Ok(Self {
            ctx,
            branch,
            answer,
            base: git.tree_entries(&base)?,
            mine: git.tree_entries("HEAD")?,
            theirs: git.tree_entries(onto)?,
            decisions: BTreeMap::new(),
            winners: BTreeMap::new(),
            settings: false,
            seen: Vec::new(),
            saved: BTreeSet::new(),
            backups: None,
            temp: None,
            from_origin: None,
        })
    }

    /// Decides every path of `stopped`, writes the new blobs and saves this machine's versions
    /// that lose. False when it reported instead, having changed nothing.
    fn decide(&mut self, stopped: &[Stopped]) -> Result<bool> {
        let mut unmodelled = Vec::new();
        let mut questions = Vec::new();
        let mut settled = BTreeMap::new();
        for stop in stopped {
            if !self.seen.contains(&stop.path) {
                self.seen.push(stop.path.clone());
            }
            match self.classify(stop)? {
                Class::Unmodelled(why) => unmodelled.push(why),
                Class::Settled(content, winner) => {
                    if let Some(winner) = winner {
                        let settled = self.settled(&stop.path, winner, false);
                        self.winners.insert(source(&stop.path), settled);
                    }
                    settled.insert(stop.path.clone(), content);
                }
                Class::Settings => self.settings = true,
                Class::Question(kind) => questions.push((stop.path.clone(), kind)),
            }
        }
        if !unmodelled.is_empty() {
            self.report_unmodelled(&unmodelled);
            return Ok(false);
        }
        let Some(answer) = self.answer else {
            let ties = match self.settings_merge()? {
                Ok(Some(sides)) => sides.ties,
                Ok(None) => Vec::new(),
                Err(why) => {
                    self.report_unmodelled(&[why]);
                    return Ok(false);
                }
            };
            if questions.is_empty() && ties.is_empty() {
                return self.settle(settled, None);
            }
            self.report_unanswered(&questions, &ties);
            return Ok(false);
        };
        for (path, kind) in &questions {
            match self.answered(path, *kind, answer)? {
                Ok(content) => {
                    settled.insert(path.clone(), content);
                }
                Err(why) => {
                    self.report_refused(path, &why, answer);
                    return Ok(false);
                }
            }
        }
        self.settle(settled, Some(answer))
    }

    /// Adds the merged `.dotman.toml`, writes the new blobs and saves the losing versions.
    fn settle(
        &mut self,
        mut settled: BTreeMap<PathBuf, Content>,
        answer: Option<Answer>,
    ) -> Result<bool> {
        match self.settings_content(answer)? {
            Ok(Some(merged)) => {
                settled.insert(PathBuf::from(SETTINGS), merged.content);
                self.from_origin = Some(merged.from_origin);
            }
            Ok(None) => {}
            Err(why) => {
                let path = Path::new(SETTINGS);
                match answer {
                    Some(answer) => self.report_refused(path, &why, answer),
                    None => self.report_unmodelled(&[why]),
                }
                return Ok(false);
            }
        }
        for (path, content) in settled {
            let decision = match content {
                Content::Entry(Some(entry)) => Decision::Set(entry),
                Content::Entry(None) => Decision::Absent,
                Content::Bytes { bytes, mode } => Decision::Set(Entry {
                    oid: self.ctx.git.write_blob(&bytes)?,
                    mode,
                }),
            };
            self.decisions.insert(path, decision);
        }
        self.save_losers()?;
        Ok(true)
    }

    fn classify(&mut self, stop: &Stopped) -> Result<Class> {
        if let Some(why) = self.unmodelled(stop) {
            return Ok(Class::Unmodelled(why));
        }
        let path = stop.path.as_path();
        let (mine, theirs) = (self.mine.get(path), self.theirs.get(path));
        if mine == theirs || GENERATED.iter().any(|name| path == Path::new(name)) {
            return Ok(Class::Settled(Content::Entry(theirs.cloned()), None));
        }
        if path == Path::new(SETTINGS) {
            match [self.config(Rev::Mine), self.config(Rev::Theirs)] {
                [Config::Unparsable(why), _] | [_, Config::Unparsable(why)] => {
                    return Ok(Class::Unmodelled(why));
                }
                [Config::Parsed(_), Config::Parsed(_)] => match self.config(Rev::Base) {
                    Config::Unparsable(why) => return Ok(Class::Unmodelled(why)),
                    _ => return Ok(Class::Settings),
                },
                _ => {}
            }
        }
        let (Some(mine), Some(theirs)) = (mine, theirs) else {
            return Ok(Class::Question(if mine.is_some() {
                Kind::RemovedThere
            } else {
                Kind::RemovedHere
            }));
        };
        let (mine, theirs) = (mine.clone(), theirs.clone());
        let source = source(path);
        let sources = Sources::group([source.as_str()]);
        let flag = sources
            .keys()
            .next()
            .map_or(Flag::Plain, |key| sources.flag(key, &source));
        if flag == Flag::Age {
            if self.same_plaintext(&mine, &theirs) {
                let content = Content::Entry(Some(theirs));
                return Ok(Class::Settled(content, Some(Winner::Theirs)));
            }
            return Ok(Class::Question(Kind::Whole));
        }
        if mine.mode == SYMLINK || theirs.mode == SYMLINK {
            return Ok(Class::Question(Kind::Whole));
        }
        for entry in [Some(&mine), Some(&theirs), self.base.get(path)]
            .into_iter()
            .flatten()
        {
            if self.blob(entry)?.contains(&0) {
                return Ok(Class::Question(Kind::Whole));
            }
        }
        Ok(Class::Question(Kind::Text {
            template: flag == Flag::Tmpl,
        }))
    }

    /// Why a stopped path is a shape dotman does not settle: a file against a directory, an entry
    /// that is not a file or a symlink, or a stage git took from a renamed path.
    fn unmodelled(&self, stop: &Stopped) -> Option<String> {
        let name = self
            .existing(&stop.path)
            .unwrap_or_else(|| stop.path.clone());
        for at in name.ancestors().filter(|at| !at.as_os_str().is_empty()) {
            let (here, there) = (kind(&self.mine, at), kind(&self.theirs, at));
            let dir_against_other =
                (here == DIRECTORY) != (there == DIRECTORY) && here != MISSING && there != MISSING;
            if dir_against_other {
                return Some(self.two_kinds(at, here, there));
            }
        }
        let path = stop.path.as_path();
        let staged = [&stop.base, &stop.ours, &stop.replayed]
            .into_iter()
            .flatten();
        let committed = [&self.base, &self.mine, &self.theirs]
            .into_iter()
            .filter_map(|tree| tree.get(path));
        if staged
            .chain(committed)
            .any(|entry| !MODES.contains(&entry.mode.as_str()))
        {
            let (here, there) = (kind(&self.mine, &name), kind(&self.theirs, &name));
            return Some(self.two_kinds(&name, here, there));
        }
        stop.follows_rename.then(|| {
            format!(
                "git matched {} with a file renamed here or in {REMOTE}/{}",
                shown(path),
                self.branch
            )
        })
    }

    fn two_kinds(&self, path: &Path, here: &str, there: &str) -> String {
        format!(
            "{} is {here} here and {there} in {REMOTE}/{}",
            shown(path),
            self.branch
        )
    }

    /// `path`, or the path git renamed it from when the stop names a copy such as `x~HEAD`, as it
    /// stands in this machine's tree or origin's, where a pull run by hand finds it; `None` when
    /// neither stands in either.
    fn existing(&self, path: &Path) -> Option<PathBuf> {
        let stands = |path: &Path| {
            [&self.mine, &self.theirs]
                .iter()
                .any(|tree| kind(tree, path) != MISSING)
        };
        if stands(path) {
            return Some(path.to_path_buf());
        }
        let name = path.file_name()?.to_str()?;
        let (stem, _) = name.split_once('~')?;
        let stem = path.with_file_name(stem);
        stands(&stem).then_some(stem)
    }

    fn config(&self, rev: Rev) -> Config {
        let (tree, place) = match rev {
            Rev::Base => (
                &self.base,
                "in the commit both sides started from".to_owned(),
            ),
            Rev::Mine => (&self.mine, "here".to_owned()),
            Rev::Theirs => (&self.theirs, format!("in {REMOTE}/{}", self.branch)),
        };
        let Some(entry) = tree.get(Path::new(SETTINGS)) else {
            return Config::Absent;
        };
        let bytes = match self.blob(entry) {
            Ok(bytes) => bytes,
            Err(e) => {
                return Config::Unparsable(format!("{SETTINGS} cannot be read {place}: {e:#}"));
            }
        };
        if bytes.is_empty() || holds_no_settings(&bytes) {
            return Config::NoSettings;
        }
        match std::str::from_utf8(&bytes)
            .map_err(anyhow::Error::from)
            .and_then(RepoConfig::parse)
        {
            Ok(config) => Config::Parsed(config),
            Err(e) => Config::Unparsable(format!("{SETTINGS} does not parse {place}: {e:#}")),
        }
    }

    fn blob(&self, entry: &Entry) -> Result<Vec<u8>> {
        self.ctx.git.blob(&entry.oid)
    }

    /// Whether both encrypted versions decrypt here to the same plaintext. Only the ciphertext is
    /// written, into the private temp dir; the plaintext stays in memory.
    fn same_plaintext(&mut self, mine: &Entry, theirs: &Entry) -> bool {
        let mut plain = |entry: &Entry, name: &str| -> Result<Vec<u8>> {
            let ciphertext = self.blob(entry)?;
            let file = self.temp_dir()?.join(name);
            std::fs::write(&file, ciphertext)
                .with_context(|| format!("cannot write {}", file.display()))?;
            self.ctx.age().decrypt(&file)
        };
        match (plain(mine, "mine.age"), plain(theirs, "theirs.age")) {
            (Ok(mine), Ok(theirs)) => mine == theirs,
            _ => false,
        }
    }

    fn temp_dir(&mut self) -> Result<PathBuf> {
        if let Some(dir) = &self.temp {
            return Ok(dir.path().to_path_buf());
        }
        // The PID in the name is what lets the temp sweep remove a killed run's copy.
        let prefix = format!("{TEMP_PREFIX}{}-", std::process::id());
        let dir = private_temp_dir(&prefix, &private_base(self.ctx)?)?;
        let path = dir.path().to_path_buf();
        self.temp = Some(dir);
        Ok(path)
    }

    fn settled(&self, path: &Path, winner: Winner, text_merge: bool) -> Settled {
        let mode = |tree: &BTreeMap<PathBuf, Entry>| {
            tree.get(path)
                .and_then(|entry| u32::from_str_radix(&entry.mode, 8).ok())
                .unwrap_or(0o100_644)
        };
        Settled {
            winner,
            text_merge,
            ours_mode: mode(&self.mine),
            theirs_mode: mode(&self.theirs),
        }
    }

    /// The content `answer` gives `path`, or why the new text it would make is refused.
    fn answered(
        &mut self,
        path: &Path,
        kind: Kind,
        answer: Answer,
    ) -> Result<Result<Content, String>> {
        let (mine, theirs) = (self.mine.get(path).cloned(), self.theirs.get(path).cloned());
        let text_merge = matches!(kind, Kind::Text { .. });
        let settled = self.settled(path, answer.winner(), text_merge);
        self.winners.insert(source(path), settled);
        let (Kind::Text { template }, Some(mine), Some(theirs)) = (kind, &mine, &theirs) else {
            return Ok(Ok(Content::Entry(match answer {
                Answer::Mine => mine,
                Answer::Theirs => theirs,
            })));
        };
        let base = self.base.get(path).cloned();
        let sides = [self.blob(mine)?, self.blob(theirs)?];
        let base_bytes = match &base {
            Some(entry) => self.blob(entry)?,
            None => Vec::new(),
        };
        let host = &self.ctx.identity().host;
        let ours_label = format!("this machine ({host})");
        let theirs_label = format!("{REMOTE}/{}", self.branch);
        let side = match answer {
            Answer::Mine => Side::Ours,
            Answer::Theirs => Side::Theirs,
        };
        let dir = self.temp_dir()?;
        let [ours_bytes, theirs_bytes] = &sides;
        let (merged, _) = self.ctx.git.merge_file(
            &dir,
            [ours_bytes, &base_bytes, theirs_bytes],
            [&ours_label, "base", &theirs_label],
            Some(side),
        )?;
        if let Some(why) = accept(self.ctx, &source(path), &merged, &sides, template) {
            return Ok(Err(why));
        }
        Ok(Ok(Content::Bytes {
            bytes: merged,
            mode: merged_mode(base.as_ref(), mine, theirs, answer),
        }))
    }

    /// The structured merge of `.dotman.toml` when it stopped, or when a settled file's
    /// `[modes]` entry or `[system]` table must follow its winner where git's merge would not;
    /// `Err` names a stopped side that does not parse.
    fn settings_merge(&self) -> Result<Result<Option<Sides>, String>> {
        let decided_as_a_file = self.seen.iter().any(|seen| seen == Path::new(SETTINGS));
        if !self.settings && (self.winners.is_empty() || decided_as_a_file) {
            return Ok(Ok(None));
        }
        let mut configs = Vec::new();
        for rev in [Rev::Base, Rev::Mine, Rev::Theirs] {
            configs.push(match self.config(rev) {
                Config::Parsed(config) => config,
                Config::Absent | Config::NoSettings => RepoConfig::default(),
                Config::Unparsable(why) if self.settings => return Ok(Err(why)),
                // git merged it cleanly, and nothing here can read it to correct the result.
                Config::Unparsable(_) => return Ok(Ok(None)),
            });
        }
        let [base, mine, theirs]: [RepoConfig; 3] = configs
            .try_into()
            .map_err(|_| anyhow!("three sides of {SETTINGS}"))?;
        let (merged, ties) = repo_config::merge(&base, &mine, &theirs, &self.winners, &[]);
        if !self.settings {
            let (plain, _) = repo_config::merge(&base, &mine, &theirs, &BTreeMap::new(), &[]);
            if plain == merged {
                return Ok(Ok(None));
            }
        }
        Ok(Ok(Some(Sides {
            merged,
            mine,
            theirs,
            ties,
        })))
    }

    /// The `.dotman.toml` content of [`Pull::settings_merge`], with each tie answered. A side's
    /// own blob is kept when the merge equals it, so nothing is rewritten for no change.
    fn settings_content(&self, answer: Option<Answer>) -> Result<Result<Option<Merged>, String>> {
        let mut sides = match self.settings_merge()? {
            Ok(Some(found)) => found,
            Ok(None) => return Ok(Ok(None)),
            Err(why) => return Ok(Err(why)),
        };
        if answer == Some(Answer::Theirs) {
            for tie in &sides.ties {
                tie.take_theirs(&mut sides.merged, &sides.theirs);
            }
        }
        let path = Path::new(SETTINGS);
        let from_origin = changed_settings(&sides.mine, &sides.merged);
        let (mine, theirs) = (self.mine.get(path), self.theirs.get(path));
        let content = match (mine, theirs) {
            (Some(entry), _) if sides.merged == sides.mine => Content::Entry(Some(entry.clone())),
            (_, Some(entry)) if sides.merged == sides.theirs => Content::Entry(Some(entry.clone())),
            _ => {
                let text = toml::to_string_pretty(&sides.merged)?;
                if RepoConfig::parse(&text)? != sides.merged {
                    bail!("the merged {SETTINGS} does not read back as it was merged");
                }
                let committed = [mine, theirs]
                    .into_iter()
                    .flatten()
                    .map(|entry| self.blob(entry))
                    .collect::<Result<Vec<_>>>()?;
                if let Some(why) = accept(self.ctx, SETTINGS, text.as_bytes(), &committed, false) {
                    return Ok(Err(why));
                }
                Content::Bytes {
                    bytes: text.into_bytes(),
                    mode: "100644".to_owned(),
                }
            }
        };
        Ok(Ok(Some(Merged {
            content,
            from_origin,
        })))
    }

    /// Saves, once per path, this machine's committed version of every decided path whose
    /// decision is not that version: as ciphertext for `{age}`, and as the target for a symlink,
    /// since that is what the blob holds.
    fn save_losers(&mut self) -> Result<()> {
        let losers: Vec<(PathBuf, Entry)> = self
            .decisions
            .iter()
            .filter(|(path, _)| !self.saved.contains(*path))
            .filter_map(|(path, decision)| {
                let mine = self.mine.get(path)?;
                (*decision != Decision::Set(mine.clone())).then(|| (path.clone(), mine.clone()))
            })
            .collect();
        for (path, entry) in losers {
            let text = path
                .to_str()
                .ok_or_else(|| anyhow!("{} is not UTF-8", path.display()))?;
            // The backup lands under `.dotman/repo/`, where a reserved root name is a plain path.
            validate_key(&format!("repo/{text}"))
                .with_context(|| format!("cannot save this machine's {}", shown(&path)))?;
            let bytes = self.blob(&entry)?;
            if self.backups.is_none() {
                let backups = self.ctx.paths.state_dir.join("backups");
                self.backups = Some(backup_run_dir(&backups, SystemTime::now())?);
            }
            let dir = self
                .backups
                .as_ref()
                .ok_or_else(|| anyhow!("no backup directory"))?;
            let saved = back_up_bytes(dir, &format!(".dotman/repo/{text}"), &bytes)?;
            output::info(&format!(
                "saved this machine's {} to {}",
                shown(&path),
                saved.display()
            ));
            self.saved.insert(path);
        }
        Ok(())
    }

    fn intro(&self, count: usize) -> String {
        let what = if count == 1 {
            "1 file was".to_owned()
        } else {
            format!("{count} files were")
        };
        let places = if count == 1 { "place" } else { "places" };
        format!(
            "the pull conflicts with your changes: {what} changed here and in {REMOTE}/{} in the same {places}. Nothing was pulled or pushed, and your files and commits are as they were.",
            self.branch
        )
    }

    fn report_unanswered(&self, questions: &[(PathBuf, Kind)], ties: &[Tie]) {
        let mut paths: Vec<&Path> = questions.iter().map(|(path, _)| path.as_path()).collect();
        if !ties.is_empty() {
            paths.push(Path::new(SETTINGS));
        }
        output::warning(&self.intro(paths.len()));
        list_paths(&paths);
        for tie in ties {
            output::warning(&format!(
                "{SETTINGS} sets {} {} differently here and in {REMOTE}/{}",
                tie.table,
                output::escape_line(&format!("{:?}", tie.key)),
                self.branch
            ));
        }
        let bin = name::bin();
        output::warning(&format!(
            "answer with `{bin} sync --keep-local` or `{bin} sync --take-repo`"
        ));
        output::warning(&format!(
            "--keep-local keeps this machine's side of each clash and --take-repo {REMOTE}/{}'s; in a text file the other machine's edits elsewhere in the file are kept, other files are taken whole, and this machine's version is saved first. Either flag also answers every other conflict and existing file this sync asks about",
            self.branch
        ));
    }

    /// The git steps for a shape dotman does not settle. `git add` names every stopped path as it
    /// stands after the abort. git's renamed copy of a path, such as `x~HEAD`, is named by the
    /// commit and the backend that made it, and a later stop holds paths of its own, so `git add
    /// -u` stages those however they were resolved.
    fn report_unmodelled(&self, why: &[String]) {
        let bin = name::bin();
        let mut listed: Vec<PathBuf> = Vec::new();
        let mut add: Vec<&PathBuf> = Vec::new();
        for path in &self.seen {
            let found = self.existing(path).unwrap_or_else(|| path.clone());
            if !listed.contains(&found) {
                listed.push(found);
            }
        }
        for path in &listed {
            if self.existing(path).as_ref() == Some(path) {
                add.push(path);
            }
        }
        let shown: Vec<&Path> = listed.iter().map(PathBuf::as_path).collect();
        output::warning(&self.intro(shown.len()));
        list_paths(&shown);
        for why in why {
            output::warning(&format!("{why}, which dotman cannot settle."));
        }
        output::warning("To resolve the pull with git:");
        let mut staging = Vec::new();
        if !add.is_empty() {
            staging.push(format!("{bin} git add -- {}", quoted(&add)));
        }
        staging.push(format!("{bin} git add -u"));
        print_steps(&[
            vec![format!(
                "{bin} git pull --rebase {REMOTE} {}",
                shell_words::quote(self.branch)
            )],
            vec![format!(
                "Edit the conflicting files under {}.",
                self.ctx.paths.repo.display()
            )],
            staging,
            vec![
                format!("env GIT_EDITOR=true {bin} git rebase --continue"),
                "Repeat 2 to 4 while the rebase keeps stopping.".to_owned(),
            ],
            vec![format!("{bin} sync")],
        ]);
    }

    fn report_refused(&self, path: &Path, why: &str, answer: Answer) {
        let bin = name::bin();
        output::warning(&format!("{}: {why}; nothing was taken", shown(path)));
        let source = source(path);
        let fix = if path == Path::new(SETTINGS) {
            format!("change and commit {SETTINGS} in the repo")
        } else {
            let target = source
                .strip_prefix(system::DIR)
                .and_then(|rel| rel.strip_prefix('/'))
                .map_or_else(
                    || add::pasteable(&self.ctx.paths.home, &key_of(&source)),
                    |rel| shell_words::quote(&format!("/{}", key_of(rel))).into_owned(),
                );
            format!("change this machine's copy with `{bin} edit {target}`")
        };
        output::warning(&format!(
            "{fix} so that the merge no longer does that, then run `{bin} sync {}` again",
            answer.flag()
        ));
    }
}

#[derive(Debug, Clone, Copy)]
enum Rev {
    Base,
    Mine,
    Theirs,
}

/// The one check every new text blob passes before it becomes a blob: the content guard over the
/// whole of `merged`, less what this machine's `[guard] allow` covers and less each finding
/// (the same rule and the same text) already in either side's committed blob; then, for a
/// template, the render. Whole versions skip it, having passed the guard on their own machine.
/// `None` when it is accepted; otherwise why not.
fn accept(
    ctx: &Context,
    source: &str,
    merged: &[u8],
    sides: &[Vec<u8>],
    template: bool,
) -> Option<String> {
    if let Some(found) = new_finding(merged, sides, &key_of(source), &ctx.machine.guard.allow) {
        return Some(format!("in the merged file, {found}"));
    }
    if !template {
        return None;
    }
    std::str::from_utf8(merged).map_or_else(
        |_| Some("the merged template is not valid UTF-8".to_owned()),
        |text| {
            template::render(source, text, ctx.template_vars())
                .err()
                .map(|e| format!("the merged template does not render: {e:#}"))
        },
    )
}

/// The guard's finding in `merged` that neither side's blob holds, an age key first. A rule that
/// spans lines can match across lines the two sides each kept, so the whole file is scanned.
fn new_finding(
    merged: &[u8],
    sides: &[Vec<u8>],
    key: &str,
    allow: &[crate::config::Allow],
) -> Option<secrets::Finding> {
    let committed: Vec<(secrets::Finding, &[u8])> = sides
        .iter()
        .flat_map(|side| secrets::findings_with_text(side))
        .collect();
    secrets::findings_with_text(merged)
        .into_iter()
        .filter(|(found, _)| !found.allowed(key, allow))
        .filter(|(found, text)| {
            !committed
                .iter()
                .any(|(side, side_text)| side.rule == found.rule && side_text == text)
        })
        .map(|(found, _)| found)
        .min_by_key(|found| found.overridable())
}

/// A side that changed the mode from the base's keeps its change; when both changed it, or the
/// base lacks the path, the answer picks.
fn merged_mode(base: Option<&Entry>, mine: &Entry, theirs: &Entry, answer: Answer) -> String {
    let base = base.map(|entry| entry.mode.as_str());
    if mine.mode == theirs.mode || base == Some(mine.mode.as_str()) {
        return theirs.mode.clone();
    }
    if base == Some(theirs.mode.as_str()) {
        return mine.mode.clone();
    }
    match answer {
        Answer::Mine => mine.mode.clone(),
        Answer::Theirs => theirs.mode.clone(),
    }
}

/// List items added or removed and map keys set differently, for the line that says how many
/// settings came from origin.
fn changed_settings(mine: &RepoConfig, merged: &RepoConfig) -> usize {
    let RepoConfig {
        ignore,
        guard,
        dirs,
        modes,
        hooks,
        age,
        system,
    } = mine;
    [
        list_changes(&ignore.patterns, &merged.ignore.patterns),
        list_changes(&guard.secret_names, &merged.guard.secret_names),
        list_changes(&guard.allow, &merged.guard.allow),
        list_changes(&dirs.paths, &merged.dirs.paths),
        map_changes(modes, &merged.modes),
        map_changes(hooks, &merged.hooks),
        list_changes(&age.recipients, &merged.age.recipients),
        map_changes(system, &merged.system),
    ]
    .into_iter()
    .fold(0, usize::saturating_add)
}

fn list_changes<T: PartialEq>(a: &[T], b: &[T]) -> usize {
    let gone = a.iter().filter(|item| !b.contains(item)).count();
    let added = b.iter().filter(|item| !a.contains(item)).count();
    gone.saturating_add(added)
}

fn map_changes<V: PartialEq>(a: &BTreeMap<String, V>, b: &BTreeMap<String, V>) -> usize {
    a.keys()
        .chain(b.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|key| a.get(*key) != b.get(*key))
        .count()
}

const DIRECTORY: &str = "a directory";
const MISSING: &str = "missing";

fn kind(tree: &BTreeMap<PathBuf, Entry>, path: &Path) -> &'static str {
    match tree.get(path).map(|entry| entry.mode.as_str()) {
        Some("100644" | "100755") => "a file",
        Some(SYMLINK) => "a symlink",
        Some("160000") => "a submodule",
        Some(_) => "an entry git does not check out",
        None if tree.keys().any(|key| key.starts_with(path)) => DIRECTORY,
        None => MISSING,
    }
}

/// The target key a source belongs to: the source with its variant block taken off.
fn key_of(source: &str) -> String {
    Sources::group([source])
        .keys()
        .next()
        .map_or_else(|| source.to_owned(), str::to_owned)
}

fn source(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

fn shown(path: &Path) -> String {
    output::escape_line(&path.to_string_lossy()).into_owned()
}

fn list_paths(paths: &[&Path]) {
    output::warning("conflicting paths:");
    for path in paths {
        output::warning(&format!("  {}", shown(path)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(mode: &str, oid: char) -> Entry {
        Entry {
            mode: mode.to_owned(),
            oid: oid.to_string().repeat(40),
        }
    }

    #[test]
    fn a_text_s_mode_follows_the_side_that_changed_it_and_a_clash_follows_the_answer() {
        let (plain, exec) = (entry("100644", 'a'), entry("100755", 'b'));
        for answer in [Answer::Mine, Answer::Theirs] {
            assert_eq!(merged_mode(Some(&plain), &plain, &exec, answer), "100755");
            assert_eq!(merged_mode(Some(&plain), &exec, &plain, answer), "100755");
            assert_eq!(merged_mode(None, &plain, &plain, answer), "100644");
        }
        assert_eq!(merged_mode(None, &plain, &exec, Answer::Mine), "100644");
        assert_eq!(merged_mode(None, &plain, &exec, Answer::Theirs), "100755");
    }

    #[test]
    fn a_path_is_a_directory_when_the_tree_holds_entries_below_it() {
        let tree: BTreeMap<PathBuf, Entry> = [
            (PathBuf::from("d/f"), entry("100644", 'a')),
            (PathBuf::from("dx"), entry("120000", 'b')),
            (PathBuf::from("m"), entry("160000", 'c')),
        ]
        .into();
        assert_eq!(kind(&tree, Path::new("d")), DIRECTORY);
        assert_eq!(kind(&tree, Path::new("d/f")), "a file");
        assert_eq!(kind(&tree, Path::new("dx")), "a symlink");
        assert_eq!(kind(&tree, Path::new("m")), "a submodule");
        assert_eq!(kind(&tree, Path::new("x")), MISSING);
    }

    #[test]
    fn settings_changes_count_list_items_and_map_keys() {
        let mine =
            RepoConfig::parse("[dirs]\npaths = [\"a\"]\n[modes]\n\"x\" = \"0600\"\n").unwrap();
        let merged = RepoConfig::parse(
            "[dirs]\npaths = [\"a\", \"b\"]\n[modes]\n\"x\" = \"0640\"\n\"y\" = \"0600\"\n",
        )
        .unwrap();
        assert_eq!(changed_settings(&mine, &merged), 3);
        assert_eq!(changed_settings(&mine, &mine), 0);
    }

    #[test]
    fn a_finding_already_in_a_side_is_dropped_and_one_the_merge_assembles_is_kept() {
        let token = concat!("ghp_", "abcdefghijklmnopqrstuvwxyz0123456789");
        let mine = format!("a\nx={token}\n").into_bytes();
        let theirs = b"a\nb\n".to_vec();
        let merged = format!("a\nb\nc\nx={token}\n");
        let found = new_finding(merged.as_bytes(), std::slice::from_ref(&theirs), "k", &[]);
        assert_eq!(found.map(|found| found.rule), Some("github-token"));
        let sides = [mine, theirs];
        assert_eq!(new_finding(merged.as_bytes(), &sides, "k", &[]), None);
        let allow = [crate::config::Allow {
            path: "k".to_owned(),
            rule: "github-token".to_owned(),
        }];
        assert_eq!(new_finding(merged.as_bytes(), &[], "k", &allow), None);

        let header = concat!("-----BEGIN PRIVATE", " KEY-----\n");
        let body = "QUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFB\n";
        let mine = format!("{header}Proc-Type: x\n!!!!\nComment: y\nzzz\n").into_bytes();
        let theirs = format!("{header}Proc-Type: x\n!!!!\nComment: y\n{body}").into_bytes();
        let merged = format!("{header}Proc-Type: x\nComment: y\n{body}");
        let sides = [mine, theirs];
        for side in &sides {
            assert_eq!(new_finding(side, &[], "k", &[]), None);
        }
        let found = new_finding(merged.as_bytes(), &sides, "k", &[]);
        assert_eq!(found.map(|found| found.rule), Some("private-key"));
    }
}
