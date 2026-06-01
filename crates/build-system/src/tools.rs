// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::bail;
use bstr::ByteSlice;
use const_format::concatcp;
use fs_err as fs;
use fs_err::File;
use strum::{EnumCount, IntoEnumIterator};
use strum_macros::{EnumCount, EnumIter};
use target_lexicon::HOST;
use which::which_in_global;

use crate::downloads::ensure_download;
use crate::metadata::{Build, CargoBinstall, Download, Embeds, Glibc};

pub(crate) struct ToolBox<'a> {
    embeds: Embeds<'a>,
    binstall: CargoBinstall<'a>,
    zig_version: &'a str,
    glibc: Glibc<'a>,
    binstall_tools: Vec<BinstallTool>,
    downloads: Vec<(&'static str, Download<'a>)>,
}

impl<'a> From<Build<'a>> for ToolBox<'a> {
    fn from(build: Build<'a>) -> Self {
        #[cfg(unix)]
        let downloads = vec![("SDKROOT", build.mac_osx_sdk)];

        // N.B.: This fails to unpack on Windows; so cross-build from Windows likely won't work right now:
        // failed to unpack `MacOSX11.3.sdk/usr/share/man/mann/ttk::progressbar.ntcl` into `\\?\C:\Users\runneradmin\AppData\Local\pexrc-dev\downloads\.tmpy6R6aW\MacOSX11.3.sdk\usr\share\man\mann\ttk::progressbar.ntcl`
        #[cfg(windows)]
        let downloads: Vec<(&'static str, Download<'a>)> = Vec::new();

        Self {
            embeds: build.embeds,
            binstall: build.cargo_binstall,
            zig_version: build.zig_version,
            glibc: build.glibc,
            binstall_tools: BinstallTool::iter().collect::<Vec<_>>(),
            downloads,
        }
    }
}

impl<'a> ToolBox<'a> {
    pub(crate) fn find_tools(self, install_dirs: InstallDirs) -> anyhow::Result<ToolInventory<'a>> {
        let mut missing: Vec<BinstallTool> = Vec::with_capacity(BinstallTool::COUNT);
        let search_path = install_dirs.search_path()?;
        let zig = if let Some(zig) = find_zig(
            &["zig", "python-zig"],
            self.zig_version,
            search_path.as_ref(),
        ) {
            Zig::Found(zig)
        } else {
            Zig::MissingVersion(self.zig_version)
        };
        for tool in self.binstall_tools {
            if let Ok(Some(exe)) = which_in_global(tool.binary_name(), Some(&search_path))
                .map(|mut found| found.next())
            {
                eprintln!(
                    "Found {tool} at {exe}",
                    tool = tool.binary_name(),
                    exe = exe.display()
                );
            } else {
                missing.push(tool)
            }
        }
        Ok(ToolInventory {
            embeds: self.embeds,
            binstall: self.binstall,
            downloads: self.downloads,
            zig,
            glibc: self.glibc,
            missing,
            install_dirs,
        })
    }
}

#[derive(Clone)]
pub struct FoundTool {
    pub env_var: &'static str,
    pub path: PathBuf,
}

const ZIG_TOOL_ENV_VAR: &str = "CARGO_ZIGBUILD_ZIG_PATH";

pub fn find_zig(binary_names: &[&str], version: &str, search_path: &OsStr) -> Option<FoundTool> {
    for binary_name in binary_names {
        if let Ok(zig_paths) = which_in_global(binary_name, Some(search_path)) {
            for zig in zig_paths {
                if let Some(zig_version) = get_zig_version(&zig) {
                    if zig_version == version {
                        return Some(FoundTool {
                            env_var: ZIG_TOOL_ENV_VAR,
                            path: zig,
                        });
                    } else {
                        eprintln!(
                            "Found zig at zig {zig_version} at {path} but want version {version}.",
                            path = zig.display()
                        );
                    }
                }
            }
        }
    }
    None
}

fn get_zig_version(zig: &Path) -> Option<String> {
    Command::new(zig)
        .arg("version")
        .stdout(Stdio::piped())
        .spawn()
        .ok()
        .and_then(|child| child.wait_with_output().ok())
        .and_then(|result| {
            if result.status.success() {
                result.stdout.to_str().ok().map(str::trim).map(String::from)
            } else {
                None
            }
        })
}

fn check_zig_version(version: &str, zig: &Path) -> bool {
    get_zig_version(zig)
        .map(|zig_version| zig_version == version)
        .unwrap_or_default()
}

