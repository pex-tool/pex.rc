// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

mod tags;

mod arch;
mod implementation;
mod linux;
mod mac;
mod markers;
mod os;
mod platform;
mod version;
mod windows;

use std::borrow::Cow;
use std::fmt::Display;
use std::ops::Deref;
use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, bail};
use pep508_rs::pep440_rs::Version;
use pep508_rs::{MarkerEnvironment, MarkerValueVersion};
use serde::{Deserialize, Serialize};
use tracing::instrument;

pub use crate::arch::Arch;
pub use crate::implementation::Implementation;
pub use crate::linux::LinuxInfo;
use crate::mac::Release;
pub use crate::markers::{PlatformRelease, PlatformVersion};
pub use crate::os::{Libc, Os};
use crate::platform::Platform;
pub use crate::version::{
    CPythonAbiInfo,
    CPythonImplementation,
    PyPyImplementation,
    PyPyVersion,
    PythonImplementation,
    PythonVersion,
};

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
struct NonEmptyVec<T>(Vec<T>);

impl<T> NonEmptyVec<T> {
    fn new(vec: Vec<T>) -> anyhow::Result<Self> {
        if vec.is_empty() {
            bail!("Given an empty vec.")
        }
        Ok(Self(vec))
    }

    fn first(&self) -> &T {
        &self.0[0]
    }
}

impl<T> Deref for NonEmptyVec<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub trait PythonPlatform<'a> {
    fn description(&self) -> impl Display;
    fn marker_env(&self) -> &MarkerEnvironment;
    fn supported_tags(&self) -> impl Iterator<Item = &'_ str>;
    fn primary_tag(&self) -> &str;
    fn version(&self) -> Cow<'_, Version> {
        Cow::Borrowed(&self.marker_env().python_full_version().version)
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct PlatformDetails<'a> {
    #[serde(borrow)]
    source: Cow<'a, str>,
    marker_env: MarkerEnvironment,
    supported_tags: NonEmptyVec<Cow<'a, str>>,
}

impl<'a> PlatformDetails<'a> {
    pub fn python_implementation(&self) -> anyhow::Result<PythonImplementation> {
        let version = self
            .marker_env
            .get_version(&MarkerValueVersion::PythonFullVersion)
            .try_into()?;
        if self.marker_env.platform_python_implementation() == "PyPy" {
            Ok(PythonImplementation::PyPy(PyPyImplementation {
                version,
                pypy_version: None,
            }))
        } else {
            Ok(PythonImplementation::CPython(CPythonImplementation {
                version,
                abi_info: CPythonAbiInfo {
                    free_threaded: None,
                    debug: false,
                    pymalloc: None,
                    ucs4: None,
                },
            }))
        }
    }
}

impl<'a> PlatformDetails<'a> {
    pub fn new(
        source: impl Display,
        marker_env: MarkerEnvironment,
        supported_tags: Vec<Cow<'a, str>>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            source: Cow::Owned(source.to_string()),
            marker_env,
            supported_tags: NonEmptyVec::new(supported_tags)?,
        })
    }

    pub fn python(
        python_exe: &Path,
        python_version: PythonImplementation,
    ) -> anyhow::Result<PlatformDetails<'a>> {
        let platform = Platform::current()?;
        let release_info = Os::current_release()?;
        Ok(Self {
            source: Cow::Owned(format!(
                "interpreter at {python_exe}",
                python_exe = python_exe.display()
            )),
            marker_env: markers::calculate(
                python_version,
                platform,
                Some(PlatformRelease(release_info.release)),
                Some(PlatformVersion(release_info.version)),
            )?,
            supported_tags: NonEmptyVec::new(
                tags::calculate(python_version, platform)
                    .into_iter()
                    .map(Cow::Owned)
                    .collect(),
            )?,
        })
    }
}

impl<'a> PythonPlatform<'a> for PlatformDetails<'a> {
    fn description(&self) -> impl Display {
        &self.source
    }

    fn marker_env(&self) -> &MarkerEnvironment {
        &self.marker_env
    }

    fn supported_tags(&self) -> impl Iterator<Item = &'_ str> {
        self.supported_tags.iter().map(AsRef::as_ref)
    }

    fn primary_tag(&self) -> &str {
        self.supported_tags.first()
    }
}

pub const PYTHON_PLATFORM_LONG_HELP: &str = r#"
Can be either the path to a local Python executable or else a Python platform spec.
In its simplest form, the spec can be just a Python version number; and CPython will be
assumed. The version number must be in <major>.<minor>(.<micro>) form. If the micro version
is not specified, 0 is used. For example:
+ 3.14
+ 3.14.5

The Python implementation can be selected by prefixing the version with cpython or pypy:
+ cpython-3.14
+ pypy-3.11

For convenience, the `-` is not required here and use of python is permitted, which maps
to cpython. This mimics Python binary names on unix platforms:
+ python3.14
+ pypy3.11

Cpython versions can be further suffixed with the following abi flags:
+ t: A free-threaded build (Only applies to CPython 3.13 and newer).
+ d: A debug build.
+ m: A pymalloc build (Only applies to CPython 3.7 and older).
+ u: A ucs4 Unicode build (Only applies to CPython 3.2 and older).

