// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsStr;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

use clap::Args;
use fs_err::File;

#[derive(Args)]
pub struct Output {
    /// A file to send output to; STDOUT by default.
    #[arg(short = 'o', long, help_heading = "Output")]
    output: Option<PathBuf>,
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
