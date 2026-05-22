// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(exact_size_is_empty)]

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::{env, io};

use anyhow::{anyhow, bail};
use bstr::ByteSlice;
use build_system::{
    BuildTarget,
    EmbedsConfiguration,
    FoundTool,
    classify_targets,
    ensure_tools_installed,
};
use fs_err as fs;
use fs_err::File;
use itertools::Itertools;

const BUILD_STD_CARGO_OPTIONS: &[&str] = &[
    // N.B.: This gets us the stdlib compiled locally and LTO'ed into the binaries and libraries
    // we produce for space (and speed) savings.
    // See https://github.com/johnthagen/min-sized-rust for the inspiration.
    "-Zbuild-std=std,panic_abort",
    "-Zbuild-std-features=optimize_for_size",
];

const DEFAULT_RUSTFLAGS: &[&str] = &[
    // This gets us lib musl dynamic linking.
    "-Ctarget-feature=-crt-static",
];

const OPTIMIZE_SIZE_RUSTFLAGS: &[&str] = &[
    // See https://github.com/johnthagen/min-sized-rust for the inspiration.
    "-Zunstable-options",
    "-Zfmt-debug=none",
    "-Zlocation-detail=none",
    "-Cpanic=immediate-abort",
];

#[derive(Copy, Clone)]
enum Optimizations {
    Default,
    BuildStd,
    All,
}

impl Optimizations {
    fn parse(value: OsString) -> anyhow::Result<Self> {
        let decoded = value
            .into_string()
            .map_err(|err| anyhow!("Failed to decode optimization: {err}", err = err.display()))?;
        match decoded.as_str() {
            "default" => Ok(Self::Default),
            "build-std" => Ok(Self::BuildStd),
            "all" => Ok(Self::All),
            value => bail!(
                "Invalid optimization choice: {value}\n\
                Valid values are:\n\
                default:   Standard Rust small size optimizations (s or z).\n\
                build-std: Build std locally and include it in LTO.\n\
                all:       All of the above plus more extreme size optimization sizes that \n\
                           sacrifice debuggability on crash.\
                "
            ),
        }
    }

    fn cargo_options(&self) -> &[&str] {
        match self {
            Optimizations::Default => &[],
            Optimizations::BuildStd | Optimizations::All => BUILD_STD_CARGO_OPTIONS,
        }
    }

    fn rust_flags(&self) -> String {
        match self {
            Optimizations::Default | Optimizations::BuildStd => DEFAULT_RUSTFLAGS.join(" "),
            Optimizations::All => OPTIMIZE_SIZE_RUSTFLAGS
                .iter()
                .chain(DEFAULT_RUSTFLAGS.iter())
                .join(" "),
        }
    }
}