pub struct InstallDirs {
    bin_dir: PathBuf,
    pub(crate) data_dir: PathBuf,
    pub(crate) download_dir: PathBuf,
}

impl InstallDirs {
    pub fn system(base: impl AsRef<Path>) -> Option<Self> {
        dirs::cache_dir().map(|cache_dir| Self::new(cache_dir.join(base)))
    }

    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            bin_dir: cache_dir.join("bin"),
            data_dir: cache_dir.join("data"),
            download_dir: cache_dir.join("downloads"),
        }
    }

    fn search_path(&self) -> anyhow::Result<Cow<'_, OsStr>> {
        if let Some(search_path) = env::var_os("PATH").as_deref().map(env::split_paths) {
            let search_path = env::join_paths(search_path.chain([self.bin_dir.clone()]))?;
            Ok(Cow::Owned(search_path))
        } else {
            Ok(Cow::Borrowed(self.bin_dir.as_os_str()))
        }
    }
}

#[derive(EnumCount, EnumIter)]
pub enum BinstallTool {
    CargoZigbuild,
    Uv,
}

impl BinstallTool {
    pub fn binary_name(&self) -> &'static str {
        match *self {
            BinstallTool::CargoZigbuild => "cargo-zigbuild",
            BinstallTool::Uv => "uv",
        }
    }
}

pub enum Zig<'a> {
    Found(FoundTool),
    MissingVersion(&'a str),
}

impl<'a> Zig<'a> {
    pub fn found(&self) -> bool {
        matches!(*self, Zig::Found(_))
    }

    pub fn missing_version(&'a self) -> Option<&'a str> {
        match *self {
            Zig::MissingVersion(version) => Some(version),
            _ => None,
        }
    }
}

pub(crate) struct ToolInventory<'a> {
    embeds: Embeds<'a>,
    glibc: Glibc<'a>,
    binstall: CargoBinstall<'a>,
    zig: Zig<'a>,
    downloads: Vec<(&'static str, Download<'a>)>,
    missing: Vec<BinstallTool>,
    install_dirs: InstallDirs,
}

pub enum ToolInstallation<'a> {
    Success((Embeds<'a>, Glibc<'a>, Vec<FoundTool>)),
    Failure((Zig<'a>, Vec<BinstallTool>, OsString)),
}

impl<'a> ToolInventory<'a> {
    pub(crate) fn ensure_tools_installed(
        self,
        cargo: &Path,
        install_missing_tools: bool,
    ) -> anyhow::Result<ToolInstallation<'a>> {
        let tool_search_path =
            if let Some(search_path) = env::var_os("PATH").as_deref().map(env::split_paths) {
                let search_path =
                    env::join_paths(search_path.chain([self.install_dirs.bin_dir.clone()]))?;
                Cow::Owned(search_path)
            } else {
                Cow::Borrowed(self.install_dirs.bin_dir.as_os_str())
            };

        let mut found_tools = Vec::new();
        if !self.missing.is_empty() || !self.zig.found() {
            if install_missing_tools {
                let zig = install_tools(
                    cargo,
                    &self.binstall,
                    self.missing.as_slice(),
                    &self.zig,
                    &self.install_dirs,
                    &tool_search_path,
                )?;
                found_tools.push(zig.into_owned());
            } else {
                return Ok(ToolInstallation::Failure((
                    self.zig,
                    self.missing,
                    tool_search_path.into_owned(),
                )));
            }
        } else if let Zig::Found(zig) = self.zig {
            found_tools.push(zig)
        }
        for (env_var, download) in &self.downloads {
            let download_path = ensure_download(download, &self.install_dirs.download_dir)?;
            found_tools.push(FoundTool {
                env_var,
                path: download_path,
            });
        }
        Ok(ToolInstallation::Success((
            self.embeds,
            self.glibc,
            found_tools,
        )))
    }
}

enum ZigTool {
    Hit(FoundTool),
    Miss(PathBuf),
}

fn zig_tool(version: &str, zig_candidate: PathBuf) -> ZigTool {
    if platform::is_executable(&zig_candidate)
        .ok()
        .unwrap_or_default()
        && check_zig_version(version, &zig_candidate)
    {
        ZigTool::Hit(FoundTool {
            env_var: ZIG_TOOL_ENV_VAR,
            path: zig_candidate,
        })
    } else {
        ZigTool::Miss(zig_candidate)
    }
}

