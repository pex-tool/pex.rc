// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io;
use std::io::{Cursor, ErrorKind, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{anyhow, bail};
use cache::{default_digest, fingerprint_file};
use fs_err as fs;
use fs_err::File;
use indexmap::{IndexMap, IndexSet};
use pex::{
    BinPath,
    DEPS_DIR,
    Layout,
    PEX_INFO_FILE,
    Pex,
    RawPexInfo,
    SRCS_DIR,
    SRCS_ZIP_DIR,
    collect_loose_user_source,
    collect_zipped_user_source_indexes,
};
use platform::{mark_executable, path_as_bytes, path_as_str, symlink_or_link_or_copy};
use python_proxy::ProxySource;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use resolver::ResolvedWheel;
use scripts::{Scripts, VenvPex, VenvPexRepl};
use serde_json::Value;
use tracing::instrument;
use wheel::{MetadataDirs, Record, WheelLayout};
use zip::ZipArchive;
use zip::read::ZipArchiveMetadata;
use zip_ext::ZipArchiveExt;

use crate::install::populate_whl_zip;
use crate::install::wheel::{
    Spread,
    WheelDetails,
    calculate_spread,
    extract_idx,
    install_scripts,
    populate_wheel_dir,
};
use crate::virtualenv::Virtualenv;
use crate::{Provenance, install};

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum Scope {
    All,
    Deps,
    Srcs,
}

impl Scope {
    pub fn as_str(&self) -> &'static str {
        match self {
            Scope::All => "all",
            Scope::Deps => "deps",
            Scope::Srcs => "srcs",
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn populate_from_loose_pex<'a>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    loose_pex: &'a Pex<'a>,
    resolved_wheels: &IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    proxy_source: &ProxySource,
    scope: Scope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    if matches!(scope, Scope::All | Scope::Deps) {
        collect_wheels_from_directory_pex(loose_pex, resolved_wheels)?
            .into_par_iter()
            .try_for_each(|(project_name, wheel_paths)| {
                let layout = WheelLayout::load_from_dir(&wheel_paths.path)?;
                let record_file = File::open(wheel_paths.path.join(format!(
                    "{dist_info_dir}/RECORD",
                    dist_info_dir = wheel_paths.metadata_dirs.dist_info_dir()
                )))?;
                let record = Record::read(record_file)?;
                let wheel_details = WheelDetails::new(
                    project_name,
                    wheel_paths.metadata_dirs,
                    layout,
                    record.wheel_has_bin_dir(),
                );
                populate_wheel_dir(
                    venv,
                    shebang_interpreter,
                    &wheel_paths.path,
                    &wheel_details,
                    proxy_source,
                    provenance.clone(),
                )
            })?;
    }
    if matches!(scope, Scope::All | Scope::Srcs) {
        populate_user_code_from_directory_pex(loose_pex, venv, populate_pex_info, provenance)?;
    }
    Ok(())
}

struct WheelPaths<'a> {
    path: PathBuf,
    metadata_dirs: &'a MetadataDirs,
}

fn collect_wheels_from_directory_pex<'a>(
    pex: &Pex,
    resolved_wheels: &'a IndexMap<&'a str, ResolvedWheel<'a>>,
) -> anyhow::Result<Vec<(&'a str, WheelPaths<'a>)>> {
    let mut wheels = Vec::with_capacity(resolved_wheels.len());
    let deps_dir = pex.path.join(DEPS_DIR);
    if deps_dir.is_dir() {
        for entry in fs::read_dir(deps_dir)? {
            let entry = entry?;
            if let Ok(wheel_file_name) = platform::os_str_as_str(&entry.file_name())
                && let Some(wheel) = resolved_wheels.get(wheel_file_name)
            {
                wheels.push((
                    wheel.project_name,
                    WheelPaths {
                        path: entry.path(),
                        metadata_dirs: &wheel.metadata_dirs,
                    },
                ))
            }
        }
    }
    Ok(wheels)
}

