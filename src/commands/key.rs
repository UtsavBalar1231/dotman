//! `dotman key`: the age identity that decrypts `{age}` files on this machine.
//!
//! Nothing here prints an identity, and replacing one always keeps a backup, because losing the
//! only identity makes every file encrypted to it unrecoverable.

use crate::cli::{KeyArgs, KeyCommand, KeyImportArgs};
use crate::commands::Outcome;
use crate::context::{Context, TEMP_PREFIX};
use crate::engine::write::{back_up_bytes, backup_run_dir};
use crate::name;
use crate::output;
use crate::paths::{create_private_dir, private_temp_dir, write_atomic};
use crate::prompt;
use crate::secrets;
use anyhow::{Context as _, Result, anyhow, bail};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, IsTerminal, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// Runs `dotman key`.
///
/// # Errors
///
/// Fails when the repo is not initialised, the lock is held, `init` finds an age identity already
/// there whose recipient is listed or cannot be derived, or `import` cannot read the key file or
/// finds it invalid.
pub fn run(args: &KeyArgs) -> Result<Outcome> {
    let ctx = Context::resolve("key")?;
    match &args.command {
        KeyCommand::Init => init(&ctx),
        KeyCommand::Import(import_args) => import(&ctx, import_args),
    }
}

fn init(ctx: &Context) -> Result<Outcome> {
    let _lock = ctx.lock(false)?;
    ctx.repo_id()?;
    if fs::symlink_metadata(&ctx.paths.age_identity).is_ok() {
        return list_existing(ctx);
    }
    create_identity(ctx)?;
    Ok(Outcome::Done)
}

fn already_exists(identity: &Path) -> anyhow::Error {
    anyhow!(
        "an age identity already exists at {}; dotman never overwrites one. `{} key import --force <file>` replaces it after a backup",
        identity.display(),
        name::bin()
    )
}

/// Lists the recipient of the identity already in place, as one left by a `key init` that stopped
/// before it listed it. Fails as before when its recipient is listed or cannot be derived. A file
/// that holds no usable identity, as an older `key init` killed mid-write left, is backed up and
/// replaced by a new identity.
fn list_existing(ctx: &Context) -> Result<Outcome> {
    let identity = &ctx.paths.age_identity;
    let text = existing(identity)?.ok_or_else(|| already_exists(identity))?;
    // Without keygen nothing can tell a damaged identity from a good one.
    ctx.age().keygen()?;
    let recipients = match usable(ctx, &text) {
        Ok(Some(recipients)) => recipients,
        Ok(None) => return Err(already_exists(identity)),
        Err(why) => {
            let backup = back_up_identity(ctx, &text)?;
            output::warning(&format!(
                "{why}, so it holds no usable age identity; backed up to {}, and a new identity replaces it",
                backup.display()
            ));
            generate_identity(ctx)?;
            return Ok(Outcome::Done);
        }
    };
    let added = add_recipients(ctx, &recipients)?;
    if added.is_empty() {
        return Err(already_exists(identity));
    }
    output::success(&format!(
        "an age identity is already at {}; added its recipient {} to [age] recipients in .dotman.toml",
        identity.display(),
        added.join(", ")
    ));
    advise_backup();
    Ok(Outcome::Done)
}

fn advise_backup() {
    output::info(&format!(
        "Back the identity up now: keep it in a password manager plus one offline copy, because without it the encrypted files cannot be recovered. On a new machine, supply it with DOTMAN_AGE_KEY or `{} key import`.",
        name::bin()
    ));
}

/// The recipients of the identity file's `text`, `None` for a plugin identity, whose recipient
/// cannot be derived here, or why it holds no usable identity.
fn usable(ctx: &Context, text: &[u8]) -> Result<Option<Vec<String>>, String> {
    let shown = ctx.paths.age_identity.display().to_string();
    match std::str::from_utf8(text) {
        Ok(text) if text.trim().is_empty() => Err(format!("{shown} is empty")),
        Ok(text) => validate(ctx, text, &shown)
            .map(|incoming| incoming.recipients)
            .map_err(|e| format!("{e:#}")),
        Err(_) => Err(format!("{shown} is not text")),
    }
}

/// Generates the identity and lists its recipient. The caller holds the lock and has checked
/// that the repo exists.
pub(crate) fn create_identity(ctx: &Context) -> Result<()> {
    let identity = &ctx.paths.age_identity;
    if fs::symlink_metadata(identity).is_ok() {
        return Err(already_exists(identity));
    }
    generate_identity(ctx)
}

