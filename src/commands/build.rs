// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::ffi::OsStr;
use std::fmt::Write as _;
use std::io::{BufReader, Seek, Write as _};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::{io, process};

use anyhow::{anyhow, bail};
use boot::{create_sh_boot_shebang, inject_boot, write_boot};
use cache::{CacheDir, DigestingWriter, Fingerprint, atomic_file};
use clap::{ArgAction, Args};
use const_format::concatcp;
use digest::Digest;
use enumset::enum_set;
use fs_err as fs;
use fs_err::File;
use indexmap::{IndexMap, IndexSet, indexmap, indexset};
use interpreter::Interpreter;
use itertools::Itertools;
use pep508_rs::Requirement;
use pex::{PexInfo, RawPexInfo};
use platform::mark_executable;
use python_platform::{PlatformDetails, PythonImplementation};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use repackage::{WheelOptions, recompress_zipped_whl_to_file};
use resolver::dependency_configuration::DependencyConfiguration;
use resolver::resolve_wheels;
use scripts::{IdentifyInterpreter, Scripts};
use serde_json::json;
use sha2::Sha256;
use target::SimplifiedTarget;
use tempfile::NamedTempFile;
use tracing::{debug_span, instrument, warn};
use url::Url;
use venv::{InstallPaths, InstalledWheel, Virtualenv, collect_installed_wheels};
use wheel::{EntryPoints, MetadataDirs, MetadataReader, WheelFile};
use zip::result::ZipError;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};
use zip_ext::ZipArchiveExt;

use crate::VERSION;
use crate::compression_method::CompressionArgs;
use crate::embeds::{AVAILABLE_TARGETS, Binary, CLIB_BY_TARGET, PROXY_BY_TARGET, PROXYW_BY_TARGET};
use crate::target::{PYTHON_PLATFORM_LONG_HELP, PythonPlatform, RequiredTargets};

enum InstalledDistributions {
    Venvs(Vec<PathBuf>),
    Wheels(Vec<PathBuf>),
}

enum PexEntryPoint {
    EntryPoint(String),
    Script(String),
}

#[derive(Args, Debug)]
#[group(skip)]
pub struct Build {
    /// Requirements to include in the PEX.
    ///
    /// If no requirements are specified, an empty hermetic PEX will be generated.
    #[arg(
        value_name = "REQUIREMENT",
        help_heading = "Contents",
        verbatim_doc_comment
    )]
    requirements: Vec<Requirement<Url>>,

    /// Venvs containing distributions to include in the PEX.
    ///
    /// There must be at least one installed distribution satisfying each direct requirement. If
    /// no targets are specified, the interpreter for each specified venv is considered a target. A
    /// full transitive closure is confirmed for each target.
    #[arg(
            long,
            visible_alias = "venv",
            value_name = "PATH",
            action = ArgAction::Append,
            help_heading = "Contents",
            conflicts_with = "wheels",
            verbatim_doc_comment
    )]
    venvs: Vec<PathBuf>,

    /// Wheels (or directories containing wheels) to include in the PEX.
    ///
    /// There must be at least one wheel satisfying each direct requirement. If no targets are
    /// specified, that is the only check performed; otherwise a full transitive closure is
    /// confirmed for each specified target.
    #[arg(
        long,
        visible_alias = "wheel",
        value_name = "PATH",
        action = ArgAction::Append,
        help_heading = "Contents",
        conflicts_with = "venvs",
        verbatim_doc_comment
    )]
    wheels: Vec<PathBuf>,

    /// Existing PEX-INFO to use for the built PEX.
    ///
    /// If the PEX-INFO is from a traditional PEX it may be edited minimally to conform to the PEXrc
    /// runtime and any specified requirements. If no PEX-INFO is supplied, it will be created from
    /// the other given inputs.
    #[arg(long, help_heading = "Contents", verbatim_doc_comment)]
    pex_info: Option<PathBuf>,

    /// Set the entry point to `module` or `module:symbol`.
    ///
    /// If just specifying `module`, Pex behaves like `python -m`, e.g. `python -m http.server`.
    /// If specifying `module:symbol`, Pex assumes symbol is a 0-arg callable and imports that
    /// symbol and invokes it as if via `sys.exit(symbol())`.
    #[arg(
        short = 'e',
        visible_short_alias = 'm',
        long,
        help_heading = "Entry Point",
        conflicts_with = "script",
        verbatim_doc_comment
    )]
    entry_point: Option<String>,

    /// Set the entry point to the given script.
    ///
    /// The script must be either a console script, gui script or data script found in one of the
    /// distributions in the PEX. For example: `pexrc build -c cowsay --venv venv/ cowsay`.
    #[arg(
        short = 'c',
        long,
        visible_alias = "console-script",
        help_heading = "Entry Point",
        conflicts_with = "entry_point",
        verbatim_doc_comment
    )]
    script: Option<String>,

    /// The Python platforms the built PEX will target at runtime.
    ///
    /// If specified, the targets will be used to resolve any specified requirements from the
    /// configured wheels. If required wheels are not present, the build will error.
    #[arg(
        long = "target",
        action = ArgAction::Append,
        help_heading = "Targets",
        value_parser = PythonPlatform::parse,
        long_help=PYTHON_PLATFORM_LONG_HELP,
        verbatim_doc_comment
    )]
    targets: Vec<PythonPlatform>,

    #[command(flatten)]
    compression_args: CompressionArgs,

    /// Instead of building a zipapp PEX, build a packed PEX.
    ///
    /// A Packed PEX is a directory containing a top-level `pex` script / `__main__.py` with wheels
    /// and other needed assets as-is under that. This can be useful in situations where using
    /// rsync-style transfer to ship incremental updates to large PEXes as opposed to having to ship
    /// the whole PEX.
    #[arg(
        long,
        help_heading = "Layout",
        default_value_t = false,
        verbatim_doc_comment
    )]
    packed: bool,

    /// Instead of booting via a Python shebang, boot via a Posix `sh` shebang.
    ///
    /// When running the PEX file directly (on Unix), instead of using a `#!/usr/bin/env python`
    /// style shebang, use a specially crafted `#!/bin/sh ...` shebang header that performs initial
    /// boot interpreter discovery smartly. If your PEX will target systems with a Posix shell at
    /// `/bin/sh` (overwhelmingly common on unix systems), this is the most robust and
    /// lowest-latency boot mode for repeated runs (at ~O(1ms)).
    ///
    /// N.B.: Both the Python and `sh` shebang headers are safe, but ignored on Windows systems.
    /// For those, you must run the PEX via Python (`python PEX`, `py PEX`, etc.) or else use an
    /// extension scheme you register with windows (Setting up a `.pyz` association is common).
    #[arg(
        long,
        help_heading = "Boot Mode",
        default_value_t = false,
        verbatim_doc_comment
    )]
    sh_boot: bool,

    /// The name of the generated PEX file.
    ///
    /// Omitting this will run PEX immediately and not save it to a file.
    ///
    /// If the name contains the {platform} placeholder, the most-specific platform tags supported
    /// by the PEX will be substituted. For example, for a multi-platform Linux x86-64, Mac ARM PEX
    /// containing platform-specific wheels, `-o 'example-{platform}.pex'` might expand to a PEX
    /// filename of `example-cp314-cp314-macosx_11_0_arm64.manylinux2014_x86_64.pex`.
    #[arg(
        short = 'o',
        long,
        visible_alias = "output-file",
        help_heading = "Output",
        verbatim_doc_comment
    )]
    output: Option<PathBuf>,

    /// Pass through arguments for execution of ephemeral PEXes.
    #[arg(allow_hyphen_values = true, last = true)]
    extra_args: Vec<String>,
}

