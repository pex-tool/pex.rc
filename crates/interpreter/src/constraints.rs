// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::ffi::OsString;
use std::fmt::{Display, Formatter};
use std::ops::Deref;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::LazyLock;

use anyhow::bail;
use indexmap::{IndexMap, IndexSet};
use log::debug;
use pep440_rs::{Operator, Version, VersionSpecifier, VersionSpecifiers};
use pep508_rs::{ExtraName, MarkerTree, PackageName, Requirement, VersionOrUrl};
use url::Url;
use which::sys::{RealSys, Sys};
use which::which_in_global;

use crate::{Interpreter, SearchPath};

#[derive(Debug, Hash, Eq, PartialEq)]
enum InterpreterImplementation {
    CPython,
    CPythonFreeThreaded,
    CPythonGil,
    PyPy,
}

impl InterpreterImplementation {
    fn of(interpreter: &Interpreter) -> Option<Self> {
        if let Some(abi_info) = interpreter.raw().cpython_abi_info {
            if let Some(free_threaded) = abi_info.free_threaded {
                if free_threaded {
                    Some(Self::CPythonFreeThreaded)
                } else {
                    Some(Self::CPythonGil)
                }
            } else {
                Some(Self::CPython)
            }
        } else {
            Some(Self::PyPy)
        }
    }

    fn matches(&self, other: &InterpreterImplementation) -> bool {
        match self {
            InterpreterImplementation::CPython => matches!(
                other,
                InterpreterImplementation::CPython
                    | InterpreterImplementation::CPythonFreeThreaded
                    | InterpreterImplementation::CPythonGil
            ),
            InterpreterImplementation::CPythonFreeThreaded => {
                *other == InterpreterImplementation::CPythonFreeThreaded
            }
            InterpreterImplementation::CPythonGil => matches!(
                other,
                InterpreterImplementation::CPython | InterpreterImplementation::CPythonGil
            ),
            InterpreterImplementation::PyPy => *other == InterpreterImplementation::PyPy,
        }
    }
}

impl InterpreterImplementation {
    fn parse(name: &PackageName, extras: &[ExtraName], source: &str) -> anyhow::Result<Self> {
        if name.as_ref() == "pypy" && extras.is_empty() {
            return Ok(Self::PyPy);
        } else if name.as_ref() == "cpython" {
            if extras.is_empty() {
                return Ok(Self::CPython);
            } else if extras.len() == 1 && extras[0].as_ref() == "free-threaded" {
                return Ok(Self::CPythonFreeThreaded);
            } else if extras.len() == 1 && extras[0].as_ref() == "gil" {
                return Ok(Self::CPythonGil);
            }
        }
        bail!(
            "Invalid interpreter implementation in: {source}\n\
            Only the following are recognized:\n\
            + PyPy: any PyPy interpreter\n\
            + CPython: any CPython interpreter\n\
            + CPython+t or CPython[free-threaded]: a free-threaded CPython interpreter\n\
            + CPython-t or CPython[gil]: a traditional GIL-enabled CPython interpreter",
        )
    }
}

#[derive(Debug, Eq, PartialEq)]
pub struct InterpreterConstraint {
    implementation: Option<InterpreterImplementation>,
    version_specifiers: Option<VersionSpecifiers>,
}

impl InterpreterConstraint {
    pub fn exact_version(interpreter: &Interpreter) -> Self {
        let python_version = Version::new(
            [
                u64::from(interpreter.raw().version.major),
                u64::from(interpreter.raw().version.minor),
                u64::from(interpreter.raw().version.micro),
            ]
            .iter(),
        );
        let version_specifier = VersionSpecifier::from_version(Operator::Equal, python_version)
            .expect("An exact version specifier is always valid.");
        Self {
            implementation: InterpreterImplementation::of(interpreter),
            version_specifiers: Some(VersionSpecifiers::from_iter([version_specifier])),
        }
    }

