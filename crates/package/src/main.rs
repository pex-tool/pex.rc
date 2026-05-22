// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::string::ToString;
use std::sync::LazyLock;
use std::{cmp, env, io};

use anyhow::{anyhow, bail};
use build_system::{BuildTarget, all_targets, classify_targets, ensure_tools_installed};
use cache::fingerprint_file;
use clap::builder::Str;
use clap::{ArgAction, Parser, ValueEnum};
use fs_err as fs;
use owo_colors::OwoColorize;
use sha2::{Digest, Sha256};

static CARGO: LazyLock<PathBuf> = LazyLock::new(|| {
    env::var_os("CARGO")
        .map(PathBuf::from)
        .unwrap_or_else(|| env!("CARGO").into())
});

static CARGO_MANIFEST_PATH: LazyLock<anyhow::Result<PathBuf>> = LazyLock::new(|| {
    let process = Command::new(CARGO.as_path())
        .args(["locate-project", "--workspace", "--message-format", "plain"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    let output = process.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "Failed to determine path to workspace Cargo.toml; process exited with {status:?}:\n\
            {stderr}",
            status = output.status,
            stderr = String::from_utf8_lossy(&output.stderr)
        );
    }
    Ok(String::from_utf8(output.stdout)?.trim_end().into())
});

static CARGO_MANIFEST_DIR: LazyLock<anyhow::Result<&Path>> =
    LazyLock::new(|| match CARGO_MANIFEST_PATH.as_ref() {
        Ok(cargo_manifest_path) => cargo_manifest_path.parent().ok_or_else(|| {
            anyhow!(
                "Failed to determine cargo project root directory from workspace Cargo.toml \
                    path: {path}",
                path = cargo_manifest_path.display()
            )
        }),
        Err(err) => bail!("Failed to determine cargo workspace root dir: {err}"),
    });

static ALL_TARGETS: LazyLock<String> = LazyLock::new(|| "all".to_string());

static AVAILABLE_TARGETS: LazyLock<Vec<Str>> = LazyLock::new(|| {
    let mut available_targets = vec![Str::from(ALL_TARGETS.as_str())];
    let cargo_manifest_dir = match CARGO_MANIFEST_DIR.as_ref() {
        Ok(manifest_dir) => manifest_dir,
        Err(err) => panic!("Failed to determine cargo workspace root dir: {err}"),
    };
    let rust_toolchain_contents =
        match fs::read_to_string(cargo_manifest_dir.join("rust-toolchain")) {
            Ok(contents) => contents,
            Err(err) => panic!("Failed to read rust-toolchain file: {err}"),
        };
    match all_targets(&rust_toolchain_contents) {
        Ok(targets) => {
            for target in targets {
                available_targets.push(Str::from(target))
            }
        }
        Err(err) => panic!("Failed to parse rust-toolchain file: {err}"),
    }
    available_targets
});

#[derive(Clone, ValueEnum)]
enum PrintFormat {
    Text,
    Json,
}

/// Pexrc Packaging System.
#[derive(Parser)]
#[command(version, about, long_about = None, styles = cli::STYLES)]
struct Cli {
    #[command(flatten)]
    verbosity: clap_verbosity_flag::Verbosity,

    #[command(flatten)]
    color: colorchoice_clap::Color,

    #[arg(long)]
    profile: Option<String>,

    #[arg(long = "target")]
    #[arg(action=ArgAction::Append)]
    #[arg(value_parser=clap::builder::PossibleValuesParser::new(AVAILABLE_TARGETS.iter()))]
    targets: Vec<String>,

    #[arg(short = 't', visible_alias = "tools", long, default_value = "false")]
    include_tools: bool,

    #[arg(short = 'o', long)]
    dist_dir: Option<PathBuf>,

    #[arg(long, default_value = "false")]
    copies: bool,

    /// Print the available targets to stdout and exit.
    #[arg(long, value_enum)]
    print_targets: Option<PrintFormat>,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    env_logger::Builder::new()
        .filter_level(cli.verbosity.into())
        .target(env_logger::Target::Stderr)
        .init();
    cli.color.write_global();

    let cargo = CARGO.as_path();
    let cargo_manifest_path = match CARGO_MANIFEST_PATH.as_ref() {
        Ok(manifest_path) => manifest_path,
        Err(err) => bail!("{err}"),
    };
    let cargo_manifest_contents = fs::read_to_string(cargo_manifest_path)?;
    let cargo_manifest_dir = match CARGO_MANIFEST_DIR.as_ref() {
        Ok(manifest_dir) => manifest_dir,
        Err(err) => bail!("{err}"),
    };
    let target_dir: PathBuf = if let Some(custom_target_dir) = env::var_os("CARGO_TARGET_DIR") {
        custom_target_dir.into()
    } else {
        cargo_manifest_dir.join("target")
    };
    let (_, glibc, found_tools) =
        ensure_tools_installed(cargo, &cargo_manifest_contents, &target_dir, false)?;

