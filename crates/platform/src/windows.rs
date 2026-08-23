// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter, Write};
use std::fs::File;
use std::path::{Component, Path, Prefix};
use std::process::Command;
use std::sync::OnceLock;
use std::{env, fmt, io};

use is_executable::IsExecutable;

use crate::Perms;

pub fn symlink_or_link_or_copy(
    src: impl AsRef<Path>,
    dst: impl AsRef<Path>,
    _relative: bool,
) -> io::Result<()> {
    crate::link_or_copy(src, dst)
}

pub fn is_executable(path: impl AsRef<Path>) -> io::Result<bool> {
    Ok(path.as_ref().is_executable())
}

pub const fn mark_executable(_file: &mut File) -> io::Result<()> {
    Ok(())
}

pub const fn set_permissions(_file: &mut File, _perms: Perms) -> io::Result<()> {
    Ok(())
}

pub fn path_as_bytes(path: &Path) -> io::Result<&[u8]> {
    crate::path_as_str(path).map(str::as_bytes)
}

pub fn exec(command: &mut Command, _files_to_keep_open: &[File]) -> io::Result<i32> {
    crate::spawn(command)
}

static TERMINAL_USES_POSIX_PATHS: OnceLock<bool> = OnceLock::new();

struct WindowsPathForTerminalOutput<'a> {
    path: &'a Path,
    terminal_uses_posix_paths: bool,
}

impl<'a> Display for WindowsPathForTerminalOutput<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        if self.terminal_uses_posix_paths {
            let mut drive_output = false;
            for (idx, component) in self.path.components().enumerate() {
                if idx > 0 && (!drive_output || !matches!(component, Component::RootDir)) {
                    f.write_char('/')?;
                }
                match component {
                    Component::CurDir => f.write_char('.')?,
                    Component::ParentDir => f.write_str("..")?,
                    Component::RootDir => {
                        if !drive_output {
                            f.write_char('/')?
                        }
                    }
                    Component::Normal(name) => f.write_str(name.to_str().ok_or(fmt::Error)?)?,
                    Component::Prefix(prefix_component) => match prefix_component.kind() {
                        Prefix::Verbatim(component) => {
                            f.write_str(component.to_str().ok_or(fmt::Error)?)?
                        }
                        Prefix::VerbatimDisk(disk_letter) | Prefix::Disk(disk_letter) => {
                            drive_output = true;
                            f.write_char('/')?;
                            f.write_char(char::from(disk_letter))?
                        }
                        _ => return Err(fmt::Error),
                    },
                }
            }
            Ok(())
        } else {
            self.path.display().fmt(f)
        }
    }
}

pub fn set_terminal_uses_posix_paths(terminal_uses_posix_paths: bool) -> Option<bool> {
    if let Err((existing_value, _)) =
        TERMINAL_USES_POSIX_PATHS.try_insert(terminal_uses_posix_paths)
    {
        Some(*existing_value)
    } else {
        None
    }
}

pub fn path_for_terminal_output(path: &Path) -> impl Display {
    WindowsPathForTerminalOutput {
        path,
        terminal_uses_posix_paths: *TERMINAL_USES_POSIX_PATHS.get_or_init(|| {
            // N.B.: This works for git bash (msys), but surely could be made more robust.
            env::var("TERM")
                .ok()
                .map(|value| value.contains("xterm"))
                .or(env::var("SHELL").ok().map(|value| value.contains('/')))
                .unwrap_or(false)
        }),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::WindowsPathForTerminalOutput;

    fn wp(path: &str, terminal_uses_posix_paths: bool) -> String {
        WindowsPathForTerminalOutput {
            path: Path::new(path),
            terminal_uses_posix_paths,
        }
        .to_string()
    }

    fn assert_posix_terminal_output(expected: &str, actual: &str) {
        assert_eq!(expected, wp(actual, true).as_str());
    }

    fn assert_windows_terminal_output(expected: &str, actual: &str) {
        assert_eq!(expected, wp(actual, false).as_str());
    }

    #[test]
    fn test_path_for_terminal_output() {
        assert_posix_terminal_output("/C", "C:");
        assert_posix_terminal_output("/C", r"C:\");
        assert_posix_terminal_output("/Z/foo", r"Z:\foo");
        assert_posix_terminal_output("/Z/foo", r"Z:\foo\");

        assert_windows_terminal_output("C:", "C:");
        assert_windows_terminal_output(r"C:\", r"C:\");
        assert_windows_terminal_output(r"Z:\foo", r"Z:\foo");
        assert_windows_terminal_output(r"Z:\foo\", r"Z:\foo\");
    }
}
