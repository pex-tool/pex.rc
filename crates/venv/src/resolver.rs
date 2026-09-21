// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::ffi::OsStr;
use std::fmt::{Display, Formatter, Write as _};
use std::hash::{Hash, Hasher};
use std::io;
use std::io::{Seek, Write};
use std::path::{Component, Path, PathBuf};

use anyhow::{anyhow, bail};
use cache::DigestingReader;
use digest::Digest;
use fs_err as fs;
use fs_err::File;
use platform::PosixPath;
use python_platform::PythonVersion;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use repackage::WheelOptions;
use repackage::original_wheel_info::{OriginalWheelInfo, ZipFileName};
use sha2::Sha256;
use tracing::instrument;
use wheel::{EntryPoints, MetadataDirs, Record, Tag, WHEEL, WheelDir, record};
use zip::ZipWriter;
use zip::write::SimpleFileOptions;

use crate::Virtualenv;
use crate::script::PythonScript;

pub struct InstallPaths<'a> {
    python_version: PythonVersion,
    data: Cow<'a, Path>,
    headers_base: Cow<'a, Path>,
    platlib: Cow<'a, Path>,
    purelib: Cow<'a, Path>,
    scripts: Cow<'a, Path>,
}

impl<'a> InstallPaths<'a> {
    pub fn for_venv(venv: &'a Virtualenv<'a>) -> anyhow::Result<Self> {
        let get_sysconfig_path = |name| {
            venv.interpreter
                .details
                .paths
                .get(name)
                .map(PathBuf::as_path)
                .map(Cow::Borrowed)
                .ok_or_else(|| {
                    anyhow!(
                        "The venv at {venv} unexpectedly has no sysconfig path for '{name}'",
                        venv = venv.prefix().display()
                    )
                })
        };

        Ok(Self {
            python_version: venv.interpreter.details.version,
            data: get_sysconfig_path("data")?,
            headers_base: Cow::Owned(venv.prefix().join("include").join("site").join(format!(
                "python{major}.{minor}",
                major = venv.interpreter.details.version.major,
                minor = venv.interpreter.details.version.minor
            ))),
            platlib: get_sysconfig_path("platlib")?,
            purelib: get_sysconfig_path("purelib")?,
            scripts: get_sysconfig_path("scripts")?,
        })
    }

    fn headers(&self, project_name: &str) -> PathBuf {
        self.headers_base.join(project_name)
    }
}

struct WheelEntry<'a> {
    installed_path: Cow<'a, Path>,
    zip_path: String,
    script: Option<(File, PythonScript)>,
}

impl<'a> WheelEntry<'a> {
    fn pack(
        &mut self,
        whl_zip: &mut ZipWriter<impl Write + Seek>,
        file_options: SimpleFileOptions,
        record_writer: &mut record::Writer<impl Write>,
    ) -> anyhow::Result<()> {
        let zip_path = self.zip_path.to_string();
        whl_zip.start_file(&zip_path, file_options)?;
        let (fingerprint, size) = if let Some((file, python_script)) = self.script.as_mut() {
            let mut contents = Vec::with_capacity(usize::try_from(file.metadata()?.len())?);
            python_script.re_write(file, &mut contents)?;
            let mut src = DigestingReader::new(Sha256::new(), contents.as_slice());
            io::copy(&mut src, whl_zip)?;
            src.into_fingerprint_and_size()
        } else {
            let mut src =
                DigestingReader::new(Sha256::new(), File::open(self.installed_path.as_ref())?);
            io::copy(&mut src, whl_zip)?;
            src.into_fingerprint_and_size()
        };
        record_writer.write_entry(zip_path, Some(("sha256", &fingerprint, size)))
    }
}

pub struct InstalledWheel {
    project_name: String,
    version: String,
    tags: Vec<String>,
    build: Option<String>,
    record: Record,
    metadata_dirs: MetadataDirs,
    root_is_purelib: bool,
    entry_points: EntryPoints,
}

impl PartialEq<Self> for InstalledWheel {
    fn eq(&self, other: &Self) -> bool {
        self.project_name == other.project_name
            && self.version == other.version
            && self.tags == other.tags
            && self.build == other.build
    }
}

impl Eq for InstalledWheel {}

impl Hash for InstalledWheel {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.project_name.hash(state);
        self.version.hash(state);
        self.tags.hash(state);
        self.build.hash(state);
    }
}

