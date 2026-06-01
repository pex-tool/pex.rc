// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![cfg(windows)]

use std::collections::{HashMap, HashSet};
use std::env;
use std::ffi::OsString;
use std::fs::FileType;
use std::ops::Coroutine;
use std::path::PathBuf;
use std::pin::Pin;

use indexmap::IndexSet;
use which::sys::{RealSys, Sys};
use windows_registry::{CURRENT_USER, LOCAL_MACHINE};

use crate::constraints::{PythonBinarySpec, calculate_compatible_binary_specs};
use crate::{InterpreterConstraints, SearchPath, SelectionStrategy};

fn iter_python_exes(
    constraints: &InterpreterConstraints,
    python_binary_specs: IndexSet<PythonBinarySpec>,
    pex_python: Option<OsString>,
    search_path: Option<OsString>,
    known_python_exes: Option<impl Iterator<Item = PathBuf>>,
) -> Pin<Box<impl Coroutine<Yield = PathBuf, Return = ()>>> {
    let mut explicit_pythons = known_python_exes
        .map(|exes| exes.collect::<IndexSet<_>>())
        .unwrap_or_default();

    // C.F.: https://peps.python.org/pep-0514/
    let pex_python_path =
        search_path.map(|search_path| env::split_paths(&search_path).collect::<Vec<_>>());
    let mut versioned_pythons: HashMap<(u8, u8), IndexSet<PathBuf>> = HashMap::new();
    for root_key in CURRENT_USER
        .open(r"Software\Python")
        .into_iter()
        .chain(LOCAL_MACHINE.open(r"Software\Python").into_iter())
    {
        if let Ok(companies) = root_key.keys() {
            for company in companies {
                if &company == "PyLauncher" {
                    continue;
                }
                if let Ok(company_key) = root_key.open(&company)
                    && let Ok(tags) = company_key.keys()
                {
                    for tag in tags {
                        if let Ok(tag_key) = company_key.open(&tag) {
                            let version = if let Ok(version) = tag_key.get_string("SysVersion") {
                                let mut components = version.split(".");
                                if let Some(component) = components.next()
                                    && let Ok(major) = component.parse::<u8>()
                                    && let Some(component) = components.next()
                                    && let Ok(minor) = component.parse::<u8>()
                                {
                                    if !constraints.contains_version(major, minor) {
                                        continue;
                                    } else {
                                        Some((major, minor))
                                    }
                                } else {
                                    None
                                }
                            } else {
                                None
                            };

                            if let Some(python) =
                                if let Ok(install_path_key) = tag_key.open("InstallPath") {
                                    if let Ok(python_exe) =
                                        install_path_key.get_string("ExecutablePath")
                                    {
                                        Some(PathBuf::from(python_exe))
                                    // Older Pythons (I found this to be true of CPython 2.7) have
                                    // no ExecutablePath and just a sys.prefix default key.
                                    } else if let Ok(sys_prefix) = install_path_key.get_string(
                                        // N.B.: This is for real even though the spec says
                                        // `(Default)` and `regedit.exe` displays that as well.
                                        "",
                                    ) {
                                        Some(PathBuf::from(sys_prefix).join("python.exe"))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            {
                                if explicit_pythons.contains(&python) {
                                    continue;
                                }
                                if versioned_pythons
                                    .values()
                                    .any(|pythons| pythons.contains(&python))
                                {
                                    continue;
                                }
                                if pex_python_path
                                    .as_ref()
                                    .map(|ppp| !ppp.iter().any(|entry| python.starts_with(entry)))
                                    .unwrap_or_default()
                                {
                                    continue;
                                }
                                if let Some(version) = version {
                                    explicit_pythons.shift_remove(&python);
                                    versioned_pythons
                                        .entry(version)
                                        .or_insert_with(IndexSet::new)
                                        .insert(python);
                                } else {
                                    explicit_pythons.insert(python);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    let mut seen: HashSet<PathBuf> = HashSet::with_capacity(
        explicit_pythons.len()
            + versioned_pythons
                .values()
                .fold(0, |sum, pythons| sum + pythons.len()),
    );
    seen.extend(explicit_pythons.clone());
    seen.extend(versioned_pythons.values().flatten().cloned());
    Box::pin(
        #[coroutine]
        static move || {
            let mut names: IndexSet<&'static str> = IndexSet::new();
            for python_binary_spec in &python_binary_specs {
                if pex_python.is_none() && !explicit_pythons.is_empty() {
                    names.insert(python_binary_spec.name);
                }
                if let Some(pythons) =
                    versioned_pythons.remove(&(python_binary_spec.major, python_binary_spec.minor))
                {
                    for python in pythons {
                        if let Some(pex_python) = pex_python.as_ref() {
                            if let Some(file_name) = python.file_name()
                                && file_name == pex_python
                            {
                                yield python
                            } else {
                                versioned_pythons
                                    .entry((python_binary_spec.major, python_binary_spec.minor))
                                    .or_insert_with(IndexSet::new)
                                    .insert(python);
                            }
                        } else {
                            if let Some(file_stem) = python.file_stem()
                                && let Some(file_stem) = file_stem.to_str()
                                && file_stem.starts_with(python_binary_spec.name)
                            {
                                yield python
                            } else {
                                versioned_pythons
                                    .entry((python_binary_spec.major, python_binary_spec.minor))
                                    .or_insert_with(IndexSet::new)
                                    .insert(python);
                            }
                        }
                    }
                }
            }

            if let Some(name) = pex_python.as_ref() {
                for python in explicit_pythons {
                    if let Some(file_name) = python.file_name()
                        && file_name == name
                    {
                        yield python
                    }
                }
            } else {
                for name in &names {
                    let mut unused = IndexSet::new();
                    for python in explicit_pythons {
                        if let Some(file_stem) = python.file_stem()
                            && let Some(file_stem) = file_stem.to_str()
                            && file_stem.starts_with(name)
                        {
                            yield python
                        } else {
                            unused.insert(python);
                        }
                    }
                    explicit_pythons = unused;
                }
            }

            if let Some(search_path) = pex_python_path.or_else(|| {
                RealSys
                    .env_path()
                    .map(|path| env::split_paths(&path).collect())
            }) {
                for entry in search_path {
                    if let Ok(listing) = entry.read_dir() {
                        for path in listing {
                            if let Ok(file) = path
                                && file
                                    .file_type()
                                    .ok()
                                    .as_ref()
                                    .map(FileType::is_file)
                                    .unwrap_or_default()
                            {
                                let file = file.path();
                                if seen.contains(&file) {
                                    continue;
                                }
                                if !platform::is_executable(&file).ok().unwrap_or_default() {
                                    continue;
                                }
                                if let Some(name) = pex_python.as_ref() {
                                    if let Some(file_name) = file.file_name()
                                        && file_name == name
                                    {
                                        yield file
                                    }
                                } else {
                                    if let Some(file_stem) = file.file_stem()
                                        && let Some(file_stem) = file_stem.to_str()
                                    {
                                        for name in &names {
                                            if file_stem.starts_with(name) {
                                                yield file;
                                                break;
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        },
    )
}

pub(crate) fn iter_possibly_compatible_python_exes(
    constraints: &InterpreterConstraints,
    selection_strategy: SelectionStrategy,
    search_path: SearchPath,
    include_pex_compatible: bool,
) -> anyhow::Result<impl Iterator<Item = PathBuf>> {
    let (pex_python, search_path, known_python_exes) = search_path.into_parts()?;
    let python_binary_specs = calculate_compatible_binary_specs(
        constraints,
        selection_strategy,
        None,
        include_pex_compatible,
    );
    Ok(std::iter::from_coroutine(iter_python_exes(
        constraints,
        python_binary_specs,
        pex_python,
        search_path,
        known_python_exes,
    )))
}
