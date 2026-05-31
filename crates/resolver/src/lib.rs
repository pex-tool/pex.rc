// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use dashmap::DashMap;
use indexmap::IndexMap;
use logging_timer::time;
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{ExtraName, PackageName, Requirement, VersionOrUrl};
use python_platform::PythonPlatform;
use url::Url;
use wheel::{MetadataDirs, MetadataReader, Tag, WheelDir, WheelFile, WheelMetadata};

use crate::dependency_configuration::DependencyConfiguration;

pub mod dependency_configuration;

pub struct ResolvedWheel<'a> {
    file_name: &'a str,
    pub project_name: &'a str,
    pub version: &'a str,
    pub root_is_purelib: bool,
    pub metadata_dirs: MetadataDirs,
}

impl<'a> ResolvedWheel<'a> {
    pub fn data_dir(&'a self) -> WheelDir<'a> {
        self.metadata_dirs.data_dir()
    }

    pub fn dist_info_dir(&'a self) -> WheelDir<'a> {
        self.metadata_dirs.dist_info_dir()
    }

    pub fn pex_info_dir(&'a self) -> WheelDir<'a> {
        self.metadata_dirs.pex_info_dir()
    }
}

#[derive(Clone)]
pub struct CollectWheelMetadata<'a>(Arc<DashMap<&'a str, WheelMetadata<'a>>>);

impl<'a> Default for CollectWheelMetadata<'a> {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> CollectWheelMetadata<'a> {
    pub fn new() -> Self {
        Self(Arc::new(DashMap::new()))
    }

    pub fn into_collected(self) -> anyhow::Result<Vec<WheelMetadata<'a>>> {
        let metadata = Arc::try_unwrap(self.0)
            .ok()
            .ok_or_else(|| anyhow!("Metadata is still being collected."))?;
        Ok(metadata.into_iter().map(|(_, metadata)| metadata).collect())
    }

    fn collect(&self, file_name: &'a str, metadata_func: impl FnOnce() -> WheelMetadata<'a>) {
        self.0.entry(file_name).or_insert_with(metadata_func);
    }
}

#[time("debug", "{}")]
pub fn resolve_wheels<'a>(
    target: &impl PythonPlatform<'a>,
    requirements: Vec<Requirement<Url>>,
    wheel_files: impl Fn() -> anyhow::Result<Vec<WheelFile<'a>>>,
    metadata_reader: &mut impl MetadataReader,
    dependency_configuration: &DependencyConfiguration,
    collect_extra_metadata: Option<CollectWheelMetadata<'a>>,
    ignore_errors: bool,
) -> anyhow::Result<IndexMap<&'a str, ResolvedWheel<'a>>> {
    let supported_tags: HashMap<Tag, usize> = target
        .supported_tags()
        .enumerate()
        .map(|(idx, tag)| Tag::parse(tag).map(|tag| (tag, idx)))
        .collect::<anyhow::Result<_>>()?;

    let ranked_wheel_files = wheel_files()?
        .into_iter()
        .filter_map(|wheel_file| {
            for tag in &wheel_file.tags {
                if let Some(rank) = supported_tags.get(tag) {
                    return Some(RankedWheelFile {
                        wheel_file,
                        rank: *rank,
                    });
                }
            }
            None
        })
        .collect::<Vec<_>>();

    let ranked_wheels = read_wheel_metadata(
        target.version().as_ref(),
        ranked_wheel_files,
        metadata_reader,
    )?;

    struct WheelInfo<'b> {
        file_name: &'b str,
        raw_project_name: &'b str,
        raw_version: &'b str,
        version: Version,
        requires_dists: Vec<Requirement<Url>>,
        requires_python: Option<VersionSpecifiers>,
        root_is_purelib: bool,
        rank: usize,
        metadata_dirs: MetadataDirs,
    }

    let mut wheels_by_project_name: HashMap<PackageName, Vec<WheelInfo>> =
        HashMap::with_capacity(ranked_wheels.len());
    for ranked_wheel in ranked_wheels {
        wheels_by_project_name
            .entry(ranked_wheel.metadata.project_name)
            .or_default()
            .push(WheelInfo {
                file_name: ranked_wheel.metadata.file_name,
                raw_project_name: ranked_wheel.metadata.raw_project_name,
                raw_version: ranked_wheel.metadata.raw_version,
                version: ranked_wheel.metadata.version,
                requires_dists: ranked_wheel.metadata.requires_dists,
                requires_python: ranked_wheel.metadata.requires_python,
                root_is_purelib: ranked_wheel.metadata.root_is_purelib,
                rank: ranked_wheel.rank,
                metadata_dirs: ranked_wheel.metadata.metadata_dirs,
            })
    }
    for wheels in wheels_by_project_name.values_mut() {
        wheels.sort_by_key(|WheelInfo { rank, .. }| *rank);
    }

    let mut resolved_by_project_name: IndexMap<RequirementKey, ResolvedWheel> =
        IndexMap::with_capacity(wheels_by_project_name.len());
    let mut indexed_extras: Vec<Vec<ExtraName>> = vec![vec![]];
    let mut to_resolve: VecDeque<(Requirement<Url>, usize)> = requirements
        .into_iter()
        .filter_map(|requirement| {
            if dependency_configuration.excluded(&requirement) {
                None
            } else {
                Some((requirement, 0))
            }
        })
        .collect::<VecDeque<_>>();

