// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;

use indexmap::IndexMap;
use python_platform::PythonImplementation;

use crate::constraints::calculate_compatible_binary_specs;
use crate::{InterpreterConstraints, SelectionStrategy, VersionSpec};

#[cfg(unix)]
pub(crate) fn iter_possibly_compatible_python_exes(
    constraints: &InterpreterConstraints,
    selection_strategy: SelectionStrategy,
    search_path: crate::SearchPath,
    include_pex_compatible: bool,
) -> anyhow::Result<impl Iterator<Item = std::path::PathBuf>> {
    _unix::iter_possibly_compatible_python_exes(
        constraints,
        selection_strategy,
        search_path,
        include_pex_compatible,
    )
}

// N.B.: We need to be able to caclculate unix Python executable names from windows when
// cross-building / injecting a PEX that will target unix with a --sh-boot header.
pub fn calculate_compatible_binary_names(
    constraints: &InterpreterConstraints,
    selection_strategy: SelectionStrategy,
    preferred_interpreter: Option<PythonImplementation>,
    include_pex_compatible: bool,
) -> IndexMap<OsString, Option<VersionSpec>> {
    let binary_specs = calculate_compatible_binary_specs(
        constraints,
        selection_strategy,
        preferred_interpreter,
        include_pex_compatible,
    );
    let mut binary_names: IndexMap<OsString, Option<VersionSpec>> = IndexMap::new();
    for binary_spec in &binary_specs {
        binary_names.insert(
            format!(
                "{name}{major}.{minor}{suffix}",
                name = binary_spec.name,
                major = binary_spec.major,
                minor = binary_spec.minor,
                suffix = binary_spec.suffix.unwrap_or("")
            )
            .into(),
            Some(VersionSpec::MajorMinor(
                binary_spec.major,
                binary_spec.minor,
            )),
        );
    }
    for binary_spec in &binary_specs {
        binary_names.insert(
            format!(
                "{name}{major}",
                name = binary_spec.name,
                major = binary_spec.major
            )
            .into(),
            Some(VersionSpec::Major(binary_spec.major)),
        );
    }
    for binary_spec in &binary_specs {
        binary_names.insert(binary_spec.name.into(), None);
    }
    binary_names
}

#[cfg(unix)]
mod _unix {
    use std::collections::HashSet;
    use std::ffi::OsString;
    use std::path::PathBuf;

    use which::sys::{RealSys, Sys};
    use which::which_in_global;

    use super::calculate_compatible_binary_names;
    use crate::{InterpreterConstraints, SearchPath, SelectionStrategy};

    pub(super) fn iter_possibly_compatible_python_exes(
        constraints: &InterpreterConstraints,
        selection_strategy: SelectionStrategy,
        search_path: SearchPath,
        include_pex_compatible: bool,
    ) -> anyhow::Result<impl Iterator<Item = PathBuf>> {
        let (python, search_path, known_python_exes) = search_path.into_parts()?;
        let binary_names = if let Some(python) = python {
            vec![python]
        } else {
            calculate_compatible_binary_names(
                constraints,
                selection_strategy,
                None,
                include_pex_compatible,
            )
            .into_keys()
            .collect()
        };
        Ok(PythonExeIter {
            known_python_exes,
            search_path,
            binary_names: binary_names.into_iter(),
            which_fn: which_in_global,
            binary_paths: None,
            seen: HashSet::new(),
        })
    }

    struct PythonExeIter<
        KnownPythonExes: Iterator<Item = PathBuf>,
        Name,
        BinaryNames: Iterator<Item = Name>,
        BinaryPaths: Iterator<Item = PathBuf>,
        WhichError,
        WhichFunction: Fn(Name, Option<OsString>) -> Result<BinaryPaths, WhichError>,
    > {
        known_python_exes: Option<KnownPythonExes>,
        search_path: Option<OsString>,
        binary_names: BinaryNames,
        which_fn: WhichFunction,
        binary_paths: Option<BinaryPaths>,
        seen: HashSet<PathBuf>,
    }

