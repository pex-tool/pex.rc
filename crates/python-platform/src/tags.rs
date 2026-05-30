// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use crate::arch::Arch;
use crate::linux::LibcVersion;
use crate::mac::Release;
use crate::os::Libc;
use crate::platform::{Linux, Mac, Platform, Windows};
use crate::version::{CPythonImplementation, PyPyImplementation, PythonImplementation};

pub(crate) fn calculate(python_version: PythonImplementation, platform: Platform) -> Vec<String> {
    let platforms = calculate_supported_platforms(platform);
    let mut tags = Vec::with_capacity(2048);
    match python_version {
        PythonImplementation::CPython(version) => {
            add_cpython_tags(&mut tags, platforms.as_slice(), version)
        }
        PythonImplementation::PyPy(version) => {
            add_pypy_tags(&mut tags, platforms.as_slice(), version)
        }
    }
    add_compatible_tags(&mut tags, platforms.as_slice(), python_version);
    tags
}

fn calculate_supported_platforms(platform: Platform) -> Vec<String> {
    match platform {
        Platform::Linux(linux) => calculate_linux_platforms(linux),
        Platform::Mac(mac) => calculate_mac_platforms(mac),
        Platform::Windows(windows) => vec![calculate_windows_platform(windows).to_string()],
    }
}

fn calculate_linux_platforms(linux: Linux) -> Vec<String> {
    let mut platforms = Vec::with_capacity(64);
    platforms.push(format!("linux_{arch}", arch = linux.arch.as_linux_arch()));
    match linux.libc {
        Libc::Gnu(libc_version) => {
            add_manylinux_platforms(&mut platforms, linux.arch, libc_version)
        }
        Libc::Musl(libc_version) => {
            add_musllinux_platforms(&mut platforms, linux.arch, libc_version)
        }
    }
    platforms
}

fn add_manylinux_platforms(platforms: &mut Vec<String>, arch: Arch, glibc_version: LibcVersion) {
    let oldest_glibc2 = match arch {
        Arch::X64 => LibcVersion::new(2, 5),
        _ => LibcVersion::new(2, 17),
    };
    let mut glibc_max_list = Vec::with_capacity(64);
    glibc_max_list.push(glibc_version);
    for major in (2..glibc_version.major).rev() {
        glibc_max_list.push(LibcVersion::new(major, 50))
    }
    for glibc_max in glibc_max_list {
        let min_minor = if glibc_max.major == oldest_glibc2.major {
            oldest_glibc2.minor
        } else {
            0
        };
        for minor in (min_minor..=glibc_max.minor).rev() {
            if LibcVersion::new(glibc_max.major, minor) <= glibc_version {
                platforms.push(format!(
                    "manylinux_{major}_{minor}_{arch}",
                    major = glibc_max.major,
                    arch = arch.as_linux_arch()
                ));
                match (glibc_max.major, minor) {
                    (2, 17) => {
                        platforms
                            .push(format!("manylinux2014_{arch}", arch = arch.as_linux_arch()));
                    }
                    (2, 12) => {
                        platforms
                            .push(format!("manylinux2010_{arch}", arch = arch.as_linux_arch()));
                    }
                    (2, 5) => {
                        platforms.push(format!("manylinux1_{arch}", arch = arch.as_linux_arch()));
                    }
                    _ => {}
                }
            }
        }
    }
}

fn add_musllinux_platforms(platforms: &mut Vec<String>, arch: Arch, musl_version: LibcVersion) {
    for minor in (0..=musl_version.minor).rev() {
        platforms.push(format!(
            "musllinux_{major}_{minor}_{arch}",
            major = musl_version.major,
            arch = arch.as_linux_arch()
        ))
    }
}

fn calculate_mac_binary_formats(major: u16, minor: u8, arm64: bool) -> &'static [&'static str] {
    if arm64 {
        &["arm64", "universal2"]
    } else if (major, minor) < (10, 4) {
        &[]
    } else {
        &[
            "x86_64",
            "intel",
            "fat64",
            "fat3",
            "universal2",
            "universal",
        ]
    }
}

fn calculate_mac_platforms(mac: Mac) -> Vec<String> {
    let mut platforms = Vec::with_capacity(64);
    if mac.release >= Release::new(10, 0) && mac.release < Release::new(11, 0) {
        // Prior to macOS 11, each yearly release of macOS bumped the minor version number. The
        // major version was always 10.
        for minor_version in (0..=mac.release.minor).rev() {
            for binary_format in calculate_mac_binary_formats(10, minor_version, mac.arm64) {
                platforms.push(format!("macosx_10_{minor_version}_{binary_format}"));
            }
        }
    }
    if mac.release >= Release::new(11, 0) {
        // Starting with macOS 11, each yearly release bumps the major version number. The minor
        // versions are now the mid-year updates.
        for major_version in (11..=mac.release.major).rev() {
            for binary_format in calculate_mac_binary_formats(major_version, 0, mac.arm64) {
                platforms.push(format!("macosx_{major_version}_0_{binary_format}"));
            }
        }
        // macOS 11 on x86_64 is compatible with binaries from previous releases.
        // Arm64 support was introduced in 11.0, so no Arm binaries from previous
        // releases exist.
        //
        // However, the "universal2" binary format can have a
        // macOS version earlier than 11.0 when the x86_64 part of the binary supports
        // that version of macOS.
        if mac.arm64 {
            for minor_version in (4..=16).rev() {
                platforms.push(format!("macosx_10_{minor_version}_universal2"));
            }
        } else {
            for minor_version in (4..=16).rev() {
                for binary_format in calculate_mac_binary_formats(10, minor_version, false) {
                    platforms.push(format!("macosx_10_{minor_version}_{binary_format}"));
                }
            }
        }
    }
    platforms
}

