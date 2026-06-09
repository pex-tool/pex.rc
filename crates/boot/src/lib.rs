// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::borrow::Cow;
use std::fmt::Display;
use std::io::{ErrorKind, Read, Seek, Write};
use std::path::Path;

use anyhow::{anyhow, bail};
use cache::CacheDir;
use const_format::str_split;
use fs_err as fs;
use fs_err::File;
use interpreter::{
    InterpreterConstraints,
    SearchPath,
    SelectionStrategy,
    VersionSpec,
    calculate_compatible_unix_binary_names,
};
use itertools::Itertools;
use pex::{Pex, PexPath, RawPexInfo};
use pexrs::venv_dir;
use platform::PosixPath;
use python_platform::PythonImplementation;
use zip::ZipWriter;
use zip::write::{FileOptionExtension, FileOptions};

const SH_BOOT_SHEBANG: &[u8] = b"#!/bin/sh\n";
const SH_BOOT_PARTS: [&str; 4] = str_split!(include_str!("boot.sh"), "# --- split --- #\n");

pub fn sh_boot_shebang(
    pex: &Pex,
    hermetic: bool,
    escaped: bool,
    preferred_interpreter: Option<PythonImplementation>,
) -> anyhow::Result<Option<String>> {
    let mut sh_boot_shebang_buffer: [_; SH_BOOT_SHEBANG.len()] = [0; SH_BOOT_SHEBANG.len()];
    let mut pex_fp = File::open(pex.file())?;
    match pex_fp.read_exact(&mut sh_boot_shebang_buffer) {
        Ok(()) => {
            if sh_boot_shebang_buffer != SH_BOOT_SHEBANG {
                return Ok(None);
            }
        }
        Err(err) if err.kind() == ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => bail!(
            "Failed to determine if {pex} uses a `--sh-boot` shebang header: {err}",
            pex = pex.path.display()
        ),
    };
    Ok(Some(create_sh_boot_shebang(
        pex.path.display(),
        pex.info.raw(),
        hermetic,
        escaped,
        preferred_interpreter,
    )?))
}

pub fn create_sh_boot_shebang(
    subject: impl Display,
    pex_info: &RawPexInfo,
    hermetic: bool,
    escaped: bool,
    preferred_interpreter: Option<PythonImplementation>,
) -> anyhow::Result<String> {
    let pex_path = PexPath::from_pex_info(pex_info, false);
    let additional_pexes = pex_path.load_pexes()?;

    let venv_dir = venv_dir(
        None,
        subject,
        pex_info,
        &SearchPath::EMPTY,
        &additional_pexes,
    )?;
    let venv_relpath = venv_dir.strip_prefix(CacheDir::root()?)?;

    let interpreter_constraints =
        InterpreterConstraints::try_from(pex_info.interpreter_constraints.as_slice())?;
    let selection_strategy: SelectionStrategy = pex_info
        .interpreter_selection_strategy
        .unwrap_or(pex::InterpreterSelectionStrategy::Oldest)
        .into();
    let pythons = calculate_compatible_unix_binary_names(
        &interpreter_constraints,
        selection_strategy,
        preferred_interpreter,
        true,
    )
    .into_iter()
    .map(|(binary_name, version_spec)| {
        binary_name
            .into_string()
            .map(|binary_name| (binary_name, version_spec))
            .map_err(|err| anyhow!("{err}", err = err.display()))
    })
    .collect::<anyhow::Result<Vec<_>>>()?;
    let python_args = if hermetic {
        if pythons.iter().any(|(_, version_spec)| {
            matches!(version_spec, None | Some(VersionSpec::Major(_)))
                || matches!(
                    version_spec,
                    Some(VersionSpec::MajorMinor(major, minor)) if (*major, *minor) < (3, 4)
                )
        }) {
            "-sE"
        } else {
            "-I"
        }
    } else {
        ""
    };
    let pexrc_root = if let Some(pex_root) = pex_info.pex_root.as_deref() {
        Cow::Owned(PosixPath::try_from(pex_root)?.to_string())
    } else {
        Cow::Borrowed("")
    };
    Ok(format!(
        "{shebang}{start_escape}{header}{vars}{body}{end_escape}\n",
        shebang = SH_BOOT_PARTS[0], // N.B.: SH_BOOT_SHEBANG
        start_escape = if escaped { "'''': pshprs\n" } else { "" },
        header = SH_BOOT_PARTS[1],
        vars = SH_BOOT_PARTS[2]
            .replace("{pexrc_root}", pexrc_root.as_ref())
            .replace(
                "{venv_relpath}",
                &PosixPath::relpath(venv_relpath)?.to_string()
            )
            .replace(
                "{pythons}",
                &pythons
                    .iter()
                    .map(|(binary_name, _)| binary_name)
                    .join("\n")
            )
            .replace("{python_args}", python_args),
        body = SH_BOOT_PARTS[3].trim_end(),
        end_escape = if escaped { "\n'''\n" } else { "\n" },
    ))
}

const PY_BOOT: &[u8] = include_bytes!("boot.py");

pub fn inject_boot<T: FileOptionExtension + Copy>(
    zip: &mut ZipWriter<impl Write + Seek>,
    file_options: FileOptions<T>,
) -> anyhow::Result<()> {
    zip.start_file("__pex__/__init__.py", file_options)?;
    zip.write_all(PY_BOOT)?;
    zip.start_file("__main__.py", file_options)?;
    zip.write_all(PY_BOOT)?;
    Ok(())
}

pub fn write_boot(dest_dir: &Path, shebang: &str) -> anyhow::Result<()> {
    let main_py_path = dest_dir.join("__main__.py");
    let mut file = File::create_new(&main_py_path)?;
    file.write_all(shebang.as_bytes().trim_ascii_end())?;
    file.write_all(b"\n\n")?;
    file.write_all(PY_BOOT)?;
    fs::copy(&main_py_path, dest_dir.join("__pex__").join("__init__.py"))?;
    platform::mark_executable(file.file_mut())?;
    platform::symlink_or_link_or_copy(&main_py_path, dest_dir.join("pex"), true)?;
    Ok(())
}
