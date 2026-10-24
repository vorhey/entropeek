use std::io::{self, IsTerminal};
use std::process::ExitCode;

use clap::Parser;

use entropeek::cli::{self, Cli, Command, Outcome, Runtime};
use entropeek::env::Env;
use entropeek::tui::icons::IconChoice;
use entropeek::tui::theme::ThemeChoice;
use entropeek::{actions, provider};

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            // Errors can quote paths and ids from session files.
            eprintln!("entropeek: {}", entropeek::text::sanitize_multiline(&format!("{err:#}")));
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> anyhow::Result<()> {
    let env = Env::from_system()?;
    let theme = cli
        .theme
        .or_else(|| ThemeChoice::from_env(std::env::var("ENTROPEEK_THEME").ok().as_deref()))
        .unwrap_or_default();
    let icons = cli
        .icons
        .or_else(|| IconChoice::from_env(std::env::var("ENTROPEEK_ICONS").ok().as_deref()))
        .unwrap_or_default();
    let ctx = Runtime {
        env: &env,
        providers: std::sync::Arc::new(provider::all(&env)),
        no_cache: cli.no_cache,
        theme,
        icons,
    };
    let interactive = io::stdout().is_terminal() && io::stdin().is_terminal();
    let command = match cli.command {
        Some(command) => command,
        None if interactive => Command::Tui,
        None => Command::List(cli::ListArgs::default()),
    };
    let outcome = match command {
        Command::Tui => entropeek::tui::run(&ctx)?,
        other => cli::run(other, &ctx, &mut io::stdout().lock())?,
    };
    match outcome {
        Outcome::Done => Ok(()),
        Outcome::Exec(command) => Err(actions::exec(&command)),
    }
}
