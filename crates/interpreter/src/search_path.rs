// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};

use anyhow::bail;
use indexmap::{IndexSet, indexset};
use logging_timer::time;
use same_file::is_same_file;

#[derive(Clone)]
pub struct SearchPath {
    pex_python: Option<OsString>,
    pex_python_path: Option<Vec<PathBuf>>,
    python_exes: Option<IndexSet<PathBuf>>,
}

impl SearchPath {
    pub const EMPTY: Self = Self {
        pex_python: None,
        pex_python_path: None,
        python_exes: None,
    };

    pub fn known(python_exes: IndexSet<PathBuf>) -> Self {
        Self {
            pex_python: None,
            pex_python_path: None,
            python_exes: Some(python_exes),
        }
    }

    #[time("debug", "SearchPath.{}")]
    pub fn from_env() -> anyhow::Result<Self> {
        let (pex_python, python_exe) = if let Some(pex_python) = env::var_os("PEX_PYTHON") {
            let python = PathBuf::from(pex_python);
            if platform::is_executable(&python).unwrap_or_default() {
                (None, Some(python))
            } else {
                (Some(python.into_os_string()), None)
            }
        } else {
            (None, None)
        };

        let mut python_exes: Option<IndexSet<PathBuf>> = None;
        let pex_python_path = if let Some(pex_python_path) = env::var_os("PEX_PYTHON_PATH") {
            let path = env::split_paths(&pex_python_path);
            if let Some(python) = python_exe {
                let mut contained = false;
                for entry in path {
                    if python.starts_with(entry) {
                        contained = true;
                        break;
                    }
                }
                if !contained {
                    bail!(
                        "The given PEX_PYTHON {python} is not contained in the given \
                        PEX_PYTHON_PATH: {pex_python_path}",
                        python = python.display(),
                        pex_python_path = pex_python_path.display()
                    )
                }
                python_exes = Some(indexset![python]);
                None
            } else if let Some(pex_python) = pex_python.as_ref() {
                for entry in path {
                    if platform::is_executable(&entry).unwrap_or_default()
                        && entry.ends_with(pex_python)
                    {
                        python_exes.get_or_insert_default().insert(entry);
                    } else if entry.is_dir() {
                        let python_exe = entry.join(pex_python);
                        if platform::is_executable(&python_exe).unwrap_or_default() {
                            python_exes.get_or_insert_default().insert(python_exe);
                        }
                    }
                }
                None
            } else {
                Some(
                    path.filter_map(|entry| {
                        if platform::is_executable(&entry).unwrap_or_default() {
                            python_exes.get_or_insert_default().insert(entry);
                            None
                        } else {
                            Some(entry)
                        }
                    })
                    .collect::<Vec<_>>(),
                )
            }
        } else {
            None
        };

        Ok(Self {
            pex_python,
            pex_python_path,
            python_exes,
        })
    }

    pub fn is_empty(&self) -> bool {
        self.pex_python.is_none() && self.pex_python_path.is_none() && self.python_exes.is_none()
    }

    pub fn pex_python(&self) -> Option<&OsStr> {
        self.pex_python.as_deref()
    }

    pub fn pex_python_path(&self) -> Option<&[PathBuf]> {
        self.pex_python_path.as_deref()
    }

    pub fn python_exes(&self) -> Option<&IndexSet<PathBuf>> {
        self.python_exes.as_ref()
    }

    pub fn contains(&self, python_exe: &Path) -> bool {
        if self.is_empty() {
            return true;
        }
        if let Some(python_exes) = self.python_exes.as_ref() {
            for exe in python_exes {
                if is_same_file(exe, python_exe).unwrap_or_default() {
                    return true;
                }
            }
        }
        if let Some(pex_python) = self.pex_python.as_ref()
            && !python_exe.ends_with(pex_python)
        {
            return false;
        }
        if let Some(pex_python_path) = self.pex_python_path.as_ref() {
            for path in pex_python_path {
                if python_exe.starts_with(path) {
                    return true;
                }
            }
        }
        false
    }

    pub fn unique_interpreter(&self) -> Option<&Path> {
        if self.pex_python.is_none()
            && self.pex_python_path.is_none()
            && let Some(python_exes) = self.python_exes.as_ref()
            && python_exes.len() == 1
        {
            Some(&python_exes[0])
        } else {
            None
        }
    }

    pub(crate) fn into_parts(
        self,
    ) -> anyhow::Result<(
        Option<OsString>,
        Option<OsString>,
        Option<impl Iterator<Item = PathBuf>>,
    )> {
        let pex_python_path = if let Some(pex_python_path) = self.pex_python_path {
            Some(env::join_paths(pex_python_path)?)
        } else {
            None
        };
        Ok((
            self.pex_python,
            pex_python_path,
            self.python_exes.map(IndexSet::into_iter),
        ))
    }
}
