// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::fmt::{Display, Formatter};
use std::fs::FileType;
use std::io::{Read, Seek};
use std::ops::Range;
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;

use anyhow::{anyhow, bail};
use interpreter::Tag;
use ouroboros::self_referencing;
use pep440_rs::Version;
use pep508_rs::PackageName;
use zip::ZipArchive;

pub struct WheelDir<'a> {
    project_name: &'a str,
    version: &'a str,
    suffix: &'a str,
}

impl<'a> WheelDir<'a> {
    pub(crate) fn contains(&self, path: &Path) -> bool {
        if let Some(Component::Normal(start)) = path.components().next() {
            let start = start.as_encoded_bytes();
            if start.starts_with(self.project_name.as_bytes()) {
                let start = &start[self.project_name.len()..];
                if start.starts_with(b"-") {
                    let start = &start[1..];
                    if start.starts_with(self.version.as_bytes()) {
                        let start = &start[self.version.len()..];
                        if start.starts_with(b".") {
                            let start = &start[1..];
                            return start == self.suffix.as_bytes();
                        }
                    }
                }
            }
        }
        false
    }

    pub fn as_path(&self) -> PathBuf {
        self.to_string().into()
    }
}

impl<'a> Display for WheelDir<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{project_name}-{version}.{suffix}",
            project_name = self.project_name,
            version = self.version,
            suffix = self.suffix
        )
    }
}

struct MetadataDir<'a> {
    dir_name: Cow<'a, str>,
    project_name_range: Range<usize>,
    version_range: Range<usize>,
}

fn locate_metadata_dir<'a>(
    project_name: &PackageName,
    version: &Version,
    suffix: &str,
    file_names: impl Iterator<Item = Cow<'a, str>>,
    listing_source: impl Display,
) -> anyhow::Result<MetadataDir<'a>> {
    let mut pattern = String::new();
    pattern.push_str(r"(?<project_name>");
    for char in project_name.as_ref().chars() {
        match char {
            '-' => pattern.push_str(r"[_.]+"),
            _ => pattern.push(char),
        }
    }
    pattern.push_str(r")-(?<version>.+)\.");
    pattern.push_str(suffix);
    let pattern = regex::RegexBuilder::new(&pattern)
        .case_insensitive(true)
        .build()?;
    for file_name in file_names {
        let (project_name_range, version_range) =
            if let Some(captures) = pattern.captures(file_name.as_ref()) {
                let version_match = captures.name("version").expect("We matched all captures");
                if let Ok(ref matched_version) = Version::from_str(version_match.as_str())
                    && matched_version == version
                {
                    let project_name = captures
                        .name("project_name")
                        .expect("We matched all captures");
                    (project_name.range(), version_match.range())
                } else {
                    continue;
                }
            } else {
                continue;
            };

        return Ok(MetadataDir {
            dir_name: file_name,
            project_name_range,
            version_range,
        });
    }
    bail!(
        "Failed to find {suffix} metadata dir in listing of {listing_source} for {project_name} \
        {version}."
    )
}

#[self_referencing]
pub struct MetadataDirs {
    dist_info_dir_name: String,
    #[borrows(dist_info_dir_name)]
    project_name: &'this str,
    #[borrows(dist_info_dir_name)]
    version: &'this str,
}

impl MetadataDirs {
    pub(crate) fn locate_in_dir(
        wheel_dir: &Path,
        project_name: &PackageName,
        version: &Version,
    ) -> anyhow::Result<Self> {
        let read_dir = wheel_dir.read_dir()?;
        let listing = read_dir.into_iter().filter_map(|result| {
            result.ok().and_then(|entry| {
                if entry
                    .file_type()
                    .ok()
                    .as_ref()
                    .map(FileType::is_dir)
                    .unwrap_or_default()
                {
                    entry.file_name().into_string().ok().map(Cow::Owned)
                } else {
                    None
                }
            })
        });
        Self::locate(project_name, version, listing, wheel_dir.display())
    }