impl Build {
    pub fn execute(self) -> anyhow::Result<()> {
        if !self.extra_args.is_empty()
            && let Some(output) = self.output.as_deref()
        {
            bail!(
                "Extra args ({extra_args}) are only applicable for ephemeral PEXes.\n\
                This PEX would be generated to {path}.",
                extra_args = shlex::try_join(self.extra_args.iter().map(String::as_str))?,
                path = output.display()
            );
        }
        assert!(
            self.venvs.is_empty() || self.wheels.is_empty(),
            "We should never get here by arrangement of a mutex condition between venvs and wheels \
            via clap `conflicts_with`."
        );
        let installed_distributions = if !self.venvs.is_empty() {
            Some(InstalledDistributions::Venvs(self.venvs))
        } else if !self.wheels.is_empty() {
            Some(InstalledDistributions::Wheels(self.wheels))
        } else {
            None
        };

        let entry_point = self
            .entry_point
            .map(PexEntryPoint::EntryPoint)
            .or_else(|| self.script.map(PexEntryPoint::Script));
        let wheel_options = self.compression_args.into_wheel_options(None);

        if let Some(pex_info) = self.pex_info {
            let pex_info_file = File::open(&pex_info)?;
            let size = pex_info_file.metadata()?.len();
            let mut pex_info = PexInfo::parse(
                BufReader::new(pex_info_file),
                size,
                Some(|| Cow::Owned(pex_info.display().to_string())),
            )?;
            pex_info.with_raw_mut(|pi| pi.build_properties.insert("pexrc_version", json!(VERSION)));
            let requirements = if self.requirements.is_empty() {
                pex_info
                    .raw()
                    .requirements
                    .iter()
                    .map(|requirement| Ok(requirement.parse::<Requirement<Url>>()?))
                    .collect::<anyhow::Result<Vec<_>>>()?
            } else {
                pex_info.with_raw_mut(|pi| {
                    pi.requirements = self
                        .requirements
                        .iter()
                        .map(ToString::to_string)
                        .map(Cow::Owned)
                        .collect()
                });
                self.requirements
            };
            let (pythons, wheels) = resolve_wheel_files(
                self.targets,
                &wheel_options,
                installed_distributions,
                requirements,
                pex_info.raw(),
            )?;
            pex_info.with_raw_mut(|raw_pex_info| {
                adjust_requirements(raw_pex_info, &wheels)?;
                if let Some(entry_point) = entry_point {
                    resolve_entry_point(raw_pex_info, entry_point, &wheels)?;
                }
                build_pex(
                    pythons,
                    wheels,
                    wheel_options,
                    raw_pex_info,
                    self.packed,
                    self.sh_boot,
                    self.output,
                    self.extra_args,
                )
            })
        } else {
            let mut pex_info = RawPexInfo {
                build_properties: indexmap! {
                    "pex_version" => json!(concatcp!("rc ", VERSION)),
                    "pexrc_version" => json!(VERSION),
                },
                requirements: self
                    .requirements
                    .iter()
                    .map(ToString::to_string)
                    .map(Cow::Owned)
                    .collect(),
                ..Default::default()
            };
            let (pythons, wheels) = resolve_wheel_files(
                self.targets,
                &wheel_options,
                installed_distributions,
                self.requirements,
                &pex_info,
            )?;
            adjust_requirements(&mut pex_info, &wheels)?;
            if let Some(entry_point) = entry_point {
                resolve_entry_point(&mut pex_info, entry_point, &wheels)?;
            }
            build_pex(
                pythons,
                wheels,
                wheel_options,
                &mut pex_info,
                self.packed,
                self.sh_boot,
                self.output,
                self.extra_args,
            )
        }
    }
}

