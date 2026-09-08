// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::io::{BufReader, Seek, Write};
use std::path::PathBuf;
use std::{cmp, fs};

use cache::Fingerprint;
use clap::Args;
use cli::Output;
use fs_err::File;
use owo_colors::OwoColorize;
use pex::Pex;
use platform::path_for_terminal_output;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use repackage::repackage_wheels;
use scripts::Scripts;
use venv::{InstallPaths, Virtualenv, collect_installed_wheels};

use crate::compression_method::CompressionArgs;
use crate::source;

#[derive(Args)]
#[group(skip)]
pub struct Extract {
    #[command(flatten)]
    compression_args: CompressionArgs,

    #[command(flatten)]
    output: Output,

    /// The directory to extract the wheels to.
    #[arg(short = 'd', long)]
    dest_dir: PathBuf,

    /// The source to extract dependency wheels from.
    ///
    /// Can be a path to a PEX or a venv or else a URL pointing to a PEX.
    #[arg(value_name = "SOURCE", verbatim_doc_comment)]
    source: String,
}

impl Extract {
    pub fn execute(self) -> anyhow::Result<()> {
        self.output.configure()?;

        let options = self.compression_args.into_wheel_options(None);
        let path = source::to_path(self.source, Some(&self.dest_dir))?;
        let wheels =
            if let Ok(venv) = Virtualenv::load(Cow::Borrowed(&path), &mut Scripts::Embedded) {
                let install_paths = InstallPaths::for_venv(&venv)?;
                collect_installed_wheels(&venv)?
                    .into_par_iter()
                    .map(|installed_wheel| {
                        fs::create_dir_all(&self.dest_dir)?;
                        let dest_whl_path = self.dest_dir.join(installed_wheel.file_name()?);
                        let mut whl_file = File::create(&dest_whl_path)?;
                        installed_wheel.pack(&install_paths, &options, &mut whl_file)?;
                        whl_file.flush()?;
                        whl_file.rewind()?;
                        Ok(File::open(dest_whl_path)?)
                    })
                    .collect::<anyhow::Result<Vec<_>>>()?
            } else {
                let pex = Pex::load(&path)?;
                repackage_wheels(&pex, &options, &self.dest_dir)?
            };
        to_dir(wheels)
    }
}

fn to_dir(wheels: Vec<File>) -> anyhow::Result<()> {
    let count = wheels.len();

    let mut wheel_info = Vec::with_capacity(count);
    let mut max_width = 0;
    for wheel in wheels {
        let path = path_for_terminal_output(wheel.path()).to_string();
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
