// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use clap::Args;

#[derive(Args)]
pub struct Build {
    /// Requirements to include in the PEX.
    #[arg(value_name = "REQUIREMENT")]
    requirements: Vec<String>,
}

impl Build {
    pub fn execute(self) -> anyhow::Result<()> {
        todo!("Creating a PEX from sources and requirements is coming soon.")
    }
}
