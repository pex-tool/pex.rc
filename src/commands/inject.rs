// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::{HashMap, HashSet};
use std::fmt::{Display, Formatter};
use std::io;
use std::io::{BufRead, BufReader, Read, Seek, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow, bail};
use boot::{inject_boot, sh_boot_shebang, write_boot};
use cache::{DigestingReader, Fingerprint, default_digest};
use clap::{ArgAction, Args};
use enumset::EnumSet;
use fs_err as fs;
use fs_err::File;
use indexmap::IndexSet;
use interpreter::Interpreter;
use log::info;
use owo_colors::OwoColorize;
use pex::{Layout, Pex};
use platform::mark_executable;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use scripts::{IdentifyInterpreter, Scripts};
use target::SimplifiedTarget;
use tempfile::NamedTempFile;
use wheel::WheelFile;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::compression_method::CompressionArgs;
use crate::embeds::{Binary, CLIB_BY_TARGET, PROXY_BY_TARGET, PROXYW_BY_TARGET};
use crate::package::{WheelOptions, repackage_wheels};
use crate::source;

#[derive(Args)]
#[group(skip)]
pub struct Inject {
    #[command(flatten)]
    compression_args: CompressionArgs,

    #[arg(long = "target", action = ArgAction::Append)]
    targets: Vec<crate::simplified_target::SimplifiedTarget>,

    #[arg(short = 'p', long)]
    preferred_python: Option<PathBuf>,

    /// The PEXes to inject with a native runtime. Can be paths or URLs.
    #[arg(value_name = "PEX", required = true)]
    pexes: Vec<String>,
}

impl Inject {
    pub fn execute(self) -> anyhow::Result<()> {
        let (clibs, proxies) = if !self.targets.is_empty() {
            (
                self.targets
                    .iter()
                    .map(|target| {
                        CLIB_BY_TARGET
                            .get(target)
                            .expect("The allowed --target values are all keys in CLIB_BY_TARGET.")
                    })
                    .collect::<Vec<_>>(),
                self.targets
                    .iter()
                    .map(|target| {
                        PROXY_BY_TARGET
                            .get(target)
                            .expect("The allowed --target values are all keys in PROXY_BY_TARGET.")
                    })
                    .chain(
                        self.targets
                            .iter()
                            .filter_map(|target| PROXYW_BY_TARGET.get(target)),
                    )
                    .collect::<Vec<_>>(),
            )
        } else {
            (
                CLIB_BY_TARGET.values().collect::<Vec<_>>(),
                PROXY_BY_TARGET
                    .values()
                    .chain(PROXYW_BY_TARGET.values())
                    .collect::<Vec<_>>(),
            )
        };
        let pexes = self
            .pexes
            .into_iter()
            .map(|source| source::to_path(source, None))
            .collect::<anyhow::Result<Vec<_>>>()?;
        let options = self.compression_args.into_wheel_options(None);
        inject_all(
            pexes,
            &options,
            clibs.as_slice(),
            proxies.as_slice(),
            self.preferred_python.as_deref(),
        )
    }
}

fn inject_all(
    pexes: Vec<PathBuf>,
    options: &WheelOptions,
    clibs: &[&Binary],
    proxies: &[&Binary],
    preferred_python: Option<&Path>,
) -> anyhow::Result<()> {
    for pex in pexes {
        inject(&pex, options, clibs, proxies, preferred_python)?
    }
    Ok(())
}

#[derive(Eq, PartialEq, Hash)]
struct RequiredTarget<'a> {
    targets: EnumSet<SimplifiedTarget>,
    required_by: &'a str,
}

impl<'a> RequiredTarget<'a> {
    fn satisfied_by(&self, target: SimplifiedTarget) -> bool {
        self.targets.contains(target)
    }
}

impl<'a> Display for RequiredTarget<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{targets} required by {wheel}",
            targets = self.targets,
            wheel = self.required_by
        )
    }
}

struct RequiredTargets<'a> {
    pex: &'a Path,
    required_targets: IndexSet<RequiredTarget<'a>>,
}

