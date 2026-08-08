// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};
use std::ops::Deref;
use std::str::FromStr;
use std::sync::LazyLock;

use anyhow::bail;
use regex::{Regex, RegexBuilder};
use serde::{Deserialize, Serialize};
use serde_with::{DeserializeFromStr, SerializeDisplay};

use crate::implementation::Implementation;

#[derive(
    Copy,
    Clone,
    Debug,
    Default,
    Eq,
    PartialEq,
    Ord,
    PartialOrd,
    Hash,
    DeserializeFromStr,
    SerializeDisplay,
)]
pub enum ReleaseLevel {
    #[default]
    Final,
    Rc,
    Beta,
    Alpha,
}

impl FromStr for ReleaseLevel {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Ok(match s {
            "final" => Self::Final,
            "candidate" => Self::Rc,
            "beta" | "b" => Self::Beta,
            "alpha" | "a" => Self::Alpha,
            _ => bail!("Not a recognized CPython releaselevel: {s}"),
        })
    }
}

impl Display for ReleaseLevel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ReleaseLevel::Final => "final",
            ReleaseLevel::Rc => "rc",
            ReleaseLevel::Beta => "b",
            ReleaseLevel::Alpha => "a",
        })
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, Deserialize, Serialize)]
pub struct PythonVersion {
    pub major: u8,
    pub minor: u8,
    pub micro: u8,
    pub releaselevel: ReleaseLevel,
    pub serial: u8,
}

impl Display for PythonVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{major}.{minor}.{micro}",
            major = self.major,
            minor = self.minor,
            micro = self.micro
        )?;
        match (self.releaselevel, self.serial) {
            (ReleaseLevel::Alpha, serial) => write!(f, "a{serial}")?,
            (ReleaseLevel::Beta, serial) => write!(f, "b{serial}")?,
            (ReleaseLevel::Rc, serial) => write!(f, "rc{serial}")?,
            (ReleaseLevel::Final, _) => {}
        }
        Ok(())
    }
}

impl PythonVersion {
    pub const fn simple(major: u8, minor: u8) -> Self {
        Self {
            major,
            minor,
            micro: 0,
            releaselevel: ReleaseLevel::Final,
            serial: 0,
        }
    }

    pub fn new(major: u8, minor: u8, micro: Option<u8>) -> Self {
        Self {
            major,
            minor,
            micro: micro.unwrap_or_default(),
            releaselevel: ReleaseLevel::Final,
            serial: 0,
        }
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct CPythonAbiInfo {
    pub free_threaded: Option<bool>,
    pub debug: bool,
    pub pymalloc: Option<bool>,
    pub ucs4: Option<bool>,
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct CPythonImplementation {
    pub version: PythonVersion,
    pub abi_info: CPythonAbiInfo,
}

impl Deref for CPythonImplementation {
    type Target = PythonVersion;

    fn deref(&self) -> &Self::Target {
        &self.version
    }
}

impl CPythonImplementation {
    pub fn free_threaded(&self) -> bool {
        self.abi_info.free_threaded.unwrap_or_default()
    }

    pub fn debug(&self) -> bool {
        self.abi_info.debug
    }

    pub fn pymalloc(&self) -> bool {
        self.abi_info.pymalloc.unwrap_or_default()
    }

    pub fn ucs4(&self) -> bool {
        self.abi_info.ucs4.unwrap_or_default()
    }
}

#[derive(Copy, Clone, Debug, Default, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct PyPyVersion(u8, u8, u8);

impl PyPyVersion {
    pub fn major(&self) -> u8 {
        self.0
    }

    pub fn minor(&self) -> u8 {
        self.1
    }

    pub fn patch(&self) -> u8 {
        self.2
    }
}

impl Display for PyPyVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{major}.{minor}.{patch}",
            major = self.0,
            minor = self.1,
            patch = self.2
        )
    }
}

impl FromStr for PyPyVersion {
    type Err = anyhow::Error;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut components = value.split(".");
        let major = components
            .next()
            .expect("A split always yields one component")
            .parse::<u8>()?;
        let minor = if let Some(minor) = components.next() {
            minor.parse::<u8>()?
        } else {
            0
        };
        let patch = if let Some(patch) = components.next() {
            patch.parse::<u8>()?
        } else {
            0
        };
        Ok(PyPyVersion(major, minor, patch))
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub struct PyPyImplementation {
    pub version: PythonVersion,
    pub pypy_version: Option<PyPyVersion>,
}

impl Deref for PyPyImplementation {
    type Target = PythonVersion;

    fn deref(&self) -> &Self::Target {
        &self.version
    }
}

#[derive(Copy, Clone)]
pub enum PythonImplementation {
    CPython(CPythonImplementation),
    PyPy(PyPyImplementation),
}

impl Deref for PythonImplementation {
    type Target = PythonVersion;

    fn deref(&self) -> &Self::Target {
        match self {
            PythonImplementation::CPython(CPythonImplementation { version, .. })
            | PythonImplementation::PyPy(PyPyImplementation { version, .. }) => version,
        }
    }
}

pub(crate) fn parse(
    implementation: Implementation,
    version: &str,
) -> anyhow::Result<PythonImplementation> {
    Ok(match implementation {
        Implementation::CPython => {
            PythonImplementation::CPython(parse_cpython_implementation(version)?)
        }
        Implementation::PyPy => PythonImplementation::PyPy(parse_pypy_implementation(version)?),
    })
}

static PYTHON_VERSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(
        r"^
(?<major_minor>\d+\.\d+)
(?:
    \.(?<micro>\d+)
    (?:
        (?<release>a|b|rc|final)(?<serial>\d+)
    )?
)?
$
",
    )
    .ignore_whitespace(true)
    .build()
    .expect("This is a known valid regex.")
});

