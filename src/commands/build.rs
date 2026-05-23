// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use clap::Args;

#[derive(Args)]
pub struct BuildArgs {
    /// Requirements to include in the PEX.
    #[arg(value_name = "REQUIREMENT")]
    requirements: Vec<String>,
}

pub fn create_pex(_build_args: BuildArgs) -> anyhow::Result<()> {
    todo!("Creating a PEX from sources and requirements is coming soon.")
}
