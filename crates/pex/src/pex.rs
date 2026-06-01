// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::ffi::OsStr;
use std::fs::FileType;
use std::io;
use std::io::{BufReader, Read, Seek};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, bail};
use dashmap::DashMap;
use fs_err as fs;
use fs_err::File;
use indexmap::IndexMap;
use interpreter::{Interpreter, InterpreterConstraints, SearchPath, Tag};
use itertools::Itertools;
use log::{Level, debug, warn};
use logging_timer::{time, timer};
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{ExtraName, PackageName, Requirement, VersionOrUrl};
use python_platform::PythonPlatform;
use rayon::prelude::*;
use scripts::{IdentifyInterpreter, Scripts};
use strum_macros::{AsRefStr, EnumString};
use url::Url;
use walkdir::{DirEntry, WalkDir};
use zip::ZipArchive;

use crate::wheel::{MetadataDirs, MetadataReader, WheelFile, WheelMetadata};
use crate::{DependencyConfiguration, InterpreterSelectionStrategy, PexInfo, WheelDir};

#[derive(AsRefStr, EnumString)]
pub enum Layout {
    #[strum(serialize = "loose")]
    Loose,
    #[strum(serialize = "packed")]
    Packed,
    #[strum(serialize = "zipapp")]
    ZipApp,
}

impl Layout {
    pub fn load(pex: &Path) -> anyhow::Result<Self> {
        let layout = if pex.is_file() {
            Layout::ZipApp
        } else {
            let deps_dir = pex.join(".deps");
            if deps_dir.is_dir()
                && let Some(wheel) = fs::read_dir(&deps_dir)?
                    .filter_map(|entry| {
                        entry
                            .ok()
                            .filter(|e| e.path().extension() == Some(OsStr::new("whl")))
                    })
                    .next()
                && wheel
                    .file_type()
                    .ok()
                    .as_ref()
                    .map(FileType::is_file)
                    .unwrap_or_default()
            {
                Layout::Packed
            } else {
                Layout::Loose
            }
        };
        Ok(layout)
    }
}

pub fn collect_loose_user_source(pex: &Path) -> anyhow::Result<Vec<DirEntry>> {
    let excludes: HashSet<PathBuf> = [
        ".deps",
        "PEX-INFO",
        "__main__.py",
        "__pex__",
        "__pycache__",
        "pex",
        "pex-repl",
    ]
    .into_iter()
    .map(|rel_path| pex.join(rel_path))
    .collect();
    Ok(WalkDir::new(pex)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| !excludes.contains(entry.path()))
        .collect::<Result<Vec<_>, _>>()?)
}

pub fn filter_zipped_user_source(file_name: &str) -> bool {
    ![".deps/", "__pex__/"]
        .iter()
        .any(|dir_prefix| file_name.starts_with(dir_prefix))
        && ![".deps/", "__pex__/", "PEX-INFO", "__main__.py"].contains(&file_name)
}

pub fn collect_zipped_user_source_indexes(pex: &ZipArchive<impl Read + Seek>) -> Vec<usize> {
    pex.file_names()
        .enumerate()
        .filter_map(|(idx, name)| {
            if filter_zipped_user_source(name) {
                Some(idx)
            } else {
                None
            }
        })
        .collect::<Vec<_>>()
}

pub struct Pex<'a> {
    pub path: &'a Path,
    pub info: PexInfo,
    pub layout: Layout,
}

pub struct ResolvedWheels<'a> {
    pub interpreter: Interpreter,
    pub wheels: IndexMap<&'a str, ResolvedWheel<'a>>,
}

pub struct ResolveError {
    pub python_exe: PathBuf,
    pub err: anyhow::Error,
}

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

pub struct Resolve<'a> {
    pub interpreter: Interpreter,
    pub wheels: IndexMap<&'a str, ResolvedWheel<'a>>,
    pub scripts: Scripts,
    pub additional_wheels: Vec<(&'a Pex<'a>, IndexMap<&'a str, ResolvedWheel<'a>>)>,
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

