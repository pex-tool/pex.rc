// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

mod extract;
mod info;

use std::path::Path;

use clap::Subcommand;
pub(crate) use extract::{ExtractArgs, extract};
pub(crate) use info::{InfoArgs, display as info};
use pex::Pex;

use crate::commands::repository;

#[derive(Subcommand)]
pub(crate) enum Repository {
    /// Print information about the distributions in a PEX file.
    Info(InfoArgs),
    /// Extract all distributions from a PEX file.
    Extract(ExtractArgs),
}

impl Repository {
    pub(crate) fn execute_command(self, python: Option<&Path>, pex: Pex) -> anyhow::Result<()> {
        match self {
            Repository::Info(args) => repository::info(python, pex, args),
            Repository::Extract(args) => repository::extract(python, pex, args),
        }
    }
}
