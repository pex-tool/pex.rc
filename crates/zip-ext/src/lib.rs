// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::io::{Read, Seek};

use anyhow::Context;
use zip::ZipArchive;
use zip::read::ZipFile;

pub trait ZipArchiveExt<R: Read + Seek> {
    fn by_name_ex(&mut self, name: &str) -> anyhow::Result<ZipFile<'_, R>>;
}

impl<R: Read + Seek> ZipArchiveExt<R> for ZipArchive<R> {
    fn by_name_ex(&mut self, name: &str) -> anyhow::Result<ZipFile<'_, R>> {
        self.by_name(name).with_context(|| name.to_owned())
    }
}