#[allow(clippy::too_many_arguments)]
fn populate_from_packed_pex<'a>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    packed_pex: &'a Pex<'a>,
    resolved_wheels: &IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    proxy_source: &ProxySource,
    scope: Scope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    if matches!(scope, Scope::All | Scope::Deps) {
        collect_wheels_from_directory_pex(packed_pex, resolved_wheels)?
            .into_par_iter()
            .try_for_each(|(project_name, wheel_paths)| {
                populate_whl_zip(
                    venv,
                    shebang_interpreter,
                    &wheel_paths.path,
                    None,
                    project_name,
                    wheel_paths.metadata_dirs,
                    proxy_source,
                    provenance.clone(),
                )
            })?;
    }
    if matches!(scope, Scope::All | Scope::Srcs) {
        populate_user_code_from_directory_pex(packed_pex, venv, populate_pex_info, provenance)?;
    }
    Ok(())
}

fn populate_user_code_from_directory_pex<'a>(
    directory_pex: &'a Pex<'a>,
    venv: &Virtualenv,
    populate_pex_info: bool,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let user_code = collect_loose_user_source(directory_pex.path)?;
    user_code.into_par_iter().try_for_each(|path| {
        let dst_path =
            venv.site_packages_path(path.strip_prefix(directory_pex.path).expect(
                "Walked directory PEX paths should be child paths of the directory PEX root dir.",
            ).trim_prefix(SRCS_DIR));
        if path.is_dir() {
            fs::create_dir_all(dst_path)?;
        } else {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)?;
            }
            match File::create_new(&dst_path) {
                Ok(mut dst) => {
                    provenance.record(path.display(), dst_path);
                    io::copy(&mut File::open(&path)?, &mut dst)?;
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    let (size, fingerprint) = fingerprint_file(&path, default_digest())?;
                    provenance.record_collision(path.display(), fingerprint, size, dst_path);
                }
                Err(err) => bail!("{err}"),
            }
        }
        Ok(())
    })?;
    if populate_pex_info {
        fs::copy(
            directory_pex.path.join(PEX_INFO_FILE),
            venv.prefix().join(PEX_INFO_FILE),
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn populate_from_zip_app_with_whl_deps<'a>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    zip_app_pex: &'a Pex<'a>,
    resolved_wheels: &IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    proxy_source: &ProxySource,
    scope: Scope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let pex_zip = ZipArchive::new(File::open(zip_app_pex.path)?)?;
    let metadata = pex_zip.metadata();
    if matches!(scope, Scope::All | Scope::Deps) {
        let wheel_file_names = resolved_wheels.into_iter().collect::<Vec<_>>();
        wheel_file_names
            .into_par_iter()
            .try_for_each(|(wheel_file_name, wheel)| {
                let zip_fp = File::open(zip_app_pex.path)?;
                let mut zip =
                    unsafe { ZipArchive::unsafe_new_with_metadata(zip_fp, metadata.clone()) };
                let whl_name = [DEPS_DIR, wheel_file_name].join("/");
                let whl_file = zip.by_name_seek(&whl_name)?;
                let mut whl_zip = ZipArchive::new(whl_file)?;
                let whl_zip_metadata = whl_zip.metadata();
                let layout = if let Ok(layout_file) = whl_zip.by_name_ex(WheelLayout::file_name()) {
                    Some(WheelLayout::read(layout_file)?)
                } else {
                    None
                };
                let record_name = format!(
                    "{dist_info_dir}/RECORD",
                    dist_info_dir = wheel.dist_info_dir()
                );
                let record = Record::read(Cursor::new(io::read_to_string(
                    whl_zip.by_name_ex(&record_name)?,
                )?))?;
                let wheel_details = WheelDetails::new(
                    wheel.project_name,
                    &wheel.metadata_dirs,
                    layout,
                    record.wheel_has_bin_dir(),
                );
                (0..whl_zip.len()).into_par_iter().try_for_each(|index| {
                    let zip_fp = File::open(zip_app_pex.path)?;
                    let mut zip =
                        unsafe { ZipArchive::unsafe_new_with_metadata(zip_fp, metadata.clone()) };
                    let whl_file = zip.by_name_seek(&whl_name)?;
                    let mut whl_zip = unsafe {
                        ZipArchive::unsafe_new_with_metadata(whl_file, whl_zip_metadata.clone())
                    };
                    extract_whl_idx(
                        venv,
                        shebang_interpreter,
                        &wheel_details,
                        index,
                        &mut whl_zip,
                        proxy_source,
                        format!("{zip}/{whl_name}", zip = zip_app_pex.path.display()),
                        provenance.clone(),
                    )
                })
            })?;
    }
    if matches!(scope, Scope::All | Scope::Srcs) {
        populate_user_sources_from_zip(
            venv,
            shebang_interpreter,
            zip_app_pex,
            proxy_source,
            provenance,
            &pex_zip,
            metadata,
        )?;
        if populate_pex_info {
            let mut pex_zip = ZipArchive::new(File::open(zip_app_pex.path)?)?;
            let mut pex_info_src_fp = pex_zip.by_name_ex(PEX_INFO_FILE)?;
            let mut pex_info_dst_fp = File::create_new(venv.prefix().join(PEX_INFO_FILE))?;
            io::copy(&mut pex_info_src_fp, &mut pex_info_dst_fp)?;
        }
    }
    Ok(())
}

fn populate_user_sources_from_zip(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    zip_app_pex: &Pex,
    proxy_source: &ProxySource,
    provenance: Arc<Provenance>,
    pex_zip: &ZipArchive<File>,
    metadata: Arc<ZipArchiveMetadata>,
) -> anyhow::Result<()> {
    let extract_indexes = collect_zipped_user_source_indexes(pex_zip);
    extract_indexes
        .into_par_iter()
        .try_for_each(|index| -> anyhow::Result<()> {
            let zip_fp = File::open(zip_app_pex.path)?;
            let mut zip = unsafe { ZipArchive::unsafe_new_with_metadata(zip_fp, metadata.clone()) };
            let spread = if let Some(name) = zip.name_for_index(index)
                && name.starts_with(SRCS_ZIP_DIR)
            {
                if name == SRCS_ZIP_DIR {
                    return Ok(());
                }
                Some(Spread::Move(
                    venv.site_packages_path(
                        name.trim_prefix(SRCS_ZIP_DIR)
                            .split("/")
                            .collect::<PathBuf>(),
                    ),
                ))
            } else {
                None
            };
            extract_idx(
                venv,
                shebang_interpreter,
                index,
                spread,
                &mut zip,
                proxy_source,
                zip_app_pex.path.display(),
                provenance.clone(),
            )?;
            Ok(())
        })?;
    Ok(())
}

struct DepFilter<'a>(&'a IndexMap<&'a str, ResolvedWheel<'a>>);

