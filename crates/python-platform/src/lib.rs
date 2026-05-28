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
use std::io::{BufRead, BufReader, Cursor};
use std::ops::Deref;
use std::path::Path;
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail};
pub use linux::LinuxInfo;
use logging_timer::time;
pub use markers::{PlatformRelease, PlatformVersion};
use pep508_rs::MarkerEnvironment;
use pep508_rs::pep440_rs::Version;
use serde::{Deserialize, Serialize};

pub use crate::arch::Arch;
use crate::implementation::Implementation;
use crate::mac::Release;
pub use crate::os::{Libc, Os};
use crate::platform::Platform;

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct NonEmptyVec<T>(Vec<T>);

impl<T> NonEmptyVec<T> {
    pub fn new(vec: Vec<T>) -> anyhow::Result<Self> {
        if vec.is_empty() {
            bail!("Given an empty vec.")
        }
        Ok(Self(vec))
    }
}

impl<T> Deref for NonEmptyVec<T> {
    type Target = Vec<T>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub trait PythonPlatform {
    fn description(&self) -> impl Display;
    fn marker_env(&self) -> &MarkerEnvironment;
    fn supported_tags(&self) -> &NonEmptyVec<String>;
    fn version(&self) -> Cow<'_, Version> {
        Cow::Borrowed(&self.marker_env().python_full_version().version)
    }
    fn primary_tag(&self) -> &str {
        self.supported_tags()[0].as_ref()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct PlatformDetails {
    pub source: String,
    pub marker_env: MarkerEnvironment,
    pub supported_tags: NonEmptyVec<String>,
}

impl PlatformDetails {
    pub fn new(
        source: impl Display,
        marker_env: MarkerEnvironment,
        supported_tags: Vec<String>,
    ) -> anyhow::Result<Self> {
        Ok(Self {
            source: source.to_string(),
            marker_env,
            supported_tags: NonEmptyVec::new(supported_tags)?,
        })
    }

    pub fn spawn(
        python: &Path,
    ) -> anyhow::Result<impl FnOnce() -> anyhow::Result<PlatformDetails>> {
        let child = Command::new(python)
            .arg("-V")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(move || {
            let output = child.wait_with_output()?;
            let version = parse_version(output.stdout).or_else(|_| parse_version(output.stderr))?;
            parse(&version, None, None)
        })
    }

    pub fn python(python: &Path) -> anyhow::Result<PlatformDetails> {
        Self::spawn(python)?()
    }
}

fn parse_version(data: Vec<u8>) -> anyhow::Result<String> {
    let line = BufReader::new(Cursor::new(data))
        .lines()
        .next()
        .ok_or_else(|| anyhow!("No Python version output found."))?;
    let mut text = line?;
    let start = text
        .find(" ")
        .ok_or_else(|| anyhow!("No Python version output found."))?;
    text.drain(..start + 1);
    let version = text
        .split(" ")
        .next()
        .expect("Should split at least 1 element.");
    if version.is_empty() {
        bail!("No Python version output found.")
    }
    text.truncate(version.len());
    Ok(text)
}

impl PythonPlatform for PlatformDetails {
    fn description(&self) -> impl Display {
        &self.source
    }

    fn marker_env(&self) -> &MarkerEnvironment {
        &self.marker_env
    }

    fn supported_tags(&self) -> &NonEmptyVec<String> {
        &self.supported_tags
    }
}

#[time("debug", "python-platform.{}")]
pub fn parse<'a>(
    spec: &'a str,
    platform_release: Option<PlatformRelease<'a>>,
    platform_version: Option<PlatformVersion<'a>>,
) -> anyhow::Result<PlatformDetails> {
    let mut components = spec.split("-");
    let implementation_or_version = components
        .next()
        .expect("There is always at least one split component.");
    let python_version =
        if let Ok(implementation) = Implementation::parse(implementation_or_version) {
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
            let os = Os::parse(os)?;
            let arch = if let Some(arch) = components.next() {
                Arch::parse(arch)?
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

    let marker_env = markers::calculate(
        &python_version,
        &platform,
        platform_release,
        platform_version,
    )?;

    let supported_tags = tags::calculate(&python_version, platform);

    PlatformDetails::new(
        format!("abbreviated platform {spec}"),
        marker_env,
        supported_tags,
    )
}
