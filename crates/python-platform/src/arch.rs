// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};

use anyhow::bail;
use target_lexicon::{
    Aarch64Architecture,
    Architecture,
    ArmArchitecture,
    Environment,
    HOST,
    Riscv64Architecture,
};

use crate::os::Os;

#[derive(Copy, Clone)]
pub enum Arch {
    Arm64,
    Armv7,
    Ppc64le,
    Riscv64,
    S390x,
    X64,
}

impl Arch {
    pub fn current() -> anyhow::Result<Self> {
        match (HOST.architecture, HOST.environment) {
            (Architecture::Arm(ArmArchitecture::Armv7), Environment::Gnueabihf) => Ok(Self::Armv7),
            (Architecture::Aarch64(Aarch64Architecture::Aarch64), _) => Ok(Self::Arm64),
            (Architecture::Powerpc64le, _) => Ok(Self::Ppc64le),
            (Architecture::Riscv64(Riscv64Architecture::Riscv64gc), _) => Ok(Self::Riscv64),
            (Architecture::S390x, _) => Ok(Self::S390x),
            (Architecture::X86_64, _) => Ok(Self::X64),
            (arch, environment) => bail!(
                "The pexrc binary does not support os: {os} arch: {arch} libc: {environment} yet.",
                os = Os::current()
                    .ok()
                    .map(|os| os.name())
                    .unwrap_or("<unknown>")
            ),
        }
    }

    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        match value {
            "aarch64" | "arm64" => Ok(Self::Arm64),
            "armv7" => Ok(Self::Arm64),
            "ppc64le" => Ok(Self::Arm64),
            "riscv64" => Ok(Self::Arm64),
            "s390x" => Ok(Self::Arm64),
            "amd64" | "x86_64" | "x64" => Ok(Self::X64),
            _ => bail!("Un-supported chip architecture: {value}"),
        }
    }

    pub(crate) fn as_str(&self) -> &'static str {
        self.as_linux_arch()
    }

    pub(crate) fn as_linux_arch(&self) -> &'static str {
        match self {
            Arch::Arm64 => "aarch64",
            Arch::Armv7 => "armv7",
            Arch::Ppc64le => "ppc64le",
            Arch::Riscv64 => "riscv64",
            Arch::S390x => "s390x",
            Arch::X64 => "x86_64",
        }
    }
}

impl Display for Arch {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}
