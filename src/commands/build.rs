// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{BufReader, Write};
use std::path::{Path, PathBuf};
use std::{io, process};

use anyhow::{anyhow, bail};
use boot::{create_sh_boot_shebang, inject_boot, write_boot};
use cache::{DigestingReader, default_digest};
use clap::{ArgAction, Args};
use const_format::concatcp;
use fs_err as fs;
use fs_err::File;
use indexmap::{IndexSet, indexmap};
use interpreter::Interpreter;
use log::warn;
use pep508_rs::Requirement;
use pex::{PexInfo, RawPexInfo};
use platform::mark_executable;
use python_platform::PythonImplementation;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use repackage::{WheelOptions, recompress_zipped_whl};
use resolver::dependency_configuration::DependencyConfiguration;
use resolver::resolve_wheels;
use scripts::{IdentifyInterpreter, Scripts};
use serde_json::json;
use target::SimplifiedTarget;
use tempfile::NamedTempFile;
use url::Url;
use wheel::{MetadataDirs, MetadataReader, WheelFile};
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::VERSION;
use crate::compression_method::CompressionArgs;
use crate::embeds::{AVAILABLE_TARGETS, Binary, CLIB_BY_TARGET, PROXY_BY_TARGET, PROXYW_BY_TARGET};
use crate::target::{PYTHON_PLATFORM_LONG_HELP, PythonPlatform, RequiredTargets};

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
        verbatim_doc_comment
    )]
    wheels: Vec<PathBuf>,

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

    /// Existing PEX-INFO to use for the built PEX.
    ///
    /// If the PEX-INFO is from a traditional PEX it may be edited minimally to conform to the PEXrc
    /// runtime and any specified requirements. If no PEX-INFO is supplied, it will be created from
    /// the other given inputs.
    #[arg(long, help_heading = "Contents", verbatim_doc_comment)]
    pex_info: Option<PathBuf>,

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
}

