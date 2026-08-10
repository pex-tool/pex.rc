// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::path::{Path, PathBuf};
use std::str::FromStr;

use anyhow::bail;
use enumset::EnumSet;
use indexmap::IndexSet;
use target::SimplifiedTarget;
use wheel::WheelFile;

use crate::embeds::Binary;

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

#[derive(Eq, PartialEq, Hash)]
struct RequiredTarget<'a> {
    targets: EnumSet<SimplifiedTarget>,
    required_by: &'a str,
}

impl<'a> RequiredTarget<'a> {
    fn satisfied_by(&self, target: SimplifiedTarget) -> bool {
        self.targets.contains(target)
    }
}

impl<'a> Display for RequiredTarget<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{targets} required by {wheel}",
            targets = self.targets,
            wheel = self.required_by
        )
    }
}

pub struct RequiredTargets<'a, S: Display> {
    pub subject: S,
    required_targets: IndexSet<RequiredTarget<'a>>,
}

impl<'a, S: Display> RequiredTargets<'a, S> {
    pub fn for_wheel_files(
        subject: S,
        wheel_files: impl Iterator<Item = &'a WheelFile<'a>>,
    ) -> anyhow::Result<Self> {
        let mut targets_by_project_name = HashMap::new();
        for wheel_file in wheel_files {
            targets_by_project_name
                .entry(&wheel_file.project_name)
                .or_insert_with(HashSet::new)
                .extend({
                    let compatible = wheel_file
                        .tags
                        .iter()
                        .filter_map(|tag| {
                            SimplifiedTarget::for_platform_tag(tag.platform)
                                .map(|targets| {
                                    targets.map(|targets| (wheel_file.file_name, targets))
                                })
                                .ok()
                        })
                        .collect::<Vec<_>>();
                    if compatible.is_empty() {
                        bail!(
                            "There are no pexrc binaries available that support {wheel}.",
                            wheel = wheel_file.file_name
                        )
                    }
                    compatible
                });
        }
        let mut required_targets = IndexSet::new();
        for required in targets_by_project_name.values() {
            if required.contains(&None) {
                // If a project has an "-any" whl, we can always resolve that, potentially at the
                // cost of perf; so we ignore these projects.
                continue;
            }
            for required_target in required {
                let (required_by, targets) =
                    required_target.expect("We confirmed all targets were Some above.");
                required_targets.insert(RequiredTarget {
                    targets,
                    required_by,
                });
            }
        }
        Ok(Self {
            subject,
            required_targets,
        })
    }

    pub fn select_binaries<'b>(
        &self,
        binaries: &[&'b Binary<'b>],
    ) -> anyhow::Result<IndexSet<&'b Binary<'b>>> {
        if self.required_targets.is_empty() {
            return Ok(binaries.iter().copied().collect());
        }
        let mut selected = IndexSet::with_capacity(binaries.len());
        for required_target in &self.required_targets {
            let mut satisifed = false;
            for binary in binaries {
                if required_target.satisfied_by(binary.target) {
                    selected.insert(*binary);
                    satisifed = true;
                }
            }
            if !satisifed {
                bail!(
                    "This pexrc binary has no clib that satisfies {required_target} for {subject}.",
                    subject = self.subject
                )
            }
        }
        Ok(selected)
    }

    pub fn unique_targets(&self) -> EnumSet<SimplifiedTarget> {
        self.required_targets
            .iter()
            .flat_map(|target| target.targets)
            .collect()
    }
}