    let rust_toolchain_contents = fs::read_to_string(cargo_manifest_dir.join("rust-toolchain"))?;
    let classified_targets = classify_targets(&rust_toolchain_contents, &glibc)?;

    if let Some(print_format) = cli.print_targets {
        let mut all_targets = classified_targets
            .iter()
            .map(BuildTarget::as_str)
            .collect::<Vec<_>>();
        all_targets.sort();
        match print_format {
            PrintFormat::Text => {
                for target in all_targets {
                    println!("{target}");
                }
            }
            PrintFormat::Json => {
                let stdout = io::stdout().lock();
                serde_json::to_writer(stdout, &all_targets)?;
            }
        }
        return Ok(());
    }

    let profile = cli.profile.as_deref().unwrap_or("dev");
    let (profile_dir_name, profile_target_suffix) = if profile == "dev" {
        ("debug", Some("-debug"))
    } else {
        (profile, None)
    };

    let mut env = vec![("PEXRC_TARGETS", "all")];
    if cli.include_tools {
        env.push(("PEXRC_CLIB_FEATURES", "tools"));
    }

    let built = if cli.targets.is_empty() {
        let result = Command::new(cargo)
            .args(["build", "--profile", profile])
            .envs(env)
            .spawn()?
            .wait()?;
        if !result.success() {
            bail!("Build via cargo build failed!");
        }
        let current_target = BuildTarget::current(&glibc);
        vec![(
            target_dir
                .join(profile_dir_name)
                .join(current_target.binary_name("pexrc", None).as_ref()),
            current_target.fully_qualified_binary_name("pexrc", profile_target_suffix)?,
        )]
    } else {
        let targeted: HashSet<String> = if cli.targets.contains(&ALL_TARGETS) {
            AVAILABLE_TARGETS.iter().map(Str::to_string).collect()
        } else {
            cli.targets.into_iter().collect()
        };
        let mut built: Vec<(PathBuf, String)> = Vec::with_capacity(targeted.len());
        let zigbuild_targets = classified_targets
            .iter()
            .filter(|target| targeted.contains(target.as_str()))
            .collect::<Vec<_>>();
        if !zigbuild_targets.is_empty() {
            let mut command = Command::new(cargo);
            command.args(["zigbuild", "--profile", profile]);
            for target in zigbuild_targets {
                command.args(["--target", target.zigbuild_target()]);
                built.push((
                    target_dir
                        .join(target.as_str())
                        .join(profile_dir_name)
                        .join(target.binary_name("pexrc", None).as_ref()),
                    target.fully_qualified_binary_name("pexrc", profile_target_suffix)?,
                ));
            }
            command.envs(env.clone());
            for found_tool in &found_tools {
                command.env(found_tool.env_var, &found_tool.path);
            }
            let result = command.spawn()?.wait()?;
            if !result.success() {
                bail!("Cross-build via cargo-zigbuild failed!");
            }
        }
        built
    };

    if let Some(dist_dir) = cli.dist_dir {
        let mut max_width = 0;
        for (_, dst_file_name) in &built {
            max_width = cmp::max(max_width, dst_file_name.len());
        }
        fs::create_dir_all(&dist_dir)?;
        let count = built.len();
        anstream::println!(
            "Packaging {count} {binaries} in {dist_dir}:",
            binaries = if count == 1 { "binary" } else { "binaries" },
            dist_dir = dist_dir.display()
        );
        let dist_dir = dist_dir.canonicalize()?;
        for (idx, (src, dst_file_name)) in built.iter().enumerate() {
            let dst = dist_dir.join(dst_file_name);
            if dst.exists() {
                fs::remove_file(&dst)?;
            }
            let (size, fingerprint) = fingerprint_file(src, Sha256::new())?;
            if cli.copies {
                fs::copy(src, &dst)?;
            } else {
                platform::symlink_or_link_or_copy(src, &dst, true)?;
            }
            fs::write(
                dst.with_added_extension("sha256"),
                format!(
                    "{hex_digest} *{dst_file_name}",
                    hex_digest = fingerprint.hex_digest()
                ),
            )?;
            anstream::println!(
                "{idx:>3}. {path} {pad}{size:<9} bytes {alg}:{fingerprint}",
                idx = (idx + 1).yellow(),
                path = dst_file_name.blue(),
                pad = " ".repeat(max_width - dst_file_name.len()),
                alg = "sha256-base64".green(),
                fingerprint = fingerprint.base64_digest().green(),
            )
        }
    } else {
        anstream::println!("{}", "Build complete!".green());
    }
    Ok(())
}
