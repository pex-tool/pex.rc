// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::fmt::Display;
use std::fs::File;
use std::io;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::os::unix::{self};
use std::path::Path;
use std::process::Command;

use rustix::fs::{Access, OFlags, access, fcntl_getfl, fcntl_setfl};
use rustix::io::Errno;

use crate::Perms;

pub fn symlink_or_link_or_copy(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    relative: bool,
) -> io::Result<()> {
    symlink(src, dst, relative)
}

pub fn symlink(src: impl AsRef<Path>, dst: impl AsRef<Path>, relative: bool) -> io::Result<()> {
    let src = if relative
        && let Some(rel_base) = dst.as_ref().parent()
        && let Some(rel_path) = pathdiff::diff_paths(src.as_ref(), rel_base)
    {
        Cow::Owned(rel_path)
    } else {
        Cow::Borrowed(src.as_ref())
    };
    unix::fs::symlink(src, dst)
}

pub fn is_executable(path: impl AsRef<Path>) -> io::Result<bool> {
    if !path
        .as_ref()
        .metadata()
        .map_err(|err| {
            io::Error::new(
                err.kind(),
                format!(
                    "Failed to determine if {path} is a file: {err}",
                    path = path.as_ref().display(),
                ),
            )
        })?
        .is_file()
    {
        return Ok(false);
    }
    match access(path.as_ref(), Access::EXEC_OK) {
        Ok(()) => Ok(true),
        Err(err) => {
            if err == Errno::ACCESS {
                Ok(false)
            } else {
                // N.B.: There is not currently a canned conversion from nix errno's to ErrorKinds.
                // Laziness prevails here - we preserve the errno and desc in the message only (
                // Display for Errno renders both).
                Err(io::Error::other(format!(
                    "Failed to determine access mode bits for {path}: {err}",
                    path = path.as_ref().display(),
                )))
            }
        }
    }
}

pub fn mark_executable(file: &mut File) -> io::Result<()> {
    set_permissions(file, Perms::Mode(0o755))
}

pub fn set_permissions(file: &mut File, perms: Perms) -> io::Result<()> {
    match perms {
        Perms::Perms(permissions) => file.set_permissions(permissions),
        Perms::Mode(mode) => {
            let mut permissions = file.metadata()?.permissions();
            permissions.set_mode(mode);
            file.set_permissions(permissions)
        }
    }
}

pub fn path_as_bytes(path: &Path) -> io::Result<&[u8]> {
    Ok(path.as_os_str().as_bytes())
}

pub fn exec(command: &mut Command, files_to_keep_open: &[File]) -> io::Result<i32> {
    for file in files_to_keep_open {
        let mut flags = fcntl_getfl(file)?;
        flags |= OFlags::CLOEXEC;
        fcntl_setfl(file, flags)?;
    }
    Err(command.exec())
}

pub fn path_for_terminal_output(path: &Path) -> impl Display {
    path.display()
}
