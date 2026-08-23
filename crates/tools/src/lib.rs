// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

mod commands;
mod resolve;

use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use logging_timer::time;
use pex::Pex;

use crate::commands::graph::GraphArgs;
use crate::commands::info::InfoArgs;
use crate::commands::interpreter::InterpreterArgs;
use crate::commands::platforms::PlatformsArgs;
use crate::commands::repository::Repository;
use crate::commands::venv::VenvArgs;
use crate::commands::{extract, graph, info, interpreter, platforms, venv};

/// Pex Tools.
#[derive(Parser)]
#[command(version, about, long_about = None, bin_name = "PEX_TOOLS=1 {PEX}", styles = cli::STYLES)]
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
    /// Extract the PEX to a directory.
    Extract {
        /// The directory to extract the PEX into.
        #[arg()]
        dest_dir: PathBuf,
    },
    /// Dumps the PEX-INFO JSON contained in a PEX.
    Info(InfoArgs),
    /// Prints the path of the preferred interpreter to run the given PEX with, if any.
    Interpreter(InterpreterArgs),
    /// Generates a dot graph of the dependencies contained in a PEX.
    Graph(GraphArgs),
    /// List the platforms this PEX can run on.
    Platforms(PlatformsArgs),
    /// Interact with the Python distribution repository contained in a PEX.
    #[command(subcommand)]
    Repository(Repository),
    /// Creates a venv from the PEX.
    Venv(VenvArgs),
}

#[time("debug", "{}")]
fn parse_cli(pex: &Path, argv: Vec<String>) -> anyhow::Result<Cli> {
    let cli = Cli::parse_from(
        [pex.to_string_lossy().into_owned()]
            .iter()
            .chain(argv.iter()),
    );
    logging::init(cli.verbosity.map(|verbosity| verbosity.log_level_filter()))?;
    cli.color.write_global();
    Ok(cli)
}

#[time("debug", "{}")]
pub fn main(python: Option<&Path>, pex: &Path, argv: Vec<String>) -> anyhow::Result<()> {
    let cli = parse_cli(pex, argv)?;
    match cli.command {
        Commands::Extract { dest_dir } => extract::unzip(pex, &dest_dir),
        Commands::Graph(args) => graph::create(python, Pex::load(pex)?, args),
        Commands::Info(args) => info::display(Pex::load(pex)?, args),
        Commands::Interpreter(args) => interpreter::display(python, Pex::load(pex)?, args),
        Commands::Platforms(args) => platforms::list(Pex::load(pex)?, args),
        Commands::Repository(repository) => repository.execute_command(python, Pex::load(pex)?),
        Commands::Venv(args) => venv::create(python, Pex::load(pex)?, args),
    }
}
