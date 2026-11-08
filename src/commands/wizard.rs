//! The questions `dotman init` asks, each answered by a flag or, in an interactive run, by the user.
//!
//! Answers are gathered first and applied afterwards, so a refused flag or a failed `gh auth`
//! check stops the run before anything is created, and a dry run applies nothing.

use crate::cli::InitArgs;
use crate::commands::key::{self, Incoming};
use crate::config::Config;
use crate::context::Context;
use crate::name;
use crate::output;
use crate::proc;
use crate::prompt;
use crate::remote;
use crate::system::{self, Scope};
use anyhow::{Context as _, Result, anyhow, bail};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Output, Stdio};

const GH_INSTALL: &str = "https://cli.github.com/, or pacman -S github-cli, apt install gh, dnf install gh or brew install gh";
const KEY_PREFIXES: [&str; 2] = ["AGE-SECRET-KEY-", "AGE-PLUGIN-"];

/// What to do about `origin`.
#[derive(Debug, Default, PartialEq, Eq)]
pub enum Remote {
    /// Leave it as it is.
    #[default]
    Keep,
    /// Point `origin` at this URL.
    Set(String),
    /// Create this private GitHub repo with `gh` and point `origin` at it.
    Create(String),
}

/// Git identity to store, because git has none.
#[derive(Debug)]
pub struct GitIdentity {
    name: Option<String>,
    email: Option<String>,
    global: bool,
}

/// Everything the wizard decided, not yet applied.
#[derive(Default)]
pub struct Answers {
    identity: Option<GitIdentity>,
    remote: Remote,
    host: Option<String>,
    profiles: Option<Vec<String>>,
    roots: Vec<PathBuf>,
    create_key: bool,
    /// The age identity to import, for an existing repo.
    pub age_key: Option<Incoming>,
}

/// How much of what no flag answered to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Ask {
    /// Nothing: a non-interactive run, or a dry run.
    Nothing,
    /// Only the remote, for a repo that already exists.
    Remote,
    /// Every question.
    Everything,
}

/// One run of the questions.
pub struct Wizard<'a> {
    /// The repo and machine the answers are for.
    pub ctx: &'a Context,
    /// Where the flags are.
    pub args: &'a InitArgs,
    /// What to ask when no flag answered.
    pub ask: Ask,
}