fn calculate_windows_platform(windows: Windows) -> &'static str {
    if windows.arm64 {
        "win_arm64"
    } else {
        "win_amd64"
    }
}

fn add_cpython_tags(
    tags: &mut Vec<String>,
    platforms: &[String],
    python_version: CPythonImplementation,
) {
    let major = python_version.major;
    let minor = python_version.minor;

    let interpreter = format_args!("cp{major}{minor}");

    let threading = if python_version.free_threaded() {
        "t"
    } else {
        ""
    };
    let debug = if python_version.debug() { "d" } else { "" };
    let pymalloc = if python_version.pymalloc() { "m" } else { "" };
    let ucs4 = if python_version.ucs4() { "u" } else { "" };

    let abi = format!("cp{major}{minor}{threading}{debug}{pymalloc}{ucs4}");
    let abis: &[String] = if python_version.debug() {
        &[format!("cp{major}{minor}{threading}"), abi]
    } else {
        &[abi]
    };

    for abi in abis {
        for platform in platforms {
            tags.push(format!("{interpreter}-{abi}-{platform}"));
        }
    }

    // N.B.: PEP 384 was first implemented in Python 3.2. The free-threaded builds do not support
    // abi3.
    let of_abi3_era = (major, minor) >= (3, 2);
    let abi3 = of_abi3_era && !python_version.free_threaded();

    // PEP 803 was first implemented in Python 3.15 but, per PEP 803, this returns tags going back
    // to Python 3.2 to mirror the abi3 implementation and leave open the possibility of abi3t
    // wheels supporting older Python versions.
    let abi3t = of_abi3_era && python_version.free_threaded();

    if abi3 {
        for platform in platforms {
            tags.push(format!("{interpreter}-abi3-{platform}"));
        }
    }
    if abi3t {
        for platform in platforms {
            tags.push(format!("{interpreter}-abi3t-{platform}"));
        }
    }
    for platform in platforms {
        tags.push(format!("{interpreter}-none-{platform}"));
    }
    if abi3 || abi3t {
        for minor_version in (2..minor).rev() {
            for platform in platforms {
                if abi3 {
                    tags.push(format!("cp{major}{minor_version}-abi3-{platform}"));
                }
                if abi3t {
                    // Support for abi3t was introduced in Python 3.15, but in principle abi3t
                    // wheels are possible for older limited API versions, so allow things like
                    // cp37-abi3t-platform")
                    tags.push(format!("cp{major}{minor_version}-abi3t-{platform}"));
                }
            }
        }
    }
}

fn add_pypy_tags(tags: &mut Vec<String>, platforms: &[String], python_version: PyPyImplementation) {
    let major = python_version.major;
    let minor = python_version.minor;
    if let Some(pypy_version) = python_version.pypy_version.as_ref() {
        let pypy_major = pypy_version.major();
        let pypy_minor = pypy_version.minor();
        for platform in platforms {
            tags.push(format!(
                "pp{major}{minor}-pypy{major}{minor}_pp{pypy_major}{pypy_minor}-{platform}"
            ));
        }
    }
    for platform in platforms {
        tags.push(format!("pp{major}{minor}-none-{platform}"));
    }
}

mod py_interpreter {
    use crate::PythonVersion;

    enum State {
        Latest,
        MajorOnly,
        Descending(u8),
        Finished,
    }

    pub(super) struct Range {
        version: PythonVersion,
        state: State,
    }

    impl Range {
        pub(super) fn new(version: &PythonVersion) -> Self {
            Self {
                version: *version,
                state: State::Latest,
            }
        }
    }

    impl Iterator for Range {
        type Item = String;

        fn next(&mut self) -> Option<Self::Item> {
            match self.state {
                State::Latest => {
                    self.state = State::MajorOnly;
                    Some(format!(
                        "py{major}{minor}",
                        major = self.version.major,
                        minor = self.version.minor
                    ))
                }
                State::MajorOnly => {
                    self.state = State::Descending(self.version.minor - 1);
                    Some(format!("py{major}", major = self.version.major))
                }
                State::Descending(minor) => {
                    if minor == 0 {
                        self.state = State::Finished;
                    } else {
                        self.state = State::Descending(minor - 1);
                    }
                    Some(format!("py{major}{minor}", major = self.version.major))
                }
                State::Finished => None,
            }
        }
    }
}

fn add_compatible_tags(
    tags: &mut Vec<String>,
    platforms: &[String],
    python_version: PythonImplementation,
) {
    for version in py_interpreter::Range::new(&python_version) {
        for platform in platforms {
            tags.push(format!("{version}-none-{platform}"))
        }
    }
    tags.push(match python_version {
        PythonImplementation::CPython(_) => format!(
            "cp{major}{minor}-none-any",
            major = python_version.major,
            minor = python_version.minor
        ),
        PythonImplementation::PyPy(_) => {
            format!("pp{major}-none-any", major = python_version.major)
        }
    });
    for version in py_interpreter::Range::new(&python_version) {
        tags.push(format!("{version}-none-any"))
    }
}