#[instrument(level = "debug", skip_all)]
fn resolve_entry_point(
    pex_info: &mut RawPexInfo,
    entry_point: PexEntryPoint,
    wheels: &[FingerprintedWheel],
) -> anyhow::Result<()> {
    match entry_point {
        PexEntryPoint::EntryPoint(ep) => pex_info.entry_point = Some(Cow::Owned(ep)),
        PexEntryPoint::Script(script) => {
            let matches = wheels
                .into_par_iter()
                .map(|wheel| {
                    let wheel_file = wheel
                        .path
                        .file_name()
                        .and_then(OsStr::to_str)
                        .ok_or_else(|| anyhow!("XXX"))
                        .and_then(WheelFile::parse_file_name)?;
                    let mut whl = ZipArchive::new(File::open(&wheel.path)?)?;
                    let metadata_dirs = MetadataDirs::locate_in_zip(
                        &whl,
                        "",
                        None,
                        &wheel_file.project_name,
                        &wheel_file.version,
                    )?;
                    match whl.by_name(&format!(
                        "{dist_info_dir}/entry_points.txt",
                        dist_info_dir = metadata_dirs.dist_info_dir()
                    )) {
                        Ok(file) => {
                            let entry_points = EntryPoints::load(file)?;
                            if let Some(entry_point) = entry_points.script(&script) {
                                Ok(Some((wheel, entry_point.to_string())))
                            } else {
                                Ok(None)
                            }
                        }
                        Err(ZipError::FileNotFound) => Ok(None),
                        Err(err) => Err(anyhow!("{err}")),
                    }
                })
                .collect::<anyhow::Result<Vec<_>>>()?
                .into_iter()
                .flatten()
                .collect::<Vec<_>>();
            if matches.is_empty() {
                pex_info.script = Some(Cow::Owned(script))
            } else {
                let mut entry_points = IndexMap::with_capacity(matches.len());
                for (wheel, entry_point) in matches {
                    entry_points
                        .entry(entry_point)
                        .or_insert_with(IndexSet::new)
                        .insert(&wheel.path);
                }
                if entry_points.len() > 1 {
                    let mut msg = format!(
                        "Found {count} conflicting entry point definitions for script {script}:\n",
                        count = entry_points.len()
                    );
                    for (index, (entry_point, wheel_paths)) in entry_points.iter().enumerate() {
                        writeln!(&mut msg, "{index}. {entry_point}:")?;
                        for wheel_path in wheel_paths {
                            write!(
                                &mut msg,
                                "   {wheel}",
                                wheel = wheel_path
                                    .file_name()
                                    .expect("We already parsed a wheel file name to get here.")
                                    .display()
                            )?;
                        }
                    }
                    bail!(msg)
                }
                let (entry_point, _) = entry_points
                    .into_iter()
                    .next()
                    .expect("We ensured there was element with the checks above.");
                pex_info.entry_point = Some(Cow::Owned(entry_point))
            }
        }
    }
    Ok(())
}

fn adjust_requirements(
    pex_info: &mut RawPexInfo,
    wheels: &[FingerprintedWheel],
) -> anyhow::Result<()> {
    if pex_info.requirements.is_empty() {
        pex_info.requirements.extend(
            wheels
                .iter()
                .map(|wheel| {
                    wheel
                        .path
                        .file_name()
                        .and_then(OsStr::to_str)
                        .ok_or_else(|| anyhow!("XXX"))
                        .and_then(WheelFile::parse_file_name)
                        .map(|wheel_file| Cow::Owned(wheel_file.project_name.to_string()))
                })
                .collect::<anyhow::Result<IndexSet<_>>>()?,
        );
    }
    Ok(())
}

#[instrument(level = "debug", skip_all)]
fn resolve_wheel_files(
    targets: Vec<PythonPlatform>,
    wheel_options: &WheelOptions,
    installed_distributions: Option<InstalledDistributions>,
    requirements: Vec<Requirement<Url>>,
    pex_info: &RawPexInfo,
) -> anyhow::Result<(IndexSet<PythonImplementation>, Vec<FingerprintedWheel>)> {
    if let Some(distributions) = installed_distributions {
        match distributions {
            InstalledDistributions::Venvs(venvs) => {
                let (pythons, wheels) = resolve_wheels_from_venvs(
                    targets,
                    wheel_options,
                    requirements,
                    venvs
                        .into_par_iter()
                        .map(|path| Virtualenv::load(Cow::Owned(path), &mut Scripts::Embedded))
                        .collect::<anyhow::Result<Vec<_>>>()?,
                    pex_info,
                )?;
                Ok((pythons, wheels))
            }
            InstalledDistributions::Wheels(installed_wheels) => {
                let mut wheels = Vec::with_capacity(installed_wheels.len());
                for wheel in installed_wheels {
                    if wheel.is_dir() {
                        for entry in wheel.read_dir()? {
                            let entry = entry?;
                            if entry.file_type()?.is_file()
                                && entry.file_name().as_encoded_bytes().ends_with(b".whl")
                            {
                                wheels.push(entry.path())
                            }
                        }
                    } else {
                        wheels.push(wheel)
                    }
                }
                let (pythons, wheels) =
                    resolve_wheels_from_files(targets, requirements, wheels, pex_info)?;
                let fingerprinted_wheels = wheels
                    .into_par_iter()
                    .map(|wheel| cache_wheel(&wheel, wheel_options))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                Ok((pythons, fingerprinted_wheels))
            }
        }
    } else if requirements.is_empty() {
        Ok((indexset![], vec![]))
    } else {
        bail!("Cannot resolve requirements without either `--wheels` or `--venv`.")
    }
}

