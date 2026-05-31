// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::io::Read;
use std::path::{Path, PathBuf};

use fs_err::File;
use serde::Deserialize;

#[derive(Deserialize)]
pub struct WheelLayout {
    pub stash_dir: PathBuf,
}

impl WheelLayout {
    pub const fn file_name() -> &'static str {
        ".layout.json"
    }

    pub fn load_from_dir(dir: impl AsRef<Path>) -> anyhow::Result<Option<Self>> {
        let path = dir.as_ref().join(Self::file_name());
        if path.exists() {
            return Ok(Some(serde_json::from_reader(File::open(path)?)?));
        }
        Ok(None)
    }

    pub fn read(contents: impl Read) -> anyhow::Result<Self> {
        Ok(serde_json::from_reader(contents)?)
    }
}
