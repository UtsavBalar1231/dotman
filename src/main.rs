use clap::{CommandFactory, FromArgMatches};
use clap_complete::generate;
use dotman::cli::{Cli, Commands};
use dotman::commands::{
    Outcome, add, apply, diff, doctor, edit, forget, git, import, init, key, list, log, restore,
    status, sync,
};
use dotman::context::InProgress;
use dotman::output::{self, Verbosity};
use dotman::{name, paths, sys, system};
use std::io;
use tracing_subscriber::EnvFilter;

fn main() {
    #[cfg(unix)]
    dotman::sys::restore_sigpipe();

    name::init(std::env::args_os().next().as_deref());
    let cli = Cli::from_arg_matches(&Cli::command().bin_name(name::bin()).get_matches())
        .unwrap_or_else(|e| e.exit());

    output::set_verbosity(if cli.quiet {
        Verbosity::Quiet
    } else if cli.verbose {
        Verbosity::Verbose
    } else {
        Verbosity::Normal
    });

    // The helper is the one command meant to run as root, under sudo, doas or run0.
    let helper = matches!(cli.command, Commands::SystemWrite(_));
    let json = match &cli.command {
        Commands::Status(args) => args.json,
        Commands::Apply(args) => args.json,
        Commands::Sync(args) => args.json,
        Commands::Add(args) => args.json,
        _ => false,
    };
    let tracing = tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")),
        )
        .with_target(false)
        .compact();
    if json {
        tracing.with_writer(std::io::stderr).init();
    } else {
        tracing.init();
    }
    if let Err(e) = paths::ensure_not_root(
        sys::geteuid(),
        cli.allow_root || helper,
        std::env::var_os("DOTMAN_ALLOW_ROOT").as_deref(),
    ) {
        output::error(&format!("{e:#}"));
        if json && let Err(error) = output::json::finish(1, Some("root_refused")) {
            output::error(&format!("{error:#}"));
        }
        std::process::exit(1);
    }

    let done = |result: anyhow::Result<()>| result.map(|()| Outcome::Done);
    let result = match cli.command {
        Commands::Completion { shell } => {
            let mut cmd = Cli::command();
            generate(shell, &mut cmd, name::bin(), &mut io::stdout());
            Ok(Outcome::Done)
        }
        Commands::Init(args) => init::run(&args),
        Commands::Add(args) => add::run(&args),
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
        Commands::SystemWrite(args) => done(system::helper::run(&args.plan)),
    };
    let (exit, kind, recovery) = match result {
        Ok(Outcome::Done) => (0, None, None),
        Ok(Outcome::Attention) => (3, None, None),
        Ok(Outcome::Conflict) => (4, Some("pull_conflict"), None),
        Err(e) => {
            if !e.is::<output::json::PullConflict>() {
                output::error(&format!("{e:#}"));
            }
            match (
                e.downcast_ref::<InProgress>(),
                e.downcast_ref::<output::json::PullConflict>(),
            ) {
                (Some(stopped), _) => (4, Some("in_progress"), Some(stopped.recovery())),
                (_, Some(stopped)) => (4, Some("pull_conflict"), Some(stopped.recovery.clone())),
                _ if e.is::<dotman::lock::Held>() => (1, Some("lock_held"), None),
                _ if e.is::<dotman::context::ReviewChanged>() => (3, None, None),
                _ if e.is::<sync::MassDelete>() => (1, Some("mass_delete"), None),
                _ => (1, Some("other"), None),
            }
        }
    };
    if json && let Err(e) = output::json::finish_with_recovery(exit, kind, recovery.as_deref()) {
        output::error(&format!("{e:#}"));
        std::process::exit(1);
    }
    if exit != 0 {
        std::process::exit(exit);
    }
}