impl<'a> DepFilter<'a> {
    fn filter_deps<'b>(&self, file_name: &'b str) -> Option<&'b str> {
        if !file_name.starts_with(".deps/") {
            None
        } else {
            file_name[6..]
                .split("/")
                .next()
                .filter(|&whl_name| self.0.contains_key(whl_name))
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn populate_from_zip_app<'a>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    zip_app_pex: &'a Pex<'a>,
    resolved_wheels: &IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    proxy_source: &ProxySource,
    scope: Scope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let mut pex_zip = ZipArchive::new(File::open(zip_app_pex.path)?)?;
    let metadata = pex_zip.metadata();
    let dep_filter = DepFilter(resolved_wheels);
    if matches!(scope, Scope::All | Scope::Deps) {
        let data_dirs = resolved_wheels
            .iter()
            .map(|(file_name, wheel)| {
                let layout: Option<WheelLayout> = if let Ok(layout_file) =
                    pex_zip.by_name_ex(&format!(
                        ".deps/{file_name}/{layout_file}",
                        layout_file = WheelLayout::file_name()
                    )) {
                    Some(WheelLayout::read(layout_file)?)
                } else {
                    None
                };
                let record_name = format!(
                    ".deps/{file_name}/{dist_info_dir}/RECORD",
                    dist_info_dir = wheel.dist_info_dir()
                );
                let record = Record::read(Cursor::new(io::read_to_string(
                    pex_zip.by_name_ex(&record_name)?,
                )?))?;
                Ok((
                    *file_name,
                    WheelDetails::new(
                        wheel.project_name,
                        &wheel.metadata_dirs,
                        layout,
                        record.wheel_has_bin_dir(),
                    ),
                ))
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;
        let extract_indexes = pex_zip
            .file_names()
            .enumerate()
            .filter_map(|(idx, name)| {
                dep_filter.filter_deps(name).map(|file_name| {
                    let wheel_details = data_dirs
                        .get(file_name)
                        .expect("We mapped a wheel details for each wheel file name.");
                    (idx, wheel_details)
                })
            })
            .collect::<Vec<_>>();
        extract_indexes.into_par_iter().try_for_each(
            |(index, wheel_details)| -> anyhow::Result<()> {
                let zip_fp = File::open(zip_app_pex.path)?;
                let mut zip =
                    unsafe { ZipArchive::unsafe_new_with_metadata(zip_fp, metadata.clone()) };
                extract_whl_idx(
                    venv,
                    shebang_interpreter,
                    wheel_details,
                    index,
                    &mut zip,
                    proxy_source,
                    zip_app_pex.path.display(),
                    provenance.clone(),
                )?;
                Ok(())
            },
        )?;
    }
    if matches!(scope, Scope::All | Scope::Srcs) {
        populate_user_sources_from_zip(
            venv,
            shebang_interpreter,
            zip_app_pex,
            proxy_source,
            provenance,
            &pex_zip,
            metadata,
        )?;
        if populate_pex_info {
            let mut pex_info_src_fp = pex_zip.by_name_ex(PEX_INFO_FILE)?;
            let mut pex_info_dst_fp = File::create_new(venv.prefix().join(PEX_INFO_FILE))?;
            io::copy(&mut pex_info_src_fp, &mut pex_info_dst_fp)?;
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "debug", skip_all)]
pub fn populate_user_code_and_wheels<'a>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    pex: &'a Pex<'a>,
    resolved_wheels: IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    proxy_source: &'a ProxySource<'a>,
    scope: Scope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    match pex.layout {
        Layout::Loose => populate_from_loose_pex(
            venv,
            shebang_interpreter,
            pex,
            &resolved_wheels,
            populate_pex_info,
            proxy_source,
            scope,
            provenance.clone(),
        )?,
        Layout::Packed => populate_from_packed_pex(
            venv,
            shebang_interpreter,
            pex,
            &resolved_wheels,
            populate_pex_info,
            proxy_source,
            scope,
            provenance.clone(),
        )?,
        Layout::ZipApp => {
            if pex.info.raw().deps_are_wheel_files {
                populate_from_zip_app_with_whl_deps(
                    venv,
                    shebang_interpreter,
                    pex,
                    &resolved_wheels,
                    populate_pex_info,
                    proxy_source,
                    scope,
                    provenance.clone(),
                )?
            } else {
                populate_from_zip_app(
                    venv,
                    shebang_interpreter,
                    pex,
                    &resolved_wheels,
                    populate_pex_info,
                    proxy_source,
                    scope,
                    provenance.clone(),
                )?
            }
        }
    }
    if matches!(scope, Scope::All | Scope::Deps) {
        resolved_wheels
            .into_values()
            .collect::<Vec<_>>()
            .into_par_iter()
            .try_for_each(|resolved_wheel| {
                let entry_points = venv
                    .site_packages_path(resolved_wheel.dist_info_dir().as_path())
                    .join("entry_points.txt");
                if entry_points.exists() {
                    install_scripts(
                        &entry_points,
                        venv,
                        shebang_interpreter,
                        shebang_arg,
                        proxy_source,
                        provenance.clone(),
                    )
                } else {
                    Ok(())
                }
            })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[instrument(level = "debug", skip_all)]
pub fn populate<'a>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    pex: &'a Pex<'a>,
    resolved_wheels: IndexMap<&'a str, ResolvedWheel<'a>>,
    scripts: &mut Scripts,
    bin_path_override: Option<BinPath>,
    proxy_source: &'a ProxySource<'a>,
    scope: Scope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let selected_wheels = resolved_wheels.keys().copied().collect::<Vec<_>>();
    populate_user_code_and_wheels(
        venv,
        shebang_interpreter,
        shebang_arg,
        pex,
        resolved_wheels,
        true,
        proxy_source,
        scope,
        provenance,
    )?;
    if matches!(scope, Scope::All | Scope::Srcs) {
        install::write_pex_extra_sys_path_support_files(venv, scripts)?;
        write_main(
            venv,
            shebang_interpreter,
            shebang_arg,
            pex.info.raw(),
            scripts,
            bin_path_override,
        )?;
        write_repl(
            venv,
            shebang_interpreter,
            shebang_arg,
            pex.path,
            pex.info.raw(),
            selected_wheels,
            scripts,
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn extract_whl_idx<R>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    wheel_details: &WheelDetails,
    index: usize,
    zip: &mut ZipArchive<R>,
    proxy_source: &ProxySource,
    source: impl Display,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()>
where
    R: Read + Seek,
{
    let dst_path = {
        let zip_file = zip.by_index(index)?;
        let dst_rel_path =
            if zip_file.name().starts_with(".deps/") {
                zip_file.name().splitn(3, "/").nth(2).ok_or_else(|| {
                    anyhow!("Invalid PEX .deps/ entry {name}", name = zip_file.name())
                })?
            } else {
                zip_file.name()
            }
            .split("/")
            .collect::<PathBuf>();
        calculate_spread(venv, wheel_details, &dst_rel_path)?
    };
    if dst_path.is_some() {
        extract_idx(
            venv,
            shebang_interpreter,
            index,
            dst_path,
            zip,
            proxy_source,
            source,
            provenance,
        )
    } else {
        Ok(())
    }
}

fn write_shebang_bytes(
    file: &mut File,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
) -> anyhow::Result<()> {
    file.write_all(b"#!")?;
    file.write_all(path_as_bytes(shebang_interpreter)?)?;
    if let Some(shebang_arg) = shebang_arg {
        file.write_all(b" ")?;
        file.write_all(shebang_arg.as_bytes())?;
    }
    file.write_all(b"\n")?;
    Ok(())
}

fn as_python_bool(value: bool) -> &'static str {
    if value { "True" } else { "False" }
}

struct OptionalPythonStr<'a>(Option<&'a str>);

impl<'a> Display for OptionalPythonStr<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        if let Some(value) = self.0 {
            write!(f, "r\"{value}\"")
        } else {
            f.write_str("None")
        }
    }
}

struct PythonListStr<'a>(&'a Vec<Cow<'a, str>>);

impl<'a> Display for PythonListStr<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "[")?;
        for (idx, item) in self.0.iter().enumerate() {
            write!(f, "r\"{item}\"")?;
            if idx < self.0.len() - 1 {
                write!(f, ",")?;
            }
        }
        write!(f, "]")
    }
}