impl<'a> RequiredTargets<'a> {
    fn for_pex(pex: &'a Pex) -> anyhow::Result<Self> {
        let wheels = pex
            .info
            .raw()
            .distributions
            .keys()
            .map(|wheel_file_name| WheelFile::parse_file_name(wheel_file_name))
            .collect::<anyhow::Result<Vec<_>>>()?;

        let mut targets_by_project_name = HashMap::new();
        for wheel in wheels {
            targets_by_project_name
                .entry(wheel.project_name)
                .or_insert_with(HashSet::new)
                .extend({
                    let compatible = wheel
                        .tags
                        .iter()
                        .filter_map(|tag| {
                            SimplifiedTarget::for_platform_tag(tag.platform)
                                .map(|targets| targets.map(|targets| (wheel.file_name, targets)))
                                .ok()
                        })
                        .collect::<Vec<_>>();
                    if compatible.is_empty() {
                        bail!(
                            "There are no pexrc binaries available that support {wheel}.",
                            wheel = wheel.file_name
                        )
                    }
                    compatible
                });
        }
        let mut required_targets = IndexSet::new();
        for required in targets_by_project_name.values() {
            if required.contains(&None) {
                // If a project has an "-any" whl, we can always resolve that, potentially at the cost
                // of perf; so we ignore these projects.
                continue;
            }
            for required_target in required {
                let (required_by, targets) =
                    required_target.expect("We confirmed all targets were Some above.");
                required_targets.insert(RequiredTarget {
                    targets,
                    required_by,
                });
            }
        }
        Ok(Self {
            pex: pex.path,
            required_targets,
        })
    }

    fn select_binaries<'b>(
        &self,
        binaries: &[&'b Binary<'b>],
    ) -> anyhow::Result<IndexSet<&'b Binary<'b>>> {
        if self.required_targets.is_empty() {
            return Ok(binaries.iter().copied().collect());
        }
        let mut selected = IndexSet::with_capacity(binaries.len());
        for required_target in &self.required_targets {
            let mut satisifed = false;
            for binary in binaries {
                if required_target.satisfied_by(binary.target) {
                    selected.insert(*binary);
                    satisifed = true;
                }
            }
            if !satisifed {
                bail!(
                    "This pexrc binary has no clib that satisfies {required_target} in PEX {pex}.",
                    pex = self.pex.display()
                )
            }
        }
        Ok(selected)
    }
}

fn inject(
    pex: &Path,
    options: &WheelOptions,
    clibs: &[&Binary],
    proxies: &[&Binary],
    preferred_python: Option<&Path>,
) -> anyhow::Result<()> {
    let pex = Pex::load(pex)?;
    let required_targets = RequiredTargets::for_pex(&pex)?;
    let clibs = required_targets.select_binaries(clibs)?;
    let proxies = required_targets.select_binaries(proxies)?;
    let preferred_interpreter = if let Some(python) = preferred_python {
        let identification_script = IdentifyInterpreter::read(&mut Scripts::Embedded)?;
        Some(Interpreter::load(python, &identification_script)?)
    } else {
        None
    };
    match pex.layout {
        Layout::Loose | Layout::Packed => {
            inject_pex_dir(pex, options, clibs, proxies, preferred_interpreter.as_ref())
        }
        Layout::ZipApp => {
            inject_pex_zip(pex, options, clibs, proxies, preferred_interpreter.as_ref())
        }
    }
}

