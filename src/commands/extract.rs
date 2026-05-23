// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::cmp;
use std::io::BufReader;
use std::path::PathBuf;

use cache::Fingerprint;
use clap::Args;
use owo_colors::OwoColorize;
use pex::Pex;

use crate::compression_method::CompressionArgs;
use crate::source;

#[derive(Args)]
pub struct ExtractArgs {
    #[command(flatten)]
    compression_args: CompressionArgs,

    /// The directory to extract the wheels to.
    #[arg(short = 'd', long)]
    dest_dir: PathBuf,

    /// The PEX to extract dependency wheels from. Can be a path or URL.
    #[arg(value_name = "PEX")]
    pex: String,
}

pub fn to_dir(extract_args: ExtractArgs) -> anyhow::Result<()> {
    let pex = source::to_path(extract_args.pex, Some(&extract_args.dest_dir))?;
    let pex = Pex::load(&pex)?;
    let options = extract_args.compression_args.into_wheel_options(None);

    let wheels = pex::repackage_wheels(&pex, &options, &extract_args.dest_dir)?;
    let count = wheels.len();

    let mut wheel_info = Vec::with_capacity(count);
    let mut max_width = 0;
    for wheel in wheels {
        let path = wheel.path().display().to_string();
        max_width = cmp::max(max_width, path.len());
        wheel_info.push((
            path,
            wheel.metadata()?,
            Fingerprint::try_from(BufReader::new(wheel))?,
        ));
    }

    anstream::println!(
        "Extracted {count} {wheels}:",
        count = count.yellow(),
        wheels = if count == 1 { "wheel" } else { "wheels" }
    );
    for (idx, (path, metadata, fingerprint)) in wheel_info.into_iter().enumerate() {
        anstream::println!(
            "{idx:>3}. {path} {pad}{size:<8} bytes {alg}:{fingerprint}",
            idx = (idx + 1).yellow(),
            pad = " ".repeat(max_width - path.len()),
            size = metadata.len().yellow(),
            alg = "sha256-base64".green(),
            fingerprint = fingerprint.base64_digest().green(),
        )
    }
    Ok(())
}