/// Generates an identity in a 0700 temp directory beside the identity file and renames it into
/// place at 0600, over a file already there, then lists its recipient. A kill leaves at most the
/// temp directory, which a later run sweeps, never a partial identity where the key belongs.
fn generate_identity(ctx: &Context) -> Result<()> {
    let identity = &ctx.paths.age_identity;
    let dir = identity
        .parent()
        .ok_or_else(|| anyhow!("{} has no parent directory", identity.display()))?;
    create_private_dir(dir, false).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = private_temp_dir(&format!("{TEMP_PREFIX}{}-", std::process::id()), dir)?;
    let staged = tmp.path().join("identity");
    ctx.age().generate(&staged)?;
    let mut file = open_regular(&staged)?;
    // keygen creates the file at 0600 itself; this covers one that does not.
    file.set_permissions(Permissions::from_mode(0o600))?;
    let mut text = Vec::new();
    file.read_to_end(&mut text)?;
    file.sync_all()?;
    let recipients = ctx.age().recipients_of(&text)?;
    fs::rename(&staged, identity)
        .with_context(|| format!("cannot move the new identity to {}", identity.display()))?;
    File::open(dir)
        .and_then(|d| d.sync_all())
        .with_context(|| format!("cannot fsync {}", dir.display()))?;
    let added = add_recipients(ctx, &recipients)?;
    output::success(&format!(
        "created an age identity at {}",
        identity.display()
    ));
    for recipient in &recipients {
        let note = if added.contains(recipient) {
            ", added to [age] recipients in .dotman.toml"
        } else {
            ""
        };
        output::info(&format!("  recipient {recipient}{note}"));
    }
    advise_backup();
    Ok(())
}

fn import(ctx: &Context, args: &KeyImportArgs) -> Result<Outcome> {
    let from_stdin = args.file == Path::new("-");
    let shown = if from_stdin {
        "standard input".to_owned()
    } else {
        args.file.display().to_string()
    };
    let text =
        read_input(&args.file, from_stdin).with_context(|| format!("cannot read {shown}"))?;
    let _lock = ctx.lock(false)?;
    ctx.repo_id()?;
    let incoming = validate(ctx, &text, &shown)?;
    install(ctx, &incoming, args.force, args.add_recipient)
}

/// Identity text that holds an age identity and whose recipients were derived.
pub struct Incoming {
    text: String,
    recipients: Option<Vec<String>>,
}

/// Derives the recipients before anything is written, so a damaged key is refused and nothing
/// changes. `shown` names the source in messages.
pub(crate) fn validate(ctx: &Context, text: &str, shown: &str) -> Result<Incoming> {
    let starts = |prefix: &str| text.lines().any(|line| line.trim().starts_with(prefix));
    let (native, plugin) = (starts("AGE-SECRET-KEY-1"), starts("AGE-PLUGIN-"));
    if !native && !plugin {
        bail!("{shown} holds no age identity: no AGE-SECRET-KEY-1 or AGE-PLUGIN- line");
    }
    let recipients = match ctx.age().recipients_of(text.as_bytes()) {
        Ok(recipients) => Some(recipients),
        Err(e) if native => return Err(e.context(format!("{shown} is not a usable age identity"))),
        Err(_) => None,
    };
    Ok(Incoming {
        text: text.to_owned(),
        recipients,
    })
}

fn replace_refused(identity: &Path) -> anyhow::Error {
    anyhow!(
        "a different age identity is already at {}; replacing it loses every file encrypted only to it. --force replaces it after a backup",
        identity.display()
    )
}

/// Fails when installing `incoming` would replace a different identity without `force`.
pub(crate) fn check_replaceable(ctx: &Context, incoming: &Incoming, force: bool) -> Result<()> {
    let identity = &ctx.paths.age_identity;
    match existing(identity)? {
        Some(old) if old != incoming.text.as_bytes() && !force => Err(replace_refused(identity)),
        _ => Ok(()),
    }
}