    fn parse(constraint: &str) -> anyhow::Result<Self> {
        if let Ok(version_specifiers) = VersionSpecifiers::from_str(constraint) {
            return Ok(Self {
                implementation: None,
                version_specifiers: Some(version_specifiers),
            });
        }

        for (prefix, implementation) in [
            ("CPython+t", InterpreterImplementation::CPythonFreeThreaded),
            ("CPython-t", InterpreterImplementation::CPythonGil),
        ] {
            if let Some(suffix) = constraint.strip_prefix(prefix) {
                let version_specifiers = if suffix.is_empty() {
                    None
                } else {
                    Some(VersionSpecifiers::from_str(suffix)?)
                };
                return Ok(Self {
                    implementation: Some(implementation),
                    version_specifiers,
                });
            }
        }

        let requirement: Requirement<Url> = Requirement::from_str(constraint)?;
        if requirement.marker != MarkerTree::default() {
            bail!(
                "Marker expressions are not supported in interpreter constraints; \
                given: {constraint}"
            );
        }

        let implementation =
            InterpreterImplementation::parse(&requirement.name, &requirement.extras, constraint)?;
        if let Some(version_or_url) = requirement.version_or_url {
            match version_or_url {
                VersionOrUrl::Url(_url) => bail!(
                    "Direct reference URLs are not supported for interpreter constraints, \
                    version specifiers can be used to restrict interpreter versions instead; \
                    given: {constraint}"
                ),
                VersionOrUrl::VersionSpecifier(version_specifiers) => Ok(Self {
                    implementation: Some(implementation),
                    version_specifiers: Some(version_specifiers),
                }),
            }
        } else {
            Ok(Self {
                implementation: Some(implementation),
                version_specifiers: None,
            })
        }
    }

    fn contains(&self, interpreter: &Interpreter) -> bool {
        if let Some(implementation) = self.implementation.as_ref() {
            if let Some(other_implementation) = InterpreterImplementation::of(interpreter) {
                if !implementation.matches(&other_implementation) {
                    return false;
                }
            } else {
                return false;
            }
        }
        self.contains_version(
            interpreter.raw().version.major,
            interpreter.raw().version.minor,
        )
    }

    fn contains_version(&self, major: u8, minor: u8) -> bool {
        if let Some(version_specifiers) = self.version_specifiers.as_ref() {
            let version = Version::new([u64::from(major), u64::from(minor)]);
            return version_specifiers.contains(&version);
        }
        true
    }

    pub fn version_specifiers(&self) -> Option<&VersionSpecifiers> {
        self.version_specifiers.as_ref()
    }
}

impl Display for InterpreterConstraint {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(implementation) = self.implementation.as_ref() {
            match implementation {
                InterpreterImplementation::CPython => f.write_str("CPython")?,
                InterpreterImplementation::CPythonFreeThreaded => f.write_str("CPython+t")?,
                InterpreterImplementation::CPythonGil => f.write_str("CPython-t")?,
                InterpreterImplementation::PyPy => f.write_str("PyPy")?,
            }
        }
        if let Some(version_specifiers) = self.version_specifiers.as_ref() {
            write!(f, "{version_specifiers}")?;
        }
        Ok(())
    }
}

static SUPPORTED_VERSIONS: LazyLock<Vec<(u8, u8)>> = LazyLock::new(|| {
    let max_minor = {
        let (_, minor) = crate::version::LATEST_STABLE.deref();
        // Give a 1-year buffer to account for testing the next release.
        minor + 1
    };
    [(2, 7)]
        .into_iter()
        .chain((5..=max_minor).map(|minor| (3, minor)))
        .collect()
});

static SUPPORTED_VERSIONS_NEWEST_FIRST: LazyLock<Vec<(u8, u8)>> = LazyLock::new(|| {
    let mut supported_versions = SUPPORTED_VERSIONS.clone();
    supported_versions.reverse();
    supported_versions
});

#[derive(Eq, PartialEq)]
pub enum SelectionStrategy {
    Oldest,
    Newest,
}

#[derive(Hash, Eq, PartialEq)]
struct PythonBinarySpec {
    name: &'static str,
    major: u8,
    minor: u8,
    suffix: Option<&'static str>,
}

pub enum VersionSpec {
    MajorMinor(u8, u8),
    Major(u8),
}

pub struct InterpreterConstraints(Vec<InterpreterConstraint>);

