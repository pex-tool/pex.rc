// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use clap::{ArgAction, Args};

#[derive(Args, Debug)]
#[group(skip)]
pub struct Build {
    /// Requirements to include in the PEX.
    #[arg(value_name = "REQUIREMENT", help_heading = "Contents")]
    requirements: Vec<String>,

    /// Wheels (or directories containing wheels) to include in the PEX.
    #[arg(
        long,
        visible_alias = "wheel",
        value_name = "PATH",
        action = ArgAction::Append,
        help_heading = "Contents"
    )]
    wheels: Vec<PathBuf>,
}

impl Build {
    pub fn execute(self) -> anyhow::Result<()> {
        todo!("Creating a PEX from sources and requirements is coming soon: {self:#?}")
    }
}
