// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::str::FromStr;
use std::sync::LazyLock;

use anyhow::bail;
use pep440_rs::Version;
use regex::Regex;

use crate::implementation::Implementation;

pub(crate) struct CPythonVersion {
    version: Version,
    pub(crate) debug: bool,
    pub(crate) free_threaded: bool,
    pub(crate) pymalloc: bool,
    pub(crate) ucs4: bool,
}

pub(crate) trait SimpleVersion {
    fn major_version(&self) -> u64;
    fn minor_version(&self) -> u64;
}

impl SimpleVersion for Version {
    fn major_version(&self) -> u64 {
        self.release()[0]
    }

    fn minor_version(&self) -> u64 {
        self.release()[1]
    }
}

impl SimpleVersion for CPythonVersion {
    fn major_version(&self) -> u64 {
        self.version.major_version()
    }

    fn minor_version(&self) -> u64 {
        self.version.minor_version()
    }
}

pub(crate) struct PyPyVersion {
    version: Version,
    pub(crate) pypy_version: Option<Version>,
}

impl SimpleVersion for PyPyVersion {
    fn major_version(&self) -> u64 {
        self.version.major_version()
    }

    fn minor_version(&self) -> u64 {
        self.version.minor_version()
    }
}

pub(crate) enum PythonVersion {
    CPython(CPythonVersion),
    PyPy(PyPyVersion),
}

impl PythonVersion {
    pub(crate) fn version(&self) -> &Version {
        match self {
            PythonVersion::CPython(CPythonVersion { version, .. })
            | PythonVersion::PyPy(PyPyVersion { version, .. }) => version,
        }
    }
}

impl SimpleVersion for PythonVersion {
    fn major_version(&self) -> u64 {
        self.version().major_version()
    }

    fn minor_version(&self) -> u64 {
        self.version().minor_version()
    }
}

pub(crate) fn parse(
    implementation: Implementation,
    version: &str,
) -> anyhow::Result<PythonVersion> {
    Ok(match implementation {
        Implementation::CPython => PythonVersion::CPython(parse_cpython_version(version)?),
        Implementation::PyPy => PythonVersion::PyPy(parse_pypy_version(version)?),
    })
}

static PYTHON_VERSION_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^(?<version>\d+\.\d+(\.\d+)?)(?:(?:a|b|rc)\d+)?(?<flags>[dtmu]+)?$")
        .expect("This is a known valid regex.")
});

fn parse_python_version(value: &str) -> anyhow::Result<Version> {
    let mut version = Version::from_str(value)?;
    let release = version.release();
    if release.len() < 2 {
        bail!(
            "The Python version must have at least a major version and minor version, but \
            given version of {version}"
        )
    }
    if release.len() == 2 {
        let major = version.major_version();
        let minor = version.minor_version();
        version = version.with_release([major, minor, 0])
    };
    Ok(version)
}

fn parse_cpython_version(value: &str) -> anyhow::Result<CPythonVersion> {
    if let Some(captures) = PYTHON_VERSION_REGEX.captures(value) {
        let version = parse_python_version(
            captures
                .name("version")
                .expect("The version capture was required.")
                .as_str(),
        )?;
        let mut debug = false;
        let mut free_threaded = false;
        let mut pymalloc = false;
        let mut ucs4 = false;
        if let Some(flags) = captures.name("flags") {
            for flag in flags.as_str().chars() {
                match flag {
                    'd' => debug = true,
                    't' => {
                        if version < Version::new([3, 13]) {
                            bail!(
                                "The t version flag indicating a free-threaded CPython build only \
                                applies for versions 3.13 and newer; but given version of {version}"
                            )
                        }
                        free_threaded = true;
                    }
                    'm' => {
                        if version >= Version::new([3, 8]) {
                            bail!(
                                "The m version flag indicating a pymalloc CPython build only \
                                applies for versions prior to 3.8; but given version of {version}"
                            )
                        }
                        pymalloc = true;
                    }
                    'u' => {
                        if version >= Version::new([3, 3]) {
                            bail!(
                                "The m version flag indicating a pymalloc CPython build only \
                                applies for versions prior to 3.3; but given version of {version}"
                            )
                        }
                        ucs4 = true;
                    }
                    flag => {
                        bail!("Un-recognized version flag {flag} for Cpython version {version}")
                    }
                }
            }
        }
        Ok(CPythonVersion {
            version,
            debug,
            free_threaded,
            pymalloc,
            ucs4,
        })
    } else {
        bail!("Invalid CPython version: {value}")
    }
}

fn parse_pypy_version(value: &str) -> anyhow::Result<PyPyVersion> {
    let mut components = value.splitn(2, "_");
    let version = parse_python_version(
        components
            .next()
            .expect("The version split will always yield at least one component."),
    )?;
    let pypy_version = if let Some(pypy_version) = components.next() {
        Some(Version::from_str(pypy_version)?)
    } else {
        None
    };
    Ok(PyPyVersion {
        version,
        pypy_version,
    })
}
