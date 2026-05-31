// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::str::FromStr;

use anyhow::anyhow;
use mailparse::MailHeaderMap;
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{PackageName, Requirement};
use python_pkginfo::Metadata;
use url::Url;

use crate::file::{MetadataDirs, WheelFile};

pub struct WheelMetadata<'a> {
    pub file_name: &'a str,
    pub raw_project_name: &'a str,
    pub project_name: PackageName,
    pub raw_version: &'a str,
    pub version: Version,
    pub requires_dists: Vec<Requirement<Url>>,
    pub requires_python: Option<VersionSpecifiers>,
    pub root_is_purelib: bool,
    pub metadata_dirs: MetadataDirs,
}

pub trait MetadataReader {
    fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs>;
    fn read(
        &mut self,
        metadata_dirs: &MetadataDirs,
        wheel_file: &WheelFile,
        file_name: &str,
    ) -> anyhow::Result<String>;
}

fn parse_root_is_purelib_from_wheel(content: &[u8]) -> anyhow::Result<bool> {
    let msg = mailparse::parse_mail(content)?;
    let headers = msg.get_headers();
    let header = headers
        .get_first_header("Root-Is-Purelib")
        .ok_or_else(|| anyhow!(""))?;
    Ok(matches!(
        rfc2047_decoder::decode(header.get_value_raw())?.as_str(),
        "true" | "True"
    ))
}

impl<'a> WheelMetadata<'a> {
    pub fn parse(
        wheel_file: WheelFile<'a>,
        metadata_dirs: MetadataDirs,
        metadata_reader: &mut impl MetadataReader,
    ) -> anyhow::Result<Self> {
        let metadata = Metadata::parse(
            metadata_reader
                .read(&metadata_dirs, &wheel_file, "METADATA")?
                .as_bytes(),
        )?;
        let mut requires_dists: Vec<Requirement<Url>> =
            Vec::with_capacity(metadata.requires_dist.len());
        for requires_dist in metadata.requires_dist {
            requires_dists.push(Requirement::from_str(&requires_dist)?)
        }
        let requires_python = if let Some(requires_python) = metadata.requires_python {
            Some(VersionSpecifiers::from_str(&requires_python)?)
        } else {
            None
        };

        let root_is_purelib = parse_root_is_purelib_from_wheel(
            metadata_reader
                .read(&metadata_dirs, &wheel_file, "WHEEL")?
                .as_bytes(),
        )?;

        Ok(Self {
            file_name: wheel_file.file_name,
            raw_project_name: wheel_file.raw_project_name,
            project_name: wheel_file.project_name,
            raw_version: wheel_file.raw_version,
            version: wheel_file.version,
            requires_dists,
            requires_python,
            root_is_purelib,
            metadata_dirs,
        })
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;
    use std::fmt::Display;
    use std::io;
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::str::FromStr;

    use fs_err::File;
    use pep440_rs::{Version, VersionSpecifiers};
    use pep508_rs::{PackageName, Requirement};
    use rstest::*;
    use testing::{tmp_dir, venv_python_exe};
    use zip::ZipArchive;

    use crate::{MetadataDirs, MetadataReader, WheelFile, WheelMetadata};

    #[fixture]
    #[once]
    fn requests_2_32_5_whl(tmp_dir: PathBuf, venv_python_exe: PathBuf) -> PathBuf {
        let wheel_dir = tmp_dir.join("wheels");
        assert!(
            Command::new(venv_python_exe)
                .args([
                    "-m",
                    "pip",
                    "download",
                    "--no-deps",
                    "--only-binary",
                    ":all:",
                    "--dest",
                ])
                .arg(&wheel_dir)
                .arg("requests==2.32.5")
                .spawn()
                .unwrap()
                .wait()
                .unwrap()
                .success()
        );
        let mut matches = glob::glob(wheel_dir.join("*.whl").to_str().unwrap()).unwrap();
        let wheel = matches.next().unwrap();
        assert!(matches.next().is_none());
        wheel.unwrap()
    }

    #[rstest]
    fn test_parse_wheel_chroot(requests_2_32_5_whl: &Path) {
        let file_name = requests_2_32_5_whl
            .file_name()
            .and_then(OsStr::to_str)
            .unwrap();
        let wheel_file = WheelFile::parse_file_name(file_name).unwrap();
        assert_eq!("requests", wheel_file.raw_project_name);
        assert_eq!("2.32.5", wheel_file.raw_version);

        struct RequestsMetadataReader<D: Display>(ZipArchive<File>, D);
        impl<D: Display> MetadataReader for RequestsMetadataReader<D> {
            fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs> {
                wheel_file.metadata_dirs_from_zip(&self.0, &self.1, None)
            }
            fn read(
                &mut self,
                metadata_dirs: &MetadataDirs,
                _wheel_file: &WheelFile,
                file_name: &str,
            ) -> anyhow::Result<String> {
                let dist_info_dir = metadata_dirs.dist_info_dir();
                Ok(io::read_to_string(
                    self.0.by_name(&format!("{dist_info_dir}/{file_name}"))?,
                )?)
            }
        }
        let mut metadata_reader = RequestsMetadataReader(
            ZipArchive::new(File::open(requests_2_32_5_whl).unwrap()).unwrap(),
            requests_2_32_5_whl.display(),
        );
        let metadata_dirs = metadata_reader.locate_dirs(&wheel_file).unwrap();
        let wheel = WheelMetadata::parse(wheel_file, metadata_dirs, &mut metadata_reader).unwrap();
        assert_eq!(
            PackageName::new("requests".into()).unwrap(),
            wheel.project_name
        );
        assert_eq!(Version::new([2, 32, 5]), wheel.version);
        assert_eq!(
            Some(VersionSpecifiers::from_str(">=3.9").unwrap()),
            wheel.requires_python
        );
        assert_eq!(
            vec![
                Requirement::from_str("charset_normalizer<4,>=2").unwrap(),
                Requirement::from_str("idna<4,>=2.5").unwrap(),
                Requirement::from_str("urllib3<3,>=1.21.1").unwrap(),
                Requirement::from_str("certifi>=2017.4.17").unwrap(),
                Requirement::from_str("PySocks!=1.5.7,>=1.5.6; extra == \"socks\"").unwrap(),
                Requirement::from_str("chardet<6,>=3.0.2; extra == \"use-chardet-on-py3\"")
                    .unwrap(),
            ],
            wheel.requires_dists
        );
    }
}
