// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use clap::Args;
use fs_err as fs;
use python_proxy::ProxySource;
use target::Target;

use crate::embeds::read_proxy_content;

#[derive(Args)]
pub struct ScriptArgs {
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

pub fn create(script_args: ScriptArgs) -> anyhow::Result<()> {
    let target = if let Some(target) = script_args.target {
        target.into()
    } else {
        let current_target = Target::current()?;
        current_target.simplified_target_triple()?
    };

    let is_gui = script_args.gui;
    let proxy_bytes = Box::new(read_proxy_content(target, is_gui)?);
    let script = fs::read_to_string(script_args.script)?;
    let target_script = fs::File::create(script_args.output_file)?;
    python_proxy::create(
        ProxySource::Read(proxy_bytes),
        &script_args.python,
        target_script.into_file(),
        Some(script),
        is_gui,
    )
}