For example:
+ cpython-3.14t
+ 3.14.5td
+ 2.7mu

PyPy versions can be suffixed by the PyPy release following an underscore:
+ pypy-3.11_7.3
+ pypy-2.7.18_7.3

In the preceding forms, the Python platform spec is rendered for the current operating
system and chip architecture. You can further refine the spec by specifying these as
suffixes.

The basic operating system suffixes are:
+ 3.14.5-linux
+ 3.14.5-macos
+ 3.14.5-windows

When using these, defaults for each operating system are chosen:
+ linux: 4.4.302-cip103 (January 2016) & glibc 2.17 (December 2012) & x86_64
+ macos: 11.3 (Big Sur April 2021) & aarch64
+ windows: 10 (first released July 2015) & x86_64

Linux can be further refined by using the manylinux and musllinux standards; for example:
+ 3.14.5-manylinux1
+ 3.14.5-manylinux2014
+ 3.14.5-manylinux_2_43
+ 3.14.5-musllinux_1_2

macOS can be further refined by specifying the release in <major>_<minor>(_<patch>) form:
+ 3.14.5-macos_10_6
+ 3.14.5-macos_11_7_11
+ 3.14.5-macos_26_5

Windows can be further refined by specifying the release as well:
+ 3.14.5-windows_11

Finally, when specifying an operating system, an explicit chip architecture suffix can be
selected from among the following:
+ aarch64 (or arm64)
+ armv7 [^1]
+ ppc64le [^1]
+ riscv64 [^1]
+ s390x [^1]
+ x86_64 (or x64 or amd64)

With this, you have a full [^2] specification Python platform specification. For example:
+ pypy-3.11_7.3-manylinux_2_17-aarch64
+ cpython-3.14.5-macos_26_5-arm64
+ cpython-3.14.5-windows_11-amd64

[^1]: These chip architectures are only supported for Linux.
[^2]: The derived Python platform specification is complete save for the platform_version
      environment marker that appears to be unused in the wild. Its value is defaulted to
      "<unknown>".
"#;

#[instrument(level = "debug", skip_all)]
pub fn parse<'a>(
    spec: Cow<'a, str>,
    platform_release: Option<PlatformRelease<'a>>,
    platform_version: Option<PlatformVersion<'a>>,
) -> anyhow::Result<PlatformDetails<'a>> {
    let mut components = spec.split("-");
    let implementation_or_version = components
        .next()
        .expect("There is always at least one split component.");
    let python_version =
        if let Ok(implementation) = Implementation::from_str(implementation_or_version) {
            let version = components.next().ok_or_else(|| {
                anyhow!(
                    "Expected a Python platform specification starting with \
                    <implementation>-<version> (e.g.: cpython-3.14.5)\n\
                    or <implementation><version> or else just a version; given: {spec}"
                )
            })?;
            version::parse(implementation, version)?
        } else if let Some(index) = implementation_or_version.find(['2', '3'])
            && index > 0
        {
            let implementation = &implementation_or_version[0..index];
            let implementation = if implementation == "python" {
                Implementation::CPython
            } else {
                Implementation::from_str(implementation)?
            };
            let version = &implementation_or_version[index..];
            version::parse(implementation, version)?
        } else {
            version::parse(Implementation::CPython, implementation_or_version)?
        };

    let (platform, platform_release, platform_version) = {
        let (os, arch, platform_release, platform_version) = if let Some(os) = components.next() {
            let os = os.parse()?;
            let arch = if let Some(arch) = components.next() {
                arch.parse()?
            } else {
                match &os {
                    Os::Linux(_) => Arch::X64,
                    Os::Mac(version) => {
                        if version < &Release::new(10, 16) {
                            Arch::X64
                        } else {
                            // macOS 10.16 (a.k.a 11.0 (a.k.a Big Sur)) was the 1st to support Apple
                            // Silicon. Although there was a transition period, we chose this
                            // point as the cutoff with no further justification.
                            Arch::Arm64
                        }
                    }
                    Os::Windows(_) => Arch::X64,
                }
            };
            (os, arch, platform_release, platform_version)
        } else {
            let os = Os::current()?;
            let release_info = Os::current_release()?;
            let (platform_release, platform_version) = match os {
                Os::Linux(_) => (
                    Some(platform_release.unwrap_or(PlatformRelease(release_info.release))),
                    Some(platform_version.unwrap_or(PlatformVersion(release_info.version))),
                ),
                Os::Mac(_) | Os::Windows(_) => (
                    platform_release,
                    Some(platform_version.unwrap_or(PlatformVersion(release_info.version))),
                ),
            };
            (os, Arch::current()?, platform_release, platform_version)
        };
        (
            Platform::from_parts(os, arch)?,
            platform_release,
            platform_version,
        )
    };

    if components.next().is_some() {
        bail!(
            "A Python platform specification can have at most 4 components:\n\
            <implementation>-<version>-<os>-<arch>\n\
            Given: {spec}"
        )
    }

    let marker_env =
        markers::calculate(python_version, platform, platform_release, platform_version)?;

    let supported_tags = tags::calculate(python_version, platform)
        .into_iter()
        .map(Cow::Owned)
        .collect();

    PlatformDetails::new(
        format!("abbreviated platform {spec}"),
        marker_env,
        supported_tags,
    )
}