impl InstalledWheel {
    fn load(dist_info_dir: PathBuf) -> anyhow::Result<Self> {
        let metadata_dirs = MetadataDirs::from_dist_info_dir(&dist_info_dir)?;
        let installed_wheel_dir = dist_info_dir.parent().ok_or_else(|| {
            anyhow!(
                "Invalid *.dist-info/ dir; expected a parent directory: {path}",
                path = dist_info_dir.display()
            )
        })?;
        let (record, _) = Record::parse(installed_wheel_dir, &metadata_dirs)?;
        let wheel = WHEEL::parse(fs::read(dist_info_dir.join("WHEEL"))?.as_slice())?;
        let entry_points = {
            let entry_points_txt = dist_info_dir.join("entry_points.txt");
            if entry_points_txt.exists() {
                EntryPoints::load(File::open(entry_points_txt)?)?
            } else {
                EntryPoints::empty()
            }
        };
        Ok(Self {
            project_name: metadata_dirs.borrow_project_name().to_string(),
            version: metadata_dirs.borrow_version().to_string(),
            tags: wheel.tags,
            build: wheel.build,
            record,
            metadata_dirs,
            root_is_purelib: wheel.root_is_purelib,
            entry_points,
        })
    }

    pub fn file_name(&self) -> anyhow::Result<String> {
        let mut file_name = String::with_capacity(
            self.project_name.len() + 1 + self.version.len() + self.tags.len() - 1
                + self.tags.iter().map(&String::len).sum::<usize>()
                + 4,
        );
        write!(
            &mut file_name,
            "{project_name}-{version}-",
            project_name = self.project_name,
            version = self.version
        )?;
        if let Some(build) = self.build.as_deref() {
            write!(&mut file_name, "{build}-")?;
        }

        let mut pythons = BTreeSet::new();
        let mut abis = BTreeSet::new();
        let mut platforms = BTreeSet::new();
        for tag in &self.tags {
            let tag = Tag::parse(tag)?;
            pythons.insert(tag.python);
            abis.insert(tag.abi);
            platforms.insert(tag.platform);
        }
        for (index, python) in pythons.into_iter().enumerate() {
            if index > 0 {
                file_name.push('.')
            }
            write!(&mut file_name, "{python}")?;
        }
        for (index, abi) in abis.into_iter().enumerate() {
            if index == 0 {
                file_name.push('-')
            } else {
                file_name.push('.')
            }
            write!(&mut file_name, "{abi}")?;
        }
        for (index, platform) in platforms.into_iter().enumerate() {
            if index == 0 {
                file_name.push('-')
            } else {
                file_name.push('.')
            }
            write!(&mut file_name, "{platform}")?;
        }

        write!(&mut file_name, ".whl")?;
        Ok(file_name)
    }

    // https://docs.python.org/2.7/library/sysconfig.html#sysconfig.get_path
    // Each scheme is itself composed of a series of paths and each path has a unique identifier. Python currently uses eight paths:
    //
    //     stdlib: directory containing the standard Python library files that are not platform-specific.
    //     platstdlib: directory containing the standard Python library files that are platform-specific.
    //
    //     platlib: directory for site-specific, platform-specific files.
    //     purelib: directory for site-specific, non-platform-specific files.
    //
    //     include: directory for non-platform-specific header files.
    //     platinclude: directory for platform-specific header files.
    //
    //     scripts: directory for script files.
    //     data: directory for data files.
    //
    // {distribution}-{version}.data/ contains one subdirectory for each non-empty install scheme
    // key not already covered, where the subdirectory name is an index into a dictionary of install
    // paths (e.g. data, scripts, headers, purelib, platlib).
    //
    // Of these 5 headers DNE. You'd think sysconfig_paths["include"] (or "platinclude") would be
    // the right answer here but both `pip`, and by emulation, `uv pip`, map `*.data/headers` to
    // `<venv>/include/site/pythonX.Y/<project name>`. Traditional PEXes honors this; so we need to
    // as well.
    //
    // The "mess" is admitted and described at length here:
    // + https://discuss.python.org/t/clarification-on-a-wheels-header-data/9305
    // + https://discuss.python.org/t/deprecating-the-headers-wheel-data-key/23712

    const COMPILED_PYTHON_EXTENSIONS: &[&str] = &["pyc", "pyo", "pyd"];
    const IGNORED_METADATA_FILES: &[&str] = &["RECORD", "INSTALLER", "REQUESTED"];

