// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(str_as_str)]

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt::Display;
use std::io;
use std::io::{Cursor, Read, Seek, Write};
use std::ops::{Deref, DerefMut};
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use anyhow::bail;
use chrono::{DateTime, Utc};
use fs_err as fs;
use fs_err::File;
use logging_timer::time;
use pex::{Layout, Pex};
use platform::PosixPath;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use walkdir::WalkDir;
use wheel::{Record, WheelFile, WheelLayout};
use zip::read::ZipArchiveMetadata;
use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::original_wheel_info::{OriginalWheelInfo, ZipFileName};

pub mod original_wheel_info;

#[derive(Copy, Clone)]
enum DirPexDepType {
    Chroot,
    OriginalWhl,
    ZippedChroot,
}

pub struct WheelOptions {
    compression_method: CompressionMethod,
    compression_level: Option<i64>,
    timestamp: Option<DateTime<Utc>>,
}

impl WheelOptions {
    pub fn new(
        compression_method: CompressionMethod,
        compression_level: Option<i64>,
        timestamp: Option<DateTime<Utc>>,
    ) -> Self {
        Self {
            compression_method,
            compression_level,
            timestamp,
        }
    }

    pub fn file_options(&self) -> anyhow::Result<SimpleFileOptions> {
        self.add_timestamp(
            SimpleFileOptions::default()
                .compression_method(self.compression_method)
                .compression_level(self.compression_level),
        )
    }

    fn add_timestamp(&self, options: SimpleFileOptions) -> anyhow::Result<SimpleFileOptions> {
        Ok(if let Some(timestamp) = self.timestamp {
            options.last_modified_time(zip::DateTime::try_from(timestamp.naive_utc())?)
        } else {
            options
        })
    }
}

#[time("debug", "{}")]
pub fn repackage_wheels(
    pex: &Pex,
    options: &WheelOptions,
    dest_dir: &Path,
) -> anyhow::Result<Vec<File>> {
    let wheel_files = pex
        .info
        .parse_distributions()
        .collect::<Result<Vec<_>, _>>()?;
    match pex.layout {
        Layout::Loose | Layout::Packed => {
            let dep_type = if pex.info.raw().deps_are_wheel_files {
                DirPexDepType::OriginalWhl
            } else if matches!(pex.layout, Layout::Packed) {
                DirPexDepType::ZippedChroot
            } else {
                DirPexDepType::Chroot
            };
            wheel_files
                .into_par_iter()
                .map(|wheel_file: WheelFile| {
                    repackage_directory_pex_wheel(
                        pex.path,
                        &wheel_file,
                        dep_type,
                        options,
                        dest_dir,
                    )
                })
                .collect::<anyhow::Result<Vec<_>>>()
        }
        Layout::ZipApp => {
            let pex_zip = ZipArchive::new(File::open(pex.path)?)?;
            let zip_metadata = pex_zip.metadata();
            wheel_files
                .into_par_iter()
                .map(|wheel_file: WheelFile| {
                    repackage_zipapp_pex_wheel(
                        pex.path,
                        zip_metadata.clone(),
                        &wheel_file,
                        pex.info.raw().deps_are_wheel_files,
                        options,
                        dest_dir,
                    )
                })
                .collect::<anyhow::Result<Vec<_>>>()
        }
    }
}

fn repackage_zipapp_pex_wheel(
    pex_zip: &Path,
    zip_metadata: Arc<ZipArchiveMetadata>,
    wheel_file: &WheelFile,
    is_whl_zip: bool,
    options: &WheelOptions,
    dest_dir: &Path,
) -> anyhow::Result<File> {
    let mut pex_zip_fp =
        unsafe { ZipArchive::unsafe_new_with_metadata(File::open(pex_zip)?, zip_metadata.clone()) };
    if is_whl_zip {
        let wheel_prefix = format!(
            ".deps/{wheel_file_name}",
            wheel_file_name = wheel_file.file_name
        );
        recompress_zipped_whl(
            ZipArchive::new(pex_zip_fp.by_name_seek(&wheel_prefix)?)?,
            wheel_file,
            options,
            dest_dir,
        )
    } else {
        recompress_zipped_whl_chroot(
            pex_zip_fp,
            pex_zip.display(),
            wheel_file,
            options,
            dest_dir,
            true,
        )
    }
}

