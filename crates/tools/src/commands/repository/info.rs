// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::io::Write;
use std::path::Path;

use clap::Args;
use cli::{Json, Output};
use pex::{Pex, PexPath};
use serde_json::json;

use crate::resolve::resolve;

#[derive(Args)]
pub(crate) struct InfoArgs {
    /// Print the distributions requirements in addition to its name version and path.
    #[arg(short = 'v', long, default_value_t = false)]
    verbose: bool,

    #[command(flatten)]
    json: Json,

    #[command(flatten)]
    output: Output,
}

pub(crate) fn display(python: Option<&Path>, pex: Pex, args: InfoArgs) -> anyhow::Result<()> {
    let pex_path = PexPath::from_pex_info(pex.info.raw(), true);
    let additional_pexes = pex_path.load_pexes()?;
    let (_, wheels) = resolve(python, &pex, &additional_pexes)?;

    let mut output = args.output.writer()?;
    for (project_name, wheel_info) in wheels {
        let location = pex.path.join(".deps").join(wheel_info.file_name);
        if args.verbose {
            args.json.serialize(
                &mut output,
                &json!({
                    "project_name": project_name,
                    "version": wheel_info.version,
                    "requires_python": wheel_info.requires_python,
                    "requires_dists": wheel_info.requires_dists,
                    "location": location
                }),
            )?;
        } else {
            writeln!(
                output,
                "{project_name} {version} {location}",
                version = wheel_info.version,
                location = location.display()
            )?;
        }
    }

    Ok(())
}