    pub fn pack(
        &self,
        install_paths: &InstallPaths,
        wheel_options: &WheelOptions,
        dest: impl Write,
    ) -> anyhow::Result<()> {
        let mut whl_zip = ZipWriter::new_stream(dest);
        let file_options = wheel_options.file_options()?;
        let site_packages_dir = if self.root_is_purelib {
            &install_paths.purelib
        } else {
            &install_paths.platlib
        };
        let mut record_writer = Record::writer(Vec::new());

        if let Some(wheel_info) = OriginalWheelInfo::load_from_dir(
            site_packages_dir.join(self.metadata_dirs.pex_info_dir().as_path()),
        )? {
            let data_dir = self.metadata_dirs.data_dir();
            for (zip_file_name, file_options) in
                wheel_info.iter_file_options(file_options, wheel_options.timestamp)?
            {
                if zip_file_name.is_dir() {
                    whl_zip.add_directory(zip_file_name.to_string(), file_options)?;
                } else if !self.skip(zip_file_name.as_path())
                    && let Some(mut wheel_entry) = self.zip_file_name_to_wheel_entry(
                        install_paths,
                        site_packages_dir.as_ref(),
                        &data_dir,
                        zip_file_name,
                    )?
                {
                    wheel_entry.pack(&mut whl_zip, file_options, &mut record_writer)?;
                }
            }
        } else {
            for entry in self.record.entries() {
                if self.skip(&entry.path) {
                    continue;
                }
                if let Some(mut wheel_entry) = self.record_entry_to_wheel_entry(
                    install_paths,
                    site_packages_dir.as_ref(),
                    entry,
                )? {
                    wheel_entry.pack(&mut whl_zip, file_options, &mut record_writer)?;
                }
            }
        }

        let record_path = format!(
            "{dist_info_dir}/RECORD",
            dist_info_dir = self.metadata_dirs.dist_info_dir()
        );
        record_writer.write_entry(&record_path, None)?;
        let record = record_writer.into_inner()?;
        whl_zip.start_file(record_path, file_options)?;
        io::copy(&mut record.as_slice(), &mut whl_zip)?;

        whl_zip.finish()?;
        Ok(())
    }

    fn skip(&self, path: impl AsRef<Path>) -> bool {
        let entry_path = path.as_ref();
        if self.metadata_dirs.dist_info_dir().contains(entry_path)
            && entry_path.components().count() == 2
            && let Some(file_name) = entry_path.file_name().and_then(OsStr::to_str)
            && Self::IGNORED_METADATA_FILES.contains(&file_name)
        {
            return true;
        }
        if let Some(ext) = entry_path.extension().and_then(OsStr::to_str)
            && Self::COMPILED_PYTHON_EXTENSIONS.contains(&ext)
        {
            return true;
        }
        false
    }