fn repackage_directory_pex_wheel(
    pex_dir: &Path,
    wheel_file: &WheelFile,
    dep_type: DirPexDepType,
    options: &WheelOptions,
    dest_dir: &Path,
) -> anyhow::Result<File> {
    let wheel_path = pex_dir.join(".deps").join(wheel_file.file_name);
    match dep_type {
        DirPexDepType::Chroot => compress_whl_chroot(&wheel_path, wheel_file, options, dest_dir),
        DirPexDepType::OriginalWhl => recompress_zipped_whl(
            ZipArchive::new(File::open(wheel_path)?)?,
            wheel_file,
            options,
            dest_dir,
        ),
        DirPexDepType::ZippedChroot => recompress_zipped_whl_chroot(
            ZipArchive::new(File::open(&wheel_path)?)?,
            wheel_path.display(),
            wheel_file,
            options,
            dest_dir,
            false,
        ),
    }
}

pub fn recompress_zipped_whl(
    mut wheel: ZipArchive<impl Read + Seek>,
    wheel_file: &WheelFile,
    options: &WheelOptions,
    dest_dir: &Path,
) -> anyhow::Result<File> {
    fs::create_dir_all(dest_dir)?;
    let dest_wheel = dest_dir.join(wheel_file.file_name);
    let compressed = File::create(&dest_wheel)?;
    let mut compressed_whl = ZipWriter::new(compressed);
    for index in 0..wheel.len() {
        let entry = wheel.by_index_raw(index)?;
        if entry.name().ends_with(".pyc") {
            continue;
        }
        if entry.compression() == options.compression_method && options.timestamp.is_none() {
            compressed_whl.raw_copy_file(entry)?;
        } else if entry.is_dir() {
            compressed_whl.add_directory(entry.name(), options.add_timestamp(entry.options())?)?;
        } else {
            drop(entry);
            let mut entry = wheel.by_index(index)?;
            // N.B.: entry.options is actually lossy (loses high bits); so we can't round-trip
            // more exotic permissions faithfully currently. An example of this is the cowsay 6.1
            // wheel on PyPi whose RECORD has 0o100664 which gets truncated to 0o644. Note also
            // though that `raw_copy_file` (used above when no transcoding is needed) _does_
            // preserve these bits.
            // See: https://github.com/zip-rs/zip2/issues/433
            compressed_whl.start_file(
                entry.name(),
                options.add_timestamp(
                    entry
                        .options()
                        .compression_method(options.compression_method)
                        .compression_level(options.compression_level),
                )?,
            )?;

            io::copy(&mut entry, &mut compressed_whl)?;
        }
    }
    compressed_whl.finish()?;
    Ok(File::open(dest_wheel)?)
}