fn main() -> anyhow::Result<()> {
    println!("cargo::rerun-if-changed=crates");

    env_logger::init();

    let cargo: PathBuf = env::var("CARGO")?.into();
    let cargo_manifest_contents = {
        let manifest_path = env::var("CARGO_MANIFEST_PATH")?;
        println!("cargo::rerun-if-changed={manifest_path}");
        fs::read_to_string(manifest_path)?
    };
    let target_dir: PathBuf = if let Some(custom_target_dir) = env::var_os("CARGO_TARGET_DIR") {
        custom_target_dir.into()
    } else {
        PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap()).join("target")
    };

    let (mut embeds, glibc, found_tools) =
        ensure_tools_installed(&cargo, &cargo_manifest_contents, &target_dir, true)?;
    println!("cargo::rerun-if-env-changed=PROFILE");
    let profile = env::var("PROFILE")?;
    let embeds_configuration = embeds.configuration_for(&profile);

    println!("cargo::rerun-if-env-changed=PEXRC_TARGETS");
    let all_targets = match env::var("PEXRC_TARGETS")
        .ok()
        .as_deref()
        .map(str::to_ascii_lowercase)
    {
        Some(targets) if targets == "all" => true,
        Some(targets) if targets == "current" => false,
        Some(targets) => bail!(
            "Unrecognized custom targets `PEXRC_TARGETS={targets}`.\n\
            Only `current` (the default) and `all` are recognized."
        ),
        None => false,
    };

    let embeds_dir = {
        let out_dir = env::var_os("OUT_DIR").unwrap();
        let embeds_dir = PathBuf::from(out_dir).join("embeds");
        if all_targets {
            embeds_dir.join("all")
        } else {
            embeds_dir.join("current")
        }
    };
    if embeds_dir.is_dir() {
        fs::remove_dir_all(&embeds_dir)?;
    }
    fs::create_dir_all(&embeds_dir)?;
    println!(
        "cargo::rustc-env=EMBEDS_DIR={embeds_dir}",
        embeds_dir = embeds_dir.to_str().ok_or_else(|| {
            anyhow!(
                "Build cannot proceed with project housed in a non-UTF-8 directory: {embeds_dir}",
                embeds_dir = embeds_dir.display()
            )
        })?
    );

    // N.B.: We need to use a custom --target-dir to avoid a deadlock on the ambient target that
    // would otherwise occur calling into cargo build recursively below.
    let tgt_path = target_dir.join("embeds");
    let tgt_arg = tgt_path.to_str().ok_or_else(|| {
        anyhow!(
            "The target directory of {target_dir} must be a UTF-8 path",
            target_dir = target_dir.display()
        )
    })?;

    let clib_optimizations = Optimizations::BuildStd;
    println!("cargo::rerun-if-env-changed=PEXRC_OPTIMIZATIONS");
    let python_proxy_optimizations = if let Some(value) = env::var_os("PEXRC_OPTIMIZATIONS") {
        Optimizations::parse(value)?
    } else {
        Optimizations::All
    };

    println!("cargo::rerun-if-env-changed=PEXRC_CLIB_FEATURES");
    let optional_clib_args: &[Cow<'_, str>] =
        if let Ok(clib_features) = env::var("PEXRC_CLIB_FEATURES") {
            &[Cow::Borrowed("--features"), Cow::Owned(clib_features)]
        } else {
            &[]
        };

    let python_proxyw_options = [Cow::Borrowed("--features"), Cow::Borrowed("windows")];

    println!("cargo::rerun-if-env-changed=PEXRC_TARGETS");
    if all_targets {
        println!("cargo::rerun-if-changed=rust-toolchain");
        let rust_toolchain_contents = fs::read_to_string("rust-toolchain")?;
        let targets = classify_targets(&rust_toolchain_contents, &glibc)?;
        custom_cargo_build(
            &cargo,
            &["zigbuild", "--target-dir", tgt_arg, "--package", "clib"],
            optional_clib_args,
            embeds_configuration.profile,
            &found_tools,
            targets.iter().map(BuildTarget::zigbuild_target),
            clib_optimizations,
        )?;
        custom_cargo_build(
            &cargo,
            &[
                "zigbuild",
                "--target-dir",
                tgt_arg,
                "--package",
                "python-proxy",
                "--bin",
                "python-proxy",
            ],
            &[],
            embeds_configuration.profile,
            &found_tools,
            targets.iter().map(BuildTarget::zigbuild_target),
            python_proxy_optimizations,
        )?;
        custom_cargo_build(
            &cargo,
            &[
                "zigbuild",
                "--target-dir",
                tgt_arg,
                "--package",
                "python-proxy",
                "--bin",
                "python-proxyw",
            ],
            &python_proxyw_options,
            embeds_configuration.profile,
            &found_tools,
            targets
                .iter()
                .filter_map(|target| {
                    if target.is_windows() {
                        Some(target.zigbuild_target())
                    } else {
                        None
                    }
                })
                .collect::<Vec<_>>()
                .into_iter(),
            python_proxy_optimizations,
        )?;
        collect_embeds(
            targets.as_slice(),
            &tgt_path,
            embeds_configuration,
            &embeds_dir,
            true,
        )
    } else {
        let selected_target = env::var("TARGET")?;
        let target = BuildTarget::classify(&selected_target, &glibc);
        let targets = vec![&target];
        custom_cargo_build(
            &cargo,
            &["build", "--target-dir", tgt_arg, "--package", "clib"],
            optional_clib_args,
            embeds_configuration.profile,
            &found_tools,
            targets.iter().map(|target| target.as_str()),
            clib_optimizations,
        )?;
        custom_cargo_build(
            &cargo,
            &[
                "build",
                "--target-dir",
                tgt_arg,
                "--package",
                "python-proxy",
                "--bin",
                "python-proxy",
            ],
            &[],
            embeds_configuration.profile,
            &found_tools,
            targets.iter().map(|target| target.as_str()),
            python_proxy_optimizations,
        )?;
        if target.is_windows() {
            custom_cargo_build(
                &cargo,
                &[
                    "build",
                    "--target-dir",
                    tgt_arg,
                    "--package",
                    "python-proxy",
                    "--bin",
                    "python-proxyw",
                ],
                &python_proxyw_options,
                embeds_configuration.profile,
                &found_tools,
                targets.iter().map(|target| target.as_str()),
                python_proxy_optimizations,
            )?;
        }
        collect_embeds(
            targets.as_slice(),
            &tgt_path,
            embeds_configuration,
            &embeds_dir,
            true,
        )
    }
}