impl Wizard<'_> {
    /// The questions for a repo that is being created.
    pub fn new_repo(&self, origin: Option<&str>) -> Result<Answers> {
        let mut answers = Answers {
            identity: self.identity()?,
            remote: self.remote(origin)?,
            ..Answers::default()
        };
        self.machine(&mut answers)?;
        answers.roots = self.roots(|| self.ask_roots())?;
        answers.create_key = self.create_key()?;
        Ok(answers)
    }

    /// The questions for a cloned repo, asking only what this machine needs.
    pub fn existing_repo(&self, age_key: Option<Incoming>) -> Result<Answers> {
        let mut answers = Answers {
            identity: self.identity()?,
            age_key,
            ..Answers::default()
        };
        self.machine(&mut answers)?;
        answers.roots = self.roots(|| self.offer_roots())?;
        if answers.age_key.is_none() && self.asks_all() {
            answers.age_key = self.ask_age_key()?;
        }
        Ok(answers)
    }

    fn asks_all(&self) -> bool {
        self.ask == Ask::Everything
    }

    /// Fails for a flag value that cannot be used, before anything is created or cloned.
    pub fn check_flags(&self) -> Result<()> {
        let args = self.args;
        args.git_name.as_deref().map(check_name).transpose()?;
        args.git_email.as_deref().map(check_email).transpose()?;
        args.host.clone().map(check_word).transpose()?;
        for profile in &args.profile {
            check_word(profile.clone())?;
        }
        if !args.system_root.is_empty() {
            self.check_roots(&args.system_root)?;
        }
        if let Some(slug) = args.create_github.as_deref().filter(|s| !valid_slug(s)) {
            bail!("`{slug}` is not OWNER/NAME");
        }
        Ok(())
    }

    fn identity(&self) -> Result<Option<GitIdentity>> {
        let args = self.args;
        let given = args.git_name.is_some() || args.git_email.is_some();
        if !self.identity_missing()? {
            if given {
                output::info(
                    "git already has an identity; --git-name, --git-email and --git-global are ignored",
                );
            }
            return Ok(None);
        }
        let mut identity = GitIdentity {
            name: args.git_name.as_deref().map(check_name).transpose()?,
            email: args.git_email.as_deref().map(check_email).transpose()?,
            global: args.git_global,
        };
        if self.asks_all() {
            if identity.name.is_none() {
                identity.name = prompt::ask_parsed("Git user.name (empty to skip):", |a| {
                    check_name(a).map_err(|e| format!("{e}"))
                })?;
            }
            if identity.email.is_none() {
                identity.email = prompt::ask_parsed("Git user.email (empty to skip):", |a| {
                    check_email(a).map_err(|e| format!("{e}"))
                })?;
            }
            let both = identity.name.is_some() && identity.email.is_some();
            if both && !identity.global {
                identity.global = prompt::ask_yes_no("Store them globally, for every repo? [y/N]")?;
            }
        }
        if identity.name.is_none() && identity.email.is_none() {
            output::info(&format!(
                "git has no user.name and user.email; pass --git-name and --git-email, or set them with `{bin} git config user.name \"Your Name\"` before `{bin} sync`",
                bin = name::bin()
            ));
            return Ok(None);
        }
        Ok(Some(identity))
    }

    fn identity_missing(&self) -> Result<bool> {
        let git = &self.ctx.git;
        if self.ctx.paths.repo.is_dir() {
            Ok(!git.identity_configured()?)
        } else {
            Ok(git.author_email()?.is_none())
        }
    }

    fn remote(&self, origin: Option<&str>) -> Result<Remote> {
        let args = self.args;
        if let Some(url) = &args.remote {
            return Ok(Remote::Set(url.clone()));
        }
        if let Some(slug) = &args.create_github {
            return if valid_slug(slug) {
                Ok(Remote::Create(slug.clone()))
            } else {
                Err(anyhow!("`{slug}` is not OWNER/NAME"))
            };
        }
        if args.no_remote || origin.is_some() || self.ask == Ask::Nothing {
            return Ok(Remote::Keep);
        }
        output::info("Where will this repo live?");
        output::info("  1. a git URL you paste");
        output::info("  2. a new private GitHub repo, made with gh");
        output::info("  3. nowhere yet");
        match prompt::ask_parsed("Choose 1-3 [3]:", |a| prompt::choice(a, 3))? {
            Some(1) => Ok(prompt::ask_parsed("Remote URL for origin:", |a| {
                usable_url(a).map(str::to_owned)
            })?
            .map_or(Remote::Keep, Remote::Set)),
            Some(2) => Self::ask_github(),
            _ => Ok(Remote::Keep),
        }
    }

    fn ask_github() -> Result<Remote> {
        let gh = match find_gh().and_then(|gh| require_auth(&gh).map(|()| gh)) {
            Ok(gh) => gh,
            Err(e) => {
                output::warning(&format!("{e:#}"));
                return Ok(Remote::Keep);
            }
        };
        let default = login(&gh).map(|user| format!("{user}/dotfiles"));
        let shown = default.as_deref().unwrap_or("OWNER/NAME");
        let question = format!("Repo to create [{shown}]:");
        let chosen = prompt::ask_parsed(&question, |a| {
            valid_slug(a)
                .then(|| a.to_owned())
                .ok_or_else(|| format!("`{a}` is not OWNER/NAME"))
        })?;
        Ok(chosen.or(default).map_or(Remote::Keep, Remote::Create))
    }

    fn machine(&self, answers: &mut Answers) -> Result<()> {
        let args = self.args;
        answers.host = args.host.clone().map(check_word).transpose()?;
        if !args.profile.is_empty() {
            answers.profiles = Some(
                args.profile
                    .iter()
                    .map(|p| check_word(p.clone()))
                    .collect::<Result<Vec<String>>>()
                    .map(dedup)?,
            );
        }
        if !self.asks_all() || (answers.host.is_some() && answers.profiles.is_some()) {
            return Ok(());
        }
        let id = self.ctx.identity();
        output::info(&format!(
            "this machine: host {}, os {}, distro {}, arch {}",
            id.host, id.os, id.distro, id.arch
        ));
        if answers.host.is_none() {
            let question = format!("Host name [{}] (empty keeps it):", id.host);
            answers.host = prompt::ask_parsed(&question, |a| {
                check_word(a.to_owned()).map_err(|e| format!("{e}"))
            })?
            .filter(|host| *host != id.host);
        }
        if answers.profiles.is_none() {
            let now = self.ctx.machine.profiles.join(",");
            let question = format!("Profiles, comma-separated [{now}] (empty keeps them):");
            answers.profiles = prompt::ask_parsed(&question, parse_profiles)?;
        }
        Ok(())
    }

    fn roots(&self, ask: impl FnOnce() -> Result<Vec<PathBuf>>) -> Result<Vec<PathBuf>> {
        let args = self.args;
        if !args.system_root.is_empty() {
            self.check_roots(&args.system_root)?;
            return Ok(args.system_root.clone());
        }
        if args.no_system || !self.asks_all() {
            return Ok(Vec::new());
        }
        ask()
    }

    fn check_roots(&self, roots: &[PathBuf]) -> Result<()> {
        let mut machine = self.ctx.machine.clone();
        machine.system.roots = roots.to_vec();
        for root in Scope::new(&self.ctx.paths, &machine).roots() {
            if let Some(why) = root.problem {
                bail!("`{}` cannot be a system root: it {why}", root.shown);
            }
        }
        Ok(())
    }

    fn ask_roots(&self) -> Result<Vec<PathBuf>> {
        if !prompt::ask_yes_no("Manage system files outside $HOME on this machine? [y/N]")? {
            return Ok(Vec::new());
        }
        let roots = prompt::ask_parsed(
            "Directories to manage, comma-separated, such as /etc (empty for none):",
            |answer| {
                let roots: Vec<PathBuf> = split_list(answer).map(PathBuf::from).collect();
                self.check_roots(&roots).map_err(|e| format!("{e:#}"))?;
                Ok(roots)
            },
        )?;
        Ok(roots.unwrap_or_default())
    }

    fn offer_roots(&self) -> Result<Vec<PathBuf>> {
        let macos = self.ctx.system_scope().macos();
        let mut offered: Vec<String> = self
            .ctx
            .managed_system()?
            .keys()
            .filter_map(|rel| system::offered_root(&system::key_of(rel), macos))
            .collect();
        offered.sort();
        offered.dedup();
        if offered.is_empty() {
            return Ok(Vec::new());
        }
        let list = offered.join(", ");
        output::info(&format!("this repo manages system files under {list}"));
        let roots: Vec<PathBuf> = offered.into_iter().map(PathBuf::from).collect();
        if !prompt::ask_yes_no(&format!("Write them on this machine? [y/N] ({list})"))? {
            return Ok(Vec::new());
        }
        self.check_roots(&roots)?;
        Ok(roots)
    }

    fn create_key(&self) -> Result<bool> {
        let args = self.args;
        if args.create_key {
            return Ok(true);
        }
        if args.no_key || !self.asks_all() {
            return Ok(false);
        }
        prompt::ask_yes_no("Create an age key to encrypt secrets? [y/N]")
    }

    fn ask_age_key(&self) -> Result<Option<Incoming>> {
        let ctx = self.ctx;
        if ctx.age().has_key_source() {
            return Ok(None);
        }
        let count = crate::secrets::encrypted_sources(ctx)?.len();
        if count == 0 {
            return Ok(None);
        }
        output::info(&format!(
            "{count} encrypted files need an age key, and this machine has none"
        ));
        prompt::ask_parsed(
            "Paste the AGE-SECRET-KEY line, give the path of the file that holds it, or leave empty to skip:",
            |answer| self.read_key(answer),
        )
    }

    fn read_key(&self, answer: &str) -> Result<Incoming, String> {
        let (text, shown) = match classify_key(answer) {
            KeyAnswer::Pasted(text) => (text, "the pasted identity".to_owned()),
            KeyAnswer::Path(path) => {
                let path = expand_home(&path, &self.ctx.paths.home);
                let text = fs::read_to_string(&path)
                    .map_err(|e| format!("cannot read {}: {e}", path.display()))?;
                (text, path.display().to_string())
            }
        };
        key::validate(self.ctx, &text, &shown).map_err(|e| format!("{e:#}"))
    }

    /// Fails for an answer that cannot be applied, before anything is created. `origin` is the
    /// current `origin`. A dry run does not run `gh` or the age CLI.
    pub fn check(&self, answers: &Answers, origin: Option<&str>, dry_run: bool) -> Result<()> {
        if let Remote::Create(slug) = &answers.remote {
            if let Some(url) = origin {
                bail!("origin is already {url}; --create-github would not replace it");
            }
            let gh = find_gh()?;
            if !dry_run {
                require_auth(&gh).with_context(|| format!("cannot create {slug}"))?;
            }
        }
        if answers.create_key && !dry_run && !self.identity_exists() {
            self.ctx.age().require_cli()?;
            self.ctx.age().keygen()?;
        }
        Ok(())
    }

    fn identity_exists(&self) -> bool {
        fs::symlink_metadata(&self.ctx.paths.age_identity).is_ok()
    }

    /// Says what applying `answers` would do.
    pub fn describe(&self, answers: &Answers) {
        let ctx = self.ctx;
        if let Some(identity) = &answers.identity {
            output::info(&format!(
                "would set {} in the {} git config",
                identity.listed(),
                identity.scope()
            ));
        }
        match &answers.remote {
            Remote::Keep => {}
            Remote::Set(url) => output::info(&format!(
                "would set origin to {}",
                crate::git::redact_credentials(url)
            )),
            Remote::Create(slug) => output::info(&format!(
                "would create the private GitHub repo {slug} with gh and set origin to it"
            )),
        }
        if self.machine_for(answers) != ctx.machine {
            output::info(&format!(
                "would write {}: {}",
                ctx.paths.config_file.display(),
                Self::changes(answers)
            ));
        }
        if answers.create_key {
            if self.identity_exists() {
                output::info(&format!(
                    "an age identity exists at {}; no new one would be made",
                    ctx.paths.age_identity.display()
                ));
            } else {
                output::info(&format!(
                    "would create an age identity at {}",
                    ctx.paths.age_identity.display()
                ));
            }
        }
        if answers.age_key.is_some() {
            output::info(&format!(
                "would import the age identity to {}",
                ctx.paths.age_identity.display()
            ));
        }
    }

    fn changes(answers: &Answers) -> String {
        let mut parts = Vec::new();
        if let Some(host) = &answers.host {
            parts.push(format!("host {host}"));
        }
        if let Some(profiles) = &answers.profiles {
            parts.push(format!("profiles [{}]", profiles.join(", ")));
        }
        if !answers.roots.is_empty() {
            let roots: Vec<String> = answers
                .roots
                .iter()
                .map(|root| root.display().to_string())
                .collect();
            parts.push(format!("system roots [{}]", roots.join(", ")));
        }
        parts.join("; ")
    }

    /// The machine config with the answers in it.
    #[must_use]
    pub fn machine_for(&self, answers: &Answers) -> Config {
        let mut machine = self.ctx.machine.clone();
        if let Some(host) = &answers.host {
            machine.identity.host = Some(host.clone());
        }
        if let Some(profiles) = &answers.profiles {
            machine.profiles.clone_from(profiles);
        }
        for root in &answers.roots {
            if !machine.system.roots.contains(root) {
                machine.system.roots.push(root.clone());
            }
        }
        machine
    }

    /// Writes the answers: the git identity, `origin`, the machine config, then the age
    /// identity. Returns a context that sees the new machine config, when it changed. The caller
    /// holds the lock and has created the repo.
    pub fn apply(&self, answers: &Answers) -> Result<Option<Context>> {
        let base = self.ctx;
        if let Some(identity) = &answers.identity {
            self.store_identity(identity)?;
        }
        match &answers.remote {
            Remote::Keep => {}
            Remote::Set(url) => set_origin(base, url)?,
            Remote::Create(slug) => {
                let url = create_private(&find_gh()?, slug)?;
                set_origin(base, &url)?;
            }
        }
        if answers.remote != Remote::Keep {
            remote::note_origin(base, false);
        }
        let machine = self.machine_for(answers);
        let updated = if machine == base.machine {
            None
        } else {
            machine.save(&base.paths.config_file, false)?;
            output::success(&format!(
                "wrote {} ({})",
                base.paths.config_file.display(),
                Self::changes(answers)
            ));
            Some(Context::new(base.paths.clone(), machine, "init"))
        };
        let current = updated.as_ref().unwrap_or(base);
        if answers.create_key {
            self.create_key_in(current)?;
        }
        if let Some(incoming) = &answers.age_key {
            key::install(current, incoming, self.args.force, false)?;
        }
        Ok(updated)
    }

    fn create_key_in(&self, ctx: &Context) -> Result<()> {
        if self.identity_exists() {
            output::info(&format!(
                "an age identity already exists at {}; no new one was made",
                ctx.paths.age_identity.display()
            ));
            return Ok(());
        }
        key::create_identity(ctx)
    }

    fn store_identity(&self, identity: &GitIdentity) -> Result<()> {
        let git = &self.ctx.git;
        for (key, value) in [
            ("user.name", &identity.name),
            ("user.email", &identity.email),
        ] {
            if let Some(value) = value {
                git.set_config(key, value, identity.global)?;
            }
        }
        output::success(&format!(
            "set {} in the {} git config",
            identity.listed(),
            identity.scope()
        ));
        if !git.identity_configured()? {
            output::info("git still lacks one of user.name and user.email");
        }
        Ok(())
    }
}