impl InterpreterConstraints {
    pub fn try_from<S: AsRef<str>>(constraints: &[S]) -> anyhow::Result<Self> {
        Ok(Self(
            constraints
                .iter()
                .map(|constraint| InterpreterConstraint::parse(constraint.as_ref()))
                .collect::<anyhow::Result<Vec<_>>>()?,
        ))
    }

    pub fn into_constraints(self) -> Vec<InterpreterConstraint> {
        self.0
    }

    pub fn contains(&self, interpreter: &Interpreter) -> bool {
        self.0.is_empty()
            || self
                .0
                .iter()
                .any(|constraint| constraint.contains(interpreter))
    }

    fn calculate_compatible_binary_specs(
        &self,
        selection_strategy: SelectionStrategy,
        preferred_interpreter: Option<&Interpreter>,
    ) -> IndexSet<PythonBinarySpec> {
        let mut binary_specs: IndexSet<PythonBinarySpec> = IndexSet::new();
        if let Some(interpreter) = preferred_interpreter
            && self.contains(interpreter)
        {
            let implementation = InterpreterImplementation::of(interpreter);
            insert_specs(
                &mut binary_specs,
                implementation.as_ref(),
                interpreter.raw().version.major,
                interpreter.raw().version.minor,
            );
        }
        if !self.0.is_empty() {
            let versions = match selection_strategy {
                SelectionStrategy::Oldest => &SUPPORTED_VERSIONS,
                SelectionStrategy::Newest => &SUPPORTED_VERSIONS_NEWEST_FIRST,
            };
            for (major, minor) in versions.iter() {
                for constraint in &self.0 {
                    if constraint.contains_version(*major, *minor) {
                        insert_specs(
                            &mut binary_specs,
                            constraint.implementation.as_ref(),
                            *major,
                            *minor,
                        );
                    }
                }
            }
        }
        for (major, minor) in SUPPORTED_VERSIONS_NEWEST_FIRST.iter() {
            insert_specs(&mut binary_specs, None, *major, *minor);
        }
        binary_specs
    }