#[derive(Hash, Eq, PartialEq)]
enum Platform<'a> {
    Details(PlatformDetails<'a>),
    Interpreter(Cow<'a, Interpreter>),
}

struct VenvRepository(PathBuf);

impl MetadataReader for VenvRepository {
    fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs> {
        MetadataDirs::locate_in_dir(&self.0, &wheel_file.project_name, &wheel_file.version)
    }

    fn read(
        &mut self,
        metadata_dirs: &MetadataDirs,
        _wheel_file: &WheelFile,
        file_name: &str,
    ) -> anyhow::Result<String> {
        let mut metadata_file_path = self.0.join(metadata_dirs.dist_info_dir().as_path());
        metadata_file_path.push(file_name);
        Ok(fs::read_to_string(metadata_file_path)?)
    }
}

fn resolve_wheels_from_venvs(
    targets: Vec<PythonPlatform>,
    wheel_options: &WheelOptions,
    requirements: Vec<Requirement<Url>>,
    venvs: Vec<Virtualenv>,
    pex_info: &RawPexInfo,
) -> anyhow::Result<(IndexSet<PythonImplementation>, Vec<FingerprintedWheel>)> {
    let (venvs, mut repositories) = {
        let inventory = venvs
            .into_par_iter()
            .map(inventory_venv)
            .collect::<anyhow::Result<Vec<_>>>()?;
        let mut venvs = IndexMap::with_capacity(inventory.len());
        let mut repositories = Vec::with_capacity(inventory.len());
        for (venv, repository) in inventory {
            venvs.insert(venv.prefix().join(&venv.site_packages_relpath), venv);
            repositories.push(repository);
        }
        (venvs, repositories)
    };

    let platforms = if targets.is_empty() {
        venvs
            .values()
            .map(|venv| Platform::Interpreter(Cow::Borrowed(&venv.interpreter)))
            .collect()
    } else {
        targets
            .iter()
            .map(|target| match target {
                PythonPlatform::Spec(spec) => python_platform::parse(spec, None, None)
                    .map_err(|err| {
                        anyhow!(
                            "Failed to parse --target {spec}: {err}\n\
                            {PYTHON_PLATFORM_LONG_HELP}"
                        )
                    })
                    .map(Platform::Details),
                PythonPlatform::Interpreter(path) => {
                    IdentifyInterpreter::read(&mut Scripts::Embedded)
                        .and_then(|identification_script| {
                            Interpreter::load(path, &identification_script)
                        })
                        .map(|interpreter| Platform::Interpreter(Cow::Owned(interpreter)))
                }
            })
            .collect::<anyhow::Result<Vec<_>>>()?
    };

    let dependency_configuration = DependencyConfiguration::parse(
        pex_info.excluded.as_slice(),
        pex_info.overridden.as_slice(),
    )?;

    let mut wheels = IndexMap::new();
    let mut pythons = IndexSet::new();
    let mut errors_by_platform = IndexMap::new();
    for (installed_wheels, venv_repository) in &mut repositories {
        for platform in &platforms {
            if wheels.contains_key(platform) {
                continue;
            }
            let mut requirements = requirements.clone();
            let wheel_files = installed_wheels
                .keys()
                .map(|file_name| WheelFile::parse_file_name(file_name))
                .collect::<anyhow::Result<Vec<_>>>()?;
            if requirements.is_empty() {
                requirements.extend(
                    wheel_files
                        .iter()
                        .map(|wheel_file| Ok(Requirement::from_str(wheel_file.raw_project_name)?))
                        .collect::<anyhow::Result<IndexSet<_>>>()?,
                )
            }
            let result = match platform {
                Platform::Details(platform) => resolve_wheels(
                    platform,
                    requirements,
                    wheel_files,
                    venv_repository,
                    &dependency_configuration,
                    None,
                    pex_info.ignore_errors,
                ),
                Platform::Interpreter(interpreter) => resolve_wheels(
                    interpreter.as_ref(),
                    requirements,
                    wheel_files,
                    venv_repository,
                    &dependency_configuration,
                    None,
                    pex_info.ignore_errors,
                ),
            };
            match result {
                Ok(resolved_wheels) => {
                    for installed_wheel in resolved_wheels
                        .keys()
                        .map(|file_name| installed_wheels.get(*file_name))
                    {
                        wheels
                            .entry(platform)
                            .or_insert_with(|| {
                                (
                                    venvs.get(&venv_repository.0).expect(
                                        "All venvs are indexed by their site-packages path.",
                                    ),
                                    IndexSet::new(),
                                )
                            })
                            .1
                            .insert(installed_wheel.expect(
                                "Resolved wheel_files were derived from installed_wheels.",
                            ));
                        pythons.insert(match platform {
                            Platform::Details(details) => details.python_implementation()?,
                            Platform::Interpreter(interpreter) => {
                                interpreter.details.python_implementation()
                            }
                        });
                    }
                    break;
                }
                Err(err) => errors_by_platform
                    .entry(platform)
                    .or_insert_with(Vec::new)
                    .push(err),
            }
        }
    }
    if !errors_by_platform.is_empty() {
        // TODO: XXX: Better error message.
        bail!(
            "Failed to resolve wheels for {count} platforms:\n{errs}",
            count = errors_by_platform.len(),
            errs = errors_by_platform.values().flatten().join("\n")
        )
    }

    let mut wheel_paths = vec![];
    for (_, (venv, installed_wheels)) in wheels {
        let install_paths = InstallPaths::for_venv(venv)?;
        wheel_paths.append(
            &mut installed_wheels
                .into_par_iter()
                .map(|installed_wheel| pack_wheel(installed_wheel, &install_paths, wheel_options))
                .collect::<anyhow::Result<Vec<_>>>()?,
        );
    }
    Ok((pythons, wheel_paths))
}

fn wheel_path(file_name: &str, wheel_options: &WheelOptions) -> anyhow::Result<PathBuf> {
    let options_dir_name = match (
        wheel_options.compression_method,
        wheel_options.compression_level,
    ) {
        (CompressionMethod::Stored, None) => Cow::Borrowed("stored-default"),
        (CompressionMethod::Deflated, None) => Cow::Borrowed("deflated-default"),
        (CompressionMethod::Zstd, None) => Cow::Borrowed("zstd-default"),
        (CompressionMethod::Stored, Some(level)) => Cow::Owned(format!("stored-{level}")),
        (CompressionMethod::Deflated, Some(level)) => Cow::Owned(format!("deflated-{level}")),
        (CompressionMethod::Zstd, Some(level)) => Cow::Owned(format!("zstd-{level}")),
        _ => bail!(
            "Unsupported compression method {method:?}",
            method = wheel_options.compression_method
        ),
    };
    let mut whl_path = CacheDir::Wheel.path()?.join(options_dir_name.as_ref());
    whl_path.push(file_name);
    Ok(whl_path)
}

struct FingerprintedWheel {
    path: PathBuf,
    file_name: String,
    fingerprint: Fingerprint,
}

type WheelDigestAlgorithm = Sha256;
static ALGORITHM_NAME: &str = "sha256";

fn cache_wheel(wheel: &Path, wheel_options: &WheelOptions) -> anyhow::Result<FingerprintedWheel> {
    let time_cache = debug_span!("cache_wheel", wheel=%wheel.display());
    let _time_cache = time_cache.enter();
    let wheel_file = WheelFile::parse_file_name(
        wheel
            .file_name()
            .and_then(OsStr::to_str)
            .ok_or_else(|| anyhow!("XXX"))?,
    )?;
    let wheel_path = wheel_path(wheel_file.file_name, wheel_options)?;
    let fingerprint_path = wheel_path.with_added_extension(ALGORITHM_NAME);
    if let Some(fingerprint) = atomic_file(&wheel_path, |whl_file| {
        let whl_zip = ZipArchive::new(File::open(wheel)?)?;
        let mut dest = DigestingWriter::new(WheelDigestAlgorithm::new(), whl_file);
        recompress_zipped_whl_to_file(whl_zip, &mut dest, wheel_options)?;
        let (mut whl_file, fingerprint, _) = dest.into_parts();
        whl_file.rewind()?;
        let mut fingerprint_file = tempfile::Builder::new()
            .prefix(wheel_file.file_name)
            .suffix(ALGORITHM_NAME)
            .tempfile_in(
                fingerprint_path
                    .parent()
                    .expect("The wheel and fingerprint are stored in a cache directory."),
            )?;
        fingerprint.store(&mut fingerprint_file)?;
        fingerprint_file.persist(&fingerprint_path)?;
        Ok(fingerprint)
    })? {
        Ok(FingerprintedWheel {
            path: wheel_path,
            file_name: wheel_file.file_name.to_string(),
            fingerprint,
        })
    } else {
        let mut fingerprint_file = File::open(fingerprint_path)?;
        let fingerprint =
            Fingerprint::load(&mut fingerprint_file, WheelDigestAlgorithm::output_size())?;
        Ok(FingerprintedWheel {
            path: wheel_path,
            file_name: wheel_file.file_name.to_string(),
            fingerprint,
        })
    }
}

fn pack_wheel(
    wheel: &InstalledWheel,
    install_paths: &InstallPaths,
    wheel_options: &WheelOptions,
) -> anyhow::Result<FingerprintedWheel> {
    let wheel_file = wheel.file_name()?;
    let time_pack = debug_span!("pack_wheel", wheel_file = wheel_file);
    let _time_pack = time_pack.enter();
    let wheel_path = wheel_path(&wheel_file, wheel_options)?;
    let fingerprint_path = wheel_path.with_added_extension(ALGORITHM_NAME);
    if let Some(fingerprint) = atomic_file(&wheel_path, |whl_file| {
        let mut digester = DigestingWriter::new(WheelDigestAlgorithm::new(), whl_file);
        wheel.pack(install_paths, wheel_options, &mut digester)?;
        let fingerprint = digester.into_fingerprint();
        let mut fingerprint_file = tempfile::Builder::new()
            .prefix(&wheel_file)
            .suffix(ALGORITHM_NAME)
            .tempfile_in(
                fingerprint_path
                    .parent()
                    .expect("The wheel and fingerprint are stored in a cache directory."),
            )?;
        fingerprint.store(&mut fingerprint_file)?;
        fingerprint_file.persist(&fingerprint_path)?;
        Ok(fingerprint)
    })? {
        Ok(FingerprintedWheel {
            path: wheel_path,
            file_name: wheel_file,
            fingerprint,
        })
    } else {
        let mut fingerprint_file = File::open(fingerprint_path)?;
        let fingerprint =
            Fingerprint::load(&mut fingerprint_file, WheelDigestAlgorithm::output_size())?;
        Ok(FingerprintedWheel {
            path: wheel_path,
            file_name: wheel_file,
            fingerprint,
        })
    }
}

type Repository = (IndexMap<String, InstalledWheel>, VenvRepository);

fn inventory_venv(venv: Virtualenv) -> anyhow::Result<(Virtualenv, Repository)> {
    let installed_wheels = collect_installed_wheels(&venv)?
        .into_iter()
        .map(|installed_wheel| {
            installed_wheel
                .file_name()
                .map(|file_name| (file_name, installed_wheel))
        })
        .collect::<anyhow::Result<IndexMap<_, _>>>()?;
    let repository = VenvRepository(
        venv.interpreter
            .details
            .prefix
            .join(venv.site_packages_relpath.as_ref()),
    );
    Ok((venv, (installed_wheels, repository)))
}

fn resolve_wheels_from_files(
    targets: Vec<PythonPlatform>,
    requirements: Vec<Requirement<Url>>,
    wheels: Vec<PathBuf>,
    pex_info: &RawPexInfo,
) -> anyhow::Result<(IndexSet<PythonImplementation>, Vec<PathBuf>)> {
    if targets.is_empty() || requirements.is_empty() {
        return Ok((indexset!(), wheels));
    }

    let dependency_configuration = DependencyConfiguration::parse(
        pex_info.excluded.as_slice(),
        pex_info.overridden.as_slice(),
    )?;

    let file_names = file_names(wheels.iter().map(AsRef::as_ref))?
        .into_iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>();
    let mut wheel_paths_by_file_name: IndexMap<&str, PathBuf> =
        IndexMap::with_capacity(wheels.len());
    for (file_name, path) in file_names.iter().zip(wheels) {
        wheel_paths_by_file_name.insert(file_name, path);
    }
    let mut wheel_repository = Wheels::new(wheel_paths_by_file_name);
    let mut pythons = IndexSet::new();
    let mut resolved_file_names: IndexSet<&str> = IndexSet::with_capacity(file_names.len());
    for target in &targets {
        let resolved_wheels = match target {
            PythonPlatform::Spec(spec) => {
                let platform = python_platform::parse(spec, None, None).map_err(|err| {
                    anyhow!(
                        "Failed to parse --target {spec}: {err}\n\
                        {PYTHON_PLATFORM_LONG_HELP}"
                    )
                })?;
                pythons.insert(platform.python_implementation()?);
                let wheel_files = file_names
                    .iter()
                    .map(|file_name| WheelFile::parse_file_name(file_name))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                resolve_wheels(
                    &platform,
                    requirements.clone(),
                    wheel_files,
                    &mut wheel_repository,
                    &dependency_configuration,
                    None,
                    pex_info.ignore_errors,
                )?
            }
            PythonPlatform::Interpreter(path) => {
                let identification_script = IdentifyInterpreter::read(&mut Scripts::Embedded)?;
                let interpreter = Interpreter::load(path, &identification_script)?;
                pythons.insert(interpreter.details.python_implementation());
                let wheel_files = file_names
                    .iter()
                    .map(|file_name| WheelFile::parse_file_name(file_name))
                    .collect::<anyhow::Result<Vec<_>>>()?;
                resolve_wheels(
                    &interpreter,
                    requirements.clone(),
                    wheel_files,
                    &mut wheel_repository,
                    &dependency_configuration,
                    None,
                    pex_info.ignore_errors,
                )?
            }
        };
        resolved_file_names.extend(resolved_wheels.keys());
    }
    let wheel_paths = wheel_repository.select(resolved_file_names.into_iter())?;
    Ok((pythons, wheel_paths))
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "debug", skip_all)]
fn build_pex(
    pythons: IndexSet<PythonImplementation>,
    wheels: Vec<FingerprintedWheel>,
    wheel_options: WheelOptions,
    pex_info: &mut RawPexInfo,
    packed: bool,
    sh_boot: bool,
    output: Option<PathBuf>,
    extra_args: Vec<String>,
) -> anyhow::Result<()> {
    match output {
        Some(path) => {
            let subject = Cow::Owned(format!("PEX at {path}", path = path.display()));
            create_pex(
                subject,
                false,
                pythons,
                wheels,
                wheel_options,
                pex_info,
                packed,
                sh_boot,
                &path,
            )
        }
        None => {
            let subject = Cow::Borrowed("ephemeral PEX");
            if packed {
                let chroot = tempfile::tempdir()?;
                let path = chroot.path();
                create_pex(
                    subject,
                    true,
                    pythons,
                    wheels,
                    wheel_options,
                    pex_info,
                    packed,
                    sh_boot,
                    path,
                )?;
                execute_pex(path, extra_args)
            } else {
                let pex = NamedTempFile::new()?;
                let path = pex.path();
                create_pex(
                    subject,
                    true,
                    pythons,
                    wheels,
                    wheel_options,
                    pex_info,
                    packed,
                    sh_boot,
                    path,
                )?;
                execute_pex(path, extra_args)
            }
        }
    }
}

