// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::{Component, Path, PathBuf};

use anyhow::anyhow;
use csv::{StringRecord, Terminator};
use fs_err::File;
use ouroboros::self_referencing;

use crate::wheel::file::MetadataDirs;

pub(crate) struct Entry<'a> {
    pub(crate) path: Cow<'a, Path>,
    pub(crate) raw_path: &'a str,
    pub(crate) hash: &'a str,
    pub(crate) size: &'a str,
}

fn parse_entry_record<'a>(
    row: usize,
    record: Result<&'a StringRecord, &'a csv::Error>,
) -> Option<anyhow::Result<Entry<'a>>> {
    match record {
        Ok(record) => {
            if record.is_empty() {
                return None;
            }
            let fields = record.into_iter().collect::<Vec<_>>();
            match fields.as_slice() {
                &[raw_path, hash, size] => {
                    // N.B.: The spec here is very poor:
                    // https://packaging.python.org/specifications/recording-installed-packages/#the-record-file
                    // There is no such thing as "on Windows" since a non-platform-specific wheel
                    // could be created on Windows or Unix and uploaded to a registry. That said,
                    // the occurrence of a dir name like bin/suffix\\ on Windows or vice versa seems
                    // unlikely due to all the problems it would cause the wheel author when people
                    // went to use it.
                    #[cfg(unix)]
                    let path = Cow::Borrowed(Path::new(raw_path));
                    #[cfg(windows)]
                    let path = Cow::Owned(raw_path.split("/").collect());

                    Some(Ok(Entry {
                        path,
                        raw_path,
                        hash,
                        size,
                    }))
                }
                _ => Some(Err(anyhow!(
                    "Each row should have path,hash,size: row {row} is missing fields: {record}",
                    record = record.as_slice()
                ))),
            }
        }
        Err(err) => Some(Err(anyhow!("{err}"))),
    }
}

#[self_referencing]
pub struct Record {
    records: Vec<csv::Result<StringRecord>>,
    terminator: Terminator,

    #[borrows(records)]
    #[covariant]
    entries: Vec<Entry<'this>>,
}

impl Record {
    pub(crate) fn parse(
        wheel_dir: &Path,
        metadata_dirs: &MetadataDirs,
    ) -> anyhow::Result<(Self, PathBuf)> {
        let record_rel_path = metadata_dirs.dist_info_dir().as_path().join("RECORD");
        let record = Self::read(File::open(wheel_dir.join(&record_rel_path))?)?;
        Ok((record, record_rel_path))
    }

    pub fn read(source: impl Read + Seek) -> anyhow::Result<Self> {
        let mut first_line = String::new();
        let mut buffered_source = BufReader::new(source);
        buffered_source.read_line(&mut first_line)?;
        buffered_source.rewind()?;
        let terminator = if first_line.ends_with("\r\n") {
            Terminator::CRLF
        } else {
            Terminator::Any(b'\n')
        };
        let records = csv::ReaderBuilder::new()
            .has_headers(false)
            .quote(b'"')
            .delimiter(b',')
            .terminator(Terminator::CRLF)
            .from_reader(buffered_source)
            .into_records()
            .collect::<Vec<_>>();
        Self::try_new(records, terminator, |records| {
            records
                .iter()
                .enumerate()
                .filter_map(|(idx, record)| parse_entry_record(idx + 1, record.as_ref()))
                .collect()
        })
    }

    pub(crate) fn entries(&self) -> &[Entry<'_>] {
        self.borrow_entries().as_slice()
    }

    pub fn wheel_has_bin_dir(&self) -> bool {
        self.entries().iter().any(|entry| {
            matches!(entry.path.components().next(), Some(Component::Normal(name)) if name == "bin")
        })
    }

    pub(crate) fn filtered(
        &self,
        metadata_dirs: &MetadataDirs,
        stash_dir: Option<&Path>,
        legacy_bin_dir: Option<&Path>,
    ) -> anyhow::Result<Vec<u8>> {
        let mut data = Vec::new();
        let mut writer = csv::WriterBuilder::new()
            .has_headers(false)
            .quote(b'"')
            .delimiter(b',')
            .terminator(*self.borrow_terminator())
            .from_writer(&mut data);
        let pex_info_dir = metadata_dirs.pex_info_dir();
        for entry in self.borrow_entries() {
            if pex_info_dir.contains(entry.path.as_ref()) {
                continue;
            }
            if let Some(stash_dir) = stash_dir
                && entry.path.starts_with(stash_dir)
            {
                continue;
            }
            if let Some(legacy_bin_dir) = legacy_bin_dir
                && entry.path.starts_with(legacy_bin_dir)
            {
                continue;
            }
            writer.write_field(entry.raw_path)?;
            writer.write_field(entry.hash)?;
            writer.write_field(entry.size)?;
            writer.write_record(None::<&[u8]>)?;
        }
        writer.flush()?;
        drop(writer);
        Ok(data)
    }
}