    pub(crate) fn locate_in_zip(
        zip: &ZipArchive<impl Read + Seek>,
        zip_source: impl Display,
        prefix: Option<&str>,
        project_name: &PackageName,
        version: &Version,
    ) -> anyhow::Result<Self> {
        let listing = zip.file_names().filter_map(|name| {
            let path_name = if let Some(prefix) = prefix
                && let Some(suffix) = name.strip_prefix(prefix)
            {
                suffix
            } else {
                name
            };
            path_name.split("/").next().map(Cow::Borrowed)
        });
        Self::locate(project_name, version, listing, zip_source)
    }

    fn locate<'a>(
        project_name: &PackageName,
        version: &Version,
        file_names: impl Iterator<Item = Cow<'a, str>>,
        listing_source: impl Display,
    ) -> anyhow::Result<Self> {
        let metadata_dir = locate_metadata_dir(
            project_name,
            version,
            "dist-info",
            file_names,
            listing_source,
        )?;
        Ok(Self::new(
            metadata_dir.dir_name.to_string(),
            |dist_info_dir| &dist_info_dir[metadata_dir.project_name_range],
            |dist_info_dir| &dist_info_dir[metadata_dir.version_range],
        ))
    }

    pub fn dist_info_dir(&self) -> WheelDir<'_> {
        self.wheel_dir("dist-info")
    }

    pub fn data_dir(&self) -> WheelDir<'_> {
        self.wheel_dir("data")
    }

    pub fn pex_info_dir(&self) -> WheelDir<'_> {
        self.wheel_dir("pex-info")
    }

    fn wheel_dir<'a>(&'a self, suffix: &'a str) -> WheelDir<'a> {
        WheelDir {
            project_name: self.borrow_project_name(),
            version: self.borrow_version(),
            suffix,
        }
    }
}

impl Clone for MetadataDirs {
    fn clone(&self) -> Self {
        let dist_info_dir_name = self.borrow_dist_info_dir_name().clone();
        let project_name = self.borrow_project_name();
        let project_name_index = dist_info_dir_name
            .find(project_name)
            .expect("We know our dist-info dir name contains the project name.");
        let version = self.borrow_version();
        let version_index = dist_info_dir_name
            .find(version)
            .expect("We know our dist-info dir name contains the version.");
        Self::new(
            dist_info_dir_name,
            |dist_info_dir| {
                &dist_info_dir[project_name_index..project_name_index + project_name.len()]
            },
            |dist_info_dir| &dist_info_dir[version_index..version_index + version.len()],
        )
    }
}

pub struct WheelFile<'a> {
    pub file_name: &'a str,
    pub(crate) raw_project_name: &'a str,
    pub project_name: PackageName,
    pub(crate) raw_version: &'a str,
    pub(crate) version: Version,
    _build_tag: Option<&'a str>,
    pub tags: Vec<Tag<'a>>,
}