fn file_names<'a>(paths: impl ExactSizeIterator<Item = &'a Path>) -> anyhow::Result<Vec<&'a str>> {
    let mut file_names = Vec::with_capacity(paths.len());
    for path in paths {
        let file_name = path
            .file_name()
            .and_then(|file_name| file_name.to_str())
            .ok_or_else(|| {
                anyhow!(
                    "Invalid path {path}: file name is not UTF-8.",
                    path = path.display()
                )
            })?;
        file_names.push(file_name);
    }
    Ok(file_names)
}

struct Wheels<'a> {
    wheel_files: IndexMap<&'a str, PathBuf>,
    wheel_zips: IndexMap<String, ZipArchive<File>>,
}

impl<'a> Wheels<'a> {
    fn new(wheel_files: IndexMap<&'a str, PathBuf>) -> Self {
        let wheel_zips = IndexMap::with_capacity(wheel_files.len());
        Self {
            wheel_files,
            wheel_zips,
        }
    }

    fn select(
        mut self,
        file_names: impl ExactSizeIterator<Item = &'a str>,
    ) -> anyhow::Result<Vec<PathBuf>> {
        let mut paths = Vec::with_capacity(file_names.len());
        for file_name in file_names {
            paths.push(
                self.wheel_files
                    .shift_remove(file_name)
                    .ok_or_else(|| anyhow!("XXX"))?,
            )
        }
        Ok(paths)
    }
}

