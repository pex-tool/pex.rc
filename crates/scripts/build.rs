// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::env;
use std::path::PathBuf;

use build_system::{
    InstallDirs,
    download_virtualenv,
    ensure_tools_installed,
    prepare_venv_activation_scripts,
};
use fs_err as fs;

fn main() -> anyhow::Result<()> {
    env_logger::init();

    let workspace_root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap())
        .join("..")
        .join("..");

    let target_dir: PathBuf = if let Some(custom_target_dir) = env::var_os("CARGO_TARGET_DIR") {
        custom_target_dir.into()
    } else {
        workspace_root.join("target")
    };

    let install_dirs = InstallDirs::system("pexrc-dev").unwrap_or_else(|| {
        let cache_base_dir = target_dir.join(".pexrc-dev");
        println!(
            "cargo::warning=Failed to discover the user cache dir; using {cache_base_dir}",
            cache_base_dir = cache_base_dir.display()
        );
        InstallDirs::new(cache_base_dir)
    });

    let cargo_manifest_contents = {
        let workspace_manifest_path = workspace_root.join("Cargo.toml");
        println!(
            "cargo::rerun-if-changed={workspace_manifest_path}",
            workspace_manifest_path = workspace_manifest_path.display()
        );
        fs::read_to_string(workspace_manifest_path)?
    };

    let virtualenv_py = download_virtualenv(&cargo_manifest_contents, &install_dirs)?;
    println!(
        "cargo::rustc-env=VIRTUALENV_PY={virtualenv_py}",
        virtualenv_py = virtualenv_py.display()
    );

    ensure_tools_installed(&cargo_manifest_contents, &target_dir, true)?;
    let venv_activation_scripts_dir =
        prepare_venv_activation_scripts(&workspace_root, &install_dirs)?;
    println!(
        "cargo::rerun-if-changed={activation_scripts_dir}",
        activation_scripts_dir = venv_activation_scripts_dir.display()
    );
    println!(
        "cargo::rustc-env=ACTIVATION_SCRIPTS_DIR={activation_scripts_dir}",
        activation_scripts_dir = venv_activation_scripts_dir.display()
    );

    Ok(())
}