impl Build {
    pub fn execute(self) -> anyhow::Result<()> {
        let wheel_options = self.compression_args.into_wheel_options(None);
        let mut wheels = Vec::with_capacity(self.wheels.len());
        for wheel in self.wheels {
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
            pex_info.with_raw_mut(|raw_pex_info| {
                build_pex(
                    self.targets,
                    requirements,
                    wheels,
                    wheel_options,
                    raw_pex_info,
                    self.packed,
                    self.sh_boot,
                    self.output,
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
            build_pex(
                self.targets,
                self.requirements,
                wheels,
                wheel_options,
                &mut pex_info,
                self.packed,
                self.sh_boot,
                self.output,
            )
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn build_pex(
    targets: Vec<PythonPlatform>,
    requirements: Vec<Requirement<Url>>,
    mut wheels: Vec<PathBuf>,
    wheel_options: WheelOptions,
    pex_info: &mut RawPexInfo,
    packed: bool,
    sh_boot: bool,
    output: Option<PathBuf>,
) -> anyhow::Result<()> {
    if !targets.is_empty() {
        let dependency_configuration = DependencyConfiguration::parse(
            pex_info.excluded.as_slice(),
            pex_info.overridden.as_slice(),
        )?;

        let file_names = file_names(wheels.iter().map(AsRef::as_ref))?
            .into_iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        let wheel_files = || {
            file_names
                .iter()
                .map(|file_name| WheelFile::parse_file_name(file_name))
                .collect::<anyhow::Result<Vec<_>>>()
        };
        let mut wheel_paths_by_file_name: HashMap<&str, PathBuf> =
            HashMap::with_capacity(wheels.len());
        for (file_name, path) in file_names.iter().zip(wheels) {
            wheel_paths_by_file_name.insert(file_name, path);
        }
        let mut wheel_repository = Wheels::new(wheel_paths_by_file_name);
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
        wheels = wheel_repository.select(resolved_file_names.into_iter())?;
    }

    match output {
        Some(path) => {
            let subject = Cow::Owned(format!("PEX at {path}", path = path.display()));
            create_pex(
                subject,
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
                    wheels,
                    wheel_options,
                    pex_info,
                    packed,
                    sh_boot,
                    path,
                )?;
                execute_pex(path)
            } else {
                let pex = NamedTempFile::new()?;
                let path = pex.path();
                create_pex(
                    subject,
                    wheels,
                    wheel_options,
                    pex_info,
                    packed,
                    sh_boot,
                    path,
                )?;
                execute_pex(path)
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
    wheel_files: HashMap<&'a str, PathBuf>,
    wheel_zips: HashMap<String, ZipArchive<File>>,
}

impl<'a> Wheels<'a> {
    fn new(wheel_files: HashMap<&'a str, PathBuf>) -> Self {
        let wheel_zips = HashMap::with_capacity(wheel_files.len());
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
                    .remove(file_name)
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
            zip.by_name(&format!("{dist_info_dir}/{file_name}"))?,
        )?)
    }
}

fn create_pex(
    subject: Cow<'_, str>,
    wheels: Vec<PathBuf>,
    wheel_options: WheelOptions,
    pex_info: &mut RawPexInfo,
    packed: bool,
    sh_boot: bool,
    path: &Path,
) -> anyhow::Result<()> {
    let wheel_files = file_names(wheels.iter().map(AsRef::as_ref))?
        .into_iter()
        .map(WheelFile::parse_file_name)
        .collect::<anyhow::Result<Vec<_>>>()?;
    let required_targets = RequiredTargets::for_wheel_files(subject, wheel_files.iter())?;
    let mut targets = required_targets.unique_targets();
    let all_targets = SimplifiedTarget::all();
    if targets.is_empty() && *AVAILABLE_TARGETS != all_targets {
        targets |= *AVAILABLE_TARGETS;
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
        create_packed_pex(
            wheels,
            wheel_options,
            pex_info,
            clibs,
            proxies,
            sh_boot,
            path,
        )
    } else {
        create_zipapp(
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

fn create_packed_pex(
    wheels: Vec<PathBuf>,
    wheel_options: WheelOptions,
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

    let shebang = if sh_boot {
        // TODO: XXX hermetic option.
        let hermetic = true;
        // TODO: Derive the preferred Python.
        let _preferred_python: Option<PythonImplementation> = None;
        Cow::Owned(create_sh_boot_shebang(
            "<subject>",
            pex_info,
            hermetic,
            false,
            None,
        )?)
    } else {
        // TODO: XXX: shebang option + if not set default selection.
        Cow::Borrowed("#!/usr/bin/env python\n")
    };

    let deps_dir = tempfile::tempdir()?;
    let zips = wheels
        .into_par_iter()
        .map(|wheel| {
            let whl_zip = ZipArchive::new(File::open(&wheel)?)?;
            let whl_file = WheelFile::parse_file_name(
                wheel
                    .file_name()
                    .ok_or_else(|| anyhow!("XXX"))?
                    .to_str()
                    .ok_or_else(|| anyhow!("YYY"))?,
            )?;
            recompress_zipped_whl(whl_zip, &whl_file, &wheel_options, deps_dir.path())
                .map(|file| (whl_file.file_name.to_string(), file))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let deps_dir = dest_dir.path().join(".deps");
    fs::create_dir(&deps_dir)?;
    for (file_name, zip) in zips {
        let mut src = DigestingReader::new(default_digest(), zip);
        let mut dst_zip = File::create_new(deps_dir.join(&file_name))?;
        io::copy(&mut src, &mut dst_zip)?;
        pex_info.distributions.insert(
            Cow::Owned(file_name),
            Cow::Owned(src.into_fingerprint().hex_digest()),
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
    } else {
        fs::remove_file(path)?;
    }
    fs::rename(&dest_dir, path)?;
    dest_dir.disable_cleanup(true);
    Ok(())
}

fn create_zipapp(
    wheels: Vec<PathBuf>,
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
    if sh_boot {
        // TODO: XXX hermetic option.
        let hermetic = true;
        // TODO: Derive the preferred Python.
        let _preferred_python: Option<PythonImplementation> = None;
        let sh_boot_shebang = create_sh_boot_shebang("<subject>", pex_info, hermetic, false, None)?;
        dst_zip_fp.write_all(sh_boot_shebang.as_bytes())?;
    } else {
        // TODO: XXX: shebang option + if not set default selection.
        dst_zip_fp.write_all(b"#!/usr/bin/env python\n")?;
    }
    let mut dst_zip = ZipWriter::new(&dst_zip_fp);

    let directory_options = SimpleFileOptions::default();
    let file_options = wheel_options.file_options()?;
    let deflated_file_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let stored_file_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored);

    // TODO: XXX: Extract this up earlier.
    let deps_dir = tempfile::tempdir()?;
    let zips = wheels
        .into_par_iter()
        .map(|wheel| {
            let whl_zip = ZipArchive::new(File::open(&wheel)?)?;
            let whl_file = WheelFile::parse_file_name(
                wheel
                    .file_name()
                    .ok_or_else(|| anyhow!("XXX"))?
                    .to_str()
                    .ok_or_else(|| anyhow!("YYY"))?,
            )?;
            recompress_zipped_whl(whl_zip, &whl_file, &wheel_options, deps_dir.path())
                .map(|file| (whl_file.file_name.to_string(), file))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    for (file_name, zip) in zips {
        dst_zip.start_file(format!(".deps/{file_name}"), stored_file_options)?;
        let mut src = DigestingReader::new(default_digest(), zip);
        io::copy(&mut src, &mut dst_zip)?;
        pex_info.distributions.insert(
            Cow::Owned(file_name),
            Cow::Owned(src.into_fingerprint().hex_digest()),
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

fn execute_pex(pex: &Path) -> anyhow::Result<()> {
    let exit_code = pexrs::boot(None, vec![], pex, vec![], false)?;
    process::exit(exit_code)
}
