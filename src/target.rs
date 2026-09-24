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