impl GitIdentity {
    const fn listed(&self) -> &'static str {
        match (&self.name, &self.email) {
            (Some(_), Some(_)) => "user.name and user.email",
            (Some(_), None) => "user.name",
            _ => "user.email",
        }
    }

    const fn scope(&self) -> &'static str {
        if self.global { "global" } else { "repo's" }
    }
}

pub(crate) fn set_origin(ctx: &Context, url: &str) -> Result<()> {
    if ctx.git.remote_url("origin")?.as_deref() != Some(url) {
        ctx.git.set_remote_url("origin", url)?;
        output::success(&format!(
            "origin is {}",
            crate::git::redact_credentials(url)
        ));
    }
    Ok(())
}

/// A URL that git would not read as an option.
pub fn usable_url(url: &str) -> Result<&str, String> {
    if url.is_empty() || url.starts_with('-') {
        Err(format!("`{url}` is not a usable remote URL"))
    } else {
        Ok(url)
    }
}

fn check_name(name: &str) -> Result<String> {
    let name = name.trim().to_owned();
    if name.is_empty() || name.contains(['\n', '<', '>']) {
        bail!("`{name}` is not a usable git name");
    }
    Ok(name)
}

fn check_email(email: &str) -> Result<String> {
    let email = email.trim().to_owned();
    if !email.contains('@') || email.contains(['\n', '<', '>', ' ']) {
        bail!("`{email}` is not an email address");
    }
    Ok(email)
}

