// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::bail;

use crate::arch::Arch;
use crate::mac::Release as MacRelease;
use crate::os::{Libc, Os};
use crate::windows::Release as WindowsRelease;

#[derive(Copy, Clone)]
pub struct Linux {
    pub(crate) arch: Arch,
    pub(crate) libc: Libc,
}

#[derive(Copy, Clone)]
pub struct Mac {
    pub(crate) arm64: bool,
    pub(crate) release: MacRelease,
}

pub struct Windows {
    pub(crate) arm64: bool,
    pub(crate) release: Option<WindowsRelease>,
}

pub(crate) enum Platform {
    Linux(Linux),
    Mac(Mac),
    Windows(Windows),
}

impl Platform {
    pub(crate) fn from_parts(os: Os, arch: Arch) -> anyhow::Result<Self> {
        Ok(match (os, arch) {
            (Os::Linux(libc), arch) => Self::Linux(Linux { arch, libc }),
            (Os::Mac(release), Arch::Arm64) => Self::Mac(Mac {
                arm64: true,
                release,
            }),
            (Os::Mac(release), Arch::X64) => Self::Mac(Mac {
                arm64: false,
                release,
            }),
            (Os::Mac(_), arch) => bail!("Not a supported chip architecture for macOS: {arch}"),
            (Os::Windows(release), Arch::Arm64) => Self::Windows(Windows {
                arm64: true,
                release,
            }),
            (Os::Windows(release), Arch::X64) => Self::Windows(Windows {
                arm64: false,
                release,
            }),
            (Os::Windows(_), arch) => {
                bail!("Not a supported chip architecture for Windows: {arch}")
            }
        })
    }
}
