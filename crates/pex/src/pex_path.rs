// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::env;
use std::path::Path;

use crate::{Pex, RawPexInfo};

pub struct PexPath<'a>(Vec<Cow<'a, Path>>);

impl<'a> PexPath<'a> {
    pub fn from_pex_info(pex_info: &'a RawPexInfo, allow_env_override: bool) -> Self {
        let mut pex_path: Vec<Cow<'a, Path>> = Vec::with_capacity(pex_info.pex_paths.len());
        if allow_env_override && let Some(path) = env::var_os("PEX_PATH") {
            pex_path.extend(env::split_paths(&path).map(Cow::Owned))
        } else if !pex_info.pex_paths.is_empty() {
            pex_path.extend(pex_info.pex_paths.iter().cloned())
        } else if let Some(legacy_pex_path) = pex_info.pex_path.as_ref()
            && !legacy_pex_path.is_empty()
        {
            // Legacy PEX-INFO stored this in a single string as a colon-separated list.
            for entry in legacy_pex_path.split(':') {
                pex_path.push(Cow::Borrowed(Path::new(entry)))
            }
        }
        Self(pex_path)
    }

    pub fn load_pexes(&self) -> anyhow::Result<Vec<Pex<'_>>> {
        let mut pexes = Vec::with_capacity(self.0.len());
        for path in &self.0 {
            pexes.push(Pex::load(path)?)
        }
        Ok(pexes)
    }
}