/// A host or profile name, which conditions such as `host=laptop` must be able to name.
fn check_word(word: String) -> Result<String> {
    let valid = !word.is_empty()
        && word
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(word)
    } else {
        bail!(
            "`{word}` cannot be used in a condition such as host=laptop: use letters, digits, `_` and `-`"
        )
    }
}

fn split_list(answer: &str) -> impl Iterator<Item = &str> {
    answer
        .split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
}

fn parse_profiles(answer: &str) -> Result<Vec<String>, String> {
    split_list(answer)
        .map(|part| check_word(part.to_owned()).map_err(|e| format!("{e}")))
        .collect::<Result<Vec<_>, _>>()
        .map(dedup)
}

fn dedup(mut words: Vec<String>) -> Vec<String> {
    let mut seen = Vec::new();
    words.retain(|word| {
        let new = !seen.contains(word);
        if new {
            seen.push(word.clone());
        }
        new
    });
    words
}

/// `OWNER/NAME` as GitHub names repos.
#[must_use]
pub fn valid_slug(slug: &str) -> bool {
    let word = |part: &str| {
        !part.is_empty()
            && !part.starts_with(['-', '.'])
            && part
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'))
    };
    slug.split_once('/')
        .is_some_and(|(owner, name)| word(owner) && word(name))
}

