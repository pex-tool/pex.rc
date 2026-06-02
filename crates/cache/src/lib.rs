// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

mod atomic;
mod fingerprint;
mod key;

use std::borrow::Cow;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::sync::LazyLock;
use std::{env, fs};

use anyhow::anyhow;
pub use atomic::{atomic_dir, atomic_file};
use dtor::dtor;
pub use fingerprint::{
    DigestingReader,
    Fingerprint,
    HashOptions,
    default_digest,
    fingerprint_file,
    hash_file,
};
pub use key::Key;
use log::{debug, warn};
use logging_timer::time;
use tempfile::TempDir;

pub fn cache_dir(name: &str, alt_name: &str) -> Option<PathBuf> {
    if let Some(cache_dir) = dirs::cache_dir() {
        Some(cache_dir.join(name))
    } else {
        dirs::home_dir().map(|home_dir| home_dir.join(alt_name))
    }
}

fn is_home_dir(component: Component) -> bool {
    matches!(component, Component::Normal(name) if name == "~")
}

fn expand_home_dir(path: PathBuf) -> Result<PathBuf, Cow<'static, str>> {
    let mut expand = false;
    let mut count = 0;
    for component in path.components() {
        count += 1;
        if !expand && is_home_dir(component) {
            expand = true;
        }
    }
    if !expand {
        return Ok(path);
    }
    let home_dir = dirs::home_dir().ok_or_else(|| {
        Cow::Owned(format!(
            "Failed to expand home dir in {path}",
            path = path.display()
        ))
    })?;
    let mut expanded_path = PathBuf::with_capacity(count);
    for component in path.components() {
        if is_home_dir(component) {
            expanded_path.push(&home_dir)
        } else {
            expanded_path.push(component)
        }
    }
    Ok(expanded_path.iter().collect())
}

pub enum CacheRoot {
    Dir(PathBuf),
    TempDir(TempDir),
}

impl Deref for CacheRoot {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        match self {
            CacheRoot::Dir(root) => root,
            CacheRoot::TempDir(root) => root.path(),
        }
    }
}

impl AsRef<Path> for CacheRoot {
    fn as_ref(&self) -> &Path {
        self.deref()
    }
}

fn ensure_writeable(path: PathBuf) -> Result<CacheRoot, Cow<'static, str>> {
    for ancestor in path.ancestors() {
        if !ancestor.exists() {
            continue;
        }
        if tempfile::tempfile_in(ancestor).is_ok() {
            return Ok(CacheRoot::Dir(path));
        }
    }
    tempfile::tempdir()
        .inspect(|temp_dir| {
            warn!(
                "The pexrc cache root of {path} is not writeable\n\
                Falling back to a temporary cache root of {fallback} which will hurt \
                performance.",
                path = path.display(),
                fallback = temp_dir.path().display()
            )
        })
        .map(CacheRoot::TempDir)
        .map_err(|err| {
            Cow::Owned(format!(
                "The pexrc cache root of {path} is not writeable and a temporary cache dir \
                could not be established: {err}",
                path = path.display()
            ))
        })
}

static PEXRC_ROOT: LazyLock<Result<CacheRoot, Cow<'static, str>>> = LazyLock::new(|| {
    if let Some(pexrc_root) = env::var_os("PEXRC_ROOT") {
        expand_home_dir(pexrc_root.into())
    } else if let Some(pex_root) = env::var_os("PEX_ROOT") {
        expand_home_dir(pex_root.into()).map(|pex_root| pex_root.join("rc").join("cache"))
    } else if let Some(cache_dir) = cache_dir("pexrc", ".pexrc") {
        Ok(cache_dir)
    } else {
        Err(Cow::Borrowed(
            "Failed to calculate a PEXRC_ROOT directory!\n\
            The PEXRC_ROOT environment variable was not set, an operating-system standard cache \
            dir could not be determined and the user's home directory could not be determined.",
        ))
    }
    .and_then(ensure_writeable)
});

#[dtor(unsafe)]
fn cleanup_tmp_cache_root() {
    if let Some(Ok(CacheRoot::TempDir(temp_dir))) = LazyLock::get(&PEXRC_ROOT) {
        if let Err(err) = fs::remove_dir_all(temp_dir.path()) {
            warn!(
                "Leaked temp pexrc cache root {dir}: {err}",
                dir = temp_dir.path().display()
            )
        } else {
            debug!(
                "Removed temp pexrc cache root: {dir}",
                dir = temp_dir.path().display()
            )
        }
    }
}

pub enum CacheDir {
    Interpreter,
    PythonProxy,
    Venv,
}

impl CacheDir {
    pub fn root() -> anyhow::Result<&'static CacheRoot> {
        PEXRC_ROOT.as_ref().map_err(|err| anyhow!("{err}"))
    }

    fn version(&self) -> &'static str {
        match self {
            CacheDir::Interpreter => "2",
            CacheDir::PythonProxy => "0",
            CacheDir::Venv => "1",
        }
    }

    #[time("debug", "CacheDir.{}")]
    pub fn path(&self) -> anyhow::Result<PathBuf> {
        Self::root().map(|pexrc_root| {
            match self {
                CacheDir::Interpreter => pexrc_root.join("interpreters"),
                CacheDir::PythonProxy => pexrc_root.join("python-proxies"),
                CacheDir::Venv => pexrc_root.join("venvs"),
            }
            .join(self.version())
        })
    }
}

pub fn read_lock() -> Result<fs::File, Cow<'static, str>> {
    let root = match PEXRC_ROOT.as_ref().map_err(|err| err.clone())? {
        CacheRoot::Dir(root) => {
            fs::create_dir_all(root)
                .map_err(|_| Cow::Borrowed("Failed to create PEXRC_ROOT dir."))?;
            root
        }
        CacheRoot::TempDir(root) => root.path(),
    };
    let lock = fs::File::create(root.join(".lck"))
        .map_err(|_| Cow::Borrowed("Failed to open PEXRC_ROOT read lock."))?;
    lock.lock_shared()
        .map_err(|_| Cow::Borrowed("Failed to obtain PEXRC_ROOT read lock."))?;
    Ok(lock)
}
