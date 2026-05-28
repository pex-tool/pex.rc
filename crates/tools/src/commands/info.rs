// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use clap::Args;
use cli::{Json, Output};
use logging_timer::time;
use pex::Pex;

#[derive(Args)]
pub(crate) struct InfoArgs {
    #[command(flatten)]
    json: Json,

    #[command(flatten)]
    output: Output,
}

#[time("debug", "{}")]
pub(crate) fn display(pex: Pex, args: InfoArgs) -> anyhow::Result<()> {
    args.json.serialize(args.output.writer()?, &pex.info.raw())
}