#[derive(Debug, PartialEq, Eq)]
enum KeyAnswer {
    Pasted(String),
    Path(PathBuf),
}

fn classify_key(answer: &str) -> KeyAnswer {
    if KEY_PREFIXES.iter().any(|prefix| answer.starts_with(prefix)) {
        KeyAnswer::Pasted(format!("{answer}\n"))
    } else {
        KeyAnswer::Path(PathBuf::from(answer))
    }
}

fn expand_home(path: &Path, home: &Path) -> PathBuf {
    path.strip_prefix("~")
        .map_or_else(|_| path.to_path_buf(), |rest| home.join(rest))
}

/// Reads the identity in `path` for `--age-key-file`.
pub fn read_key_file(ctx: &Context, path: &Path, force: bool) -> Result<Incoming> {
    let text =
        fs::read_to_string(path).with_context(|| format!("cannot read {}", path.display()))?;
    let incoming = key::validate(ctx, &text, &path.display().to_string())?;
    key::check_replaceable(ctx, &incoming, force)?;
    Ok(incoming)
}

fn find_gh() -> Result<PathBuf> {
    proc::find_program("gh").ok_or_else(|| {
        anyhow!("creating a GitHub repo needs the gh CLI, which is not installed; install it: {GH_INSTALL}")
    })
}

