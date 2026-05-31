// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::path::{Path, PathBuf};
use std::str::FromStr;

pub const PYTHON_PLATFORM_LONG_HELP: &str = r#"
Can be either the path to a local Python executable or else a Python platform spec.
In its simplest form, the spec can be just a Python version number; and CPython will be
assumed. The version number must be in <major>.<minor>(.<micro>) form. If the micro version
is not specified, 0 is used. For example:
+ 3.14
+ 3.14.5

The Python implementation can be selected by prefixing the version with cpython or pypy:
+ cpython-3.14.5
+ pypy-3.11

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

#[derive(Clone, Debug)]
pub enum PythonPlatform {
    Spec(String),
    Interpreter(PathBuf),
}

impl PythonPlatform {
    pub fn parse(value: &str) -> anyhow::Result<Self> {
        let interpreter = Path::new(value);
        if interpreter.is_file() && platform::is_executable(interpreter)? {
            Ok(Self::Interpreter(interpreter.to_owned()))
        } else {
            Ok(Self::Spec(value.to_owned()))
        }
    }
}

impl FromStr for PythonPlatform {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::parse(s)
    }
}