fn install_tools<'a>(
    cargo: &Path,
    cargo_binstall: &CargoBinstall,
    tools: &[BinstallTool],
    zig: &'a Zig,
    install_dirs: &InstallDirs,
    search_path: &OsStr,
) -> anyhow::Result<Cow<'a, FoundTool>> {
    for tool in tools {
        binstall(
            cargo_binstall,
            install_dirs,
            search_path,
            cargo,
            tool.binary_name(),
        )?;
    }

    match zig {
        Zig::Found(zig) => Ok(Cow::Borrowed(zig)),
        Zig::MissingVersion(version) => {
            fs::create_dir_all(&install_dirs.bin_dir)?;

            let lock_file = File::create(install_dirs.bin_dir.join(".python-zig.lck"))?;
            lock_file.lock()?;
            let python_zig = install_dirs.bin_dir.join("python-zig");
            match zig_tool(version, python_zig) {
                ZigTool::Hit(zig_tool) => return Ok(Cow::Owned(zig_tool)),
                ZigTool::Miss(python_zig) if !env::consts::EXE_EXTENSION.is_empty() => {
                    let python_zig_exe = python_zig.with_extension(env::consts::EXE_EXTENSION);
                    if let ZigTool::Hit(zig_tool) = zig_tool(version, python_zig_exe) {
                        return Ok(Cow::Owned(zig_tool));
                    }
                }
                _ => {}
            }

            let zig_requirement = format!("ziglang=={version}");
            let result = Command::new("uv")
                .args(["tool", "install", "--force", &zig_requirement])
                .env("UV_TOOL_DIR", install_dirs.data_dir.join("uv").as_os_str())
                .env("UV_TOOL_BIN_DIR", install_dirs.bin_dir.as_os_str())
                .stderr(Stdio::piped())
                .spawn()?
                .wait_with_output()?;
            if !result.status.success() {
                bail!(
                    "Failed to install zig {version} via `uv tool install {zig_requirement}`:\n\
                {stderr}",
                    stderr = result.stderr.to_str_lossy()
                )
            } else if let Some(zig) = find_zig(&["python-zig"], version, search_path) {
                Ok(Cow::Owned(zig))
            } else {
                bail!(
                    "Failed to find zig on PATH={search_path} after installing via \
                    `uv tool install --force {zig_requirement}`.",
                    search_path = search_path.to_string_lossy()
                )
            }
        }
    }
}

const CARGO_BINSTALL_FILE_NAME: &str = concatcp!("cargo-binstall", env::consts::EXE_SUFFIX);

fn binstall(
    cargo_binstall: &CargoBinstall,
    install_dirs: &InstallDirs,
    search_path: &OsStr,
    cargo: &Path,
    spec: &str,
) -> anyhow::Result<()> {
    if let Ok(Some(exe)) =
        which_in_global("cargo-binstall", Some(search_path)).map(|mut matches| matches.next())
    {
        eprintln!("Found cargo-binstall at {exe}", exe = exe.display());
    } else {
        let current_target = HOST.to_string();
        if let Some(download) = cargo_binstall.download_for(&current_target)? {
            let cargo_binstall = ensure_download(&download, &install_dirs.download_dir)?
                .join(CARGO_BINSTALL_FILE_NAME);
            let cargo_binstall_fp = File::open(&cargo_binstall)?;
            cargo_binstall_fp.lock()?;
            let dst = install_dirs.bin_dir.join(CARGO_BINSTALL_FILE_NAME);
            if dst.exists() {
                fs::remove_file(&dst)?;
            } else {
                fs::create_dir_all(&install_dirs.bin_dir)?;
            }
            fs::hard_link(&cargo_binstall, &dst)?;
        } else {
            let spec = format!("cargo-binstall@{version}", version = cargo_binstall.version);
            let result = Command::new(cargo)
                .args(["install", "--locked", &spec])
                .stderr(Stdio::piped())
                .spawn()?
                .wait_with_output()?;
            if !result.status.success() {
                bail!(
                    "Failed to install cargo-binstall to bootstrap tools with:\n{stderr}",
                    stderr = result.stderr.to_str_lossy()
                )
            }
        }
    }

    let result = Command::new(cargo)
        .env("PATH", search_path)
        .args(["binstall", "--no-confirm", spec])
        // N.B.: binstall logs to stdout :/; so we squelch.
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?
        .wait_with_output()?;
    if !result.status.success() {
        bail!(
            "Failed to install {spec}:\n{stderr}",
            stderr = result.stderr.to_str_lossy()
        )
    }
    Ok(())
}