fn parse_python_version(value: &str) -> anyhow::Result<PythonVersion> {
    if let Some(captures) = PYTHON_VERSION_REGEX.captures(value) {
        let mut major_minor = captures
            .name("major_minor")
            .expect("The major_minor capture was required.")
            .as_str()
            .split(".");
        let major = major_minor
            .next()
            .expect("We captured major")
            .parse::<u8>()?;
        let minor = major_minor
            .next()
            .expect("We captured minor")
            .parse::<u8>()?;
        let mut version = PythonVersion::simple(major, minor);
        if let Some(micro) = captures.name("micro") {
            version.micro = micro.as_str().parse::<u8>()?;
            if let (Some(release), Some(serial)) =
                (captures.name("release"), captures.name("serial"))
            {
                version.releaselevel = release.as_str().parse()?;
                version.serial = serial.as_str().parse::<u8>()?;
            }
        }
        return Ok(version);
    }
    bail!("Not a valid Python version: {value}")
}

static CPYTHON_VERSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?<version>.+[^dtmu])(?<flags>[dtmu]+)?$").expect("This is a known valid regex.")
});

fn parse_cpython_implementation(value: &str) -> anyhow::Result<CPythonImplementation> {
    if let Some(captures) = CPYTHON_VERSION_REGEX.captures(value) {
        let version = parse_python_version(
            captures
                .name("version")
                .expect("The version capture was required")
                .as_str(),
        )?;
        let mut abi_info = CPythonAbiInfo::default();
        if let Some(flags) = captures.name("flags") {
            for flag in flags.as_str().chars() {
                match flag {
                    'd' => abi_info.debug = true,
                    't' => {
                        if version < PythonVersion::simple(3, 13) {
                            bail!(
                                "The t version flag indicating a free-threaded CPython build only \
                                applies for versions 3.13 and newer; but given version of {version}"
                            )
                        }
                        abi_info.free_threaded = Some(true);
                    }
                    'm' => {
                        if version >= PythonVersion::simple(3, 8) {
                            bail!(
                                "The m version flag indicating a pymalloc CPython build only \
                                applies for versions prior to 3.8; but given version of {version}"
                            )
                        }
                        abi_info.pymalloc = Some(true);
                    }
                    'u' => {
                        if version >= PythonVersion::simple(3, 3) {
                            bail!(
                                "The m version flag indicating a pymalloc CPython build only \
                                applies for versions prior to 3.3; but given version of {version}"
                            )
                        }
                        abi_info.ucs4 = Some(true);
                    }
                    flag => {
                        bail!("Un-recognized version flag {flag} for Cpython version {version}")
                    }
                }
            }
        }
        Ok(CPythonImplementation { version, abi_info })
    } else {
        bail!("Invalid CPython version: {value}")
    }
}

fn parse_pypy_implementation(value: &str) -> anyhow::Result<PyPyImplementation> {
    let mut components = value.splitn(2, "_");
    let version = parse_python_version(
        components
            .next()
            .expect("The version split will always yield at least one component."),
    )?;
    let pypy_version = if let Some(pypy_version) = components.next() {
        Some(pypy_version.parse()?)
    } else {
        None
    };
    Ok(PyPyImplementation {
        version,
        pypy_version,
    })
}

#[cfg(test)]
mod tests {
    use crate::version::{ReleaseLevel, parse_cpython_implementation};
    use crate::{CPythonAbiInfo, CPythonImplementation, PythonVersion};

    #[test]
    fn test_parse_cpython_implementation() {
        assert_eq!(
            CPythonImplementation {
                version: PythonVersion {
                    major: 3,
                    minor: 14,
                    micro: 0,
                    releaselevel: ReleaseLevel::Final,
                    serial: 0
                },
                abi_info: CPythonAbiInfo {
                    free_threaded: None,
                    debug: false,
                    pymalloc: None,
                    ucs4: None
                },
            },
            parse_cpython_implementation("3.14").unwrap()
        );

        assert_eq!(
            CPythonImplementation {
                version: PythonVersion {
                    major: 3,
                    minor: 14,
                    micro: 5,
                    releaselevel: ReleaseLevel::Final,
                    serial: 0
                },
                abi_info: CPythonAbiInfo {
                    free_threaded: None,
                    debug: false,
                    pymalloc: None,
                    ucs4: None
                },
            },
            parse_cpython_implementation("3.14.5").unwrap()
        );

        assert_eq!(
            CPythonImplementation {
                version: PythonVersion {
                    major: 3,
                    minor: 14,
                    micro: 0,
                    releaselevel: ReleaseLevel::Final,
                    serial: 0
                },
                abi_info: CPythonAbiInfo {
                    free_threaded: Some(true),
                    debug: true,
                    pymalloc: None,
                    ucs4: None
                },
            },
            parse_cpython_implementation("3.14dt").unwrap()
        );

        assert_eq!(
            CPythonImplementation {
                version: PythonVersion {
                    major: 3,
                    minor: 15,
                    micro: 0,
                    releaselevel: ReleaseLevel::Beta,
                    serial: 1
                },
                abi_info: CPythonAbiInfo {
                    free_threaded: Some(true),
                    debug: false,
                    pymalloc: None,
                    ucs4: None
                },
            },
            parse_cpython_implementation("3.15.0b1t").unwrap()
        );
    }
}