fn gh(program: &Path, args: &[&str]) -> Result<Output> {
    proc::command(program)
        .args(args)
        .env("GH_PROMPT_DISABLED", "1")
        .stdin(Stdio::null())
        .output()
        .with_context(|| format!("cannot run {}", program.display()))
}

fn require_auth(program: &Path) -> Result<()> {
    if gh(program, &["auth", "status", "-h", "github.com"])?
        .status
        .success()
    {
        Ok(())
    } else {
        bail!(
            "gh is not logged in to github.com; run `gh auth login`, then `{} init` again",
            name::bin()
        )
    }
}

fn login(program: &Path) -> Option<String> {
    let out = gh(program, &["api", "user", "-q", ".login"]).ok()?;
    let user = String::from_utf8_lossy(&out.stdout).trim().to_owned();
    (out.status.success() && !user.is_empty()).then_some(user)
}

/// Creates the private repo and returns its ssh URL. Creating and viewing by name does not
/// depend on the local repo having commits.
fn create_private(program: &Path, slug: &str) -> Result<String> {
    let created = gh(program, &["repo", "create", slug, "--private"])?;
    if !created.status.success() {
        bail!(
            "gh repo create {slug} failed: {}",
            String::from_utf8_lossy(&created.stderr).trim()
        );
    }
    output::success(&format!("created the private GitHub repo {slug}"));
    let viewed = gh(program, &["repo", "view", slug, "--json", "sshUrl"])?;
    if !viewed.status.success() {
        bail!(
            "{slug} was created, but gh repo view failed: {}; set origin with `{} init --remote <url>`",
            String::from_utf8_lossy(&viewed.stderr).trim(),
            name::bin()
        );
    }
    ssh_url_of(&String::from_utf8_lossy(&viewed.stdout))
}

