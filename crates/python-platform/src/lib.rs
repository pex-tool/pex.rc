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
use logging_timer::time;
use pep508_rs::MarkerEnvironment;
use pep508_rs::pep440_rs::Version;
use serde::{Deserialize, Serialize};

pub use crate::arch::Arch;
use crate::implementation::Implementation;
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

#[time("debug", "python-platform.{}")]
pub fn parse<'a>(
    spec: &'a str,
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
                    <implementation>-<version>\n\
                    (e.g.: cpython-3.14.5) or else just a version; given: {spec}"
                )
            })?;
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
