use clap::{CommandFactory, Parser};
use clap_complete::generate;
use dotman::cli::{Cli, Commands};
use dotman::commands::{
    Outcome, add, apply, diff, doctor, edit, forget, git, import, init, key, list, log, restore,
    status, sync,
};
use dotman::context::InProgress;
use dotman::output::{self, Verbosity};
use dotman::{paths, sys};
use std::io;
use tracing_subscriber::EnvFilter;

fn main() {
    #[cfg(unix)]
    dotman::sys::restore_sigpipe();

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .compact()
        .init();

    let cli = Cli::parse();

    output::set_verbosity(if cli.quiet {
        Verbosity::Quiet
    } else if cli.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    });

    if let Err(e) = paths::ensure_not_root(
        sys::geteuid(),
        cli.allow_root,
        std::env::var_os("DOTMAN_ALLOW_ROOT").as_deref(),
    ) {
        output::error(&format!("{e:#}"));
        std::process::exit(1);
    }

    let done = |result: anyhow::Result<()>| result.map(|()| Outcome::Done);
    let result = match cli.command {
        Commands::Completion { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            generate(shell, &mut cmd, name, &mut io::stdout());
            Ok(Outcome::Done)
        }
        Commands::Init(args) => init::run(&args),
        Commands::Add(args) => done(add::run(&args)),
        Commands::Forget(args) => done(forget::run(&args)),
        Commands::List(args) => done(list::run(&args)),
        Commands::Status(args) => done(status::run(&args)),
        Commands::Apply(args) => apply::run(&args),
        Commands::Diff(args) => done(diff::run(&args, cli.no_pager)),
        Commands::Edit(args) => edit::run(&args),
        Commands::Sync(args) => sync::run(&args),
        Commands::Log(args) => done(log::run(&args)),
        Commands::Restore(args) => done(restore::run(&args)),
        Commands::Import(args) => import::run(&args),
        Commands::Doctor(args) => doctor::run(&args).map(|healthy| {
            if !healthy {
                std::process::exit(1);
            }
            Outcome::Done
        }),
        Commands::Key(args) => key::run(&args),
        Commands::Git(args) => git::run(&args).map(|code| -> Outcome { std::process::exit(code) }),
    };
    match result {
        Ok(Outcome::Done) => {}
        Ok(Outcome::Attention) => std::process::exit(3),
        Ok(Outcome::Conflict) => std::process::exit(4),
        Err(e) => {
            output::error(&format!("{e:#}"));
            std::process::exit(if e.is::<InProgress>() { 4 } else { 1 });
        }
    }
}