    let marker_env = target.marker_env();
    let no_wheels: Vec<WheelInfo> = vec![];
    while let Some((requirement, extras_index)) = to_resolve.pop_front() {
        let requirement_key = RequirementKey::of(&requirement);

        // Already processed.
        if resolved_by_project_name.contains_key(&requirement_key) {
            continue;
        }
        if resolved_by_project_name
            .keys()
            .any(|key| key.satisfies(&requirement_key))
        {
            continue;
        }

        // Does not apply.
        if !requirement
            .marker
            .evaluate(marker_env, &indexed_extras[extras_index])
        {
            continue;
        }

        let wheels = wheels_by_project_name
            .get(&requirement.name)
            .or({
                if ignore_errors {
                    Some(&no_wheels)
                } else {
                    None
                }
            })
            .ok_or_else(|| {
                let inapplicable_wheels = wheel_files()
                    .expect(
                        "We already parsed wheel files once successfully and parsing is \
                        deterministic.",
                    )
                    .into_iter()
                    .filter_map(|wheel_file| {
                        if wheel_file.project_name == requirement.name {
                            Some(wheel_file.file_name)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>();
                let count = inapplicable_wheels.len();
                let wheels = if count == 1 { "wheel" } else { "wheels" };
                let reason = if inapplicable_wheels.is_empty() {
                    format_args!(
                        "The PEX contains {count} embedded {wheels} for project: {project}",
                        project = requirement.name
                    )
                } else {
                    format_args!(
                        "The PEX contains {count} inapplicable {wheels} for project: \
                            {project}\n\
                            {inapplicable_wheels}",
                        project = requirement.name,
                        inapplicable_wheels = inapplicable_wheels.join("\n")
                    )
                };
                anyhow!(
                    "The requirement {requirement} cannot be satisfied for {target}.\n\
                    {reason}",
                    target = target.description(),
                    reason = reason,
                )
            })?;
        for WheelInfo {
            file_name,
            raw_project_name,
            raw_version,
            version,
            requires_dists,
            requires_python,
            root_is_purelib,
            metadata_dirs,
            ..
        } in wheels
        {
            if let Some(version_or_url) = requirement.version_or_url.as_ref() {
                match version_or_url {
                    VersionOrUrl::VersionSpecifier(version_specifier) => {
                        if !version_specifier.contains(version) {
                            continue;
                        }
                    }
                    VersionOrUrl::Url(url) => bail!("URL requirements are not supported: {url}"),
                }
            }
            let extras_index = if requirement.extras.is_empty() {
                0
            } else {
                let idx = indexed_extras.len();
                indexed_extras.push(requirement.extras);
                idx
            };
            if let Some(extra_metadata) = collect_extra_metadata.as_ref() {
                extra_metadata.collect(file_name, || WheelMetadata {
                    file_name,
                    raw_project_name,
                    project_name: requirement.name.clone(),
                    raw_version,
                    version: version.clone(),
                    requires_dists: requires_dists.clone(),
                    requires_python: requires_python.clone(),
                    root_is_purelib: *root_is_purelib,
                    metadata_dirs: metadata_dirs.clone(),
                })
            }
            resolved_by_project_name.insert(
                requirement_key,
                ResolvedWheel {
                    file_name,
                    project_name: raw_project_name,
                    version: raw_version,
                    root_is_purelib: *root_is_purelib,
                    metadata_dirs: metadata_dirs.clone(),
                },
            );
            for req in requires_dists {
                if dependency_configuration.excluded(req) {
                    continue;
                }
                to_resolve.push_back((
                    dependency_configuration
                        .overridden(req, target, &indexed_extras[extras_index])?
                        .unwrap_or_else(|| req.clone()),
                    extras_index,
                ))
            }
            break;
        }
    }
    Ok(resolved_by_project_name
        .into_values()
        .map(|resolved_wheel| (resolved_wheel.file_name, resolved_wheel))
        .collect())
}

fn read_wheel_metadata<'a>(
    python_version: &Version,
    ranked_wheel_files: Vec<RankedWheelFile<'a>>,
    metadata_reader: &mut impl MetadataReader,
) -> anyhow::Result<Vec<RankedWheel<'a>>> {
    let mut ranked_wheels = Vec::with_capacity(ranked_wheel_files.len());
    for ranked_wheel_file in ranked_wheel_files {
        let metadata_dirs = metadata_reader.locate_dirs(&ranked_wheel_file.wheel_file)?;
        let metadata =
            WheelMetadata::parse(ranked_wheel_file.wheel_file, metadata_dirs, metadata_reader)?;
        if let Some(requires_python) = &metadata.requires_python
            && !requires_python.contains(python_version)
        {
            continue;
        }
        ranked_wheels.push(RankedWheel {
            metadata,
            rank: ranked_wheel_file.rank,
        });
    }
    Ok(ranked_wheels)
}

struct RankedWheelFile<'a> {
    wheel_file: WheelFile<'a>,
    rank: usize,
}

struct RankedWheel<'a> {
    metadata: WheelMetadata<'a>,
    rank: usize,
}

#[derive(Hash, Eq, PartialEq)]
struct RequirementKey {
    package_name: PackageName,
    extras: BTreeSet<ExtraName>,
}

impl RequirementKey {
    fn of(requirement: &Requirement<Url>) -> Self {
        Self {
            package_name: requirement.name.clone(),
            extras: requirement.extras.iter().cloned().collect(),
        }
    }

    fn satisfies(&self, requested: &RequirementKey) -> bool {
        self.package_name == requested.package_name && requested.extras.is_subset(&self.extras)
    }
}