    fn zip_file_name_to_wheel_entry<'a>(
        &self,
        install_paths: &InstallPaths,
        site_packages_dir: &Path,
        data_dir: &WheelDir,
        zip_file_name: &'a ZipFileName,
    ) -> anyhow::Result<Option<WheelEntry<'a>>> {
        if let Some(mut components) = data_dir.strip_prefix(zip_file_name.as_ref())
            && let Some(Component::Normal(key)) = components.next()
            && let Some(key) = key.to_str()
        {
            let mut needs_script_rewrite: Option<(File, PythonScript)> = None;
            let installed_path = match key {
                "scripts" => {
                    let script_path = install_paths.scripts.join(components);
                    let mut file = File::open(&script_path)?;
                    let size = file.metadata()?.len();
                    needs_script_rewrite =
                        PythonScript::detect(&mut file, size, install_paths.python_version)?
                            .map(|python_script| (file, python_script));
                    script_path
                }
                "headers" => install_paths.headers(&self.project_name).join(components),
                "platlib" => install_paths.platlib.join(components),
                "purelib" => install_paths.purelib.join(components),
                "data" => install_paths.data.join(components),
                _ => bail!("Unexpected *.data/ dir path: {zip_file_name}"),
            };
            Ok(Some(WheelEntry {
                installed_path: Cow::Owned(installed_path),
                zip_path: zip_file_name.to_string(),
                script: needs_script_rewrite,
            }))
        } else {
            Ok(Some(WheelEntry {
                installed_path: Cow::Owned(site_packages_dir.join(zip_file_name.as_path())),
                zip_path: zip_file_name.to_string(),
                script: None,
            }))
        }
    }

    fn record_entry_to_wheel_entry<'a>(
        &self,
        install_paths: &InstallPaths,
        site_packages_dir: &Path,
        entry: &'a record::Entry,
    ) -> anyhow::Result<Option<WheelEntry<'a>>> {
        let mut needs_script_rewrite: Option<(File, PythonScript)> = None;
        let installed_path = if entry.path.is_relative() {
            Cow::Owned(site_packages_dir.join(&entry.path).normalize_lexically()?)
        } else {
            entry.path.clone()
        };
        let zip_path = if entry.path.is_relative()
            && entry
                .path
                .components()
                .all(|component| matches!(component, Component::CurDir | Component::Normal(_)))
        {
            PosixPath::new(Cow::Borrowed(&entry.path), false)?
        } else {
            let data_dir = self.metadata_dirs.data_dir().as_path();
            let data_dir_rel_path = if let Ok(scripts_rel_path) =
                installed_path.strip_prefix(&install_paths.scripts)
            {
                let has_parent = scripts_rel_path
                    .parent()
                    .map(|parent| !parent.is_empty())
                    .unwrap_or_default();
                if !has_parent
                    && let Some(script_name) = scripts_rel_path.file_stem().and_then(OsStr::to_str)
                    && self.entry_points.is_script(script_name)
                {
                    return Ok(None);
                }
                let mut file = File::open(installed_path.as_ref())?;
                let size = file.metadata()?.len();
                needs_script_rewrite =
                    PythonScript::detect(&mut file, size, install_paths.python_version)?
                        .map(|python_script| (file, python_script));
                data_dir.join("scripts").join(scripts_rel_path)
            } else if let Ok(headers_rel_path) =
                installed_path.strip_prefix(install_paths.headers(&self.project_name))
            {
                data_dir.join("headers").join(headers_rel_path)
            } else if let Ok(platlib_rel_path) = installed_path.strip_prefix(&install_paths.platlib)
            {
                data_dir.join("platlib").join(platlib_rel_path)
            } else if let Ok(purelib_rel_path) = installed_path.strip_prefix(&install_paths.purelib)
            {
                data_dir.join("purelib").join(purelib_rel_path)
            } else if let Ok(data_rel_path) = installed_path.strip_prefix(&install_paths.data) {
                data_dir.join("data").join(data_rel_path)
            } else {
                bail!(
                    "Unexpected RECORD path: {path} ({abs_path})",
                    path = entry.path.display(),
                    abs_path = installed_path.display()
                )
            };
            PosixPath::new(Cow::Owned(data_dir_rel_path), false)?
        };
        Ok(Some(WheelEntry {
            installed_path,
            zip_path: zip_path.to_string(),
            script: needs_script_rewrite,
        }))
    }
}

impl Display for InstalledWheel {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{project_name}-{version}",
            project_name = self.project_name,
            version = self.version
        )
    }
}

