// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::anyhow;
use serde::Serialize;

#[derive(Copy, Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize)]
pub struct LibcVersion {
    pub major: u8,
    pub minor: u8,
    pub patch: Option<u8>,
}

impl LibcVersion {
    pub fn new(major: u8, minor: u8) -> Self {
        Self {
            major,
            minor,
            patch: None,
        }
    }

    pub fn parse(version: &str, sep: char) -> anyhow::Result<Self> {
        let mut components_iter = version.split(sep);
        let mut parse_component = |subject| -> anyhow::Result<u8> {
            let component = components_iter.next().ok_or_else(|| {
                anyhow!("Invalid libc version {version}: failed to parse {subject} version number")
            })?;
            component.parse::<u8>().map_err(|err| {
                anyhow!("Failed to parse {subject} version component of {version}: {err}")
            })
        };
        let major = parse_component("major")?;
        let minor = parse_component("minor")?;
        let patch = parse_component("patch").ok();
        Ok(Self {
            major,
            minor,
            patch,
        })
    }
}

#[derive(Debug, Serialize)]
pub struct Manylinux {
    glibc: Option<LibcVersion>,
    armhf: bool,
    i686: bool,
}

impl Manylinux {
    pub fn into_libc_version(self) -> Option<LibcVersion> {
        self.glibc
    }
}

#[derive(Debug, Serialize)]
pub enum LinuxInfo {
    #[serde(rename = "manylinux")]
    ManyLinux(Manylinux),
    #[serde(rename = "musllinux")]
    MuslLinux(LibcVersion),
}

impl LinuxInfo {
    #[cfg(target_os = "linux")]
    pub fn parse(exe: impl AsRef<std::path::Path>) -> anyhow::Result<Self> {
        linux_info::parse(exe)
    }
}

#[cfg(target_os = "linux")]
mod linux_info {
    use std::ffi::OsStr;
    use std::io::BufRead;
    use std::os::unix::ffi::OsStrExt;
    use std::path::Path;
    use std::process::{Child, Command, Stdio};

    use anyhow::bail;
    use elf::ElfStream;
    use elf::endian::{AnyEndian, EndianParse};
    use elf::file::{Class, FileHeader};
    use fs_err::File;

    use crate::LinuxInfo;
    use crate::linux::{LibcVersion, Manylinux};

    pub(super) fn parse(exe: impl AsRef<Path>) -> anyhow::Result<LinuxInfo> {
        let exe_fp = File::open(exe.as_ref())?;
        let mut elf: ElfStream<AnyEndian, _> = ElfStream::open_stream(exe_fp)?;
        let segments = elf.segments().clone();
        for program_header in segments {
            if program_header.p_type != elf::abi::PT_INTERP {
                continue;
            }
            let (interpreter_path, is_musl) = {
                let interpreter = elf.segment_data(&program_header)?;
                let is_musl = interpreter
                    .windows(b"musl".len())
                    .any(|window| window == b"musl");
                let eos = interpreter
                    .iter()
                    .position(|byte| *byte == 0)
                    .unwrap_or(interpreter.len());
                (Path::new(OsStr::from_bytes(&interpreter[0..eos])), is_musl)
            };
            if is_musl {
                // N.B.: Support for the Version field is in musl >= 0.9.15 only (01/03/2014) but
                // musllinux support was only added in https://peps.python.org/pep-0656/ in 2021:
                // $ docker run --rm -it python:alpine /lib/ld-musl-x86_64.so.1 >/dev/null
                // musl libc (x86_64)
                // Version 1.2.5
                // Dynamic Program Loader
                // Usage: /lib/ld-musl-x86_64.so.1 [options] [--] pathname [args]
                let output = Command::new(interpreter_path)
                    .stderr(Stdio::piped())
                    .spawn()?
                    .wait_with_output()?;
                for line in output.stderr.lines() {
                    if let Ok(line) = line
                        && line.starts_with("Version ")
                    {
                        return Ok(LinuxInfo::MuslLinux(LibcVersion::parse(
                            line["Version ".len()..].trim(),
                            '.',
                        )?));
                    }
                }
                bail!(
                    "Failed to identify musl libc version for {interpreter}",
                    interpreter = interpreter_path.display()
                )
            } else {
                // N.B.: Support for --version in glibc >= 2.33 only (01/02/2021)
                // used by >= ubuntu:21.04. The manylinux spec started with
                // https://peps.python.org/pep-0513/ in 2016; so this does not cover
                // it.
                // $ /lib64/ld-linux-x86-64.so.2 --version 2>/dev/null
                // ld.so (Ubuntu GLIBC 2.41-6ubuntu1) stable release version 2.41.
                // Copyright (C) 2025 Free Software Foundation, Inc.
                // This is free software; see the source for copying conditions.
                // There is NO warranty; not even for MERCHANTABILITY or FITNESS FOR A
                // PARTICULAR PURPOSE.
                let mut glibc_version: Option<LibcVersion> = None;
                {
                    let result = Command::new(interpreter_path)
                        .arg("--version")
                        .stdout(Stdio::piped())
                        .spawn()
                        .and_then(Child::wait_with_output);
                    if let Ok(output) = result {
                        for line in output.stdout.lines() {
                            if let Ok(line) = line
                                && let Some(index) = line.find("release version ")
                            {
                                if let Ok(libc_version) = LibcVersion::parse(
                                    line[index + "release version ".len()..].trim(),
                                    '.',
                                ) {
                                    glibc_version = Some(libc_version);
                                }
                                break;
                            }
                        }
                    }
                }
                return Ok(LinuxInfo::ManyLinux(calculate_manylinux_info(
                    elf.ehdr,
                    glibc_version,
                )?));
            }
        }
        bail!(
            "Failed to gather information about the libc linked by {exe}",
            exe = exe.as_ref().display()
        );
    }

