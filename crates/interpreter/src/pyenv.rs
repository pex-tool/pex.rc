// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![cfg(unix)]

use std::borrow::Cow;
use std::env;
use std::env::VarError;
use std::ffi::OsStr;
use std::fmt::Display;
use std::io::{BufRead, BufReader, Read};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};

use anyhow::{Context, bail};
use fs_err::File;
use log::warn;
use logging_timer::time;
use python_platform::Implementation;

pub(crate) struct Pyenv(PathBuf);

impl Pyenv {
    pub(crate) fn locate() -> Option<Self> {
        env::var_os("PYENV_ROOT").map(PathBuf::from).map(Self)
    }

    #[time("debug", "Pyenv.{}")]
    pub(crate) fn resolve_if_shim<'a>(&self, path: &'a Path) -> anyhow::Result<Cow<'a, Path>> {
        if let Ok(rel_path) = path.strip_prefix(&self.0)
            && let Some(component) = rel_path.components().next()
            && matches!(component, Component::Normal(name) if name == OsStr::from_bytes(b"shims"))
            && let Some(shim) = PythonShim::parse(path)?
        {
            Ok(Cow::Owned(shim.select_version(&self.0)?))
        } else {
            Ok(Cow::Borrowed(path))
        }
    }
}

struct PythonShim<'a> {
    path: &'a Path,
    implementation: Implementation,
    version: &'a str,
}

impl<'a> PythonShim<'a> {
    fn parse(path: &'a Path) -> anyhow::Result<Option<Self>> {
        // Sanity-check this is a shell script, which all shims should be.
        let mut file = File::open(path)?;
        let mut buf: [u8; 2] = [0; 2];
        file.read_exact(&mut buf).with_context(|| {
            format!(
                "Failed to confirm {path} as a pyenv shim script",
                path = path.display()
            )
        })?;
        if &buf != b"#!" {
            bail!(
                "Although {path} is under the pyenv shims dir, it is not a shim script.\n\
                First two bytes are {buf:?}",
                path = path.display()
            );
        }

        if let Some(file_name) = path.file_name()
            && let Some(file_name) = file_name.to_str()
            && let Some((implementation, version)) = file_name
                .strip_prefix("python")
                .map(|version| (Implementation::CPython, version))
                .or_else(|| {
                    file_name
                        .strip_prefix("pypy")
                        .map(|version| (Implementation::PyPy, version))
                })
        {
            return Ok(Some(Self {
                path,
                implementation,
                version,
            }));
        }
        Ok(None)
    }

    fn select_version(&self, pyenv_root: &Path) -> anyhow::Result<PathBuf> {
        if let Some((source, active_versions)) = active_versions(pyenv_root)? {
            for version in &active_versions {
                if let Some(search_version) = match self.implementation {
                    Implementation::CPython => {
                        Some(version.strip_prefix("pypy").unwrap_or(version))
                    }
                    Implementation::PyPy => version.strip_prefix("pypy"),
                } && search_version.starts_with(self.version)
                {
                    let mut versions_dir = pyenv_root.join("versions");
                    versions_dir.push(version);
                    versions_dir.push("bin");
                    versions_dir.push("python");
                    if let Ok(python_exe) = versions_dir.canonicalize() {
                        return Ok(python_exe);
                    }
                }
            }

            struct ActivatedVersions(Vec<String>);
            impl Display for ActivatedVersions {
                fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
                    for version in &self.0 {
                        writeln!(f, "+ {version}")?;
                    }
                    Ok(())
                }
            }
            let activated_versions = ActivatedVersions(active_versions);
            bail!(
                "The pyenv shim at {path} has no corresponding versions activated.\n\
                Versions activated via {source} are:\n\
                {activated_versions}",
                path = self.path.display()
            )
        } else {
            bail!(
                "There are no versions activated for pyenv shim at {path}",
                path = self.path.display()
            )
        }
    }
}

fn active_versions(pyenv_root: &Path) -> anyhow::Result<Option<(String, Vec<String>)>> {
    // See: https://github.com/pyenv/pyenv/blob/c425c9ec3a409a8d432bc9f4851da8e3f040bddc/README.md#understanding-python-version-selection
    if let Ok(shell_versions) = env::var("PYENV_VERSION").inspect_err(|err| {
        if let VarError::NotUnicode(value) = err {
            warn!(
                "Skipping non-utf8 env setting PYENV_VERSION={value}",
                value = value.display()
            )
        }
    }) {
        return Ok(Some((
            format!("PYENV_VERSION={shell_versions}"),
            shell_versions.split(":").map(str::to_string).collect(),
        )));
    } else {
        let start = env::var_os("PYENV_DIR")
            .map(PathBuf::from)
            .map(Ok)
            .unwrap_or_else(env::current_dir)
            .context("Failed to determine starting directory for pyenv version file search.")?;
        for dir in start.ancestors() {
            if let Ok(local_version_file) = File::open(dir.join(".python-version")) {
                return Ok(Some((
                    local_version_file.path().display().to_string(),
                    read_versions(local_version_file),
                )));
            }
        }
        if let Ok(global_version_file) = File::open(pyenv_root.join("version")) {
            return Ok(Some((
                global_version_file.path().display().to_string(),
                read_versions(global_version_file),
            )));
        }
    }
    Ok(None)
}

fn read_versions(version_file: impl Read) -> Vec<String> {
    BufReader::new(version_file)
        .lines()
        .map_while(Result::ok)
        .filter_map(|line| {
            let contents = line.trim();
            if contents.starts_with("#") {
                None
            } else {
                Some(contents.to_string())
            }
        })
        .collect()
}
