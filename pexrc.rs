// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use clap::{Args, Parser, Subcommand};
use pexrc::commands::build::Build;
use pexrc::commands::extract::Extract;
use pexrc::commands::info;
use pexrc::commands::inject::Inject;
use pexrc::commands::script::Script;

/// Pex Runtime Control.
#[derive(Parser)]
#[command(version, about, long_about = None, styles = cli::STYLES)]
struct Cli {
    #[command(flatten)]
    verbosity: Option<clap_verbosity_flag::Verbosity>,

    #[command(flatten)]
    color: colorchoice_clap::Color,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build a PEX from source files and requirements.
    Build {
        #[command(flatten)]
        jobs: Jobs,

        #[command(flatten)]
        build: Build,
    },
    /// Extract PEX dependencies as wheels.
    Extract {
        #[command(flatten)]
        jobs: Jobs,

        #[command(flatten)]
        extract: Extract,
    },
    /// Inject traditional PEXes with native runtimes.
    Inject {
        #[command(flatten)]
        jobs: Jobs,

        #[command(flatten)]
        inject: Inject,
    },
    /// Provide information about the supported target runtimes.
    Info,
    /// Create a Windows-style Python venv console script executable.
    Script(Script),
}

#[derive(Args)]
struct Jobs {
    /// The maximum number of parallel jobs to use.
    #[arg(short = 'j', long)]
    jobs: Option<usize>,
}

impl Jobs {
    fn configure(&self) -> anyhow::Result<()> {
        if let Some(jobs) = self.jobs {
            rayon::ThreadPoolBuilder::default()
                .num_threads(jobs)
                .build_global()?;
        }
        Ok(())
    }
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init(cli.verbosity.map(|verbosity| verbosity.log_level_filter()))?;
    cli.color.write_global();

    match cli.command {
        Commands::Build { jobs, build } => {
            jobs.configure()?;
            build.execute()
        }
        Commands::Extract { jobs, extract } => {
            jobs.configure()?;
            extract.execute()
        }
        Commands::Inject { jobs, inject } => {
            jobs.configure()?;
            inject.execute()
        }
        Commands::Info => info::display(),
        Commands::Script(script) => script.execute(),
    }
}
