// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

pub(crate) mod unix;
mod windows;

use std::fmt::{Display, Formatter};
use std::ops::Deref;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::LazyLock;

use anyhow::bail;
use indexmap::IndexSet;
use log::debug;
use pep440_rs::{Operator, Version, VersionSpecifier, VersionSpecifiers};
use pep508_rs::{ExtraName, MarkerTree, PackageName, Requirement, VersionOrUrl};
use python_platform::{CPythonImplementation, PythonImplementation};
use url::Url;

#[cfg(unix)]
use crate::constraints::unix::iter_possibly_compatible_python_exes;
#[cfg(windows)]
use crate::constraints::windows::iter_possibly_compatible_python_exes;
use crate::{Interpreter, SearchPath};

#[derive(Copy, Clone, Debug, Hash, Eq, PartialEq)]
enum InterpreterImplementation {
    CPython,
    CPythonFreeThreaded,
    CPythonGil,
    PyPy,
}

impl InterpreterImplementation {
    fn of(interpreter: &Interpreter) -> Option<Self> {
        if let Some(abi_info) = interpreter.details.cpython_abi_info {
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

    fn matches(&self, other: InterpreterImplementation) -> bool {
        match self {
            InterpreterImplementation::CPython => matches!(
                other,
                InterpreterImplementation::CPython
                    | InterpreterImplementation::CPythonFreeThreaded
                    | InterpreterImplementation::CPythonGil
            ),
            InterpreterImplementation::CPythonFreeThreaded => {
                other == InterpreterImplementation::CPythonFreeThreaded
            }
            InterpreterImplementation::CPythonGil => matches!(
                other,
                InterpreterImplementation::CPython | InterpreterImplementation::CPythonGil
            ),
            InterpreterImplementation::PyPy => other == InterpreterImplementation::PyPy,
        }
    }
}

impl From<PythonImplementation> for InterpreterImplementation {
    fn from(value: PythonImplementation) -> Self {
        match value {
            PythonImplementation::CPython(CPythonImplementation { abi_info, .. }) => {
                match abi_info.free_threaded {
                    Some(free_threaded) => {
                        if free_threaded {
                            InterpreterImplementation::CPythonFreeThreaded
                        } else {
                            InterpreterImplementation::CPythonGil
                        }
                    }
                    None => InterpreterImplementation::CPython,
                }
            }
            PythonImplementation::PyPy(_) => InterpreterImplementation::PyPy,
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
            + CPython: any CPython interpreter\n\
            + CPython+t or CPython[free-threaded]: a free-threaded CPython interpreter\n\
            + CPython-t or CPython[gil]: a traditional GIL-enabled CPython interpreter\n\
            + PyPy: any PyPy interpreter",
        )
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct InterpreterConstraint {
    implementation: Option<InterpreterImplementation>,
    version_specifiers: Option<VersionSpecifiers>,
}

impl InterpreterConstraint {
    pub fn exact_version(interpreter: &Interpreter) -> Self {
        let python_version = Version::new(
            [
                u64::from(interpreter.details.version.major),
                u64::from(interpreter.details.version.minor),
                u64::from(interpreter.details.version.micro),
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

    pub fn parse(constraint: &str) -> anyhow::Result<Self> {
        constraint.parse()
    }

    fn contains(&self, python_implementation: PythonImplementation) -> bool {
        if let Some(implementation) = self.implementation
            && !implementation.matches(python_implementation.into())
        {
            return false;
        }
        self.contains_version(python_implementation.major, python_implementation.minor)
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

impl FromStr for InterpreterConstraint {
    type Err = anyhow::Error;

    fn from_str(constraint: &str) -> Result<Self, Self::Err> {
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

#[derive(Debug)]
pub enum VersionSpec {
    MajorMinor(u8, u8),
    Major(u8),
}

#[derive(Debug)]
pub struct InterpreterConstraints(Vec<InterpreterConstraint>);

impl InterpreterConstraints {
    pub const EMPTY: Self = Self(vec![]);

    pub fn try_from<S: AsRef<str>>(constraints: &[S]) -> anyhow::Result<Self> {
        Ok(Self(
            constraints
                .iter()
                .map(|constraint| constraint.as_ref().parse())
                .collect::<anyhow::Result<Vec<_>>>()?,
        ))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn into_constraints(self) -> Vec<InterpreterConstraint> {
        self.0
    }

    pub fn as_slice(&self) -> &[InterpreterConstraint] {
        self.0.as_slice()
    }

    pub fn contains(&self, python_implementation: PythonImplementation) -> bool {
        self.0.is_empty()
            || self
                .0
                .iter()
                .any(|constraint| constraint.contains(python_implementation))
    }

    pub fn contains_version(&self, major: u8, minor: u8) -> bool {
        self.0.is_empty()
            || self
                .0
                .iter()
                .any(|constraint| constraint.contains_version(major, minor))
    }

    pub fn iter_possibly_compatible_python_exes(
        &self,
        selection_strategy: SelectionStrategy,
        search_path: SearchPath,
        include_pex_compatible: bool,
    ) -> anyhow::Result<impl Iterator<Item = PathBuf>> {
        iter_possibly_compatible_python_exes(
            self,
            selection_strategy,
            search_path,
            include_pex_compatible,
        )
    }
}

impl From<Vec<InterpreterConstraint>> for InterpreterConstraints {
    fn from(value: Vec<InterpreterConstraint>) -> Self {
        Self(value)
    }
}

#[derive(Hash, Eq, PartialEq)]
struct PythonBinarySpec {
    name: &'static str,
    major: u8,
    minor: u8,
    suffix: Option<&'static str>,
}

fn calculate_compatible_binary_specs(
    constraints: &InterpreterConstraints,
    selection_strategy: SelectionStrategy,
    preferred_interpreter: Option<PythonImplementation>,
    include_pex_compatible: bool,
) -> IndexSet<PythonBinarySpec> {
    let mut binary_specs: IndexSet<PythonBinarySpec> = IndexSet::new();
    if let Some(interpreter) = preferred_interpreter
        && constraints.contains(interpreter)
    {
        insert_specs(
            &mut binary_specs,
            Some(InterpreterImplementation::from(interpreter)),
            interpreter.major,
            interpreter.minor,
        );
    }
    let constraints = constraints.as_slice();
    let versions = match selection_strategy {
        SelectionStrategy::Oldest => &SUPPORTED_VERSIONS,
        SelectionStrategy::Newest => &SUPPORTED_VERSIONS_NEWEST_FIRST,
    };
    for (major, minor) in versions.iter() {
        if constraints.is_empty() && !include_pex_compatible {
            insert_specs(&mut binary_specs, None, *major, *minor);
        } else {
            for constraint in constraints {
                if constraint.contains_version(*major, *minor) {
                    insert_specs(&mut binary_specs, constraint.implementation, *major, *minor);
                }
            }
        }
    }
    if include_pex_compatible {
        for (major, minor) in SUPPORTED_VERSIONS_NEWEST_FIRST.iter() {
            insert_specs(&mut binary_specs, None, *major, *minor);
        }
    }
    binary_specs
}

fn insert_specs(
    binary_specs: &mut IndexSet<PythonBinarySpec>,
    implementation: Option<InterpreterImplementation>,
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use pep440_rs::VersionSpecifiers;

    use crate::constraints::{InterpreterConstraint, InterpreterImplementation};

    #[test]
    fn test_parse_interpreter_constraint() {
        assert_eq!(
            InterpreterConstraint {
                implementation: None,
                version_specifiers: Some(VersionSpecifiers::from_str(">=3.14").unwrap())
            },
            ">=3.14".parse().unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPython),
                version_specifiers: None
            },
            "CPython".parse().unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonFreeThreaded),
                version_specifiers: None
            },
            "CPython+t".parse().unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonFreeThreaded),
                version_specifiers: Some(VersionSpecifiers::from_str("==3.15.*").unwrap())
            },
            "CPython+t==3.15.*".parse().unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonGil),
                version_specifiers: None
            },
            "CPython-t".parse().unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonGil),
                version_specifiers: Some(VersionSpecifiers::from_str("==3.13.*").unwrap())
            },
            "CPython-t==3.13.*".parse().unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonFreeThreaded),
                version_specifiers: None
            },
            "CPython[free-threaded]".parse().unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::CPythonGil),
                version_specifiers: None
            },
            "CPython[gil]".parse().unwrap()
        );
        assert_eq!(
            InterpreterConstraint {
                implementation: Some(InterpreterImplementation::PyPy),
                version_specifiers: None
            },
            "PyPy".parse().unwrap()
        );
    }
}
