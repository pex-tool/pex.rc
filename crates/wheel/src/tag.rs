// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::{anyhow, bail};

#[derive(Debug, Eq, Hash, PartialEq)]
pub struct Tag<'a> {
    pub python: &'a str,
    pub abi: &'a str,
    pub platform: &'a str,
}

impl<'a> Tag<'a> {
    pub fn parse(tag: &'a str) -> anyhow::Result<Self> {
        let mut tags = tag.split("-");
        let python = tags
            .next()
            .ok_or_else(|| anyhow!("Failed to find python tag in {tag}"))?;
        let abi = tags
            .next()
            .ok_or_else(|| anyhow!("Failed to find abi tag in {tag}"))?;
        let platform = tags
            .next()
            .ok_or_else(|| anyhow!("Failed to find platform tag in {tag}"))?;
        if tags.next().is_some() {
            bail!("Failed to parse tag from {tag}")
        }
        Ok(Self {
            python,
            abi,
            platform,
        })
    }
}