impl<'a> Pex<'a> {
    #[time("debug", "Pex.{}")]
    pub fn load(path: &'a Path) -> anyhow::Result<Self> {
        match Layout::load(path)? {
            layout @ (Layout::Loose | Layout::Packed) => {
                let pex_info_path = path.join("PEX-INFO");
                let pex_info_fp = File::open(&pex_info_path)?;
                let size = pex_info_fp.metadata()?.len();
                let pex_info =
                    PexInfo::parse(pex_info_fp, size, Some(|| pex_info_path.to_string_lossy()))?;

                Ok(Self {
                    path,
                    info: pex_info,
                    layout,
                })
            }
            Layout::ZipApp => {
                let zip_fp = File::open(path)?;
                let mut zip = {
                    let _timer = timer!(Level::Debug; "Open PEX zip", "{}", path.display());
                    ZipArchive::new(BufReader::new(zip_fp))?
                };
                let zip_file = zip.by_name("PEX-INFO")?;
                let size = zip_file.size();
                let pex_info = PexInfo::parse(zip_file, size, Some(|| Cow::Borrowed("PEX-INFO")))?;
                Ok(Self {
                    path,
                    info: pex_info,
                    layout: Layout::ZipApp,
                })
            }
        }
    }

    pub fn file(&self) -> Cow<'a, Path> {
        match self.layout {
            Layout::Loose | Layout::Packed => Cow::Owned(self.path.join("pex")),
            Layout::ZipApp => Cow::Borrowed(self.path),
        }
    }

    pub fn scripts(&self) -> anyhow::Result<Scripts> {
        let path = self.path.to_path_buf();
        match self.layout {
            Layout::Packed | Layout::Loose => Ok(Scripts::Loose(path)),
            Layout::ZipApp => Ok(Scripts::Zipped(ZipArchive::new(File::open(&path)?)?)),
        }
    }
    pub fn dependency_configuration(&self) -> anyhow::Result<DependencyConfiguration> {
        DependencyConfiguration::load(&self.info)
    }

    #[time("debug", "Pex.{}")]
    fn resolve_wheels(
        &'a self,
        target: &impl PythonPlatform<'a>,
        dependency_configuration: &DependencyConfiguration,
        collect_extra_metadata: Option<CollectWheelMetadata<'a>>,
    ) -> anyhow::Result<IndexMap<&'a str, ResolvedWheel<'a>>> {
        let supported_tags: HashMap<Tag, usize> = target
            .supported_tags()
            .enumerate()
            .map(|(idx, tag)| Tag::parse(tag).map(|tag| (tag, idx)))
            .collect::<anyhow::Result<_>>()?;

        let wheel_files = self
            .info
            .parse_distributions()
            .collect::<Result<Vec<_>, _>>()?;

        let ranked_wheel_files = wheel_files
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

        let ranked_wheels = self.load_wheel_metadata(target, ranked_wheel_files)?;

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
        let mut to_resolve: VecDeque<(Requirement<Url>, usize)> = self
            .info
            .raw()
            .requirements
            .iter()
            .map(|requirement| {
                Requirement::from_str(requirement).map(|requirement| (requirement, 0))
            })
            .filter_map(|result| match result {
                Ok((requirement, extras_index)) => {
                    if dependency_configuration.excluded(&requirement) {
                        None
                    } else {
                        Some(Ok((requirement, extras_index)))
                    }
                }
                Err(err) => Some(Err(err)),
            })
            .collect::<Result<_, _>>()?;
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
                .or_else(|| {
                    if self.info.raw().ignore_errors {
                        Some(&no_wheels)
                    } else {
                        None
                    }
                })
                .ok_or_else(|| {
                    let inapplicable_wheels = self
                        .info
                        .parse_distributions()
                        .filter_map(|result| match result {
                            Ok(wheel_file) if wheel_file.project_name == requirement.name => {
                                Some(wheel_file.file_name)
                            }
                            _ => None,
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
                        "The PEX at {path} has requirement {requirement} that cannot be satisfied \
                        for {target}.\n\
                        {reason}",
                        path = self.path.display(),
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
                        VersionOrUrl::Url(url) => bail!(
                            "A PEX should never contain an URL requirement.\
                            The PEX at {path} requires: {url}",
                            path = self.path.display()
                        ),
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

    pub fn resolve_all(
        &'a self,
        identification_script: &IdentifyInterpreter,
        interpreter_constraints: &InterpreterConstraints,
        search_path: SearchPath,
        dependency_configuration: &DependencyConfiguration,
        collect_extra_metadata: Option<CollectWheelMetadata<'a>>,
    ) -> anyhow::Result<impl ParallelIterator<Item = Result<ResolvedWheels<'a>, ResolveError>>>
    {
        let interpreters_to_try = interpreter_constraints
            .iter_possibly_compatible_python_exes(
                self.info
                    .raw()
                    .interpreter_selection_strategy
                    .unwrap_or(InterpreterSelectionStrategy::Oldest)
                    .into(),
                search_path,
                false,
            )?
            .collect::<Vec<_>>();

        Ok(interpreters_to_try
            .into_par_iter()
            .filter_map(
                |python_exe| match Interpreter::load(&python_exe, identification_script) {
                    Ok(interpreter) => Some(interpreter),
                    Err(err) => {
                        warn!(
                            "Failed to load {python_exe}: {err}",
                            python_exe = python_exe.display()
                        );
                        None
                    }
                },
            )
            .filter(|interpreter| interpreter_constraints.contains(interpreter))
            .map(move |interpreter| {
                match self.resolve_wheels(
                    &interpreter,
                    dependency_configuration,
                    collect_extra_metadata.clone(),
                ) {
                    Ok(selected_wheels) => Ok(ResolvedWheels {
                        interpreter,
                        wheels: selected_wheels,
                    }),
                    Err(err) => Err(ResolveError {
                        python_exe: interpreter.details.path.to_path_buf(),
                        err,
                    }),
                }
            }))
    }

    #[time("debug", "Pex.{}")]
    pub fn resolve(
        &'a self,
        python_exe: Option<&Path>,
        additional_pexes: impl Iterator<Item = &'a Pex<'a>>,
        search_path: SearchPath,
        collect_extra_metadata: Option<CollectWheelMetadata<'a>>,
    ) -> anyhow::Result<Resolve<'a>> {
        let mut scripts = self.scripts()?;
        let identification_script = IdentifyInterpreter::read(&mut scripts)?;

        let interpreter_constraints =
            InterpreterConstraints::try_from(&self.info.raw().interpreter_constraints)?;
        let dependency_configuration = self.dependency_configuration()?;
        let mut errors = Vec::new();
        if let Some(python_exe) = python_exe
            && let Ok(interpreter) = Interpreter::load(python_exe, &identification_script)
            && interpreter_constraints.contains(&interpreter)
            && search_path.contains(python_exe)
        {
            match self.resolve_wheels(
                &interpreter,
                &dependency_configuration,
                collect_extra_metadata.clone(),
            ) {
                Ok(wheels) => {
                    let additional_wheels = additional_pexes
                        .map(|pex| {
                            pex.resolve_wheels(
                                &interpreter,
                                &dependency_configuration,
                                collect_extra_metadata.clone(),
                            )
                            .map(|wheels| (pex, wheels))
                        })
                        .collect::<anyhow::Result<Vec<_>>>()?;
                    return Ok(Resolve {
                        interpreter,
                        wheels,
                        scripts,
                        additional_wheels,
                    });
                }
                Err(err) => errors.push((interpreter.details.path.to_path_buf(), err)),
            }
        }

        let resolve_results_iter = self.resolve_all(
            &identification_script,
            &interpreter_constraints,
            search_path,
            &dependency_configuration,
            collect_extra_metadata.clone(),
        )?;
        let errors: Arc<Mutex<Vec<(PathBuf, anyhow::Error)>>> = Arc::new(Mutex::new(errors));
        if let Some((interpreter, wheels)) =
            resolve_results_iter.find_map_first(|result| match result {
                Ok(ResolvedWheels {
                    interpreter,
                    wheels,
                }) => Some((interpreter, wheels)),
                Err(ResolveError { python_exe, err }) => {
                    if let Err(lock_err) = errors.lock().map(|mut errors| {
                        debug!(
                            "Failed to resolve for {python_exe}: {err}",
                            python_exe = python_exe.display()
                        );
                        errors.push((python_exe, err))
                    }) {
                        debug!("Failed to record resolve error due to lock poisoning: {lock_err}");
                    }
                    None
                }
            })
        {
            let additional_wheels = additional_pexes
                .map(|pex| {
                    pex.resolve_wheels(
                        &interpreter,
                        &dependency_configuration,
                        collect_extra_metadata.clone(),
                    )
                    .map(|wheels| (pex, wheels))
                })
                .collect::<anyhow::Result<Vec<_>>>()?;
            return Ok(Resolve {
                interpreter,
                wheels,
                scripts,
                additional_wheels,
            });
        }

        let reqs = &self.info.raw().requirements;
        let requirement_count = reqs.len();
        let requirements = if requirement_count == 1 {
            "requirement"
        } else {
            "requirements"
        };

        let errors = errors.lock().map_err(|err| {
            anyhow!(
                "Failed to resolve requirements for PEX {path} and resolve errors were obfuscated \
                by a poisoned lock: {err}",
                path = self.path.display()
            )
        })?;
        let error_count = errors.len();
        let interpreters = if error_count == 1 {
            "interpreter"
        } else {
            "interpreters"
        };

        bail!(
            "Failed to resolve dependencies of PEX {path}.\n\
            \n\
            There are {requirement_count} root {requirements}:\n\
            {reqs}\n\
            \n\
            Tried resolving using {error_count} {interpreters}:\n\
            {errors}",
            path = self.path.display(),
            reqs = reqs.iter().map(|req| format!("+ {req}")).join("\n"),
            errors = errors
                .iter()
                .enumerate()
                .map(|(idx, (interpreter, err))| format!(
                    "{idx:>2} {path}: {err}",
                    idx = idx + 1,
                    path = interpreter.display()
                ))
                .join("\n")
        )
    }

    fn load_wheel_metadata(
        &'a self,
        target: &impl PythonPlatform<'a>,
        wheel_files: Vec<RankedWheelFile<'a>>,
    ) -> anyhow::Result<Vec<RankedWheel<'a>>> {
        let python_version = target.version();
        match self.layout {
            // N.B.: When deps_are_wheel_files for a `--layout loose` PEX, our layout detection
            // detects as `--layout packed`, which properly handles the .whl zips.
            Layout::Loose => read_wheel_metadata(
                python_version.as_ref(),
                wheel_files,
                &mut LoosePexMetadataReader(self.path),
            ),
            // N.B.: When deps_are_wheel_files for a `--layout packed` PEX, the packed wheel chroot
            // zips and normal .whl zips have the same for code and metadata; so no differentiation
            // in behavior is needed.
            Layout::Packed => read_wheel_metadata(
                python_version.as_ref(),
                wheel_files,
                &mut PackedPexMetadataReader(self.path),
            ),
            Layout::ZipApp => read_wheel_metadata(
                python_version.as_ref(),
                wheel_files,
                &mut ZipAppPexMetadataReader::new(self.path, self.info.raw().deps_are_wheel_files)?,
            ),
        }
    }
}
struct RankedWheelFile<'a> {
    wheel_file: WheelFile<'a>,
    rank: usize,
}

struct RankedWheel<'a> {
    metadata: WheelMetadata<'a>,
    rank: usize,
}

