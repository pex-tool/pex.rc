// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::collections::{HashMap, HashSet};
use std::env;

use anyhow::bail;
use clap::{ArgMatches, Args, CommandFactory, FromArgMatches, Parser, Subcommand};
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

    #[arg(
        short = 'X',
        long,
        default_value_t = false,
        help = cstr!(
            "Enable experimental commands (displayed in dim yellow; e.g.: \
            <dim><y>example-cmd</y></dim>)"
        )
    )]
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

impl Commands {
    fn execute(self) -> anyhow::Result<()> {
        match self {
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

impl ExperimentalCommands {
    fn from_subcommand_matches(subcommand: &str, arg_matches: &ArgMatches) -> anyhow::Result<Self> {
        match subcommand {
            "build" => Ok(ExperimentalCommands::Build {
                jobs: Jobs::from_arg_matches(arg_matches)?,
                build: Build::from_arg_matches(arg_matches)?,
            }),
            _ => bail!("The subcommand {subcommand} is not a registered experimental subcommand."),
        }
    }

    fn execute(self) -> anyhow::Result<()> {
        match self {
            ExperimentalCommands::Build { jobs, build } => {
                jobs.configure()?;
                build.execute()
            }
        }
    }
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
    let (mut cli_command, experimental_commands) =
        if env::args_os().any(|arg| arg == "-X" || arg == "--experiment") {
            let cli_command = Cli::command();
            let standard_commands = cli_command
                .get_subcommands()
                .map(|cmd| cmd.get_name())
                .collect::<HashSet<_>>();
            let mut experimental_commands = HashMap::new();
            let cli_command = ExperimentalCommands::augment_subcommands(Cli::command())
                .mut_subcommands(|cmd| {
                    if standard_commands.contains(cmd.get_name()) {
                        cmd
                    } else {
                        let header = cstr!("<y!>WARNING: Experimental</y!>");
                        let footer = cstr!(
                            "<y!>\
                            WARNING: This command is experimental and may change or be removed \
                            going forward.\
                            </y!>"
                        );
                        let original_name = cmd.get_name().to_owned();
                        let name = format!(
                            cstr!("<dim><y>{original_name}</y></dim>"),
                            original_name = original_name
                        );
                        experimental_commands.insert(name.clone(), original_name.clone());
                        cmd.name(name)
                            .alias(original_name)
                            .before_help(header)
                            .before_long_help(header)
                            .after_help(footer)
                            .after_long_help(footer)
                    }
                });
            (cli_command, Some(experimental_commands))
        } else {
            (Cli::command(), None)
        };

    let matches = cli_command.get_matches_mut();
    logging::init(
        Verbosity::<ErrorLevel>::from_arg_matches(&matches)
            .ok()
            .map(|verbosity| verbosity.log_level_filter()),
    )?;
    if let Ok(color) = Color::from_arg_matches(&matches) {
        color.write_global()
    }

    match matches.subcommand() {
        Some((subcommand, arg_matches)) => {
            if let Some(experimental_commands) = experimental_commands
                && let Some(subcommand) = experimental_commands.get(subcommand)
            {
                ExperimentalCommands::from_subcommand_matches(subcommand, arg_matches)?.execute()
            } else {
                Commands::from_arg_matches(&matches)?.execute()
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
