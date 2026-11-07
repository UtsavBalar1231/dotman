use clap::{CommandFactory, Parser};
use clap_complete::generate;
use dotman::cli::{Cli, Commands};
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

    match cli.command {
        Commands::Completion { shell } => {
            let mut cmd = Cli::command();
            let name = cmd.get_name().to_string();
            generate(shell, &mut cmd, name, &mut io::stdout());
        }
    }
}