fn ssh_url_of(json: &str) -> Result<String> {
    let value: serde_json::Value =
        serde_json::from_str(json).context("gh repo view printed no JSON")?;
    let url = value
        .get("sshUrl")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| anyhow!("gh repo view printed no sshUrl"))?;
    usable_url(url).map(str::to_owned).map_err(|e| anyhow!(e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_are_owner_slash_name() {
        for good in ["me/dotfiles", "a-b/c_d.e", "Me/x"] {
            assert!(valid_slug(good), "{good}");
        }
        for bad in [
            "", "dotfiles", "me/", "/x", "a/b/c", "-me/x", "me/-x", "me/.x", "m e/x", "me/x;y",
        ] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }

    #[test]
    fn the_ssh_url_comes_from_the_json() {
        assert_eq!(
            ssh_url_of("{\"sshUrl\":\"git@github.com:me/dots.git\"}\n").unwrap(),
            "git@github.com:me/dots.git"
        );
        for bad in [
            "",
            "not json",
            "{}",
            "{\"sshUrl\":\"\"}",
            "{\"sshUrl\":\"-x\"}",
            "{\"sshUrl\":3}",
        ] {
            assert!(ssh_url_of(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn names_are_words_a_condition_can_hold() {
        assert!(check_word("laptop-2".to_owned()).is_ok());
        for bad in ["", "a b", "a.b", "a+b", "a=b", "é"] {
            assert!(check_word(bad.to_owned()).is_err(), "{bad}");
        }
        assert_eq!(
            parse_profiles(" work, home ,work,,").unwrap(),
            ["work", "home"]
        );
        assert!(parse_profiles("work, a b").is_err());
    }

    #[test]
    fn git_names_and_emails_are_checked() {
        assert!(check_name(" Ann Lee ").is_ok_and(|n| n == "Ann Lee"));
        assert!(check_name("").is_err() && check_name("a\nb").is_err());
        assert!(check_email("a@b.c").is_ok());
        assert!(check_email("nobody").is_err() && check_email("a b@c").is_err());
    }

    #[test]
    fn a_key_answer_is_a_pasted_line_or_a_path() {
        assert_eq!(
            classify_key("AGE-SECRET-KEY-1ABC"),
            KeyAnswer::Pasted("AGE-SECRET-KEY-1ABC\n".to_owned())
        );
        assert_eq!(
            classify_key("~/keys.txt"),
            KeyAnswer::Path(PathBuf::from("~/keys.txt"))
        );
        assert_eq!(
            expand_home(Path::new("~/keys.txt"), Path::new("/home/me")),
            Path::new("/home/me/keys.txt")
        );
        assert_eq!(
            expand_home(Path::new("/k.txt"), Path::new("/home/me")),
            Path::new("/k.txt")
        );
    }

    #[test]
    fn urls_that_git_would_read_as_options_are_refused() {
        assert!(usable_url("git@example.org:me/x.git").is_ok());
        assert!(usable_url("").is_err() && usable_url("--upload-pack=x").is_err());
    }
}
