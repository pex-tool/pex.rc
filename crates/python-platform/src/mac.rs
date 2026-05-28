// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::bail;

#[derive(Copy, Clone, Eq, PartialEq, Ord, PartialOrd)]
pub struct Release {
    pub major: u16,
    pub minor: u8,
    pub patch: Option<u8>,
}

impl Release {
    pub(crate) fn new(major: u16, minor: u8) -> Self {
        Self {
            major,
            minor,
            patch: None,
        }
    }

    pub(crate) fn parse(value: &str, sep: char) -> anyhow::Result<Self> {
        let mut components = value.splitn(3, sep);
        let major = components
            .next()
            .expect("Split will always have at least one component.")
            .parse::<u16>()?;
        let minor = if let Some(minor) = components.next() {
            minor.parse::<u8>()?
        } else {
            bail!(
                "A mac version should always have a minor version component (i.e.: \
                <major>{sep}<minor>); given: {value}"
            )
        };
        let patch = if let Some(patch) = components.next() {
            Some(patch.parse::<u8>()?)
        } else {
            None
        };
        Ok(Self {
            major,
            minor,
            patch,
        })
    }

    #[cfg(target_os = "macos")]
    pub(crate) fn current() -> anyhow::Result<Self> {
        release::current()
    }
}

#[cfg(target_os = "macos")]
mod release {
    use std::path::Path;

    use crate::mac::Release;

    pub(super) fn current() -> anyhow::Result<Release> {
        let system_version_info_path =
            Path::new("/System/Library/CoreServices/SystemVersion.plist");
        let system_version_info: plist::Dictionary = plist::from_file(system_version_info_path)?;
        let product_version = system_version_info
            .get("ProductVersion")
            .and_then(plist::Value::as_string)
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Expected ProductVersion value in {path} to be a string.",
                    path = system_version_info_path.display()
                )
            })?;
        Ok(Release::parse(product_version, '.')?)
    }
}