struct ZipAppPexMetadataReader<'a> {
    pex_zip: ZipArchive<File>,
    zip_path: &'a Path,
    deps_are_wheel_files: bool,
}

impl<'a> ZipAppPexMetadataReader<'a> {
    fn new(zip_path: &'a Path, deps_are_wheel_files: bool) -> anyhow::Result<Self> {
        Ok(Self {
            pex_zip: ZipArchive::new(File::open(zip_path)?)?,
            zip_path,
            deps_are_wheel_files,
        })
    }
}

impl<'a> MetadataReader for ZipAppPexMetadataReader<'a> {
    fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs> {
        if self.deps_are_wheel_files {
            let whl = self.pex_zip.by_name_seek(&format!(
                ".deps/{wheel_file_name}",
                wheel_file_name = wheel_file.file_name
            ))?;
            let whl_zip = ZipArchive::new(whl)?;
            wheel_file.metadata_dirs_from_zip(&whl_zip, self.zip_path.display(), None)
        } else {
            let prefix = format!(
                ".deps/{wheel_file_name}/",
                wheel_file_name = wheel_file.file_name
            );
            wheel_file.metadata_dirs_from_zip(&self.pex_zip, self.zip_path.display(), Some(&prefix))
        }
    }

    fn read(
        &mut self,
        metadata_dirs: &MetadataDirs,
        wheel_file: &WheelFile,
        file_name: &str,
    ) -> anyhow::Result<String> {
        if self.deps_are_wheel_files {
            let whl = self
                .pex_zip
                .by_name_seek(&[".deps", wheel_file.file_name].join("/"))?;
            let mut whl_zip = ZipArchive::new(whl)?;
            let dist_info_dir = metadata_dirs.dist_info_dir();
            Ok(io::read_to_string(
                whl_zip.by_name(&format!("{dist_info_dir}/{file_name}"))?,
            )?)
        } else {
            let prefix = format!(
                ".deps/{wheel_file_name}/",
                wheel_file_name = wheel_file.file_name
            );
            let dist_info_dir = metadata_dirs.dist_info_dir();
            Ok(io::read_to_string(self.pex_zip.by_name(&format!(
                "{prefix}{dist_info_dir}/{file_name}"
            ))?)?)
        }
    }
}