    impl<
        KnownBinaryPaths: Iterator<Item = PathBuf>,
        BinaryNames: Iterator<Item = OsString>,
        BinaryPaths: Iterator<Item = PathBuf>,
        WhichError,
        WhichFunction: Fn(OsString, Option<OsString>) -> Result<BinaryPaths, WhichError>,
    > Iterator
        for PythonExeIter<
            KnownBinaryPaths,
            OsString,
            BinaryNames,
            BinaryPaths,
            WhichError,
            WhichFunction,
        >
    {
        type Item = PathBuf;

        fn next(&mut self) -> Option<Self::Item> {
            loop {
                if let Some(known_paths) = self.known_python_exes.as_mut() {
                    if let Some(binary_path) = known_paths.next() {
                        if let Ok(real_binary_path) = binary_path.canonicalize() {
                            if self.seen.contains(real_binary_path.as_path()) {
                                continue;
                            }
                            self.seen.insert(real_binary_path.clone());
                            return Some(real_binary_path);
                        } else {
                            // E.G: A broken symbolic link.
                            continue;
                        }
                    } else {
                        self.known_python_exes = None;
                    }
                } else if let Some(binary_paths) = self.binary_paths.as_mut() {
                    if let Some(binary_path) = binary_paths.next() {
                        if let Ok(real_binary_path) = binary_path.canonicalize() {
                            if self.seen.contains(real_binary_path.as_path()) {
                                continue;
                            }
                            self.seen.insert(real_binary_path.clone());
                            return Some(real_binary_path);
                        } else {
                            // E.G: A broken symbolic link.
                            continue;
                        }
                    } else {
                        self.binary_paths = None;
                    }
                } else if let Some(binary_name) = self.binary_names.next()
                    && let Ok(binary_paths) = (self.which_fn)(
                        binary_name,
                        self.search_path.clone().or_else(|| RealSys.env_path()),
                    )
                {
                    self.binary_paths = Some(binary_paths);
                } else {
                    return None;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use indexmap::IndexSet;

    use crate::constraints::unix::calculate_compatible_binary_names;
    use crate::{InterpreterConstraints, SelectionStrategy};

    fn os_str(value: &str) -> &OsStr {
        // SAFETY: Tests use ascii.
        unsafe { OsStr::from_encoded_bytes_unchecked(value.as_bytes()) }
    }

    // N.B.: As time advances, the supported set by PEXrc will steadily add python3.16, etc to
    // the top of this list per https://peps.python.org/pep-0602/ (c.f. code above); so we just
    // check a known run as of this test writing.
    const EXPECTED_PER_PEXRC_MAJOR_MINOR: &[&str] = &[
        "python3.15",
        "python3.15t",
        "pypy3.15",
        "python3.14",
        "python3.14t",
        "pypy3.14",
        "python3.13",
        "python3.13t",
        "pypy3.13",
        "python3.12",
        "pypy3.12",
        "python3.11",
        "pypy3.11",
        "python3.10",
        "pypy3.10",
        "python3.9",
        "pypy3.9",
        "python3.8",
        "pypy3.8",
        "python3.7",
        "pypy3.7",
        "python3.6",
        "pypy3.6",
        "python3.5",
        "pypy3.5",
        "python2.7",
        "pypy2.7",
    ];

    const EXPECTED_PER_PEXRC_REST: &[&str] =
        &["python3", "pypy3", "python2", "pypy2", "python", "pypy"];

    #[test]
    fn test_interpreter_constraints_binary_names_all_default_order() {
        let ics = InterpreterConstraints::try_from::<&str>(&[]).unwrap();
        let binary_names =
            calculate_compatible_binary_names(&ics, SelectionStrategy::Oldest, None, true)
                .into_keys()
                .collect::<IndexSet<_>>();

        let major_minor_start = binary_names.len()
            - EXPECTED_PER_PEXRC_REST.len()
            - EXPECTED_PER_PEXRC_MAJOR_MINOR.len();
        let rest_start = binary_names.len() - EXPECTED_PER_PEXRC_REST.len();

        assert_eq!(
            EXPECTED_PER_PEXRC_MAJOR_MINOR,
            &binary_names[major_minor_start..rest_start]
        );
        assert_eq!(EXPECTED_PER_PEXRC_REST, &binary_names[rest_start..]);
    }

    #[test]
    fn test_interpreter_constraints_binary_names_all_newest_first() {
        let ics = InterpreterConstraints::try_from::<&str>(&[]).unwrap();
        let binary_names =
            calculate_compatible_binary_names(&ics, SelectionStrategy::Newest, None, true)
                .into_keys()
                .collect::<IndexSet<_>>();

        assert!(
            binary_names.get_index_of(os_str("python3.15"))
                < binary_names.get_index_of(os_str("pypy3.15"))
        );
        assert!(
            binary_names.get_index_of(os_str("pypy3.15"))
                < binary_names.get_index_of(os_str("python3.14"))
        );
        assert!(
            binary_names.get_index_of(os_str("python3.14"))
                < binary_names.get_index_of(os_str("pypy3.14"))
        );
        assert!(
            binary_names.get_index_of(os_str("pypy3.14"))
                < binary_names.get_index_of(os_str("python2.7"))
        );
        assert_eq!(
            &[
                "python2.7",
                "pypy2.7",
                "python3",
                "pypy3",
                "python2",
                "pypy2",
                "python",
                "pypy"
            ],
            &binary_names[binary_names.len() - 8..]
        );
    }

    #[test]
    fn test_interpreter_constraints_complex() {
        let ics = InterpreterConstraints::try_from::<&str>(&[
            "CPython+t==3.15.*",
            "CPython[free-threaded]==3.14.*",
            "CPython-t==3.13.*",
            "CPython[gil]==3.12.*",
            "PyPy>=3.9,<3.12",
        ])
        .unwrap();

        let binary_names =
            calculate_compatible_binary_names(&ics, SelectionStrategy::Newest, None, true)
                .into_keys()
                .collect::<Vec<_>>();

        let expected_per_ics = &[
            "python3.15t",
            "python3.14t",
            "python3.13",
            "python3.12",
            "pypy3.11",
            "pypy3.10",
            "pypy3.9",
        ];

        let expected_per_pexrc_major_minor = EXPECTED_PER_PEXRC_MAJOR_MINOR
            .into_iter()
            .filter(|name| !expected_per_ics.contains(name))
            .copied()
            .collect::<Vec<_>>();

        let major_minor_start = binary_names.len()
            - EXPECTED_PER_PEXRC_REST.len()
            - expected_per_pexrc_major_minor.len();
        let rest_start = binary_names.len() - EXPECTED_PER_PEXRC_REST.len();

        assert_eq!(expected_per_ics, &binary_names[..expected_per_ics.len()]);
        assert_eq!(
            expected_per_pexrc_major_minor,
            &binary_names[major_minor_start..rest_start]
        );
        assert_eq!(EXPECTED_PER_PEXRC_REST, &binary_names[rest_start..]);

        let binary_names =
            calculate_compatible_binary_names(&ics, SelectionStrategy::Oldest, None, true)
                .into_keys()
                .collect::<Vec<_>>();

        let expected_per_ics = &[
            "pypy3.9",
            "pypy3.10",
            "pypy3.11",
            "python3.12",
            "python3.13",
            "python3.14t",
            "python3.15t",
        ];
        let expected_shared = &["pypy3", "python3", "python2", "pypy2", "pypy", "python"];

        let rest_start = binary_names.len() - expected_shared.len();

        assert_eq!(expected_per_ics, &binary_names[..expected_per_ics.len()]);
        assert_eq!(
            expected_per_pexrc_major_minor,
            &binary_names[major_minor_start..rest_start]
        );
        assert_eq!(expected_shared, &binary_names[rest_start..]);
    }
}