fn recompress_zipped_whl_chroot(
    mut zipped_wheel_chroot: ZipArchive<impl Read + Seek>,
    zip_source: impl Display,
    wheel_file: &WheelFile,
    options: &WheelOptions,
    dest_dir: &Path,
    prefixed: bool,
) -> anyhow::Result<File> {
    fs::create_dir_all(dest_dir)?;
    let file_options = options.file_options()?;
    let wheel_prefix = if prefixed {
        Some(format!(
            ".deps/{wheel_file_name}/",
            wheel_file_name = wheel_file.file_name
        ))
    } else {
        None
    };
    let prefix = wheel_prefix.as_deref().unwrap_or_default();

    let metadata_dirs = wheel_file.metadata_dirs_from_zip(
        &zipped_wheel_chroot,
        zip_source,
        wheel_prefix.as_deref(),
    )?;
    let dist_info_dir = metadata_dirs.dist_info_dir();
    let record_name = format!("{prefix}{dist_info_dir}/RECORD");
    let record = Record::read(Cursor::new(io::read_to_string(
        zipped_wheel_chroot.by_name(&record_name)?,
    )?))?;

    let (stash_dir, legacy_bin_dir) = 'result: {
        let layout_json_name = if prefixed {
            Cow::Owned(format!(
                "{prefix}{file_name}",
                file_name = WheelLayout::file_name()
            ))
        } else {
            Cow::Borrowed(WheelLayout::file_name())
        };
        match zipped_wheel_chroot.by_name(layout_json_name.as_ref()) {
            Ok(zip_file) => {
                let layout = WheelLayout::read(zip_file)?;
                break 'result (Some(layout.stash_dir), false);
            }
            Err(ZipError::FileNotFound) => {}
            Err(err) => bail!("{err}"),
        }
        let legacy_bin_dir_name = if prefixed {
            Cow::Owned(format!("{prefix}bin"))
        } else {
            Cow::Borrowed("bin")
        };
        let has_legacy_bin_dir = !record.wheel_has_bin_dir()
            && zipped_wheel_chroot
                .by_name(legacy_bin_dir_name.as_ref())
                .ok()
                .map(|entry| entry.is_dir())
                .unwrap_or_default();
        (None, has_legacy_bin_dir)
    };

    let original_wheel_info = format!(
        "{prefix}{pex_info_dir}/{file_name}",
        pex_info_dir = metadata_dirs.pex_info_dir(),
        file_name = OriginalWheelInfo::file_name()
    );

    let wheel_info = if let Ok(wheel_info) = zipped_wheel_chroot.by_name(&original_wheel_info) {
        let size = wheel_info.size();
        Some(OriginalWheelInfo::read(wheel_info, size)?)
    } else {
        None
    };

    let data_dir = metadata_dirs.data_dir().as_path();
    let mut zip_finder = ZipPathFinder {
        zip: zipped_wheel_chroot,
        prefix: wheel_prefix.as_deref(),
    };
    let (dest_wheel, compressed_whl) = if let Some(wheel_info) = wheel_info {
        let dest_wheel = dest_dir.join(wheel_info.filename());
        let mut compressed_whl = ZipWriter::new(File::create(&dest_wheel)?);
        for (zip_file_name, options) in
            wheel_info.iter_file_options(file_options, options.timestamp)?
        {
            if zip_file_name.ends_with(".pyc") {
                continue;
            }
            let name = 'result: {
                if let Ok(data_dir_rel_path) = zip_file_name.as_path().strip_prefix(&data_dir) {
                    if let Some(stash_dir) = stash_dir.as_deref() {
                        break 'result format!(
                            "{prefix}{stash_dir}/{rel_path}",
                            stash_dir = stash_dir.display(),
                            rel_path = PosixPath::relpath(
                                normalized_data_dir_relpath(
                                    stash_dir,
                                    data_dir_rel_path,
                                    wheel_file,
                                    &zip_finder
                                )?
                                .as_ref()
                            )?
                        );
                    }
                    if legacy_bin_dir {
                        let rel_path = normalized_data_dir_relpath(
                            Path::new("bin"),
                            data_dir_rel_path,
                            wheel_file,
                            &zip_finder,
                        )?;
                        assert!(starts_with(rel_path.as_ref(), "bin"));
                        break 'result format!(
                            "{prefix}{rel_path}",
                            rel_path = PosixPath::relpath(rel_path.as_ref())?
                        );
                    }
                }
                format!("{prefix}{zip_file_name}")
            };
            let mut src = match zip_finder.by_name(&name) {
                Ok(src) => src,
                Err(_) if zip_file_name.ends_with("/") => {
                    // N.B.: Pex can omit original directory entries when those directories are
                    // empty.
                    compressed_whl.add_directory(zip_file_name.to_string(), options)?;
                    continue;
                }
                Err(err) => bail!(
                    "Mapped {zip_file_name} in {file_name} to {name} which was not found: {err}",
                    file_name = wheel_file.file_name
                ),
            };
            if src.is_dir() {
                compressed_whl.add_directory(zip_file_name.to_string(), options)?;
            } else {
                compressed_whl.start_file(zip_file_name, options)?;
                if src.name() == record_name {
                    compressed_whl.write_all(
                        record
                            .filtered(
                                &metadata_dirs,
                                stash_dir.as_deref(),
                                if legacy_bin_dir {
                                    Some(Path::new("bin"))
                                } else {
                                    None
                                },
                            )?
                            .as_slice(),
                    )?;
                } else {
                    io::copy(&mut src, &mut compressed_whl)?;
                }
            }
        }
        (dest_wheel, compressed_whl)
    } else {
        let dest_wheel = dest_dir.join(wheel_file.file_name);
        let mut compressed_whl = ZipWriter::new(File::create(&dest_wheel)?);
        for entry in record.entries() {
            let dst_rel_path = entry.path.as_ref();
            let name = 'result: {
                if let Ok(data_dir_rel_path) = dst_rel_path.strip_prefix(&data_dir) {
                    if let Some(stash_dir) = stash_dir.as_deref() {
                        break 'result format!(
                            "{prefix}{stash_dir}/{rel_path}",
                            stash_dir = stash_dir.display(),
                            rel_path = PosixPath::relpath(
                                normalized_data_dir_relpath(
                                    stash_dir,
                                    data_dir_rel_path,
                                    wheel_file,
                                    &zip_finder
                                )?
                                .as_ref()
                            )?
                        );
                    }
                    if legacy_bin_dir {
                        let rel_path = normalized_data_dir_relpath(
                            Path::new("bin"),
                            data_dir_rel_path,
                            wheel_file,
                            &zip_finder,
                        )?;
                        assert!(starts_with(rel_path.as_ref(), "bin"));
                        break 'result format!(
                            "{prefix}{rel_path}",
                            rel_path = PosixPath::relpath(rel_path.as_ref())?
                        );
                    }
                }
                format!(
                    "{prefix}{rel_path}",
                    rel_path = PosixPath::relpath(dst_rel_path)?
                )
            };
            let mut src = zip_finder.by_name(&name)?;
            compressed_whl.start_file_from_path(dst_rel_path, file_options)?;
            io::copy(&mut src, &mut compressed_whl)?;
        }
        (dest_wheel, compressed_whl)
    };

    compressed_whl.finish()?;
    Ok(File::open(dest_wheel)?)
}