impl<'a> MetadataReader for Wheels<'a> {
    fn locate_dirs(&mut self, wheel_file: &WheelFile) -> anyhow::Result<MetadataDirs> {
        if let Some(path) = self.wheel_files.get(wheel_file.file_name) {
            let wheel_zip = ZipArchive::new(File::open(path)?)?;
            let metadata_dirs = MetadataDirs::locate_in_zip(
                &wheel_zip,
                path.display(),
                None,
                &wheel_file.project_name,
                &wheel_file.version,
            )?;
            self.wheel_zips
                .insert(wheel_file.file_name.to_string(), wheel_zip);
            Ok(metadata_dirs)
        } else {
            bail!("XXX")
        }
    }

    fn read(
        &mut self,
        metadata_dirs: &MetadataDirs,
        wheel_file: &WheelFile,
        file_name: &str,
    ) -> anyhow::Result<String> {
        let zip = self
            .wheel_zips
            .get_mut(wheel_file.file_name)
            .ok_or_else(|| anyhow!("XXX"))?;
        let dist_info_dir = metadata_dirs.dist_info_dir();
        Ok(io::read_to_string(
            zip.by_name_ex(&format!("{dist_info_dir}/{file_name}"))?,
        )?)
    }
}

#[allow(clippy::too_many_arguments)]
fn create_pex(
    subject: Cow<'_, str>,
    ephemeral: bool,
    pythons: IndexSet<PythonImplementation>,
    wheels: Vec<FingerprintedWheel>,
    wheel_options: WheelOptions,
    pex_info: &mut RawPexInfo,
    packed: bool,
    sh_boot: bool,
    path: &Path,
) -> anyhow::Result<()> {
    let wheel_files = file_names(wheels.iter().map(|wheel| wheel.path.as_path()))?
        .into_iter()
        .map(WheelFile::parse_file_name)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let required_targets = RequiredTargets::for_wheel_files(subject, wheel_files.iter())?;
    let mut targets = required_targets.unique_targets();
    if targets.is_empty() {
        targets |= if ephemeral {
            let current_target = SimplifiedTarget::current()?;
            enum_set!(current_target)
        } else {
            let all_targets = SimplifiedTarget::all();
            if *AVAILABLE_TARGETS != all_targets {
                warn!(
                    "The {subject} has no platform specific wheels but this pexrc binary only has support \
                    for the following platforms:\n\
                    {available_targets}\n\
                    \n\
                    The {subject} will not run on the following platforms:\n\
                    {missing_targets}\n\
                    \n\
                    If the {subject} needs to run on the missing platforms, use a pexrc binary built with \
                    support for all platforms.\n\
                    One place to find these is in the official releases here:\n\
                    https://github.com/pex-tool/pex.rc/releases/tag/v{VERSION}",
                    subject = required_targets.subject,
                    available_targets = *AVAILABLE_TARGETS,
                    missing_targets = all_targets - *AVAILABLE_TARGETS
                );
            }
            *AVAILABLE_TARGETS
        }
    }

    let clibs = targets
        .iter()
        .map(|target| {
            CLIB_BY_TARGET
                .get(&target)
                .expect("The allowed --target values are all keys in CLIB_BY_TARGET.")
        })
        .collect::<Vec<_>>();
    let proxies = targets
        .iter()
        .map(|target| {
            PROXY_BY_TARGET
                .get(&target)
                .expect("The allowed --target values are all keys in PROXY_BY_TARGET.")
        })
        .chain(
            targets
                .iter()
                .filter_map(|target| PROXYW_BY_TARGET.get(&target)),
        )
        .collect::<Vec<_>>();
    if packed {
        create_packed_pex(pythons, wheels, pex_info, clibs, proxies, sh_boot, path)
    } else {
        create_zipapp(
            pythons,
            wheels,
            wheel_options,
            pex_info,
            clibs,
            proxies,
            sh_boot,
            path,
        )
    }
}

