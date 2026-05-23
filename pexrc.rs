// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use clap::{Args, Parser, Subcommand};
use pexrc::commands::build::BuildArgs;
use pexrc::commands::extract::ExtractArgs;
use pexrc::commands::inject::InjectArgs;
use pexrc::commands::script::ScriptArgs;
use pexrc::commands::{build, extract, info, inject, script};

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
        build_args: BuildArgs,
    },
    /// Extract PEX dependencies as wheels.
    Extract {
        #[command(flatten)]
        jobs: Jobs,

        #[command(flatten)]
        extract_args: ExtractArgs,
    },
    /// Inject traditional PEXes with native runtimes.
    Inject {
        #[command(flatten)]
        jobs: Jobs,

        #[command(flatten)]
        inject_args: InjectArgs,
    },
    /// Provide information about the supported target runtimes.
    Info,
    /// Create a Windows-style Python venv console script executable.
    Script(ScriptArgs),
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
        Commands::Build { jobs, build_args } => {
            jobs.configure()?;
            build::create_pex(build_args)
        }
        Commands::Extract { jobs, extract_args } => {
            jobs.configure()?;
            extract::to_dir(extract_args)
        }
        Commands::Inject { jobs, inject_args } => {
            jobs.configure()?;
            inject::inject_all(inject_args)
        }
        Commands::Info => info::display(),
        Commands::Script(script_args) => script::create(script_args),
    }
}