fn inject_pex_dir(
    mut pex: Pex,
    options: &WheelOptions,
    clibs: IndexSet<&Binary>,
    proxies: IndexSet<&Binary>,
    preferred_interpreter: Option<&Interpreter>,
) -> anyhow::Result<()> {
    // Make sure we have a shebang early. This partially validates the pex to inject is a valid one
    // before expending too much effort copying files below.
    let shebang = if let Some(sh_boot_shebang) = sh_boot_shebang(
        &pex,
        pex.info.raw().venv_hermetic_scripts,
        true,
        preferred_interpreter,
    )? {
        sh_boot_shebang
    } else {
        let original_main = pex.path.join("__main__.py");
        io::BufReader::new(File::open(&original_main)?)
            .lines()
            .next()
            .ok_or_else(|| {
                anyhow!(
                    "Expected original PEX __main__.py to have a shebang line but {path} did not.",
                    path = original_main.display()
                )
            })??
    };

    let mut dest_pex = tempfile::tempdir_in(pex.path.parent().unwrap_or_else(|| Path::new(".")))?;
    let excludes: HashSet<PathBuf> = [
        ".bootstrap",
        ".deps",
        "PEX-INFO",
        "__main__.py",
        "__pex__",
        "__pycache__",
        "pex",
        "pex-repl",
    ]
    .into_iter()
    .map(|rel_path| pex.path.join(rel_path))
    .collect();
    for entry in walkdir::WalkDir::new(pex.path)
        .min_depth(1)
        .into_iter()
        .filter_entry(|entry| !excludes.contains(entry.path()))
    {
        let entry = entry?;
        let dst = dest_pex.path().join(entry.path().strip_prefix(pex.path)?);
        if entry.file_type().is_dir() {
            fs::create_dir_all(dst)?;
        } else {
            fs::copy(entry.path(), dst)?;
        }
    }
    let deps_dir = dest_pex.path().join(".deps");
    repackage_wheels(&pex, options, &deps_dir)?;
    let wheel_file_names = pex
        .info
        .raw()
        .distributions
        .keys()
        .copied()
        .collect::<Vec<_>>();
    let fingerprints = wheel_file_names
        .into_par_iter()
        .map(|wheel_file_name| {
            let fingerprint =
                Fingerprint::try_from(BufReader::new(File::open(deps_dir.join(wheel_file_name))?))?;
            Ok(fingerprint.hex_digest())
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    pex.info.with_raw_mut(|pex_info| {
        pex_info.deps_are_wheel_files = true;
        for (original_fp, fingerprint) in pex_info.distributions.values_mut().zip(fingerprints) {
            *original_fp = Cow::Owned(fingerprint)
        }
    });

    let mut scripts = Scripts::Embedded;
    let pex_dir = dest_pex.path().join("__pex__");
    fs::create_dir_all(&pex_dir)?;
    scripts.write(dest_pex.path())?;

    let dst = pex.path.with_extension("pexrc");
    let clib_dir = pex_dir.join(".clibs");
    fs::create_dir_all(&clib_dir)?;
    info!("Embedded clibs:");
    for clib in clibs {
        embed_in_dir(clib.path, clib.contents, &clib_dir, false)?;
    }
    let scripts_dir = pex_dir.join(".proxies");
    fs::create_dir_all(&scripts_dir)?;
    info!("Embedded proxies:");
    for proxy in proxies {
        embed_in_dir(proxy.path, proxy.contents, &scripts_dir, true)?;
    }

    pex.info
        .write(&mut File::create_new(dest_pex.path().join("PEX-INFO"))?)?;

    write_boot(dest_pex.path(), &shebang)?;

    if dst.is_dir() {
        fs::remove_dir_all(&dst)?;
    } else if dst.is_file() {
        fs::remove_file(&dst)?;
    }
    fs::rename(dest_pex.path(), dst)?;
    dest_pex.disable_cleanup(true);
    Ok(())
}

fn embed_in_dir(
    path: &Path,
    contents: &[u8],
    dst_dir: &Path,
    mark_executable: bool,
) -> anyhow::Result<()> {
    let dst_path = dst_dir.join(path.file_name().expect("Embeds have file names."));
    anstream::eprint!(
        "Writing {entry} {size} bytes to {dst_path}...",
        entry = path.display().blue(),
        size = contents.len(),
        dst_path = dst_path.display(),
    );
    let mut dst_file = File::create_new(dst_path)?;
    let mut embed_reader = zstd::Decoder::new(contents)?;
    io::copy(&mut embed_reader, &mut dst_file)?;
    if mark_executable {
        platform::mark_executable(dst_file.file_mut())?;
    }
    anstream::eprintln!("{}.", "done".green());
    Ok(())
}

fn inject_pex_zip(
    mut pex: Pex,
    options: &WheelOptions,
    clibs: IndexSet<&Binary>,
    proxies: IndexSet<&Binary>,
    preferred_interpreter: Option<&Interpreter>,
) -> anyhow::Result<()> {
    let pex_info = pex.info.raw();
    let zip_read_fp = File::open(pex.path)?;
    let mut src_zip = ZipArchive::new(&zip_read_fp)?;
    let prefix = if let Some(sh_boot_shebang) = sh_boot_shebang(
        &pex,
        pex_info.venv_hermetic_scripts,
        false,
        preferred_interpreter,
    )? {
        Some(sh_boot_shebang.into_bytes())
    } else {
        let first_entry = src_zip.by_index(0)?;
        let zip_start = first_entry.header_start();
        if zip_start > 0 {
            let mut prefix_reader = File::open(pex.path)?.take(zip_start);
            let mut prefix = Vec::with_capacity(zip_start.try_into().with_context(|| {
                format!(
                    "The zip prefix is {zip_start} bytes which is bigger than the system pointer \
                    size of {ptr_size} bits.",
                    ptr_size = usize::BITS
                )
            })?);
            prefix_reader.read_to_end(&mut prefix)?;
            Some(prefix)
        } else {
            None
        }
    };

    let mut dst_zip_fp = if let Some(parent_dir) = pex.path.parent() {
        NamedTempFile::new_in(parent_dir)?
    } else {
        NamedTempFile::new()?
    };
    if let Some(prefix) = prefix {
        dst_zip_fp.write_all(&prefix)?;
    }
    let mut dst_zip = ZipWriter::new(&dst_zip_fp);

    let file_options = options.file_options()?;
    let deflated_file_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    let directory_options = SimpleFileOptions::default();
    for index in 0..src_zip.len() {
        let mut src_file = src_zip.by_index(index)?;
        let entry_name = src_file.name();
        if [".bootstrap/", ".deps/", "PEX-INFO", "__pex__/"]
            .into_iter()
            .any(|prefix| entry_name.starts_with(prefix))
            || entry_name == "__main__.py"
        {
            continue;
        }
        if src_file.is_dir() {
            dst_zip.add_directory(entry_name, directory_options)?
        } else {
            let options = if entry_name == "PEX-INFO" {
                deflated_file_options
            } else {
                file_options
            };
            dst_zip.start_file(entry_name, options)?;
            io::copy(&mut src_file, &mut dst_zip)?;
        }
    }

    let deps_dir = tempfile::tempdir_in(pex.path.parent().unwrap_or_else(|| Path::new(".")))?;
    let stored_file_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Stored);
    repackage_wheels(&pex, options, deps_dir.path())?;
    let mut fingerprints = Vec::with_capacity(pex_info.distributions.len());
    for wheel_file_name in pex_info.distributions.keys().copied() {
        dst_zip.start_file(format!(".deps/{wheel_file_name}"), stored_file_options)?;
        let mut digesting_reader = DigestingReader::new(
            default_digest(),
            File::open(deps_dir.path().join(wheel_file_name))?,
        );
        io::copy(&mut digesting_reader, &mut dst_zip)?;
        fingerprints.push(digesting_reader.into_fingerprint().hex_digest());
    }
    pex.info.with_raw_mut(|pex_info| {
        pex_info.deps_are_wheel_files = true;
        for (original_fp, fingerprint) in pex_info.distributions.values_mut().zip(fingerprints) {
            *original_fp = Cow::Owned(fingerprint)
        }
    });

    dst_zip.add_directory("__pex__", directory_options)?;
    Scripts::Embedded.inject(&mut dst_zip, file_options)?;

    let deflate_options =
        SimpleFileOptions::default().compression_method(CompressionMethod::Deflated);
    dst_zip.add_directory("__pex__/.clibs", directory_options)?;
    info!("Embedded clibs:");
    for clib in clibs {
        embed_in_zip(
            clib.path,
            clib.contents,
            &mut dst_zip,
            "__pex__/.clibs",
            deflate_options,
        )?;
    }
    dst_zip.add_directory("__pex__/.proxies", directory_options)?;
    info!("Embedded proxies:");
    for proxy in proxies {
        embed_in_zip(
            proxy.path,
            proxy.contents,
            &mut dst_zip,
            "__pex__/.proxies",
            file_options,
        )?;
    }

    dst_zip.start_file("PEX-INFO", deflated_file_options)?;
    pex.info.write(&mut dst_zip)?;

    inject_boot(&mut dst_zip, deflate_options)?;

    dst_zip.finish()?;
    mark_executable(dst_zip_fp.as_file_mut())?;

    let dst = pex.path.with_extension("pexrc");
    if dst.is_dir() {
        fs::remove_dir_all(&dst)?;
    }
    dst_zip_fp.persist(dst)?;

    Ok(())
}

fn embed_in_zip(
    path: &Path,
    contents: &[u8],
    dst_zip: &mut ZipWriter<impl Write + Seek>,
    dst_dir: &str,
    file_options: SimpleFileOptions,
) -> anyhow::Result<()> {
    let dst_path = format!(
        "{dst_dir}/{embed}",
        embed = path
            .file_name()
            .expect("Embeds have file names.")
            .to_str()
            .expect("Embed file names are utf-8 strings.")
    );
    anstream::eprint!(
        "Writing {entry} {size} bytes to {dst_path}...",
        entry = path.display().blue(),
        size = contents.len()
    );
    dst_zip.start_file(dst_path, file_options)?;
    let mut embed_reader = zstd::Decoder::new(contents)?;
    io::copy(&mut embed_reader, dst_zip)?;
    anstream::eprintln!("{}.", "done".green());
    Ok(())
}
