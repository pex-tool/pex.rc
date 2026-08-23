// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::fmt::{Display, Formatter};
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::{Args, ValueEnum};
use fs_err::File;

#[derive(Clone, ValueEnum)]
enum PathOutputStyle {
    Auto,
    Posix,
    Windows,
}

impl Display for PathOutputStyle {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            PathOutputStyle::Auto => "auto",
            PathOutputStyle::Posix => "posix",
            PathOutputStyle::Windows => "windows",
        };
        f.write_str(value)
    }
}

#[derive(Args)]
pub struct Output {
    /// A file to send output to; STDOUT by default.
    #[arg(short = 'o', long, help_heading = "Output")]
    output: Option<PathBuf>,

    /// Set the style file-system paths are output in.
    ///
    /// By default, the style is auto-detected by examining the environment for clues from
    /// `TERM`, `SHELL`, etc.
    #[cfg(windows)]
    #[arg(long, help_heading = "Output", default_value_t = PathOutputStyle::Auto)]
    path_style: PathOutputStyle,
}

impl Output {
    pub fn writer(&self) -> anyhow::Result<impl Write> {
        Sink::new(self.output.as_deref())
    }

    pub fn file(
        &self,
        tmp_prefix: Option<&(impl AsRef<OsStr> + ?Sized)>,
        tmp_suffix: Option<&(impl AsRef<OsStr> + ?Sized)>,
    ) -> anyhow::Result<File> {
        if let Some(path) = self.output.as_deref() {
            Ok(File::create(path)?)
        } else {
            let temp = {
                let mut temp = tempfile::Builder::new();
                if let Some(prefix) = tmp_prefix {
                    temp.prefix(prefix);
                }
                if let Some(suffix) = tmp_suffix {
                    temp.suffix(suffix);
                }
                temp.tempfile()?
            };
            let (file, path) = temp.keep()?;
            Ok(File::from_parts(file, path))
        }
    }

    pub fn configure(&self) -> anyhow::Result<()> {
        if let Some(output) = self.output.as_deref() {
            let out = File::options().create(true).write(true).open(output)?;
            let mut stdout = io::stdout().lock();
            #[cfg(unix)]
            {
                use std::os::unix::io::StdioExt;
                stdout.set_fd(out)?;
            }
            #[cfg(windows)]
            {
                use std::os::windows::io::StdioExt;
                stdout.set_handle(Some(out))?;
            }
        }

        #[cfg(windows)]
        self.configure_posix_paths();

        Ok(())
    }

    #[cfg(windows)]
    pub fn configure_posix_paths(&self) {
        if let Some(terminal_uses_posix_paths) = match self.path_style {
            PathOutputStyle::Auto => None,
            PathOutputStyle::Posix => Some(true),
            PathOutputStyle::Windows => Some(false),
        } {
            if let Some(previous_value) =
                platform::windows::set_terminal_uses_posix_paths(terminal_uses_posix_paths)
            {
                panic!(
                    "Use of Posix paths was unexpectedly already configured to: {previous_value}"
                )
            }
        }
    }
}

enum Sink {
    File(File),
    Stdout(io::Stdout),
}

impl Sink {
    fn new(file: Option<&Path>) -> anyhow::Result<Self> {
        Ok(if let Some(path) = file {
            Self::File(File::create(path)?)
        } else {
            Self::Stdout(io::stdout())
        })
    }
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Sink::File(file) => file.write(buf),
            Sink::Stdout(stdout) => stdout.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Sink::File(file) => file.flush(),
            Sink::Stdout(stdout) => stdout.flush(),
        }
    }
}
