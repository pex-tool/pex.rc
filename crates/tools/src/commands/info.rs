// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use clap::Args;
use cli::{Json, Output};
use pex::Pex;
use tracing::instrument;

#[derive(Args)]
pub(crate) struct InfoArgs {
    #[command(flatten)]
    json: Json,

    #[command(flatten)]
    output: Output,
}

#[instrument(level = "debug", skip_all)]
pub(crate) fn display(pex: Pex, args: InfoArgs) -> anyhow::Result<()> {
    args.output.configure()?;

    args.json.serialize(args.output.writer()?, &pex.info.raw())
}
