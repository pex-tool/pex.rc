// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::io::{Read, Seek, Write};
use std::path::Path;

use anyhow::Context;
use chrono::{TimeZone, Utc};
use zip::read::ZipFile;
use zip::write::{SimpleFileOptions, StreamWriter};
use zip::{CompressionMethod, DateTime, ZipArchive, ZipWriter};

pub trait ZipArchiveExt<R: Read + Seek> {
    fn by_name_ex(&mut self, name: &str) -> anyhow::Result<ZipFile<'_, R>>;
}

impl<R: Read + Seek> ZipArchiveExt<R> for ZipArchive<R> {
    fn by_name_ex(&mut self, name: &str) -> anyhow::Result<ZipFile<'_, R>> {
        self.by_name(name)
            .with_context(|| format!("Failed to find zip entry {name}."))
    }
}

#[derive(Clone, Copy)]
pub enum EntryType {
    Directory,
    Executable,
    File,
}

impl EntryType {
    fn of(entry: &ZipFile<impl Read>) -> EntryType {
        if entry.is_dir() {
            EntryType::Directory
        } else if let Some(mode) = entry.unix_mode()
            && (mode & 0o111) != 0
        {
            EntryType::Executable
        } else {
            EntryType::File
        }
    }

    pub fn is_executable(entry: &ZipFile<impl Read>) -> bool {
        matches!(Self::of(entry), Self::Executable)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Modes {
    pub directory: u32,
    pub executable: u32,
    pub file: u32,
}

impl Modes {
    const DEFAULT: Self = Self {
        directory: 0o755,
        executable: 0o755,
        file: 0o644,
    };

    pub fn unix_permissions(&self, entry_type: EntryType) -> u32 {
        match entry_type {
            EntryType::Directory => self.directory,
            EntryType::Executable => self.executable,
            EntryType::File => self.file,
        }
    }
}

impl Default for Modes {
    fn default() -> Self {
        Self::DEFAULT
    }
}

pub struct PathOptions {
    last_modified_time: Option<DateTime>,
    unix_permissions: Option<u32>,
}

impl PathOptions {
    pub const DEFAULT: Self = Self {
        last_modified_time: None,
        unix_permissions: None,
    };

    pub fn last_modified_time(mut self, last_modified_time: DateTime) -> Self {
        self.last_modified_time = Some(last_modified_time);
        self
    }

    pub fn unix_permissions(mut self, permissions: u32) -> Self {
        self.unix_permissions = Some(permissions);
        self
    }
}

pub struct EntryOptions {
    path_options: PathOptions,
    entry_type: EntryType,
    compression_method: Option<CompressionMethod>,
}

impl EntryOptions {
    pub fn compression_method(mut self, compression_method: CompressionMethod) -> Self {
        self.compression_method = Some(compression_method);
        self
    }
}

#[derive(Clone, Copy)]
pub struct ZipOptions {
    pub compression_method: CompressionMethod,
    pub compression_level: Option<i64>,
    pub modes: Option<Modes>,
    pub reproducible_timestamp: bool,
}

impl ZipOptions {
    const DEFAULT: Self = Self {
        compression_method: CompressionMethod::Zstd,
        compression_level: None,
        modes: Some(Modes::DEFAULT),
        reproducible_timestamp: true,
    };

    fn file_options(&self, entry_options: &EntryOptions) -> SimpleFileOptions {
        let mut file_options = if let Some(compression_method) = entry_options.compression_method {
            SimpleFileOptions::DEFAULT.compression_method(compression_method)
        } else {
            SimpleFileOptions::DEFAULT
                .compression_method(self.compression_method)
                .compression_level(self.compression_level)
        };

        if self.modes.is_none() // Aka: `original`
            && let Some(unix_permissions) = entry_options.path_options.unix_permissions
        {
            file_options = file_options.unix_permissions(unix_permissions)
        } else {
            file_options = file_options.unix_permissions(
                self.modes
                    .unwrap_or(Modes::DEFAULT)
                    .unix_permissions(entry_options.entry_type),
            )
        }

        if self.reproducible_timestamp {
            file_options = file_options.last_modified_time(DateTime::DEFAULT)
        } else if let Some(last_modified_time) = entry_options.path_options.last_modified_time {
            file_options = file_options.last_modified_time(last_modified_time)
        }

        file_options
    }

    pub fn reproducible_timestamp_or_else(
        &self,
        timestamp_fn: impl FnOnce() -> Option<DateTime>,
    ) -> DateTime {
        if self.reproducible_timestamp {
            DateTime::DEFAULT
        } else {
            timestamp_fn().unwrap_or(DateTime::DEFAULT)
        }
    }

    pub fn chrono_timestamp(&self) -> Option<chrono::DateTime<Utc>> {
        if self.reproducible_timestamp {
            Some(
                Utc.from_utc_datetime(
                    &DateTime::DEFAULT
                        .try_into()
                        .expect("The default zip DateTime is a valid Chrono UTC DateTime."),
                ),
            )
        } else {
            None
        }
    }
}

impl Default for ZipOptions {
    fn default() -> Self {
        Self::DEFAULT
    }
}

pub struct DirectoryOptions(EntryOptions);

impl DirectoryOptions {
    pub const DEFAULT: Self = Self(EntryOptions {
        path_options: PathOptions::DEFAULT,
        entry_type: EntryType::Directory,
        compression_method: Some(CompressionMethod::Stored),
    });
}

impl From<PathOptions> for DirectoryOptions {
    fn from(path_options: PathOptions) -> Self {
        Self(EntryOptions {
            path_options,
            entry_type: EntryType::Directory,
            compression_method: Some(CompressionMethod::Stored),
        })
    }
}

pub struct FileOptions(EntryOptions);

impl FileOptions {
    pub fn from_path_options(path_options: PathOptions, executable: bool) -> Self {
        let entry_type = if executable {
            EntryType::Executable
        } else {
            EntryType::File
        };
        Self(EntryOptions {
            path_options,
            entry_type,
            compression_method: None,
        })
    }

    pub fn from_path(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let entry_type = if platform::is_executable(path)? {
            EntryType::Executable
        } else {
            EntryType::File
        };
        Ok(Self::from_entry_type(entry_type))
    }

    pub fn from_entry(entry: &ZipFile<impl Read>) -> Self {
        Self::new(EntryType::is_executable(entry))
    }

    pub fn new(executable: bool) -> Self {
        let entry_type = if executable {
            EntryType::Executable
        } else {
            EntryType::File
        };
        Self::from_entry_type(entry_type)
    }

    fn from_entry_type(entry_type: EntryType) -> Self {
        Self(EntryOptions {
            path_options: PathOptions::DEFAULT,
            entry_type,
            compression_method: None,
        })
    }

    pub fn compression_method(mut self, compression_method: CompressionMethod) -> Self {
        self.0 = self.0.compression_method(compression_method);
        self
    }
}

pub struct ZipWriterExt<W: Write + Seek> {
    underlying: ZipWriter<W>,
    options: ZipOptions,
}

impl<W: Write + Seek> ZipWriterExt<W> {
    pub fn new(inner: W, options: ZipOptions) -> Self {
        Self {
            underlying: ZipWriter::new(inner),
            options,
        }
    }

    pub fn add_directory(
        &mut self,
        name: impl Into<String>,
        directory_options: &DirectoryOptions,
    ) -> anyhow::Result<()> {
        let directory_options = self.options.file_options(&directory_options.0);
        Ok(self.underlying.add_directory(name, directory_options)?)
    }

    pub fn add_directory_from_path(
        &mut self,
        path: impl AsRef<Path>,
        directory_options: &DirectoryOptions,
    ) -> anyhow::Result<()> {
        let directory_options = self.options.file_options(&directory_options.0);
        Ok(self
            .underlying
            .add_directory_from_path(path, directory_options)?)
    }

    pub fn start_file(
        &mut self,
        name: impl ToString,
        file_options: &FileOptions,
    ) -> anyhow::Result<()> {
        let file_options = self.options.file_options(&file_options.0);
        Ok(self.underlying.start_file(name, file_options)?)
    }

    pub fn start_file_from_path(
        &mut self,
        path: impl AsRef<Path>,
        file_options: &FileOptions,
    ) -> anyhow::Result<()> {
        let file_options = self.options.file_options(&file_options.0);
        Ok(self.underlying.start_file_from_path(path, file_options)?)
    }

    pub fn raw_copy_file(&mut self, file: ZipFile<'_, impl Read>) -> anyhow::Result<()> {
        let last_modified_time = self
            .options
            .reproducible_timestamp_or_else(|| file.last_modified());
        let unix_mode = if let Some(modes) = self.options.modes {
            Some(modes.unix_permissions(EntryType::of(&file)))
        } else {
            file.unix_mode()
        };
        Ok(self
            .underlying
            .raw_copy_file_touch(file, last_modified_time, unix_mode)?)
    }

    pub fn set_comment(&mut self, comment: impl Into<Box<str>>) -> anyhow::Result<()> {
        Ok(self.underlying.set_comment(comment)?)
    }

    pub fn finish(self) -> anyhow::Result<W> {
        Ok(self.underlying.finish()?)
    }
}

impl<A: Read + Write + Seek> ZipWriterExt<A> {
    pub fn new_append(inner: A, options: ZipOptions) -> anyhow::Result<Self> {
        Ok(Self {
            underlying: ZipWriter::new_append(inner)?,
            options,
        })
    }
}

impl<W: Write> ZipWriterExt<StreamWriter<W>> {
    pub fn new_stream(inner: W, options: ZipOptions) -> ZipWriterExt<StreamWriter<W>> {
        Self {
            underlying: ZipWriter::new_stream(inner),
            options,
        }
    }
}

impl<W: Write + Seek> Write for ZipWriterExt<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.underlying.write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.underlying.flush()
    }
}
