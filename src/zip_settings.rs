// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Debug, Display, Formatter};
use std::option::Option;

use anyhow::{anyhow, bail};
use clap::{Args, ValueEnum};
use indexmap::{IndexSet, indexset};
use zip_ext::ZipOptions;

#[derive(Copy, Clone, Debug, ValueEnum)]
pub enum CompressionMethod {
    Deflated,
    Zstd,
}

impl From<CompressionMethod> for zip::CompressionMethod {
    fn from(val: CompressionMethod) -> Self {
        match val {
            CompressionMethod::Deflated => zip::CompressionMethod::Deflated,
            CompressionMethod::Zstd => zip::CompressionMethod::Zstd,
        }
    }
}

#[derive(Copy, Clone, Debug)]
enum Modes {
    Original,
    Specified(zip_ext::Modes),
}

impl Modes {
    fn specified(directory: u32, executable: u32, file: u32) -> Self {
        Self::Specified(zip_ext::Modes {
            directory,
            executable,
            file,
        })
    }

    fn reproducible_timestamp(&self) -> bool {
        matches!(self, Self::Specified(_))
    }
}

impl From<Modes> for Option<zip_ext::Modes> {
    fn from(value: Modes) -> Self {
        match value {
            Modes::Original => None,
            Modes::Specified(modes) => Some(modes),
        }
    }
}

impl Display for Modes {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        match self {
            Modes::Original => f.write_str("original"),
            Modes::Specified(modes) => write!(
                f,
                "dir={dir:o},exe={exe:o},file={file:o}",
                dir = modes.directory,
                exe = modes.executable,
                file = modes.file
            ),
        }
    }
}

fn parse_modes(value: &str) -> anyhow::Result<Modes> {
    let parse_mode = |value| -> anyhow::Result<u32> {
        let mode = u32::from_str_radix(value, 8).map_err(|err| {
            anyhow!(
                "Invalid modes value `{value}`: {err}\n\
                Must be bare octal mode digits of the mode to apply to all zip entries, e.g.: \
                `755`."
            )
        })?;
        Ok(mode)
    };
    match value.split(',').collect::<Vec<_>>().as_slice() {
        ["original"] => Ok(Modes::Original),
        [only] => {
            let mode = parse_mode(only)?;
            Ok(Modes::specified(mode, mode, mode))
        }
        values @ [_, _, _] => {
            let mut dir_mode: Option<u32> = None;
            let mut exe_mode: Option<u32> = None;
            let mut file_mode: Option<u32> = None;
            for (idx, value) in values.iter().enumerate() {
                if let Some((type_name, mode)) = value.split_once('=') {
                    let mode = parse_mode(mode)?;
                    match type_name {
                        "dir" => dir_mode = Some(mode),
                        "exe" => exe_mode = Some(mode),
                        "file" => file_mode = Some(mode),
                        _ => {
                            bail!(
                                "Invalid file type {type_name} for mode setting.\n\
                                Must be one of `dir`, `exe` or `file`."
                            )
                        }
                    }
                    continue;
                }
                bail!(
                    "Invalid mode setting value at position {index}: {value}\n\
                    Mode settings must be of the form <file_type>=<mode> where <file_type> is \
                    `dir`, `exe` or `file` and <mode> is the mode in octal digits, e.g: 755 for \
                    rwx.",
                    index = idx + 1
                )
            }
            if let (Some(directory), Some(executable), Some(file)) = (dir_mode, exe_mode, file_mode)
            {
                Ok(Modes::specified(directory, executable, file))
            } else {
                let mut missing_file_types = indexset! {"dir", "exe", "file"};
                if dir_mode.is_some() {
                    missing_file_types.shift_remove("dir");
                }
                if exe_mode.is_some() {
                    missing_file_types.shift_remove("exe");
                }
                if file_mode.is_some() {
                    missing_file_types.shift_remove("file");
                }
                struct MissingFileTypes(IndexSet<&'static str>);
                impl Display for MissingFileTypes {
                    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                        for (idx, file_type) in self.0.iter().enumerate() {
                            if idx > 0 {
                                write!(f, " and ")?;
                            }
                            f.write_str(file_type)?;
                        }
                        Ok(())
                    }
                }
                bail!(
                    "Missing mode settings for: {}\n\
                    Must supply a setting for dir`, `exe` and `file`.",
                    MissingFileTypes(missing_file_types)
                )
            }
        }
        _ => bail!(
            "Invalid mode value: {value}\n\
            The modes value must be either a single octal mode value to apply to all zip \
            entries or else individual mode values keyed by file type of the form \
            `dir=<mode>,exe=<mode>,file=<mode>`."
        ),
    }
}

#[derive(Args, Debug)]
#[command(next_help_heading = "Zipping")]
#[group(skip)]
pub struct ZipArgs {
    /// The compression method to use for wheels and other files stored in the PEX.
    #[arg(
        short = 'Z',
        long,
        value_enum,
        default_value_t = CompressionMethod::Zstd,
    )]
    compression_method: CompressionMethod,

    /// The compression level to use.
    #[arg(long)]
    compression_level: Option<i64>,

    /// The modes to use for zip entries.
    ///
    /// Modes are specified in octal; i.e.: `rwx` for a directory is specified as `755`.
    ///
    /// The value can be a single file mode value, in which case it is applied to zip directory
    /// entries, executable entries and regular file entries. Alternatively, the value can be a
    /// comma-separated list of individual modes for directories (`dir`), executables (`exe`) and
    /// files (`file`); e.g.: `dir=750,exe=750,file=640`.
    ///
    /// A value of `original` can also be used to preserve original wheel entry modes (and
    /// timestamps) when available.
    #[arg(
        long,
        value_parser = parse_modes,
        default_value = "dir=755,exe=755,file=644",
        verbatim_doc_comment
    )]
    modes: Modes,
}

impl ZipArgs {
    pub fn into_zip_options(self) -> ZipOptions {
        let reproducible_timestamp = self.modes.reproducible_timestamp();
        ZipOptions {
            compression_method: self.compression_method.into(),
            compression_level: self.compression_level,
            modes: self.modes.into(),
            reproducible_timestamp,
        }
    }
}
