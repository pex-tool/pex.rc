// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::collections::HashSet;
use std::env;

use clap::{Args, CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_verbosity_flag::{ErrorLevel, Verbosity};
use color_print::cstr;
use colorchoice_clap::Color;
use pexrc::commands::{Build, Extract, Inject, Platform, Python, Script, info};

/// Pex Runtime Control.
#[derive(Parser)]
#[command(version, about, long_about = None, styles = cli::STYLES, subcommand_required = false)]
struct Cli {
    #[command(flatten)]
    verbosity: Option<Verbosity>,

    #[command(flatten)]
    color: Color,

    /// Enable experimental commands.
    #[arg(short = 'X', long, default_value_t = false)]
    experiments: bool,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
    /// Work with supported platforms.
    #[command(subcommand)]
    Platform(Platform),
    /// Work with local Python installations.
    #[command(subcommand)]
    Python(Python),
    /// Create a Windows-style Python venv console script executable.
    Script(Script),
}

#[derive(Subcommand)]
enum ExperimentalCommands {
    /// Build a PEX from source files and requirements.
    Build {
        #[command(flatten)]
        jobs: Jobs,

        #[command(flatten)]
        build: Build,
    },
}

#[derive(Args)]
struct Jobs {
    /// The maximum number of parallel jobs to use.
    #[arg(short = 'j', long, help_heading = "Parallelism")]
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
    let (cli_command, experiments_activated) =
        if env::args_os().any(|arg| arg == "-X" || arg == "--experiment") {
            let cli_command = Cli::command();
            let standard_commands = cli_command
                .get_subcommands()
                .map(|cmd| cmd.get_name())
                .collect::<HashSet<_>>();
            let cli_command = ExperimentalCommands::augment_subcommands(Cli::command())
                .mut_subcommands(|cmd| {
                    if standard_commands.contains(cmd.get_name()) {
                        cmd
                    } else {
                        let visible_name =
                            format!(cstr!("<yellow>{name}</yellow>"), name = cmd.get_name());
                        let invisible_alias = cmd.get_name().to_owned();
                        let header = cstr!("<yellow>WARNING: Experimental</yellow>");
                        let footer = cstr!(
                            "<yellow>\
                        WARNING: This command is experimental and may change or be removed going \
                        forward.\
                        </yellow>"
                        );
                        cmd.name(visible_name)
                            .alias(invisible_alias)
                            .before_help(header)
                            .before_long_help(header)
                            .after_help(footer)
                            .after_long_help(footer)
                    }
                });
            (cli_command, true)
        } else {
            (Cli::command(), false)
        };

    let matches = cli_command.clone().get_matches();
    logging::init(
        Verbosity::<ErrorLevel>::from_arg_matches(&matches)
            .ok()
            .map(|verbosity| verbosity.log_level_filter()),
    )?;
    if let Ok(color) = Color::from_arg_matches(&matches) {
        color.write_global()
    }

    match matches.subcommand() {
        Some((subcommand, _)) => {
            if experiments_activated && ExperimentalCommands::has_subcommand(subcommand) {
                match ExperimentalCommands::from_arg_matches(&matches)? {
                    ExperimentalCommands::Build { jobs, build } => {
                        jobs.configure()?;
                        build.execute()
                    }
                }
            } else {
                match Commands::from_arg_matches(&matches)? {
                    Commands::Extract { jobs, extract } => {
                        jobs.configure()?;
                        extract.execute()
                    }
                    Commands::Inject { jobs, inject } => {
                        jobs.configure()?;
                        inject.execute()
                    }
                    Commands::Info => info::display(),
                    Commands::Platform(platform) => match platform {
                        Platform::List(list) => list.execute(),
                        Platform::Python(python) => python.execute(),
                    },
                    Commands::Python(python) => match python {
                        Python::Inspect(inspect) => inspect.execute(),
                        Python::List(list) => list.execute(),
                    },
                    Commands::Script(script) => script.execute(),
                }
            }
        }
        None => {
            cli_command
                .after_help(cstr!("<red>A subcommand is required</red>"))
                .print_help()?;
            std::process::exit(1)
        }
    }
}