struct LoosePexMetadataReader<'a>(&'a Path);

impl<'a> MetadataReader for LoosePexMetadataReader<'a> {
    fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs> {
        wheel_file.metadata_dirs(&self.0.join(".deps").join(wheel_file.file_name))
    }

    fn read(
        &mut self,
        metadata_dirs: &MetadataDirs,
        wheel_file: &WheelFile,
        file_name: &str,
    ) -> anyhow::Result<String> {
        let mut read_path = self.0.join(".deps").join(wheel_file.file_name);
        read_path.push(metadata_dirs.dist_info_dir().as_path());
        read_path.push(file_name);
        Ok(fs::read_to_string(read_path)?)
    }
}

struct PackedPexMetadataReader<'a>(&'a Path);

impl<'a> MetadataReader for PackedPexMetadataReader<'a> {
    fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs> {
        let wheel_file_path = self.0.join(".deps").join(wheel_file.file_name);
        let zip = ZipArchive::new(File::open(&wheel_file_path)?)?;
        wheel_file.metadata_dirs_from_zip(&zip, wheel_file_path.display(), None)
    }

    fn read(
        &mut self,
        metadata_dirs: &MetadataDirs,
        wheel_file: &WheelFile,
        file_name: &str,
    ) -> anyhow::Result<String> {
        let wheel_file_path = self.0.join(".deps").join(wheel_file.file_name);
        let mut zip = ZipArchive::new(File::open(&wheel_file_path)?)?;
        let dist_info_dir = metadata_dirs.dist_info_dir();
        Ok(io::read_to_string(
            zip.by_name(&format!("{dist_info_dir}/{file_name}"))?,
        )?)
    }
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

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::str::FromStr;

