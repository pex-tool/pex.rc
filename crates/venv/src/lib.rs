// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(exit_status_error)]
#![feature(normalize_lexically)]
#![feature(slice_split_once)]
#![feature(trim_prefix_suffix)]

pub mod install;
mod provenance;
mod resolver;
mod script;
pub mod virtualenv;

use std::path::Path;

use anyhow::anyhow;
use cache::{CacheDir, Key};
use fs_err as fs;
pub use provenance::{Collision, CollisionReport, Provenance};
use python_proxy::ProxySource;
pub use resolver::{InstallPaths, InstalledWheel, collect_installed_wheels};
pub use virtualenv::{Linker, Virtualenv};

pub struct PythonProxyLinker<'a>(pub &'a ProxySource<'a>);

impl<'a> Linker for PythonProxyLinker<'a> {
    #[cfg(unix)]
    fn link(&self, dest: &Path, interpreter: Option<&Path>, is_gui: bool) -> anyhow::Result<()> {
        let file_name = dest.file_name().ok_or_else(|| {
            anyhow!(
                "The destination for the python-proxy doesn't have a file name: {path}",
                path = dest.display()
            )
        })?;
        let venv_python_file_name = format!(
            ".{file_name}",
            file_name = file_name.to_str().ok_or_else(|| anyhow!(
                "The destination for the python-proxy is not a UTF-8 file name: {file_name}",
                file_name = file_name.display()
            ))?
        );

        let mut key = Key::default();
        key.property("proxied-python", &venv_python_file_name);
        let fingerprint = key.fingerprint();
        let python_proxy = CacheDir::PythonProxy
            .path()?
            .join(fingerprint.base64_digest());

        cache::atomic_file(&python_proxy, |file| {
            python_proxy::create(
                self.0,
                venv_python_file_name.as_ref(),
                file.into_file(),
                None::<&[u8]>,
                is_gui,
            )
        })?;

        if let Some(interpreter) = interpreter {
            platform::symlink_or_link_or_copy(
                interpreter,
                dest.with_file_name(&venv_python_file_name),
                false,
            )?;
        } else {
            let orig_python = dest.with_file_name(&venv_python_file_name);
            fs::rename(dest, &orig_python)?;
        }
        platform::symlink_or_link_or_copy(python_proxy, dest, true)?;
        Ok(())
    }

    #[cfg(windows)]
    fn link(&self, dest: &Path, interpreter: Option<&Path>, is_gui: bool) -> anyhow::Result<()> {
        python_proxy::create(
            &self.0,
            interpreter
                .ok_or_else(|| anyhow!("Windows venvs require an interpreter to link to."))?,
            fs::File::create(dest)?.into_file(),
            None::<&[u8]>,
            is_gui,
        )
    }
}