fn compress_whl_chroot(
    wheel_dir: &Path,
    wheel_file: &WheelFile,
    options: &WheelOptions,
    dest_dir: &Path,
) -> anyhow::Result<File> {
    fs::create_dir_all(dest_dir)?;
    let file_options = options.file_options()?;

    let metadata_dirs = wheel_file.metadata_dirs(wheel_dir)?;
    let (record, record_rel_path) = Record::parse(wheel_dir, &metadata_dirs)?;

    let (stash_dir, legacy_bin_dir) = 'result: {
        if let Some(layout) = WheelLayout::load_from_dir(wheel_dir)? {
            let stash_dir = wheel_dir.join(layout.stash_dir);
            if stash_dir.exists() {
                break 'result (Some(stash_dir), None);
            }
        }
        let bin_dir = wheel_dir.join("bin");
        if bin_dir.is_dir() && !record.wheel_has_bin_dir() {
            break 'result (None, Some(bin_dir));
        }
        (None, None)
    };

    let data_dir = metadata_dirs.data_dir().as_path();
    let pex_info_dir = wheel_dir.join(metadata_dirs.pex_info_dir().to_string());
    let (dest_wheel, compressed_whl) = if let Some(wheel_info) =
        OriginalWheelInfo::load_from_dir(pex_info_dir)?
    {
        let dest_wheel = dest_dir.join(wheel_info.filename());
        let mut compressed_whl = ZipWriter::new(File::create(&dest_wheel)?);
        for (zip_file_name, options) in
            wheel_info.iter_file_options(file_options, options.timestamp)?
        {
            if zip_file_name.ends_with(".pyc") {
                continue;
            }
            let dst_rel_path = zip_file_name.as_path();
            let dst_rel_path = dst_rel_path.as_ref();
            let mut src = wheel_dir.join(dst_rel_path);
            if let Ok(data_dir_rel_path) = dst_rel_path.strip_prefix(&data_dir) {
                if let Some(stash_dir) = stash_dir.as_deref() {
                    src = stash_dir.join(normalized_data_dir_relpath(
                        stash_dir,
                        data_dir_rel_path,
                        wheel_file,
                        &LoosePathFinder,
                    )?)
                } else if let Some(bin_dir) = legacy_bin_dir.as_deref() {
                    let rel_path = normalized_data_dir_relpath(
                        bin_dir,
                        data_dir_rel_path,
                        wheel_file,
                        &LoosePathFinder,
                    )?;
                    assert!(starts_with(rel_path.as_ref(), "bin"));
                    src = bin_dir.join(rel_path)
                }
            }
            if src.is_dir() {
                compressed_whl.add_directory_from_path(dst_rel_path, options)?;
            } else {
                compressed_whl.start_file_from_path(dst_rel_path, options)?;
                if dst_rel_path == record_rel_path {
                    compressed_whl.write_all(
                        record
                            .filtered(
                                &metadata_dirs,
                                stash_dir.as_deref().map(|dir| {
                                    dir.strip_prefix(wheel_dir)
                                        .expect("We appended the stash dir to the wheel dir above.")
                                }),
                                legacy_bin_dir.as_deref().map(|dir| {
                                    dir.strip_prefix(wheel_dir).expect(
                                        "We appended the legacy bin dir to the wheel dir above.",
                                    )
                                }),
                            )?
                            .as_slice(),
                    )?;
                } else {
                    io::copy(&mut File::open(src)?, &mut compressed_whl)?;
                }
            }
        }
        (dest_wheel, compressed_whl)
    } else {
        let dest_wheel = dest_dir.join(wheel_file.file_name);
        let mut compressed_whl = ZipWriter::new(File::create(&dest_wheel)?);
        for entry in record.entries() {
            let dst_rel_path = entry.path.as_ref();
            let mut src = wheel_dir.join(dst_rel_path);
            if let Ok(data_dir_rel_path) = dst_rel_path.strip_prefix(&data_dir) {
                if let Some(stash_dir) = stash_dir.as_deref() {
                    src = stash_dir.join(normalized_data_dir_relpath(
                        stash_dir,
                        data_dir_rel_path,
                        wheel_file,
                        &LoosePathFinder,
                    )?)
                } else if let Some(bin_dir) = legacy_bin_dir.as_deref() {
                    let rel_path = normalized_data_dir_relpath(
                        bin_dir,
                        data_dir_rel_path,
                        wheel_file,
                        &LoosePathFinder,
                    )?;
                    assert!(starts_with(rel_path.as_ref(), "bin"));
                    src = bin_dir.join(rel_path)
                }
            }
            compressed_whl.start_file_from_path(dst_rel_path, file_options)?;
            io::copy(&mut File::open(src)?, &mut compressed_whl)?;
        }
        (dest_wheel, compressed_whl)
    };

    compressed_whl.finish()?;
    Ok(File::open(dest_wheel)?)
}