    use fs_err::File;
    use indexmap::{IndexMap, IndexSet, indexset};
    use interpreter::{Interpreter, SearchPath};
    use pep440_rs::VersionSpecifiers;
    use pep508_rs::{Requirement, VersionOrUrl};
    use rstest::{fixture, rstest};
    use scripts::{IdentifyInterpreter, Scripts};
    use testing::{embedded_scripts, interpreter_identification_script, python_exe, tmp_dir};
    use url::Url;
    use version_ranges::Ranges;
    use zip::write::SimpleFileOptions;
    use zip::{CompressionMethod, ZipWriter};

    use crate::pex::ResolvedWheel;
    use crate::wheel::WheelFile;
    use crate::{Pex, PexPath};

    const EXPECTED_ANSICOLORS_PEX_WHEELS: [&str; 1] = ["ansicolors==1.1.8"];

    #[fixture]
    fn ansicolors_pex(tmp_dir: PathBuf, python_exe: &Path) -> PathBuf {
        let pex = tmp_dir.join("ansicolors.pex");
        assert!(
            Command::new("uvx")
                .arg("--python")
                .arg(python_exe)
                .args(["pex", "ansicolors==1.1.8", "-o"])
                .arg(&pex)
                .spawn()
                .unwrap()
                .wait()
                .unwrap()
                .success()
        );
        pex
    }

    const EXPECTED_REQUESTS_PEX_WHEELS: [&str; 6] = [
        "requests[socks]==2.32.5",
        "charset_normalizer<4,>=2",
        "idna<4,>=2.5",
        "urllib3<3,>=1.21.1",
        "certifi>=2017.4.17",
        "PySocks!=1.5.7,>=1.5.6; extra == \"socks\"",
    ];

