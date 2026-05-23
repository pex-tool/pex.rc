// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};

use clap::Args;
use fs_err as fs;
use python_proxy::ProxySource;
use target::{SimplifiedTarget, Target};

use crate::embeds::read_proxy_content;

#[derive(Args)]
pub struct Script {
    #[arg(long)]
    target: Option<crate::simplified_target::SimplifiedTarget>,

    #[arg(short = 'p', long)]
    python: PathBuf,

    #[arg(short = 'o', long)]
    output_file: PathBuf,

    #[arg(long, default_value_t = false)]
    gui: bool,

    #[arg(value_name = "SCRIPT")]
    script: PathBuf,
}

impl Script {
    pub fn execute(self) -> anyhow::Result<()> {
        let target = if let Some(target) = self.target {
            target.into()
        } else {
            let current_target = Target::current()?;
            current_target.simplified_target_triple()?
        };
        create(
            target,
            &self.python,
            &self.script,
            &self.output_file,
            self.gui,
        )
    }
}

pub fn create(
    target: SimplifiedTarget,
    python: &Path,
    script: &Path,
    output_file: &Path,
    is_gui: bool,
) -> anyhow::Result<()> {
    let proxy_bytes = Box::new(read_proxy_content(target, is_gui)?);
    let script = fs::read_to_string(script)?;
    let target_script = fs::File::create(output_file)?;
    python_proxy::create(
        ProxySource::Read(proxy_bytes),
        python,
        target_script.into_file(),
        Some(script),
        is_gui,
    )
}
