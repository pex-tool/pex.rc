// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::HashSet;
use std::io;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, anyhow};
use boot::{inject_boot, sh_boot_shebang, write_boot};
use cache::{DigestingReader, Fingerprint, default_digest};
use clap::{ArgAction, Args};
use fs_err as fs;
use fs_err::File;
use indexmap::IndexSet;
use interpreter::Interpreter;
use log::info;
use pex::{Layout, Pex};
use platform::mark_executable;
use python_platform::PythonImplementation;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use repackage::{WheelOptions, repackage_wheels};
use scripts::{IdentifyInterpreter, Scripts};
use tempfile::NamedTempFile;
use wheel::WheelFile;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

use crate::compression_method::CompressionArgs;
use crate::embeds::{Binary, CLIB_BY_TARGET, PROXY_BY_TARGET, PROXYW_BY_TARGET};
use crate::source;
use crate::target::RequiredTargets;

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

fn inject(
    pex: &Path,
    options: &WheelOptions,
    clibs: &[&Binary],
    proxies: &[&Binary],
    preferred_python: Option<&Path>,
) -> anyhow::Result<()> {
    let pex = Pex::load(pex)?;
    let wheel_files = pex
        .info
        .raw()
        .distributions
        .keys()
        .map(|wheel_file_name| WheelFile::parse_file_name(wheel_file_name))
        .collect::<anyhow::Result<Vec<_>>>()?;
    let pex_file = pex.file();
    let required_targets =
        RequiredTargets::for_wheel_files(pex_file.display(), wheel_files.iter())?;
    let clibs = required_targets.select_binaries(clibs)?;
    let proxies = required_targets.select_binaries(proxies)?;
    let preferred_interpreter = if let Some(python) = preferred_python {
        let identification_script = IdentifyInterpreter::read(&mut Scripts::Embedded)?;
        Some(
            Interpreter::load(python, &identification_script)?
                .details
                .python_implementation(),
        )
    } else {
        None
    };
    match pex.layout {
        Layout::Loose | Layout::Packed => {
            inject_pex_dir(pex, options, clibs, proxies, preferred_interpreter)
        }
        Layout::ZipApp => inject_pex_zip(pex, options, clibs, proxies, preferred_interpreter),
    }
}

fn inject_pex_dir(
    mut pex: Pex,
    options: &WheelOptions,
    clibs: IndexSet<&Binary>,
    proxies: IndexSet<&Binary>,
    preferred_interpreter: Option<PythonImplementation>,
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
    let wheel_file_names = pex.info.raw().distributions.keys().collect::<Vec<_>>();
    let fingerprints = wheel_file_names
        .into_par_iter()
        .map(|wheel_file_name| {
            let fingerprint = Fingerprint::try_from(BufReader::new(File::open(
                deps_dir.join(wheel_file_name.as_ref()),
            )?))?;
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
        clib.embed_in_dir(&clib_dir, false)?;
    }
    let scripts_dir = pex_dir.join(".proxies");
    fs::create_dir_all(&scripts_dir)?;
    info!("Embedded proxies:");
    for proxy in proxies {
        proxy.embed_in_dir(&scripts_dir, true)?;
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

fn inject_pex_zip(
    mut pex: Pex,
    options: &WheelOptions,
    clibs: IndexSet<&Binary>,
    proxies: IndexSet<&Binary>,
    preferred_interpreter: Option<PythonImplementation>,
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
    for wheel_file_name in pex_info.distributions.keys() {
        dst_zip.start_file(format!(".deps/{wheel_file_name}"), stored_file_options)?;
        let mut digesting_reader = DigestingReader::new(
            default_digest(),
            File::open(deps_dir.path().join(wheel_file_name.as_ref()))?,
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

    dst_zip.add_directory("__pex__/.clibs", directory_options)?;
    for clib in clibs {
        clib.embed_in_zip(&mut dst_zip, "__pex__/.clibs", deflated_file_options)?;
    }
    dst_zip.add_directory("__pex__/.proxies", directory_options)?;
    for proxy in proxies {
        proxy.embed_in_zip(&mut dst_zip, "__pex__/.proxies", file_options)?;
    }

    dst_zip.start_file("PEX-INFO", deflated_file_options)?;
    pex.info.write(&mut dst_zip)?;

    inject_boot(&mut dst_zip, deflated_file_options)?;

    dst_zip.finish()?;
    mark_executable(dst_zip_fp.as_file_mut())?;

    let dst = pex.path.with_extension("pexrc");
    if dst.is_dir() {
        fs::remove_dir_all(&dst)?;
    }
    dst_zip_fp.persist(dst)?;

    Ok(())
}