/// Copies the identity into place at 0600, backing up a different one that `force` replaces. The
/// caller holds the lock.
pub(crate) fn install(
    ctx: &Context,
    incoming: &Incoming,
    force: bool,
    add_recipient: bool,
) -> Result<Outcome> {
    let Incoming { text, recipients } = incoming;
    let identity = &ctx.paths.age_identity;
    let write = match existing(identity)? {
        None => true,
        Some(old) if old == text.as_bytes() => {
            output::info(&format!(
                "{} already holds this identity",
                identity.display()
            ));
            false
        }
        Some(_) if !force => return Err(replace_refused(identity)),
        Some(old) => {
            let backup = back_up_identity(ctx, &old)?;
            output::info(&format!(
                "backed up the old identity to {}",
                backup.display()
            ));
            true
        }
    };
    if write {
        if let Some(dir) = identity.parent() {
            create_private_dir(dir, false)
                .with_context(|| format!("cannot create {}", dir.display()))?;
        }
        write_atomic(identity, text.as_bytes(), 0o600)?;
        output::success(&format!("imported the identity to {}", identity.display()));
    }
    check_recipients(ctx, recipients.clone(), add_recipient)
}

fn back_up_identity(ctx: &Context, old: &[u8]) -> Result<PathBuf> {
    let run = backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?;
    let name = ctx.paths.age_identity.file_name().map_or_else(
        || "age-identity.txt".to_owned(),
        |name| name.to_string_lossy().into_owned(),
    );
    // Under `.dotman/`, a reserved name, so no key's backup can land on it.
    back_up_bytes(&run, &format!(".dotman/{name}"), old)
}

fn check_recipients(ctx: &Context, recipients: Option<Vec<String>>, add: bool) -> Result<Outcome> {
    let Some(recipients) = recipients else {
        output::warning(
            "the recipient of a plugin identity cannot be derived here; check [age] recipients in .dotman.toml yourself",
        );
        return Ok(Outcome::Attention);
    };
    let listed = secrets::recipient_set(&ctx.repo_config()?.age.recipients);
    let missing: Vec<String> = recipients
        .into_iter()
        .filter(|recipient| !listed.contains(recipient))
        .collect();
    if missing.is_empty() {
        output::info("its recipient is in [age] recipients");
        return Ok(Outcome::Done);
    }
    let named = missing.join(", ");
    let question = format!("add {named} to [age] recipients in .dotman.toml? [y/N]");
    if add || (prompt::interactive() && prompt::confirm(&question)?) {
        add_recipients(ctx, &missing)?;
        output::success(&format!(
            "added {named} to [age] recipients; the next `{} sync` on a machine whose key reads the encrypted files re-encrypts them",
            name::bin()
        ));
        return Ok(Outcome::Done);
    }
    output::warning(&format!(
        "the recipient {named} of this identity is not in [age] recipients in .dotman.toml, so no file is encrypted to it; `{} key import --add-recipient` adds it",
        name::bin()
    ));
    Ok(Outcome::Attention)
}

/// Appends the recipients not yet listed and returns them.
fn add_recipients(ctx: &Context, recipients: &[String]) -> Result<Vec<String>> {
    let mut repo_config = ctx.repo_config()?;
    let listed = secrets::recipient_set(&repo_config.age.recipients);
    let added: Vec<String> = recipients
        .iter()
        .filter(|recipient| !listed.contains(recipient))
        .cloned()
        .collect();
    if !added.is_empty() {
        repo_config.age.recipients.extend(added.iter().cloned());
        repo_config.save(&ctx.repo_config_file(), false)?;
    }
    Ok(added)
}

fn read_input(file: &Path, from_stdin: bool) -> Result<String> {
    let mut text = String::new();
    if from_stdin {
        if io::stdin().is_terminal() {
            output::context("paste the identity, then press Ctrl-D");
        }
        io::stdin().read_to_string(&mut text)?;
    } else {
        text = fs::read_to_string(file)?;
    }
    Ok(text)
}

/// The identity at `path`, or `None` when there is none. Anything but a regular file is refused,
/// so a symlink there is never replaced or followed.
fn existing(path: &Path) -> Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e).with_context(|| format!("cannot stat {}", path.display())),
        Ok(meta) if meta.is_file() => {
            let mut bytes = Vec::new();
            open_regular(path)?.read_to_end(&mut bytes)?;
            Ok(Some(bytes))
        }
        Ok(_) => bail!(
            "{} is not a regular file; dotman does not replace it",
            path.display()
        ),
    }
}

fn open_regular(path: &Path) -> Result<File> {
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .with_context(|| format!("cannot read {}", path.display()))?;
    if !file.metadata()?.is_file() {
        bail!("{} is not a regular file", path.display());
    }
    Ok(file)
}
