// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

use anyhow::{anyhow, bail};
use bstr::ByteSlice;
use const_format::concatcp;
use fs_err as fs;
use fs_err::File;
use strum::{EnumCount, IntoEnumIterator};
use strum_macros::{EnumCount, EnumIter};
use target_lexicon::HOST;
use which::{which_global, which_in_global};

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
                && let Ok(version) = tool.check_version(&exe)
            {
                eprintln!(
                    "Found {tool} {version} at {exe}.",
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
                        eprintln!("Found zig {zig_version} at {path}.", path = zig.display());
                        return Some(FoundTool {
                            env_var: ZIG_TOOL_ENV_VAR,
                            path: zig,
                        });
                    } else {
                        eprintln!(
                            "Skipping zig {zig_version} at {path}: want version {version}.",
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
        let allow_system_path = env::var_os("PEXRC_INSTALL_TOOLS_ALLOW_SYSTEM_PATH")
            .map(|value| value.as_encoded_bytes() == b"1")
            .unwrap_or(true);
        if allow_system_path
            && let Some(search_path) = env::var_os("PATH").as_deref().map(env::split_paths)
        {
            let search_path = env::join_paths(search_path.chain([self.bin_dir.clone()]))?;
            Ok(Cow::Owned(search_path))
        } else {
            Ok(Cow::Borrowed(self.bin_dir.as_os_str()))
        }
    }
}

struct VersionCheck<P: Fn(Output) -> anyhow::Result<String>> {
    args: &'static [&'static str],
    parse: P,
}

impl<P: Fn(Output) -> anyhow::Result<String>> VersionCheck<P> {
    fn get_version(&self, exe: &Path) -> anyhow::Result<semver::Version> {
        let output = Command::new(exe)
            .args(self.args)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        if !output.status.success() {
            bail!(
                "Failed to collect version for {exe} (exited {exit_code}):\nSTDERR:\n{stderr}",
                exe = exe.display(),
                exit_code = output.status,
                stderr = output.stderr.to_str_lossy()
            )
        }
        let raw_version = (self.parse)(output)?;
        semver::Version::parse(raw_version.trim()).map_err(|err| {
            anyhow!(
                "Failed to parse version for {exe} from {raw_version}: {err}",
                exe = exe.display()
            )
        })
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

    fn min_version(&self) -> semver::Version {
        match *self {
            BinstallTool::CargoZigbuild => semver::Version::new(0, 23, 0),
            BinstallTool::Uv => semver::Version::new(0, 12, 2),
        }
    }

    fn spec(&self) -> String {
        format!(
            "{name}@>={min_version}",
            name = self.binary_name(),
            min_version = self.min_version()
        )
    }

    fn check_version(&self, exe: &Path) -> anyhow::Result<semver::Version> {
        let vc = VersionCheck {
            args: &["-V"],
            parse: |output| {
                output
                    .stdout
                    .to_str_lossy()
                    .split(" ")
                    .nth(1)
                    .map(ToString::to_string)
                    .ok_or_else(|| {
                        anyhow!("Failed to parse version for {exe}.", exe = exe.display())
                    })
            },
        };
        let version = vc.get_version(exe)?;
        if version >= self.min_version() {
            Ok(version)
        } else {
            bail!(
                "The {name} at {exe} is version {version} but a minimum of {min_version} is \
                required.",
                name = self.binary_name(),
                exe = exe.display(),
                min_version = self.min_version()
            )
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
        install_missing_tools: bool,
    ) -> anyhow::Result<ToolInstallation<'a>> {
        let tool_search_path = self.install_dirs.search_path()?;
        let mut found_tools = Vec::new();
        if !self.missing.is_empty() || !self.zig.found() {
            if install_missing_tools {
                let zig = install_tools(
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
    cargo_binstall: &CargoBinstall,
    tools: &[BinstallTool],
    zig: &'a Zig,
    install_dirs: &InstallDirs,
    search_path: &OsStr,
) -> anyhow::Result<Cow<'a, FoundTool>> {
    for tool in tools {
        binstall(cargo_binstall, install_dirs, search_path, &tool.spec())?;
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
    spec: &str,
) -> anyhow::Result<()> {
    let cargo = which_global("cargo")?;

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
            let result = Command::new(&cargo)
                .args(["+stable", "install", "--locked", &spec])
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

    let result = Command::new(&cargo)
        .env("PATH", search_path)
        // N.B.: Ensures that binstall sub-processes that fall back to building when no download is
        // available use the stable toolchain for the build.
        .env("RUSTUP_TOOLCHAIN", "stable")
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
