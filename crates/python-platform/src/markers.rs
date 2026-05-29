// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::ops::Deref;

use pep508_rs::{MarkerEnvironment, MarkerEnvironmentBuilder};

use crate::arch::Arch;
use crate::platform::{Linux, Mac, Platform, Windows};
use crate::version::PythonImplementation;

macro_rules! generate_marker_variable {
    ( $marker_variable_type:ident ) => {
        pub struct $marker_variable_type<'a>(pub Cow<'a, str>);

        impl<'a> $marker_variable_type<'a> {
            pub fn new(value: &'a str) -> Self {
                Self(Cow::Borrowed(value))
            }
        }

        impl<'a> Deref for $marker_variable_type<'a> {
            type Target = str;

            fn deref(&self) -> &Self::Target {
                self.0.as_ref()
            }
        }
    };
}

generate_marker_variable!(PlatformRelease);
generate_marker_variable!(PlatformVersion);

pub(crate) fn calculate(
    python_version: PythonImplementation,
    platform: Platform,
    platform_release: Option<PlatformRelease>,
    platform_version: Option<PlatformVersion>,
) -> anyhow::Result<MarkerEnvironment> {
    let (implementation_name, platform_python_implementation) = match python_version {
        PythonImplementation::CPython(_) => ("cpython", "CPython"),
        PythonImplementation::PyPy(_) => ("pypy", "PyPy"),
    };

    let major_version = python_version.major;
    let (os_name, platform_system, sys_platform) = match platform {
        Platform::Linux(_) => (
            "posix",
            "Linux",
            if major_version == 2 {
                "linux2"
            } else {
                "linux"
            },
        ),
        Platform::Mac(_) => ("posix", "Darwin", "darwin"),
        Platform::Windows(_) => ("nt", "Windows", "win32"),
    };

    let platform_machine = match platform {
        Platform::Linux(Linux { arch, .. }) => match arch {
            Arch::Arm64 => "aarch64",
            Arch::Armv7 => "armv7l",
            Arch::Ppc64le => "ppc64le",
            Arch::Riscv64 => "riscv64",
            Arch::S390x => "s390x",
            Arch::X64 => "x86_64",
        },
        Platform::Mac(Mac { arm64, .. }) => {
            if arm64 {
                "arm64"
            } else {
                "x86_64"
            }
        }
        Platform::Windows(Windows { arm64, .. }) => {
            if arm64 {
                "ARM64"
            } else {
                "AMD64"
            }
        }
    };

    let platform_release = if let Some(release) = platform_release {
        Cow::Owned(release.to_string())
    } else {
        match platform {
            Platform::Linux(_) => Cow::Borrowed("<unknown>"),
            Platform::Mac(Mac { release, .. }) => Cow::Owned(format!(
                "{major}.{minor}.{patch}",
                major = release.major,
                minor = release.minor,
                patch = release.patch.unwrap_or(0)
            )),
            Platform::Windows(Windows { release, .. }) => Cow::Borrowed(
                release
                    .map(|release| release.as_str())
                    .unwrap_or("<unknown>"),
            ),
        }
    };

    let implementation_version = python_version.to_string();
    let python_version_str = format!(
        "{major}.{minor}",
        major = major_version,
        minor = python_version.minor
    );

    Ok(MarkerEnvironment::try_from(MarkerEnvironmentBuilder {
        implementation_name,
        implementation_version: &implementation_version,
        os_name,
        platform_machine,
        platform_python_implementation,
        platform_release: platform_release.as_ref(),
        platform_system,
        platform_version: platform_version.as_deref().unwrap_or("<unknown>"),
        python_full_version: &implementation_version,
        python_version: &python_version_str,
        sys_platform,
    })?)
}
