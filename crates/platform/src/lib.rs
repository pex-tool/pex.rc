// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

// N.B.: We stick to stdlib as much as possible in this crate since it is re-used by the
// python-proxy crate which produces a size-sensitive binary. Particularly, no fs-err or anyhow: we
// handle ensuring useful errors manually in this crate.

#![deny(clippy::all)]
#![cfg_attr(windows, feature(once_cell_try_insert))]

#[cfg(unix)]
pub mod unix;

#[cfg(windows)]
pub mod windows;

use std::ffi::OsStr;
use std::fmt::{Display, Formatter, Write};
use std::path::{Component, Path};
use std::process::Command;
use std::{fs, io};

pub enum Perms {
    Perms(fs::Permissions),
    Mode(u32),
}

#[cfg(unix)]
pub use unix::{
    exec,
    is_executable,
    mark_executable,
    path_as_bytes,
    path_for_terminal_output,
    set_permissions,
    symlink_or_link_or_copy,
};
#[cfg(windows)]
pub use windows::{
    exec,
    is_executable,
    mark_executable,
    path_as_bytes,
    path_for_terminal_output,
    set_permissions,
    symlink_or_link_or_copy,
};

pub fn path_as_str(path: &Path) -> io::Result<&str> {
    path.to_str().ok_or_else(|| {
        io::Error::other(format!(
            "Failed to convert non-UTF8 path to str: {path}",
            path = path.display()
        ))
    })
}

pub fn os_str_as_str(text: &OsStr) -> io::Result<&str> {
    text.to_str().ok_or_else(|| {
        io::Error::other(format!(
            "Failed to convert non-UTF8 text to str: {text}",
            text = text.display()
        ))
    })
}

pub fn link_or_copy(src: impl AsRef<Path>, dst: impl AsRef<Path>) -> io::Result<()> {
    fs::hard_link(src.as_ref(), dst.as_ref())
        .or_else(|_| fs::copy(src.as_ref(), dst.as_ref()).map(|_| ()))
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "Failed to link or copy {src} -> {dst}: {err}",
                    src = src.as_ref().display(),
                    dst = dst.as_ref().display()
                ),
            )
        })
}

pub fn spawn(command: &mut Command) -> io::Result<i32> {
    let mut child = command.spawn()?;
    child.wait().map(|exit_status| {
        exit_status
            .code()
            .unwrap_or_else(|| if exit_status.success() { 0 } else { 1 })
    })
}

pub struct PosixPath<'a>(&'a Path);

impl<'a> PosixPath<'a> {
    pub fn relpath(path: &'a Path) -> io::Result<Self> {
        Self::new(path, false)
    }

    pub fn new(path: &'a Path, absolute_allowed: bool) -> io::Result<Self> {
        if !absolute_allowed && !path.is_relative() {
            return Err(io::Error::other(format!(
                "Path {path} cannot be represented as a Posix relative path, it is absolute.",
                path = path.display()
            )));
        }
        if let Some(Component::Prefix(prefix)) = path.components().next() {
            return Err(io::Error::other(format!(
                "Path {path} cannot be represented as a Posix path because it has a Windows path \
                prefix of {prefix}",
                path = path.display(),
                prefix = prefix.as_os_str().display()
            )));
        }
        Ok(Self(path))
    }
}

impl<'a> TryFrom<&'a str> for PosixPath<'a> {
    type Error = io::Error;

    fn try_from(value: &'a str) -> Result<Self, Self::Error> {
        Self::new(Path::new(value), true)
    }
}

impl<'a> Display for PosixPath<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        for (idx, component) in self.0.components().enumerate() {
            if idx > 0 {
                f.write_char('/')?;
            }
            match component {
                Component::CurDir => f.write_char('.')?,
                Component::ParentDir => f.write_str("..")?,
                Component::RootDir => f.write_char('/')?,
                Component::Normal(name) => f.write_str(name.to_str().ok_or(std::fmt::Error)?)?,
                Component::Prefix(_) => {
                    panic!("We confirmed the path had no prefix on construction")
                }
            }
        }
        Ok(())
    }
}