    fn calculate_manylinux_info(
        header: FileHeader<AnyEndian>,
        glibc_version: Option<LibcVersion>,
    ) -> anyhow::Result<Manylinux> {
        let _32_bit_little_endian =
            matches!(header.class, Class::ELF32) && header.endianness.is_little();
        let armhf = {
            if !_32_bit_little_endian || header.e_machine != elf::abi::EM_ARM {
                false
            } else {
                // The e_flags for 32-bit arm are documented here:
                // https://github.com/ARM-software/abi-aa/blob/main/aaelf32/aaelf32.rst#52elf-header
                const EF_ARM_ABIMASK: u32 = 0xFF000000;
                const EF_ARM_ABI_VER5: u32 = 0x05000000;
                const EF_ARM_ABI_FLOAT_HARD: u32 = 0x00000400;
                if header.e_flags & EF_ARM_ABIMASK != EF_ARM_ABI_VER5 {
                    false
                } else {
                    header.e_flags & EF_ARM_ABI_FLOAT_HARD == EF_ARM_ABI_FLOAT_HARD
                }
            }
        };
        let i686 = _32_bit_little_endian && header.e_machine == elf::abi::EM_386;
        Ok(Manylinux {
            glibc: glibc_version,
            armhf,
            i686,
        })
    }
}

#[cfg(target_os = "linux")]
#[cfg(test)]
mod tests {
    use std::path::Path;

    use rstest::rstest;
    use target_lexicon::{Architecture, Endianness, Environment, HOST, PointerWidth};
    use testing::python_exe;

    use crate::linux::LinuxInfo;

    #[rstest]
    fn test_parse(python_exe: &Path) {
        let assert_linux_info = if matches!(HOST.environment, Environment::Musl) {
            |linux_info: LinuxInfo| assert!(matches!(linux_info, LinuxInfo::MuslLinux(_)))
        } else {
            |linux_info: LinuxInfo| {
                let manylinux = match linux_info {
                    LinuxInfo::ManyLinux(manylinux) => manylinux,
                    LinuxInfo::MuslLinux(libc_version) => {
                        panic!("Expected manylinux but detected musl {libc_version:?}")
                    }
                };
                assert_eq!(
                    matches!(HOST.environment, Environment::Gnu),
                    manylinux.glibc.is_some(),
                    "The build target is {HOST:#?}",
                );
                if manylinux.armhf {
                    assert_eq!(
                        PointerWidth::U32,
                        HOST.architecture.pointer_width().unwrap()
                    );
                    assert_eq!(Endianness::Little, HOST.architecture.endianness().unwrap());
                    assert!(matches!(HOST.architecture, Architecture::Arm(_)));
                }
                if manylinux.i686 {
                    assert_eq!(
                        PointerWidth::U32,
                        HOST.architecture.pointer_width().unwrap()
                    );
                    assert_eq!(Endianness::Little, HOST.architecture.endianness().unwrap());
                    assert!(matches!(HOST.architecture, Architecture::X86_32(_)));
                }
            }
        };
        let linux_info = LinuxInfo::parse(python_exe).unwrap();
        assert_linux_info(linux_info);
    }
}
