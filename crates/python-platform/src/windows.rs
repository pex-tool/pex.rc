// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};
use std::str::FromStr;

use anyhow::bail;
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

#[derive(Copy, Clone, EnumIter)]
pub enum Release {
    Post11,
    Windows11,
    Windows10,
    Windows8_1,
    Windows8,
    Windows7,
    Vista,
    XP64,
    XPMedia,
    XP,
    Windows2000,
    Post2025Server,
    Windows2025Server,
    Windows2022Server,
    Windows2019Server,
    Windows2016Server,
    Windows2012ServerR2,
    Windows2012Server,
    Windows2008ServerR2,
    Windows2008Server,
    Windows2003Server,
    Windows2000Server,
}

impl Release {
    pub(crate) fn as_str(&self) -> &'static str {
        match self {
            Release::Post11 => "post11",
            Release::Windows11 => "11",
            Release::Windows10 => "10",
            Release::Windows8_1 => "8.1",
            Release::Windows8 => "8",
            Release::Windows7 => "7",
            Release::Vista => "Vista",
            Release::XP64 => "XP64",
            Release::XPMedia => "XPMedia",
            Release::XP => "XP",
            Release::Windows2000 => "2000",
            Release::Post2025Server => "post2025Server",
            Release::Windows2025Server => "2025Server",
            Release::Windows2022Server => "2022Server",
            Release::Windows2019Server => "2019Server",
            Release::Windows2016Server => "2016Server",
            Release::Windows2012ServerR2 => "2012ServerR2",
            Release::Windows2012Server => "2012Server",
            Release::Windows2008ServerR2 => "2008ServerR2",
            Release::Windows2008Server => "2008Server",
            Release::Windows2003Server => "2003Server",
            Release::Windows2000Server => "2000Server",
        }
    }
}

impl FromStr for Release {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        for value in Self::iter() {
            if s.eq_ignore_ascii_case(value.as_str()) {
                return Ok(value);
            }
        }
        struct Releases;
        impl Display for Releases {
            fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                for value in Release::iter() {
                    writeln!(f, "{value}")?;
                }
                Ok(())
            }
        }
        bail!(
            "Invalid Windows release: {s}\n\
            The following releases are currently supported:\n\
            {Releases}"
        )
    }
}

impl Display for Release {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl Release {
    #[cfg(windows)]
    pub(crate) fn current() -> Option<Self> {
        release::current()
    }
}

pub(crate) struct Version(String);

impl Version {
    #[cfg(windows)]
    pub(crate) fn current() -> Self {
        Self(release::PlatformVersion::current().to_string())
    }
}

impl From<Version> for String {
    fn from(version: Version) -> Self {
        version.0
    }
}

#[cfg(windows)]
mod release {
    use std::fmt::{Display, Formatter};

    use windows_version::{OsVersion, is_server};

    use crate::windows::Release;

    #[derive(Copy, Clone, Ord, PartialOrd, Eq, PartialEq)]
    pub(super) struct PlatformVersion {
        major: u32,
        minor: u32,
        build: u32,
    }

    impl PlatformVersion {
        pub(super) fn current() -> Self {
            let version = OsVersion::current();
            Self {
                major: version.major,
                minor: version.minor,
                build: version.build,
            }
        }
    }

    impl Display for PlatformVersion {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            write!(
                f,
                "{major}.{minor}.{build}",
                major = self.major,
                minor = self.minor,
                build = self.build
            )
        }
    }

    #[derive(Copy, Clone)]
    struct PlatformRelease {
        version: PlatformVersion,
        release: Release,
    }

    impl PlatformRelease {
        const fn new(major: u32, minor: u32, build: u32, release: Release) -> Self {
            Self {
                version: PlatformVersion {
                    major,
                    minor,
                    build,
                },
                release,
            }
        }
    }

    // N.B.: The following tables are taken from the CPython platform module:

    const WORKSTATION_RELEASES: &[PlatformRelease] = &[
        PlatformRelease::new(10, 1, 0, Release::Post11),
        PlatformRelease::new(10, 0, 22000, Release::Windows11),
        PlatformRelease::new(6, 4, 0, Release::Windows10),
        PlatformRelease::new(6, 3, 0, Release::Windows8_1),
        PlatformRelease::new(6, 2, 0, Release::Windows8),
        PlatformRelease::new(6, 1, 0, Release::Windows7),
        PlatformRelease::new(6, 0, 0, Release::Vista),
        PlatformRelease::new(5, 2, 3790, Release::XP64),
        PlatformRelease::new(5, 2, 0, Release::XPMedia),
        PlatformRelease::new(5, 1, 0, Release::XP),
        PlatformRelease::new(5, 0, 0, Release::Windows2000),
    ];

    const SERVER_RELEASES: &[PlatformRelease] = &[
        PlatformRelease::new(10, 1, 0, Release::Post2025Server),
        PlatformRelease::new(10, 0, 26100, Release::Windows2025Server),
        PlatformRelease::new(10, 0, 20348, Release::Windows2022Server),
        PlatformRelease::new(10, 0, 17763, Release::Windows2019Server),
        PlatformRelease::new(6, 4, 0, Release::Windows2016Server),
        PlatformRelease::new(6, 3, 0, Release::Windows2012ServerR2),
        PlatformRelease::new(6, 2, 0, Release::Windows2012Server),
        PlatformRelease::new(6, 1, 0, Release::Windows2008ServerR2),
        PlatformRelease::new(6, 0, 0, Release::Windows2008Server),
        PlatformRelease::new(5, 2, 0, Release::Windows2003Server),
        PlatformRelease::new(5, 0, 0, Release::Windows2000Server),
    ];

    pub(super) fn current() -> Option<Release> {
        let version = PlatformVersion::current();
        let releases = if is_server() {
            SERVER_RELEASES
        } else {
            WORKSTATION_RELEASES
        };
        for platform_release in releases {
            if version >= platform_release.version {
                return Some(platform_release.release);
            }
        }
        None
    }
}