struct PythonListTupleStrStr<'a>(Option<&'a IndexMap<Cow<'a, str>, Cow<'a, str>>>);

impl<'a> Display for PythonListTupleStrStr<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(f, "[")?;
        if let Some(items) = self.0 {
            for (idx, (item1, item2)) in items.iter().enumerate() {
                write!(f, "(r\"{item1}\",r\"{item2}\")")?;
                if idx < items.len() - 1 {
                    write!(f, ",")?;
                }
            }
        }
        write!(f, "]")
    }
}

fn write_main(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    pex_info: &RawPexInfo,
    scripts: &mut Scripts,
    bin_path_override: Option<BinPath>,
) -> anyhow::Result<()> {
    let main_py = venv.prefix().join("__main__.py");
    let mut main_py_fp = File::create_new(&main_py)?;
    write_shebang_bytes(&mut main_py_fp, shebang_interpreter, shebang_arg)?;
    let venv_pex_script = VenvPex::read(scripts)?;
    main_py_fp.write_all(venv_pex_script.contents().as_bytes())?;

    write!(
        main_py_fp,
        "{}",
        format_args!(
            r#"

if __name__ == "__main__":
    boot(
        shebang_python=r"{shebang_python}",
        venv_bin_dir=r"{venv_bin_dir}",
        bin_path=r"{bin_path}",
        strip_pex_env={strip_pex_env},
        bind_resource_paths={bind_resource_paths},
        inject_env={inject_env},
        inject_args={inject_args},
        entry_point={entry_point},
        script={script},
        hermetic_re_exec={hermetic_re_exec},
    )
"#,
            shebang_python = path_as_str(shebang_interpreter)?,
            venv_bin_dir = venv.bin_dir_relpath,
            bin_path = bin_path_override
                .as_ref()
                .unwrap_or_else(|| pex_info.venv_bin_path.as_ref().unwrap_or(&BinPath::False))
                .as_str(),
            strip_pex_env = as_python_bool(pex_info.strip_pex_env.unwrap_or(true)),
            bind_resource_paths = PythonListTupleStrStr(pex_info.bind_resource_paths.as_ref()),
            inject_env = PythonListTupleStrStr(pex_info.inject_env.as_ref()),
            inject_args = PythonListStr(&pex_info.inject_args),
            entry_point = OptionalPythonStr(pex_info.entry_point.as_deref()),
            script = OptionalPythonStr(pex_info.script.as_deref()),
            hermetic_re_exec = OptionalPythonStr(if pex_info.venv_hermetic_scripts {
                Some(venv.interpreter.hermetic_args())
            } else {
                None
            })
        )
    )?;
    mark_executable(main_py_fp.file_mut())?;
    Ok(symlink_or_link_or_copy(
        &main_py,
        venv.prefix().join("pex"),
        true,
    )?)
}