fn custom_cargo_build<'a>(
    cargo: &Path,
    custom_build_args: &[&str],
    optional_args: &[Cow<'_, str>],
    profile: &str,
    found_tools: &[FoundTool],
    targets: impl ExactSizeIterator<Item = &'a str>,
    optimizations: Optimizations,
) -> anyhow::Result<()> {
    if targets.is_empty() {
        return Ok(());
    }

    let mut cmd = Command::new(cargo);
    let cmd = cmd
        .stderr(Stdio::piped())
        .env_remove("CARGO_ENCODED_RUSTFLAGS")
        .env("CARGO_TERM_COLOR", "always")
        .env("RUSTFLAGS", optimizations.rust_flags().as_str());
    for found_tool in found_tools {
        println!(
            "cargo::rustc-env={env_var}={path}",
            env_var = found_tool.env_var,
            path = found_tool.path.display()
        );
        cmd.env(found_tool.env_var, &found_tool.path);
    }

    cmd.args(custom_build_args)
        .args(optimizations.cargo_options());
    for arg in optional_args {
        cmd.arg(arg.as_ref());
    }

    cmd.args([
        "--profile",
        if profile == "debug" { "dev" } else { profile },
    ]);
    for target in targets {
        cmd.args(["--target", target]);
    }

    let result = cmd.spawn()?.wait_with_output()?;
    if result.status.success() {
        return Ok(());
    }
    bail!(
        "Failed to compile clib with exit code {exit_code}:\n{exe} \\\n  {args}\n{output}",
        exit_code = result.status,
        exe = cmd.get_program().to_string_lossy(),
        args = cmd.get_args().map(OsStr::to_string_lossy).join(" \\\n  "),
        output = result.stderr.to_str_lossy()
    )
}

fn collect_embeds<'a>(
    targets: &'a [impl AsRef<BuildTarget<'a>>],
    target_dir: &Path,
    embeds_configuration: &'a EmbedsConfiguration<'a>,
    embeds_dir: &Path,
    compress: bool,
) -> anyhow::Result<()> {
    let clibs_dir = embeds_dir.join("clibs");
    fs::create_dir_all(&clibs_dir)?;
    let proxies_dir = embeds_dir.join("proxies");
    fs::create_dir_all(&proxies_dir)?;
    for target in targets {
        let target = target.as_ref();
        let clib_name = target.shared_library_name("pexrc");
        let target_name = format!(
            "{target}.{clib_name}",
            target = target.simplified_target_triple()?
        );
        collect_embed(
            &clib_name,
            &clibs_dir,
            embeds_configuration,
            target,
            target_dir,
            &target_name,
            compress,
        )?;
        let python_proxy_name = target.binary_name("python-proxy", None);
        let target_name = target.fully_qualified_binary_name("python-proxy", None)?;
        collect_embed(
            &python_proxy_name,
            &proxies_dir,
            embeds_configuration,
            target,
            target_dir,
            &target_name,
            compress,
        )?;
        if target.is_windows() {
            let python_proxy_name = target.binary_name("python-proxyw", None);
            let target_name = target.fully_qualified_binary_name("python-proxyw", None)?;
            collect_embed(
                &python_proxy_name,
                &proxies_dir,
                embeds_configuration,
                target,
                target_dir,
                &target_name,
                compress,
            )?;
        }
    }
    Ok(())
}

fn collect_embed<'a>(
    embed_name: &str,
    embed_dir: &Path,
    embeds_configuration: &'a EmbedsConfiguration<'a>,
    target: &'a BuildTarget,
    target_dir: &Path,
    target_name: &str,
    compress: bool,
) -> anyhow::Result<()> {
    let embed_path = target_dir
        .join(target.as_str())
        .join(embeds_configuration.profile)
        .join(embed_name);
    if !embed_path.exists() {
        eprintln!(
            "The embed for {target} does not exist at {embed_path}!",
            embed_path = embed_path.display(),
            target = target.as_str(),
        );
    }
    let mut dst = File::create(embed_dir.join(target_name))?;
    if compress {
        let encoder = zstd::Encoder::new(dst, embeds_configuration.compression_level)?;
        io::copy(&mut File::open(embed_path)?, &mut encoder.auto_finish())?;
    } else {
        io::copy(&mut File::open(embed_path)?, &mut dst)?;
    }
    Ok(())
}