    #[fixture]
    fn requests_pex(
        tmp_dir: PathBuf,
        python_exe: &Path,
        ansicolors_pex: PathBuf,
        mut embedded_scripts: Scripts,
    ) -> PathBuf {
        let pex = tmp_dir.join("requests.pex");
        assert!(
            Command::new("uvx")
                .arg("--python")
                .arg(python_exe)
                .args(["pex", "requests[socks]==2.32.5"])
                .arg("--pex-path")
                .arg(ansicolors_pex)
                .arg("-o")
                .arg(&pex)
                .spawn()
                .unwrap()
                .wait()
                .unwrap()
                .success()
        );

        let mut zip =
            ZipWriter::new_append(File::options().read(true).write(true).open(&pex).unwrap())
                .unwrap();
        let file_options =
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
        embedded_scripts.inject(&mut zip, file_options).unwrap();
        zip.finish().unwrap();

        pex
    }

    fn assert_wheels(
        wheels: IndexMap<&str, ResolvedWheel>,
        expected_requirements: impl IntoIterator<Item = &'static str>,
    ) {
        let resolved = wheels
            .keys()
            .map(|file_name| {
                WheelFile::parse_file_name(file_name)
                    .map(|wheel_file| (wheel_file.project_name, wheel_file.version))
            })
            .collect::<Result<IndexSet<_>, _>>()
            .unwrap();
        let expected_resolve = expected_requirements
            .into_iter()
            .map(|req| Requirement::from_str(req).unwrap())
            .collect::<Vec<Requirement<Url>>>();
        for (expected_requirement, (project_name, version)) in
            itertools::zip_eq(expected_resolve, resolved)
        {
            assert_eq!(expected_requirement.name, project_name);
            let version_specifier = match expected_requirement.version_or_url {
                Some(VersionOrUrl::VersionSpecifier(version_specifier)) => version_specifier,
                _ => panic!("Expected all requirements have version specifiers."),
            };
            assert!(version_specifier.contains(&version));
        }
    }

    #[rstest]
    fn test_resolve_single(
        requests_pex: PathBuf,
        python_exe: &Path,
        interpreter_identification_script: IdentifyInterpreter,
    ) {
        let pex = Pex::load(&requests_pex).unwrap();
        let interpreter =
            Interpreter::load(python_exe, &interpreter_identification_script).unwrap();
        let dependency_configuration = pex.dependency_configuration().unwrap();
        let wheels = pex
            .resolve_wheels(&interpreter, &dependency_configuration, None)
            .unwrap();
        assert_wheels(wheels, EXPECTED_REQUESTS_PEX_WHEELS);
    }

    #[rstest]
    fn test_resolve_additional(requests_pex: PathBuf, python_exe: &Path) {
        let pex = Pex::load(&requests_pex).unwrap();
        let pex_path = PexPath::from_pex_info(&pex.info, false);
        let additional_pexes = pex_path.load_pexes().unwrap();
        let search_path = SearchPath::known(indexset![python_exe.to_path_buf()]);
        let resolve = pex
            .resolve(Some(python_exe), additional_pexes.iter(), search_path, None)
            .unwrap();

        assert_wheels(resolve.wheels, EXPECTED_REQUESTS_PEX_WHEELS);

        assert_eq!(1, resolve.additional_wheels.len());
        let (_, additional_wheels) = resolve.additional_wheels.into_iter().next().unwrap();
        assert_wheels(additional_wheels, EXPECTED_ANSICOLORS_PEX_WHEELS);
    }

    #[test]
    fn test_ranges() {
        let range1 = Ranges::from(VersionSpecifiers::from_str(">=3.9,<3.15").unwrap());
        let range2 = Ranges::from(VersionSpecifiers::from_str(">=3.10,<3.16").unwrap());
        let range3 = Ranges::from(VersionSpecifiers::from_str(">=3.10,<3.14").unwrap());
        assert!(range3.subset_of(&range1));
        assert!(range3.subset_of(&range2));
        assert!(!range1.subset_of(&range2));
        assert!(!range2.subset_of(&range1));
    }
}