impl<'a> WheelFile<'a> {
    pub fn parse_file_name(file_name: &'a str) -> anyhow::Result<Self> {
        // See: https://packaging.python.org/specifications/binary-distribution-format/#file-name-convention
        // {distribution}-{version}(-{build tag})?-{python tag}-{abi tag}-{platform tag}.whl
        if !file_name.ends_with(".whl") {
            bail!("Not a wheel file name {file_name}")
        }

        let (raw_project_name, raw_version, build_tag, python_tag, abi_tag, platform_tag) = {
            let mut trailing_components = file_name[0..file_name.len() - 4].rsplitn(4, "-");
            let platform_tag = trailing_components
                .next()
                .ok_or_else(|| anyhow!("Failed to parse platform tag from {file_name}"))?;
            let abi_tag = trailing_components
                .next()
                .ok_or_else(|| anyhow!("Failed to parse abi tag from {file_name}"))?;
            let python_tag = trailing_components
                .next()
                .ok_or_else(|| anyhow!("Failed to parse python tag from {file_name}"))?;
            let rest = trailing_components
                .next()
                .ok_or_else(|| anyhow!("Failed to parse wheel tags from {file_name}"))?;

            let mut leading_components = rest.splitn(3, "-");
            let project_name = leading_components
                .next()
                .ok_or_else(|| anyhow!("Failed to parse project name from {file_name}"))?;
            let version = leading_components
                .next()
                .ok_or_else(|| anyhow!("Failed to parse version from {file_name}"))?;
            let build_tag = leading_components.next();

            (
                project_name,
                version,
                build_tag,
                python_tag,
                abi_tag,
                platform_tag,
            )
        };

        let mut tags: Vec<Tag<'a>> = Vec::new();
        for python in python_tag.split(".") {
            for abi in abi_tag.split(".") {
                for platform in platform_tag.split(".") {
                    tags.push(Tag {
                        python,
                        abi,
                        platform,
                    })
                }
            }
        }

        let project_name = PackageName::from_str(raw_project_name)?;
        let version = Version::from_str(raw_version)?;
        Ok(Self {
            file_name,
            raw_project_name,
            project_name,
            raw_version,
            version,
            _build_tag: build_tag,
            tags,
        })
    }

    pub(crate) fn metadata_dirs(&self, wheel_dir: &Path) -> anyhow::Result<MetadataDirs> {
        MetadataDirs::locate_in_dir(wheel_dir, &self.project_name, &self.version)
    }

    pub(crate) fn metadata_dirs_from_zip(
        &self,
        zip: &ZipArchive<impl Read + Seek>,
        zip_source: impl Display,
        prefix: Option<&str>,
    ) -> anyhow::Result<MetadataDirs> {
        MetadataDirs::locate_in_zip(zip, zip_source, prefix, &self.project_name, &self.version)
    }
}

