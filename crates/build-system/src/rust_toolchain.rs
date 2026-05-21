// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;

use serde::Deserialize;
use target::{SimplifiedTarget, Target};

use crate::metadata::Glibc;

pub struct BuildTarget<'a> {
    target: Target<'a>,
    zigbuild_target: Option<String>,
}

impl<'a> BuildTarget<'a> {
    pub fn current(glibc: &'a Glibc<'a>) -> Self {
        Self::classify_target(
            Target::current().expect("We always build on targets we support."),
            glibc,
        )
    }

    pub fn classify(target: &'a str, glibc: &'a Glibc<'a>) -> Self {
        Self::classify_target(
            Target::classify(target).expect("We always build on targets we support."),
            glibc,
        )
    }
    pub fn classify_target(target: Target<'a>, glibc: &'a Glibc<'a>) -> Self {
        match target {
            Target::Apple(_) | Target::Unix(_) | Target::Windows(_) => Self {
                target,
                zigbuild_target: None,
            },
            Target::GnuLinux(triple) => Self {
                target,
                zigbuild_target: Some(format!(
                    "{triple}.{glibc_version}",
                    glibc_version = glibc.version(triple)
                )),
            },
        }
    }

    pub fn is_windows(&self) -> bool {
        matches!(self.target, Target::Windows(_))
    }

    pub fn as_str(&self) -> &str {
        self.target.as_str()
    }

    pub fn zigbuild_target(&self) -> &str {
        self.zigbuild_target
            .as_deref()
            .unwrap_or_else(|| self.target.as_str())
    }

    pub fn shared_library_name(&self, lib_name: &str) -> String {
        self.target.shared_library_name(lib_name)
    }

    pub fn binary_name(&self, binary_name: &'a str, exe_suffix: Option<&str>) -> Cow<'a, str> {
        self.target.binary_name(binary_name, exe_suffix)
    }

    pub fn simplified_target_triple(&self) -> anyhow::Result<SimplifiedTarget> {
        self.target.simplified_target_triple()
    }

    pub fn fully_qualified_binary_name(
        &self,
        binary_name: &str,
        target_suffix: Option<&str>,
    ) -> anyhow::Result<String> {
        self.target
            .fully_qualified_binary_name(binary_name, target_suffix)
    }
}

pub struct ClassifiedTargets<'a> {
    xwin_targets: Vec<BuildTarget<'a>>,
    zigbuild_targets: Vec<BuildTarget<'a>>,
}

impl<'a> ClassifiedTargets<'a> {
    pub fn parse(targets: impl Iterator<Item = &'a str>, glibc: &'a Glibc<'a>) -> Self {
        let (xwin_targets, zigbuild_targets) = targets
            .map(|target| BuildTarget::classify(target, glibc))
            .partition::<Vec<_>, _>(|build_target| {
                matches!(build_target.target, Target::Windows(_))
            });
        Self {
            // TODO: Resolve cargo xwin build issues or delete the cargo-xwin code paths.
            xwin_targets: vec![],
            zigbuild_targets: xwin_targets.into_iter().chain(zigbuild_targets).collect(),
        }
    }

    pub fn iter_zigbuild_targets(&'a self) -> impl ExactSizeIterator<Item = &'a BuildTarget<'a>> {
        self.zigbuild_targets.iter()
    }

    pub fn iter_xwin_targets(&'a self) -> impl ExactSizeIterator<Item = &'a BuildTarget<'a>> {
        self.xwin_targets.iter()
    }

    pub fn iter_all_targets(&'a self) -> impl Iterator<Item = &'a BuildTarget<'a>> {
        self.zigbuild_targets.iter().chain(self.xwin_targets.iter())
    }
}

#[derive(Deserialize)]
pub(crate) struct Toolchain<'a> {
    #[serde(borrow)]
    targets: Vec<&'a str>,
}

impl<'a> Toolchain<'a> {
    pub(crate) fn into_targets(self) -> Vec<String> {
        self.targets.into_iter().map(str::to_string).collect()
    }

    pub(crate) fn classify_targets(&self, glibc: &'a Glibc<'a>) -> ClassifiedTargets<'a> {
        ClassifiedTargets::parse(self.targets.iter().copied(), glibc)
    }
}

#[derive(Deserialize)]
struct RustToolchain<'a> {
    #[serde(borrow)]
    toolchain: Toolchain<'a>,
}

pub(crate) fn parse_toolchain(rust_toolchain_contents: &str) -> anyhow::Result<Toolchain<'_>> {
    let rust_toolchain: RustToolchain = toml::from_str(rust_toolchain_contents)?;
    Ok(rust_toolchain.toolchain)
}