#[allow(clippy::too_many_arguments)]
fn create_packed_pex(
    pythons: IndexSet<PythonImplementation>,
    wheels: Vec<FingerprintedWheel>,
    pex_info: &mut RawPexInfo,
    clibs: Vec<&Binary>,
    proxies: Vec<&Binary>,
    sh_boot: bool,
    path: &Path,
) -> anyhow::Result<()> {
    let mut dest_dir = if let Some(parent_dir) = path.parent() {
        tempfile::tempdir_in(parent_dir)
    } else {
        tempfile::tempdir()
    }?;

    let preferred_python = if pythons.len() == 1 {
        pythons.into_iter().next()
    } else {
        None
    };

    // TODO: XXX: shebang option.
    let shebang = if sh_boot {
        // TODO: XXX hermetic option.
        let hermetic = true;
        Cow::Owned(create_sh_boot_shebang(
            "<subject>",
            pex_info,
            hermetic,
            preferred_python,
        )?)
    } else if let Some(preferred_python) = preferred_python {
        Cow::Owned(match preferred_python {
            PythonImplementation::CPython(python) => format!(
                "#!/usr/bin/env python{major}.{minor}\n",
                major = python.major,
                minor = python.minor
            ),
            PythonImplementation::PyPy(pypy) => format!(
                "#!/usr/bin/env pypy{major}.{minor}\n",
                major = pypy.major,
                minor = pypy.minor
            ),
        })
    } else {
        Cow::Borrowed("#!/usr/bin/env python\n")
    };

    let deps_dir = dest_dir.path().join(".deps");
    fs::create_dir(&deps_dir)?;
    for wheel in wheels {
        platform::link_or_copy(wheel.path, deps_dir.join(&wheel.file_name))?;
        pex_info.distributions.insert(
            Cow::Owned(wheel.file_name),
            Cow::Owned(wheel.fingerprint.hex_digest()),
        );
    }
    pex_info.deps_are_wheel_files = true;

    Scripts::Embedded.write(dest_dir.path())?;

    let pex_dir = dest_dir.path().join("__pex__");
    let _deflate_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let clibs_dir = pex_dir.join(".clibs");
    fs::create_dir_all(&clibs_dir)?;
    for clib in clibs {
        clib.embed_in_dir(&clibs_dir, false)?;
    }
    let proxies_dir = pex_dir.join(".proxies");
    fs::create_dir(&proxies_dir)?;
    for proxy in proxies {
        proxy.embed_in_dir(&proxies_dir, true)?;
    }

    pex_info.finalize_pex_hash()?;
    let mut pex_info_fp = File::create_new(dest_dir.path().join("PEX-INFO"))?;
    pex_info.write(&mut pex_info_fp)?;

    write_boot(dest_dir.path(), shebang.as_ref())?;

    if path.is_dir() {
        fs::remove_dir_all(path)?;
    } else if path.is_file() {
        fs::remove_file(path)?;
    }
    fs::rename(&dest_dir, path)?;
    dest_dir.disable_cleanup(true);
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn create_zipapp(
    pythons: IndexSet<PythonImplementation>,
    wheels: Vec<FingerprintedWheel>,
    wheel_options: WheelOptions,
    pex_info: &mut RawPexInfo,
    clibs: Vec<&Binary>,
    proxies: Vec<&Binary>,
    sh_boot: bool,
    path: &Path,
) -> anyhow::Result<()> {
    let mut dst_zip_fp = if let Some(parent_dir) = path.parent() {
        NamedTempFile::new_in(parent_dir)?
    } else {
        NamedTempFile::new()?
    };

    let preferred_python = if pythons.len() == 1 {
        pythons.into_iter().next()
    } else {
        None
    };

    // TODO: XXX: shebang option.
    if sh_boot {
        // TODO: XXX hermetic option.
        let hermetic = true;
        let sh_boot_shebang =
            create_sh_boot_shebang("<subject>", pex_info, hermetic, preferred_python)?;
        dst_zip_fp.write_all(sh_boot_shebang.as_bytes())?;
    } else if let Some(preferred_python) = preferred_python {
        match preferred_python {
            PythonImplementation::CPython(python) => writeln!(
                dst_zip_fp,
                "#!/usr/bin/env python{major}.{minor}",
                major = python.major,
                minor = python.minor
            )?,
            PythonImplementation::PyPy(pypy) => writeln!(
                dst_zip_fp,
                "#!/usr/bin/env pypy{major}.{minor}",
                major = pypy.major,
                minor = pypy.minor
            )?,
        };
    } else {
        dst_zip_fp.write_all(b"#!/usr/bin/env python\n")?;
    }

    let mut dst_zip = ZipWriter::new(&dst_zip_fp);

    let directory_options = SimpleFileOptions::default();
    let file_options = wheel_options.file_options()?;
    let deflated_file_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let stored_file_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    for wheel in wheels {
        dst_zip.start_file(format!(".deps/{}", wheel.file_name), stored_file_options)?;
        let mut src = File::open(wheel.path)?;
        io::copy(&mut src, &mut dst_zip)?;
        pex_info.distributions.insert(
            Cow::Owned(wheel.file_name),
            Cow::Owned(wheel.fingerprint.hex_digest()),
        );
    }
    pex_info.deps_are_wheel_files = true;

    dst_zip.add_directory("__pex__", directory_options)?;
    Scripts::Embedded.inject(&mut dst_zip, file_options)?;

    let deflate_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    dst_zip.add_directory("__pex__/.clibs", directory_options)?;
    for clib in clibs {
        clib.embed_in_zip(&mut dst_zip, "__pex__/.clibs", deflate_options)?;
    }
    dst_zip.add_directory("__pex__/.proxies", directory_options)?;
    for proxy in proxies {
        proxy.embed_in_zip(&mut dst_zip, "__pex__/.proxies", file_options)?;
    }

    pex_info.finalize_pex_hash()?;
    dst_zip.start_file("PEX-INFO", deflated_file_options)?;
    pex_info.write(&mut dst_zip)?;

    inject_boot(&mut dst_zip, deflate_options)?;

    dst_zip.finish()?;
    mark_executable(dst_zip_fp.as_file_mut())?;

    if path.is_dir() {
        fs::remove_dir_all(path)?;
    }
    dst_zip_fp.persist(path)?;

    Ok(())
}

fn execute_pex(pex: &Path, extra_args: Vec<String>) -> anyhow::Result<()> {
    let exit_code = pexrs::boot(None, vec![], pex, extra_args, false)?;
    process::exit(exit_code)
}