impl<'a> Display for WheelFile<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.file_name)
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::str::FromStr;

    use interpreter::Tag;
    use pep440_rs::Version;
    use pep508_rs::PackageName;

    use crate::wheel::file::{WheelFile, locate_metadata_dir};

    #[test]
    fn test_parse_wheel_file_name_simple() {
        let wheel_file = WheelFile::parse_file_name("cowsay-6.1-py3-none-any.whl").unwrap();
        assert_eq!("cowsay", wheel_file.raw_project_name);
        assert_eq!(
            PackageName::from_str("cowsay").unwrap(),
            wheel_file.project_name
        );
        assert_eq!("6.1", wheel_file.raw_version);
        assert_eq!(Version::from_str("6.1").unwrap(), wheel_file.version);
        assert_eq!(
            vec![Tag {
                python: "py3",
                abi: "none",
                platform: "any"
            }],
            wheel_file.tags
        )
    }

    #[test]
    fn test_parse_wheel_file_name_multiple_tags() {
        let wheel_file = WheelFile::parse_file_name(
            "psutil-7.2.2-cp314-cp314t-manylinux2014_aarch64.manylinux_2_17_aarch64.manylinux_2_17_aarch64.whl"
        ).unwrap();
        assert_eq!("psutil", wheel_file.raw_project_name);
        assert_eq!(
            PackageName::from_str("psutil").unwrap(),
            wheel_file.project_name
        );
        assert_eq!("7.2.2", wheel_file.raw_version);
        assert_eq!(Version::from_str("7.2.2").unwrap(), wheel_file.version);
        assert_eq!(
            vec![
                Tag {
                    python: "cp314",
                    abi: "cp314t",
                    platform: "manylinux2014_aarch64"
                },
                Tag {
                    python: "cp314",
                    abi: "cp314t",
                    platform: "manylinux_2_17_aarch64"
                },
                Tag {
                    python: "cp314",
                    abi: "cp314t",
                    platform: "manylinux_2_17_aarch64"
                }
            ],
            wheel_file.tags
        )
    }

    #[test]
    fn test_parse_wheel_file_name_build_tag() {
        let wheel_file =
            WheelFile::parse_file_name("cffi-1.14.3-2-cp39-cp39-macosx_10_9_x86_64.whl").unwrap();
        assert_eq!("cffi", wheel_file.raw_project_name);
        assert_eq!(
            PackageName::from_str("cffi").unwrap(),
            wheel_file.project_name
        );
        assert_eq!("1.14.3", wheel_file.raw_version);
        assert_eq!(Some("2"), wheel_file._build_tag);
        assert_eq!(Version::from_str("1.14.3").unwrap(), wheel_file.version);
        assert_eq!(
            vec![Tag {
                python: "cp39",
                abi: "cp39",
                platform: "macosx_10_9_x86_64"
            }],
            wheel_file.tags
        )
    }

    #[test]
    fn test_locate_metadata_dir_normalized_name_alpha_and_normalized_version_trailing_zeros_less() {
        let listing = ["sqlalchemy-0.2.dist-info", "sqlalchemy-0.1.dist-info"];
        let metadata_dir = locate_metadata_dir(
            &PackageName::from_str("SQLAlchemy").unwrap(),
            &Version::from_str("0.1.0").unwrap(),
            "dist-info",
            listing.into_iter().map(Cow::Borrowed),
            "memory",
        )
        .unwrap();
        assert_eq!("sqlalchemy-0.1.dist-info", metadata_dir.dir_name.as_ref());
        assert_eq!(0..10, metadata_dir.project_name_range);
        assert_eq!(11..14, metadata_dir.version_range);
    }

    #[test]
    fn test_locate_metadata_dir_normalized_name_dots_version_trialing_zeros_more() {
        let listing = [
            "jaroco.collections-2.0.0.data",
            "jaroco_collections-1.0.0.data",
        ];
        let metadata_dir = locate_metadata_dir(
            &PackageName::from_str("jaroco.collections").unwrap(),
            &Version::from_str("1.0").unwrap(),
            "data",
            listing.into_iter().map(Cow::Borrowed),
            "memory",
        )
        .unwrap();
        assert_eq!(
            "jaroco_collections-1.0.0.data",
            metadata_dir.dir_name.as_ref()
        );
        assert_eq!(0..18, metadata_dir.project_name_range);
        assert_eq!(19..24, metadata_dir.version_range);
    }

    #[test]
    fn test_locate_metadata_dir_normalized_name_dots_dashes_underscores_runs() {
        let listing = ["foo", "pypa_specs_are_nuts-1.pex-info", "bar"];
        let metadata_dir = locate_metadata_dir(
            &PackageName::from_str("PyPA.-_specs...are-NUTs").unwrap(),
            &Version::from_str("1.0").unwrap(),
            "pex-info",
            listing.into_iter().map(Cow::Borrowed),
            "memory",
        )
        .unwrap();
        assert_eq!(
            "pypa_specs_are_nuts-1.pex-info",
            metadata_dir.dir_name.as_ref()
        );
        assert_eq!(0..19, metadata_dir.project_name_range);
        assert_eq!(20..21, metadata_dir.version_range);

        let listing = ["foo", "PyPA._specs...are_NUTs-1.pex-info", "bar"];
        let metadata_dir = locate_metadata_dir(
            &PackageName::from_str("pypa_specs_are_nuts").unwrap(),
            &Version::from_str("1.0.0").unwrap(),
            "pex-info",
            listing.into_iter().map(Cow::Borrowed),
            "memory",
        )
        .unwrap();
        assert_eq!(
            "PyPA._specs...are_NUTs-1.pex-info",
            metadata_dir.dir_name.as_ref()
        );
        assert_eq!(0..22, metadata_dir.project_name_range);
        assert_eq!(23..24, metadata_dir.version_range);
    }
}