fn starts_with(path: &Path, name: impl AsRef<OsStr>) -> bool {
    matches!(path.components().next(), Some(Component::Normal(named)) if named == name.as_ref())
}

trait ProjectPathFinder<'a> {
    fn find(
        &'a self,
        strip_prefix: &Path,
        prefix: PathBuf,
        project: &WheelFile,
        suffix: PathBuf,
    ) -> anyhow::Result<Cow<'a, Path>>;
}

struct LoosePathFinder;

impl<'a> ProjectPathFinder<'a> for LoosePathFinder {
    fn find(
        &'a self,
        strip_prefix: &Path,
        prefix: PathBuf,
        project: &WheelFile,
        suffix: PathBuf,
    ) -> anyhow::Result<Cow<'a, Path>> {
        for entry in WalkDir::new(strip_prefix.join(&prefix)).min_depth(1) {
            let entry = entry?;
            let prefix_rel_path = entry
                .path()
                .strip_prefix(strip_prefix)
                .expect("We walked the prefix; so we can always safely strip it.");
            if prefix_rel_path.ends_with(&suffix) {
                for component in prefix_rel_path.components() {
                    if let Some(name) = component.as_os_str().to_str()
                        && (name == project.raw_project_name
                            || name == project.project_name.as_ref())
                    {
                        return Ok(Cow::Owned(prefix_rel_path.to_owned()));
                    }
                }
            }
        }
        bail!(
            "Failed to find path in wheel {wheel} rooted at {root} with prefix {prefix} and \
            suffix {suffix}",
            wheel = project.file_name,
            root = strip_prefix.display(),
            prefix = prefix.display(),
            suffix = suffix.display()
        )
    }
}

