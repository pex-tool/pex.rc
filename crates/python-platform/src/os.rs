// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;

use anyhow::bail;
#[cfg(target_os = "linux")]
use logging_timer::time;

use crate::linux::LibcVersion;
use crate::mac::Release as MacRelease;
use crate::windows::Release as WindowsRelease;

#[derive(Copy, Clone)]
pub enum Libc {
    Gnu(LibcVersion),
    Musl(LibcVersion),
}

pub enum Os {
    Linux(Libc),
    Mac(MacRelease),
    Windows(Option<WindowsRelease>),
}

pub(crate) struct ReleaseInfo<'a> {
    pub(crate) release: Cow<'a, str>,
    pub(crate) version: Cow<'a, str>,
}

impl Os {
    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "linux" => {
                // We default to manylinux2014 support like out rust builds do.
                Ok(Self::Linux(Libc::Gnu(LibcVersion::new(2, 17))))
            }
            "macos" => {
                // We default to 11.3 support like our rust builds do.
                // This is macOS Big Sur from April 2021.
                Ok(Self::Mac(MacRelease::new(11, 3)))
            }
            "windows" => Ok(Self::Windows(None)),
            value if let Some(version) = value.strip_prefix("macos_") => {
                Ok(Self::Mac(MacRelease::parse(version, '_')?))
            }
            // See: https://peps.python.org/pep-0600/
            value if let Some(version) = value.strip_prefix("manylinux") => match version {
                "1" => Ok(Self::Linux(Libc::Gnu(LibcVersion::new(2, 5)))),
                "2010" => Ok(Self::Linux(Libc::Gnu(LibcVersion::new(2, 12)))),
                "2014" => Ok(Self::Linux(Libc::Gnu(LibcVersion::new(2, 17)))),
                _ if let Some(glibc_version) = version.strip_prefix("_") => Ok(Self::Linux(
                    Libc::Gnu(LibcVersion::parse(glibc_version, '_')?),
                )),
                _ => bail!("Invalid manylinux specification: {value}"),
            },
            // See: https://peps.python.org/pep-0656/
            value if let Some(version) = value.strip_prefix("musllinux_") => {
                Ok(Self::Linux(Libc::Musl(LibcVersion::parse(version, '_')?)))
            }
            value if let Some(release) = value.strip_prefix("windows_") => {
                Ok(Self::Windows(Some(WindowsRelease::parse(release)?)))
            }
            value => bail!("Un-supported operating system: {value}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[time("debug", "Os.{}")]
    pub fn current() -> anyhow::Result<Self> {
        let libc = match crate::LinuxInfo::parse(std::env::current_exe()?)? {
            crate::LinuxInfo::ManyLinux(manylinux) => Libc::Gnu(
                manylinux
                    .into_libc_version()
                    .unwrap_or(LibcVersion::new(2, 17)),
            ),
            crate::LinuxInfo::MuslLinux(libc_version) => Libc::Musl(libc_version),
        };
        Ok(Self::Linux(libc))
    }

    #[cfg(target_os = "macos")]
    pub fn current() -> anyhow::Result<Self> {
        Ok(Self::Mac(MacRelease::current()?))
    }

    #[cfg(windows)]
    pub fn current() -> anyhow::Result<Self> {
        Ok(Self::Windows(WindowsRelease::current()))
    }

    #[cfg(unix)]
    pub(crate) fn current_release<'a>() -> anyhow::Result<ReleaseInfo<'a>> {
        let uname = rustix::system::uname();
        Ok(ReleaseInfo {
            release: Cow::Owned(uname.release().to_str()?.to_owned()),
            version: Cow::Owned(uname.version().to_str()?.to_owned()),
        })
    }

    #[cfg(windows)]
    pub(crate) fn current_release<'a>() -> anyhow::Result<ReleaseInfo<'a>> {
        let release = WindowsRelease::current()
            .map(|release| release.as_str())
            .unwrap_or("<unknown>");
        let version = crate::windows::Version::current();
        Ok(ReleaseInfo {
            release: Cow::Borrowed(release),
            version: Cow::Owned(version.into()),
        })
    }

    pub(crate) fn name(&self) -> &'static str {
        match self {
            Os::Linux(_) => "linux",
            Os::Mac(_) => "macos",
            Os::Windows(_) => "windows",
        }
    }
}
