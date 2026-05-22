// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

mod downloads;
mod metadata;
mod rust_toolchain;
mod tools;

use std::borrow::Cow;
use std::env;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::bail;
use cache::atomic_dir;
use fs_err as fs;
use itertools::Itertools;
pub use metadata::{Embeds, EmbedsConfiguration, Glibc};
pub use rust_toolchain::BuildTarget;
use rust_toolchain::{Toolchain, parse_toolchain};

use crate::downloads::ensure_download;
use crate::metadata::{Metadata, parse_metadata};
use crate::tools::ToolBox;
pub use crate::tools::{BinstallTool, FoundTool, InstallDirs, ToolInstallation, Zig};

pub fn download_virtualenv(
    cargo_manifest_contents: &str,
    install_dirs: &InstallDirs,
) -> anyhow::Result<PathBuf> {
    let metadata: Metadata = parse_metadata(cargo_manifest_contents)?;
    ensure_download(&metadata.build.virtualenv, &install_dirs.download_dir)
}

pub fn prepare_venv_activation_scripts(
    workspace_root: &Path,
    install_dirs: &InstallDirs,
) -> anyhow::Result<PathBuf> {
    let python_version = fs::read_to_string(workspace_root.join(".python-version"))?;
    let venv_activation_scripts_dir = install_dirs
        .data_dir
        .join("venv-activation-scripts")
        .join(python_version.trim());
    atomic_dir(&venv_activation_scripts_dir, |work_dir| {
        Ok(Command::new("uv")
            .args(["run", "dev-cmd", "prepare-venv-activation-scripts", "--"])
            .arg(work_dir)
            .current_dir(workspace_root)
            .spawn()?
            .wait()?)
    })?;
    Ok(venv_activation_scripts_dir)
}

pub fn all_targets(rust_toolchain_contents: &str) -> anyhow::Result<Vec<String>> {
    let toolchain: Toolchain = parse_toolchain(rust_toolchain_contents)?;
    Ok(toolchain.into_targets())
}

pub fn classify_targets<'a>(
    rust_toolchain_contents: &'a str,
    glibc: &'a Glibc,
) -> anyhow::Result<Vec<BuildTarget<'a>>> {
    let toolchain: Toolchain = parse_toolchain(rust_toolchain_contents)?;
    Ok(toolchain.classify_targets(glibc))
}

pub fn ensure_tools_installed<'a>(
    cargo: &Path,
    cargo_manifest_contents: &'a str,
    target_dir: &Path,
    is_build_script: bool,
) -> anyhow::Result<(Embeds<'a>, Glibc<'a>, Vec<FoundTool>)> {
    let install_dirs = InstallDirs::system("pexrc-dev").unwrap_or_else(|| {
        let cache_base_dir = target_dir.join(".pexrc-dev");
        if is_build_script {
            println!(
                "cargo::warning=Failed to discover the user cache dir; using {cache_base_dir}",
                cache_base_dir = cache_base_dir.display()
            );
        }
        InstallDirs::new(cache_base_dir)
    });

    if is_build_script {
        println!("cargo::rerun-if-env-changed=PEXRC_INSTALL_TOOLS");
    }
    let install_missing_tools = env::var_os("PEXRC_INSTALL_TOOLS").unwrap_or_default() == "1";

    let metadata: Metadata = parse_metadata(cargo_manifest_contents)?;
    let tool_box = ToolBox::from(metadata.build);
    let tool_inventory = tool_box.find_tools(install_dirs)?;
    match tool_inventory.ensure_tools_installed(cargo, install_missing_tools)? {
        ToolInstallation::Success(result) => Ok(result),
        ToolInstallation::Failure((zig, missing_binstall_tools, tool_search_path)) => {
            bail!(
                "The following tools are required but are not installed: {tools}\n\
                Searched PATH: {search_path}\n\
                Re-run with PEXRC_INSTALL_TOOLS=1 to let the build script install these tools.",
                tools = missing_binstall_tools
                    .iter()
                    .map(|tool| Cow::Borrowed(tool.binary_name()))
                    .chain(
                        zig.missing_version()
                            .iter()
                            .map(|version| Cow::Owned(format!("zig@{version}")))
                    )
                    .join(" "),
                search_path = tool_search_path.display()
            );
        }
    }
}