    pub fn calculate_compatible_binary_names(
        &self,
        selection_strategy: SelectionStrategy,
        preferred_interpreter: Option<&Interpreter>,
    ) -> IndexMap<OsString, Option<VersionSpec>> {
        let binary_specs =
            self.calculate_compatible_binary_specs(selection_strategy, preferred_interpreter);
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

    pub fn iter_possibly_compatible_python_exes(
        &self,
        selection_strategy: SelectionStrategy,
        search_path: SearchPath,
    ) -> anyhow::Result<impl Iterator<Item = PathBuf>> {
        let (python, path, known_paths) = search_path.into_parts()?;
        let binary_names = if let Some(python) = python {
            vec![python]
        } else {
            self.calculate_compatible_binary_names(selection_strategy, None)
                .into_keys()
                .collect()
        };
        Ok(PythonExeIter {
            known_paths,
            path,
            binary_names: binary_names.into_iter(),
            which_fn: which_in_global,
            binary_paths: None,
            seen: HashSet::new(),
        })
    }
}

fn insert_specs(
    binary_specs: &mut IndexSet<PythonBinarySpec>,
    implementation: Option<&InterpreterImplementation>,
    major: u8,
    minor: u8,
) {
    match implementation {
        None => {
            binary_specs.insert(PythonBinarySpec {
                name: "python",
                major,
                minor,
                suffix: None,
            });
            if (major, minor) >= (3, 13) {
                binary_specs.insert(PythonBinarySpec {
                    name: "python",
                    major,
                    minor,
                    suffix: Some("t"),
                });
            }
            binary_specs.insert(PythonBinarySpec {
                name: "pypy",
                major,
                minor,
                suffix: None,
            });
        }
        Some(implementation) => match implementation {
            InterpreterImplementation::CPython | InterpreterImplementation::CPythonGil => {
                binary_specs.insert(PythonBinarySpec {
                    name: "python",
                    major,
                    minor,
                    suffix: None,
                });
            }
            InterpreterImplementation::CPythonFreeThreaded => {
                if (major, minor) >= (3, 13) {
                    binary_specs.insert(PythonBinarySpec {
                        name: "python",
                        major,
                        minor,
                        suffix: Some("t"),
                    });
                } else {
                    debug!(
                        "Ignoring free-threaded constraint for CPython {major}.{minor} since \
                        free-threaded CPython only exists for >=3.13."
                    );
                }
            }
            InterpreterImplementation::PyPy => {
                binary_specs.insert(PythonBinarySpec {
                    name: "pypy",
                    major,
                    minor,
                    suffix: None,
                });
            }
        },
    }
}

struct PythonExeIter<
    KnownBinaryPaths: Iterator<Item = PathBuf>,
    Name,
    BinaryNames: Iterator<Item = Name>,
    BinaryPaths: Iterator<Item = PathBuf>,
    WhichError,
    WhichFunction: Fn(Name, Option<OsString>) -> Result<BinaryPaths, WhichError>,
> {
    known_paths: Option<KnownBinaryPaths>,
    path: Option<OsString>,
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
            if let Some(known_paths) = self.known_paths.as_mut() {
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
                    self.known_paths = None;
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
                    self.path.clone().or_else(|| RealSys.env_path()),
                )
            {
                self.binary_paths = Some(binary_paths);
            } else {
                return None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::str::FromStr;

    use indexmap::IndexSet;
    use pep440_rs::VersionSpecifiers;

    use crate::constraints::{InterpreterConstraint, InterpreterImplementation};
    use crate::{InterpreterConstraints, SelectionStrategy};

    #[test]
    fn test_parse_interpreter_constraint() {
        assert_eq!(
            InterpreterConstraint {
                implementation: None,
                version_specifiers: Some(VersionSpecifiers::from_str(">=3.14").unwrap())
            },
            InterpreterConstraint::parse(">=3.14").unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPython),
                version_specifiers: None
            },
            InterpreterConstraint::parse("CPython").unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonFreeThreaded),
                version_specifiers: None
            },
            InterpreterConstraint::parse("CPython+t").unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonFreeThreaded),
                version_specifiers: Some(VersionSpecifiers::from_str("==3.15.*").unwrap())
            },
            InterpreterConstraint::parse("CPython+t==3.15.*").unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonGil),
                version_specifiers: None
            },
            InterpreterConstraint::parse("CPython-t").unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonGil),
                version_specifiers: Some(VersionSpecifiers::from_str("==3.13.*").unwrap())
            },
            InterpreterConstraint::parse("CPython-t==3.13.*").unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonFreeThreaded),
                version_specifiers: None
            },
            InterpreterConstraint::parse("CPython[free-threaded]").unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonGil),
                version_specifiers: None
            },
            InterpreterConstraint::parse("CPython[gil]").unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::PyPy),
                version_specifiers: None
            },
            InterpreterConstraint::parse("PyPy").unwrap()
        );
    }

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
        let binary_names = ics
            .calculate_compatible_binary_names(SelectionStrategy::Oldest, None)
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
        let binary_names = ics
            .calculate_compatible_binary_names(SelectionStrategy::Newest, None)
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

        let binary_names = ics
            .calculate_compatible_binary_names(SelectionStrategy::Newest, None)
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

        let expexcted_per_pexrc_major_minor = EXPECTED_PER_PEXRC_MAJOR_MINOR
            .into_iter()
            .filter(|name| !expected_per_ics.contains(name))
            .copied()
            .collect::<Vec<_>>();

        let major_minor_start = binary_names.len()
            - EXPECTED_PER_PEXRC_REST.len()
            - expexcted_per_pexrc_major_minor.len();
        let rest_start = binary_names.len() - EXPECTED_PER_PEXRC_REST.len();

        assert_eq!(expected_per_ics, &binary_names[..expected_per_ics.len()]);
        assert_eq!(
            expexcted_per_pexrc_major_minor,
            &binary_names[major_minor_start..rest_start]
        );
        assert_eq!(EXPECTED_PER_PEXRC_REST, &binary_names[rest_start..]);

        let binary_names = ics
            .calculate_compatible_binary_names(SelectionStrategy::Oldest, None)
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
            expexcted_per_pexrc_major_minor,
            &binary_names[major_minor_start..rest_start]
        );
        assert_eq!(expected_shared, &binary_names[rest_start..]);
    }
}
