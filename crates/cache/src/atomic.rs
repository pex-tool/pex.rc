// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use anyhow::anyhow;
use fs_err as fs;
use fs_err::File;
use tracing::instrument;

#[instrument(level = "debug", skip(func))]
pub fn atomic_file<T>(
    path: &Path,
    func: impl Fn(File) -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    if path.is_file() {
        return Ok(None);
    }
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        fs::create_dir_all(parent)?;
    }
    let lock_file_path = path.with_added_extension("lck");
    let lock_file = File::options()
        .write(true)
        .create(true)
        .truncate(false)
        .open(&lock_file_path)?;
    lock_file.lock()?;
    if path.is_file() {
        return Ok(None);
    }

    let result = func(lock_file)?;
    fs::rename(lock_file_path, path)?;
    Ok(Some(result))
}

#[instrument(level = "trace", skip(func))]
pub fn atomic_dir<T>(
    path: &Path,
    func: impl FnOnce(&Path) -> anyhow::Result<T>,
) -> anyhow::Result<Option<T>> {
    if path.is_dir() {
        return Ok(None);
    }

    let parent_dir = path.parent().ok_or_else(|| {
        anyhow!(
            "Cannot create an atomic directory at the root of the file-system.\n\
            Asked to create atomic dir at: {path}",
            path = path.display()
        )
    })?;
    fs::create_dir_all(parent_dir)?;
    let lock_file_path = path.with_added_extension("lck");
    let lock_file = File::create(&lock_file_path)?;
    lock_file.lock()?;
    if path.is_dir() {
        return Ok(None);
    }

    let mut temp_dir = tempfile::tempdir_in(parent_dir)?;
    let result = func(temp_dir.path())?;
    fs::rename(temp_dir.path(), path)?;
    temp_dir.disable_cleanup(true);
    Ok(Some(result))
}
