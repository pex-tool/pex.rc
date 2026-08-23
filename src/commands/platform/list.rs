// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use clap::Args;
use cli::{Json, Output};
use owo_colors::OwoColorize;
use strum::IntoEnumIterator;
use target::SimplifiedTarget;

#[derive(Args)]
#[group(skip)]
pub struct List {
    /// Output platform list in JSON.
    #[arg(long, default_value_t = false)]
    json: bool,

    #[command(flatten)]
    json_serializer: Json,

    #[command(flatten)]
    output: Output,
}

impl List {
    pub fn execute(&self) -> anyhow::Result<()> {
        self.output.configure()?;

        let mut out = self.output.writer()?;
        if self.json {
            self.json_serializer.serialize(
                &mut out,
                &SimplifiedTarget::iter()
                    .map(|target| target.as_str())
                    .collect::<Vec<_>>(),
            )?;
        } else {
            for target in SimplifiedTarget::iter() {
                anstream::println!("{}", target.as_str().blue())
            }
        }
        Ok(())
    }
}
