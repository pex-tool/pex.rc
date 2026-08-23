// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::io::Write;

use clap::Args;
use cli::{Json, Output};
use fs_err::File;
use pex::{Layout, Pex};
use target::SimplifiedTarget;
use zip::ZipArchive;

#[derive(Args)]
#[group(skip)]
pub struct PlatformsArgs {
    /// Output platform list in JSON.
    #[arg(long, default_value_t = false)]
    json: bool,

    #[command(flatten)]
    json_serializer: Json,

    #[command(flatten)]
    output: Output,
}

pub(crate) fn list(pex: Pex, args: PlatformsArgs) -> anyhow::Result<()> {
    let platforms = match pex.layout {
        Layout::Loose | Layout::Packed => calculate_supported_platforms(
            pex.path
                .join("__pex__")
                .join(".clibs")
                .read_dir()?
                .flat_map(|entry| entry.ok().map(|entry| entry.file_name()))
                .filter_map(|file_name| file_name.into_string().ok())
                .map(Cow::Owned),
        ),
        Layout::ZipApp => {
            let zip = ZipArchive::new(File::open(pex.path)?)?;
            calculate_supported_platforms(
                zip.file_names()
                    .filter_map(|file_name| file_name.strip_prefix("__pex__/.clibs/"))
                    .filter(|file_name| !file_name.is_empty())
                    .map(Cow::Borrowed),
            )
        }
    }?;
    let mut out = args.output.writer()?;
    if args.json {
        args.json_serializer.serialize(out, &platforms)?;
    } else {
        for platform in platforms {
            writeln!(&mut out, "{}", platform)?;
        }
    }
    Ok(())
}

fn calculate_supported_platforms<'a>(
    clibs: impl Iterator<Item = Cow<'a, str>>,
) -> anyhow::Result<Vec<&'static str>> {
    let mut platforms = clibs
        .flat_map(|file_name| file_name.split('.').next().map(SimplifiedTarget::try_from))
        .map(|result| result.map(|target| target.as_str()))
        .collect::<anyhow::Result<Vec<_>>>()?;
    platforms.sort();
    Ok(platforms)
}
