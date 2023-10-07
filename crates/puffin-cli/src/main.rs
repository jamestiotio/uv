use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Args, Parser, Subcommand};
use colored::Colorize;
use directories::ProjectDirs;

use crate::commands::ExitStatus;

mod commands;
mod logging;

#[derive(Parser)]
#[command(author, version, about)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Compile a `requirements.in` file to a `requirements.txt` file.
    Compile(CompileArgs),
    /// Sync dependencies from a `requirements.txt` file.
    Sync(SyncArgs),
}

#[derive(Args)]
struct CompileArgs {
    /// Path to the `requirements.txt` file to compile.
    src: PathBuf,

    /// Avoid reading from or writing to the cache.
    #[arg(long)]
    no_cache: bool,
}

#[derive(Args)]
struct SyncArgs {
    /// Path to the `requirements.txt` file to install.
    src: PathBuf,

    /// Avoid reading from or writing to the cache.
    #[arg(long)]
    no_cache: bool,
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    let _ = logging::setup_logging();

    let dirs = ProjectDirs::from("", "", "puffin");

    let result = match &cli.command {
        Commands::Compile(args) => {
            commands::compile(
                &args.src,
                dirs.as_ref()
                    .map(ProjectDirs::cache_dir)
                    .filter(|_| !args.no_cache),
            )
            .await
        }
        Commands::Sync(args) => {
            commands::sync(
                &args.src,
                dirs.as_ref()
                    .map(ProjectDirs::cache_dir)
                    .filter(|_| !args.no_cache),
            )
            .await
        }
    };

    match result {
        Ok(code) => code.into(),
        Err(err) => {
            #[allow(clippy::print_stderr)]
            {
                eprintln!("{}", "puffin failed".red().bold());
                for cause in err.chain() {
                    eprintln!("  {} {cause}", "Cause:".bold());
                }
            }
            ExitStatus::Error.into()
        }
    }
}