#[instrument(level = "debug", skip_all)]
pub fn collect_installed_wheels(venv: &Virtualenv) -> anyhow::Result<Vec<InstalledWheel>> {
    let mut entries = Vec::with_capacity(1024);
    for entry in venv.prefix().join(&venv.site_packages_relpath).read_dir()? {
        if let Ok(entry) = entry
            && let Ok(file_type) = entry.file_type()
            && file_type.is_dir()
            && entry
                .file_name()
                .as_encoded_bytes()
                .ends_with(b".dist-info")
        {
            entries.push(entry.path());
        }
    }
    entries
        .into_par_iter()
        .map(InstalledWheel::load)
        .collect::<anyhow::Result<Vec<_>>>()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use fs_err::File;
    use python_platform::PythonVersion;
    use rstest::rstest;
    use testing::tmp_dir;

    use crate::script::{PythonScript, Shebang};

    fn maybe_detect_python_script(
        chroot: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> Option<(File, PythonScript)> {
        let script_path = chroot.as_ref().join("script");
        fs::write(&script_path, contents).unwrap();
        let mut file = File::open(&script_path).unwrap();
        let size = file.metadata().unwrap().len();
        let python_script =
            PythonScript::detect(&mut file, size, PythonVersion::new(3, 14, None)).unwrap();
        python_script.map(|python_script| (file, python_script))
    }

    fn assert_python_script(
        chroot: impl AsRef<Path>,
        contents: impl AsRef<[u8]>,
    ) -> (File, PythonScript) {
        maybe_detect_python_script(chroot, contents).unwrap()
    }

    fn assert_not_python_script(chroot: impl AsRef<Path>, contents: impl AsRef<[u8]>) {
        assert!(maybe_detect_python_script(chroot, contents).is_none())
    }

    #[rstest]
    fn test_detect_posix_script_placeholder_shebang(tmp_dir: PathBuf) {
        let (mut file, python_script) = assert_python_script(&tmp_dir, "#!python");
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 8,
                    extra_content: None
                })
            },
            python_script,
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!("#!python", String::from_utf8(re_written).unwrap());

        let (mut file, python_script) = assert_python_script(&tmp_dir, "#!python\n");
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 8,
                    extra_content: None
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!("#!python\n", String::from_utf8(re_written).unwrap());

        let (mut file, python_script) = assert_python_script(&tmp_dir, "#!pythonw");
        assert_eq!(
            PythonScript {
                is_windowed: true,
                shebang: Some(Shebang {
                    end: 9,
                    extra_content: None
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!("#!pythonw", String::from_utf8(re_written).unwrap());

        let (mut file, python_script) = assert_python_script(tmp_dir, "#!pythonw\n");
        assert_eq!(
            PythonScript {
                is_windowed: true,
                shebang: Some(Shebang {
                    end: 9,
                    extra_content: None
                })
            },
            python_script,
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!("#!pythonw\n", String::from_utf8(re_written).unwrap());
    }

    #[rstest]
    fn test_detect_posix_script_shebang(tmp_dir: PathBuf) {
        let (mut file, python_script) = assert_python_script(&tmp_dir, "#!/usr/bin/python\npass");
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 17,
                    extra_content: None
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python\n\
            pass",
            String::from_utf8(re_written).unwrap()
        );

        let (mut file, python_script) = assert_python_script(&tmp_dir, "#!/usr/bin/python3\npass");
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 18,
                    extra_content: None
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python\n\
            pass",
            String::from_utf8(re_written).unwrap()
        );

        let (mut file, python_script) =
            assert_python_script(&tmp_dir, "#!/usr/bin/python3.14\npass");
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 21,
                    extra_content: None
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python\n\
            pass",
            String::from_utf8(re_written).unwrap()
        );

        assert_not_python_script(&tmp_dir, "#!/usr/bin/perl\npass");
    }

    #[rstest]
    fn test_detect_posix_script_shebang_with_args(tmp_dir: PathBuf) {
        let (mut file, python_script) =
            assert_python_script(&tmp_dir, "#!/usr/bin/python -I\npass");
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 17,
                    extra_content: None
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python -I\n\
            pass",
            String::from_utf8(re_written).unwrap()
        );

        let (mut file, python_script) =
            assert_python_script(&tmp_dir, "#!/usr/bin/python3 -I\npass");
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 18,
                    extra_content: None
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python -I\n\
            pass",
            String::from_utf8(re_written).unwrap()
        );

        let (mut file, python_script) =
            assert_python_script(&tmp_dir, "#!/usr/bin/python3.14 -I\npass");
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 21,
                    extra_content: None
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python -I\n\
            pass",
            String::from_utf8(re_written).unwrap()
        );

        assert_not_python_script(&tmp_dir, "#!/usr/bin/perl -n\npass");
    }

    #[rstest]
    fn test_detect_bin_sh_redirector_script_shebang(tmp_dir: PathBuf) {
        let (mut file, python_script) = assert_python_script(
            &tmp_dir,
            "#!/bin/sh\n\
            '''exec' /the/venv/bin/python \"$0\" \"$@\"\n\
            '''\n\
            pass",
        );
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 9,
                    extra_content: Some(9..53)
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python\n\
            pass",
            String::from_utf8(re_written).unwrap()
        );

        let (mut file, python_script) = assert_python_script(
            &tmp_dir,
            "#!/bin/sh\n\
            # coding=utf-8\n\
            '''exec' /the/venv/bin/python \"$0\" \"$@\"\n\
            '''\n\
            pass",
        );
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 9,
                    extra_content: Some(24..68)
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python\n\
            # coding=utf-8\n\
            pass",
            String::from_utf8(re_written).unwrap()
        );

        let (mut file, python_script) = assert_python_script(
            &tmp_dir,
            "#!/bin/sh\n\
            '''': pshprs\n\
            /the/venv/bin/python \"$0\" \"$@\"\n\
            '''\n\
            pass",
        );
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 9,
                    extra_content: Some(9..57)
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python\n\
            pass",
            String::from_utf8(re_written).unwrap()
        );

        let (mut file, python_script) = assert_python_script(
            &tmp_dir,
            "#!/bin/sh\n\
            # -*- coding: windows-1252 -*-\n\
            '''': pshprs\n\
            /the/venv/bin/python \"$0\" \"$@\"\n\
            '''\n\
            pass",
        );
        assert_eq!(
            PythonScript {
                is_windowed: false,
                shebang: Some(Shebang {
                    end: 9,
                    extra_content: Some(40..88)
                })
            },
            python_script
        );
        let mut re_written = Vec::new();
        python_script.re_write(&mut file, &mut re_written).unwrap();
        assert_eq!(
            "#!python\n\
            # -*- coding: windows-1252 -*-\n\
            pass",
            String::from_utf8(re_written).unwrap()
        )
    }
}