struct ZipPathFinder<'a, R: Read + Seek> {
    zip: ZipArchive<R>,
    prefix: Option<&'a str>,
}

impl<'a, R: Read + Seek> Deref for ZipPathFinder<'a, R> {
    type Target = ZipArchive<R>;

    fn deref(&self) -> &<Self as Deref>::Target {
        &self.zip
    }
}

impl<'a, R: Read + Seek> DerefMut for ZipPathFinder<'a, R> {
    fn deref_mut(&mut self) -> &mut <Self as Deref>::Target {
        &mut self.zip
    }
}

impl<'a, R: Read + Seek> ProjectPathFinder<'a> for ZipPathFinder<'a, R> {
    fn find(
        &'a self,
        strip_prefix: &Path,
        prefix: PathBuf,
        project: &WheelFile,
        suffix: PathBuf,
    ) -> anyhow::Result<Cow<'a, Path>> {
        let (strip_prefix, prefix) = if let Some(zip_prefix) = self.prefix {
            let strip_prefix = Path::new(zip_prefix).join(strip_prefix);
            let zip_file_name_path = strip_prefix.join(prefix);
            (
                Cow::Owned(strip_prefix),
                ZipFileName::from(zip_file_name_path)?,
            )
        } else {
            (
                Cow::Borrowed(strip_prefix),
                ZipFileName::from(strip_prefix.join(prefix))?,
            )
        };
        let suffix = ZipFileName::from(suffix)?;
        for file_name in self.zip.file_names() {
            if let Some(rel_path) = file_name.strip_prefix(prefix.as_str())
                && let Some(rel_path) = rel_path.strip_suffix(suffix.as_str())
            {
                for component in rel_path.split("/") {
                    if component == project.raw_project_name
                        || component == project.project_name.as_ref()
                    {
                        return Ok(Cow::Borrowed(
                            Path::new(file_name).strip_prefix(strip_prefix)?,
                        ));
                    }
                }
            }
        }
        bail!(
            "Failed to find path in wheel {wheel} zip with prefix {prefix} and suffix {suffix}",
            wheel = project.file_name,
        )
    }
}

fn normalized_data_dir_relpath<'a>(
    prefix: &Path,
    path: &'a Path,
    wheel_file: &WheelFile,
    project_path_finder: &'a impl ProjectPathFinder<'a>,
) -> anyhow::Result<Cow<'a, Path>> {
    let mut components = path.components();
    let start = components.next();
    if let Some(start) = start
        && matches!(start, Component::Normal(name) if name == "scripts")
    {
        Ok(Cow::Owned(
            [Component::Normal(OsStr::new("bin"))]
                .into_iter()
                .chain(components)
                .collect(),
        ))
    } else if let Some(start) = start
        && matches!(start, Component::Normal(name) if name == "headers")
    {
        // N.B.: You'd think sysconfig_paths["include"] would be the right answer here but both
        // `pip`, and by emulation, `uv pip`, map `*.data/headers` to
        // `<venv>/include/site/pythonX.Y/<project name>`. Traditional PEXes honors this; so we
        // need to as well.
        //
        // The "mess" is admitted and described at length here:
        // + https://discuss.python.org/t/clarification-on-a-wheels-header-data/9305
        // + https://discuss.python.org/t/deprecating-the-headers-wheel-data-key/23712
        Ok(project_path_finder.find(
            prefix,
            Path::new("include").join("site"),
            wheel_file,
            components.collect(),
        )?)
    } else {
        Ok(Cow::Borrowed(path))
    }
}
