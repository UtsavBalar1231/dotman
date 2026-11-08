//! `dotman key`: the age identity that decrypts `{age}` files on this machine.
//!
//! Nothing here prints an identity, and replacing one always keeps a backup, because losing the
//! only identity makes every file encrypted to it unrecoverable.

use crate::cli::{KeyArgs, KeyCommand, KeyImportArgs};
use crate::commands::Outcome;
use crate::context::Context;
use crate::engine::write::{back_up_bytes, backup_run_dir};
use crate::lock::Lock;
use crate::name;
use crate::output;
use crate::paths::{create_private_dir, write_atomic};
use crate::prompt;
use crate::secrets;
use anyhow::{Context as _, Result, anyhow, bail};
use std::fs::{self, File, OpenOptions, Permissions};
use std::io::{self, IsTerminal, Read};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::Path;
use std::time::SystemTime;

/// Runs `dotman key`.
pub fn run(args: &KeyArgs) -> Result<Outcome> {
    let ctx = Context::resolve("key")?;
    match &args.command {
        KeyCommand::Init => init(&ctx),
        KeyCommand::Import(import_args) => import(&ctx, import_args),
    }
}

fn init(ctx: &Context) -> Result<Outcome> {
    let _lock = Lock::acquire(&ctx.paths.lock_file(), false)?;
    ctx.guard()?;
    ctx.repo_id()?;
    create_identity(ctx)?;
    Ok(Outcome::Done)
}

/// Generates the identity and lists its recipient. The caller holds the lock and has checked
/// that the repo exists.
pub(crate) fn create_identity(ctx: &Context) -> Result<()> {
    let identity = &ctx.paths.age_identity;
    if fs::symlink_metadata(identity).is_ok() {
        bail!(
            "an age identity already exists at {}; dotman never overwrites one. `{} key import --force <file>` replaces it after a backup",
            identity.display(),
            name::bin()
        );
    }
    if let Some(dir) = identity.parent() {
        create_private_dir(dir, false)
            .with_context(|| format!("cannot create {}", dir.display()))?;
    }
    ctx.age().generate(identity)?;
    let mut file = open_regular(identity)?;
    // keygen creates the file at 0600 itself; this covers one that does not.
    file.set_permissions(Permissions::from_mode(0o600))?;
    let mut text = Vec::new();
    file.read_to_end(&mut text)?;
    let recipients = ctx.age().recipients_of(&text)?;
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
    output::info(&format!(
        "Back the identity up now: keep it in a password manager plus one offline copy, because without it the encrypted files cannot be recovered. On a new machine, supply it with DOTMAN_AGE_KEY or `{} key import`.",
        name::bin()
    ));
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
    let _lock = Lock::acquire(&ctx.paths.lock_file(), false)?;
    ctx.guard()?;
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
            let run = backup_run_dir(&ctx.paths.state_dir.join("backups"), SystemTime::now())?;
            let name = identity.file_name().map_or_else(
                || "age-identity.txt".to_owned(),
                |name| name.to_string_lossy().into_owned(),
            );
            // Under `.dotman/`, a reserved name, so no key's backup can land on it.
            let backup = back_up_bytes(&run, &format!(".dotman/{name}"), &old)?;
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
            output::info("paste the identity, then press Ctrl-D");
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