fn write_repl(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    pex: &Path,
    pex_info: &RawPexInfo,
    selected_wheels: Vec<&str>,
    scripts: &mut Scripts,
) -> anyhow::Result<()> {
    let mut pex_repl_py_fp = File::create_new(venv.prefix().join("pex-repl"))?;
    write_shebang_bytes(&mut pex_repl_py_fp, shebang_interpreter, shebang_arg)?;
    let venv_pex_repl_script = VenvPexRepl::read(scripts)?;
    pex_repl_py_fp.write_all(venv_pex_repl_script.contents().as_bytes())?;

    let activation_summary = if selected_wheels.is_empty() {
        format_args!("")
    } else {
        format_args!(
            "{req_count} {requirements} and {dist_count} activated {distributions}",
            req_count = pex_info.requirements.len(),
            requirements = if pex_info.requirements.len() == 1 {
                "requirement"
            } else {
                "requirements"
            },
            dist_count = selected_wheels.len(),
            distributions = if selected_wheels.len() == 1 {
                "distribution"
            } else {
                "distributions"
            }
        )
    };

    struct ActivationDetails<'a> {
        requirements: &'a IndexSet<Cow<'a, str>>,
        selected_wheels: &'a Vec<&'a str>,
    }

    impl<'a> Display for ActivationDetails<'a> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            if !self.requirements.is_empty() {
                writeln!(f, "Requirements:")?;
                for requirement in self.requirements {
                    writeln!(f, "  {requirement}")?;
                }
                writeln!(f, "Activated Distributions:")?;
                for selected_wheel in self.selected_wheels {
                    writeln!(f, "  {selected_wheel}")?;
                }
            }
            Ok(())
        }
    }

    let pex_version =
        if let Some(Value::String(version)) = pex_info.build_properties.get("pex_version") {
            version
        } else {
            "(unknown version)"
        };
    write!(
        pex_repl_py_fp,
        "{}",
        format_args!(
            r#"


_PS1 = "{ps1}"
_PS2 = "{ps2}"
_PEX_VERSION = "{pex_version}"
_SEED_PEX = r"{seed_pex}"
_ACTIVATION_SUMMARY = "{activation_summary}"
_ACTIVATION_DETAILS = """{activation_details}"""


if __name__ == "__main__":
    import os

    _create_pex_repl(
        ps1=_PS1,
        ps2=_PS2,
        pex_version=_PEX_VERSION,
        pex_info=os.path.join(os.path.dirname(__file__), "PEX-INFO"),
        seed_pex=_SEED_PEX,
        activation_summary=_ACTIVATION_SUMMARY,
        activation_details=_ACTIVATION_DETAILS,
        history=os.environ.get("PEX_INTERPRETER_HISTORY", "0").lower() in ("1", "true"),
        history_file=os.environ.get("PEX_INTERPRETER_HISTORY_FILE")
    )()
"#,
            ps1 = ">>>",
            ps2 = "...",
            seed_pex = path_as_str(pex)?,
            activation_summary = activation_summary,
            activation_details = ActivationDetails {
                requirements: &pex_info.requirements,
                selected_wheels: &selected_wheels
            },
        )
    )?;
    mark_executable(pex_repl_py_fp.file_mut())?;

    Ok(())
}
