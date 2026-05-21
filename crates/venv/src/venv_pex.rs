// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::io::{BufReader, Cursor, ErrorKind, Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{env, io};

use anyhow::{anyhow, bail};
use cache::{Fingerprint, default_digest, fingerprint_file};
use fs_err as fs;
use fs_err::File;
use indexmap::IndexMap;
use logging_timer::time;
use pex::{
    BinPath,
    EntryPoint,
    EntryPoints,
    Layout,
    MetadataDirs,
    Pex,
    RawPexInfo,
    Record,
    ResolvedWheel,
    WheelDir,
    WheelLayout,
    collect_loose_user_source,
    collect_zipped_user_source_indexes,
    filter_zipped_user_source,
};
use platform::{Perms, mark_executable, path_as_bytes, path_as_str, symlink_or_link_or_copy};
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use scripts::{Scripts, VenvPex, VenvPexRepl};
use serde_json::Value;
use zip::ZipArchive;

use crate::Provenance;
use crate::virtualenv::Virtualenv;

#[derive(Copy, Clone, Eq, PartialEq)]
pub enum InstallScope {
    All,
    Deps,
    Srcs,
}

impl InstallScope {
    pub fn as_str(&self) -> &'static str {
        match self {
            InstallScope::All => "all",
            InstallScope::Deps => "deps",
            InstallScope::Srcs => "srcs",
        }
    }
}

fn populate_from_loose_pex<'a>(
    venv: &Virtualenv,
    loose_pex: &'a Pex<'a>,
    resolved_wheels: &IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    scope: InstallScope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    if matches!(scope, InstallScope::All | InstallScope::Deps) {
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
                populate_wheel_dir(venv, &wheel_paths.path, &wheel_details, provenance.clone())
            })?;
    }
    if matches!(scope, InstallScope::All | InstallScope::Srcs) {
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
    let deps_dir = pex.path.join(".deps");
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

fn spread(
    pex: &Pex,
    wheel: ResolvedWheel,
    virtualenv: &Virtualenv,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let entry_points = virtualenv
        .site_packages_path(wheel.dist_info_dir().as_path())
        .join("entry_points.txt");
    if entry_points.exists() {
        install_scripts(
            pex,
            &entry_points,
            virtualenv,
            shebang_interpreter,
            shebang_arg,
            provenance,
        )?;
    }
    Ok(())
}

fn install_scripts(
    pex: &Pex,
    entry_points_txt: &Path,
    virtualenv: &Virtualenv,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let entry_points = EntryPoints::load(File::open(entry_points_txt)?)?;
    if entry_points.is_empty() {
        return Ok(());
    }

    for (name, entry_point, is_gui) in entry_points
        .console_scripts()
        .map(|(name, entry_point)| (name, entry_point, false))
        .chain(
            entry_points
                .gui_scripts()
                .map(|(name, entry_point)| (name, entry_point, true)),
        )
    {
        let script_path = virtualenv
            .script_path(name)
            .with_extension(env::consts::EXE_EXTENSION);
        let script_contents =
            create_script_contents(shebang_interpreter, shebang_arg, entry_point)?;
        let script_file = match File::create_new(&script_path) {
            Ok(script_file) => {
                provenance.record(entry_point, script_path);
                script_file
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                let fingerprint =
                    Fingerprint::try_from(BufReader::new(script_contents.as_bytes()))?;
                provenance.record_collision(
                    entry_point,
                    fingerprint,
                    script_contents.len(),
                    script_path,
                );
                return Ok(());
            }
            Err(err) => bail!("{err}"),
        };
        write_script(
            pex,
            shebang_interpreter,
            script_file,
            script_contents,
            is_gui,
        )?;
    }
    Ok(())
}

#[cfg(unix)]
fn write_script(
    _pex: &Pex,
    _shebang_interpreter: &Path,
    mut script_file: File,
    script_contents: String,
    _is_gui: bool,
) -> anyhow::Result<()> {
    script_file.write_all(script_contents.as_bytes())?;
    mark_executable(script_file.file_mut())?;
    Ok(())
}

#[cfg(windows)]
fn write_script(
    pex: &Pex,
    shebang_interpreter: &Path,
    script_file: File,
    script_contents: String,
    is_gui: bool,
) -> anyhow::Result<()> {
    python_proxy::create(
        python_proxy::ProxySource::Pex(pex),
        shebang_interpreter,
        script_file.into_file(),
        Some(script_contents),
        is_gui,
    )
}

fn create_script_contents(
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    entry_point: &EntryPoint,
) -> anyhow::Result<String> {
    struct RenderShebang<'a>(&'a str, Option<&'a str>);
    impl<'a> Display for RenderShebang<'a> {
        fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
            write!(f, "#!{interpreter}", interpreter = self.0)?;
            if let Some(args) = self.1 {
                write!(f, " {args}")?;
            }
            Ok(())
        }
    }
    let shebang = RenderShebang(path_as_str(shebang_interpreter)?, shebang_arg);

    match entry_point {
        EntryPoint::Callable {
            module: modname,
            attribute_chain: attrs,
        } => {
            struct RenderAttrsTuple<'a>(Vec<&'a str>);
            impl<'a> Display for RenderAttrsTuple<'a> {
                fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                    write!(f, "(")?;
                    for attr in &self.0 {
                        write!(f, "\"{attr}\",")?;
                    }
                    write!(f, ")")?;
                    Ok(())
                }
            }

            Ok(format!(
                r##"{shebang}
# -*- coding: utf-8 -*-
import importlib
import sys

entry_point = importlib.import_module("{modname}")
for attr in {attrs_tuple}:
    entry_point = getattr(entry_point, attr)

if __name__ == "__main__":
    sys.exit(entry_point())
"##,
                attrs_tuple = RenderAttrsTuple(attrs.split(".").collect())
            ))
        }
        EntryPoint::Module(modname) => Ok(format!(
            r##"{shebang}
# -*- coding: utf-8 -*-
import runpy
import sys

if __name__ == "__main__":
    runpy.run_module("{modname}", run_name="__main__", alter_sys=True)
    sys.exit(0)
"##,
        )),
    }
}

struct WheelDetails<'a> {
    project_name: &'a str,
    data_dir: WheelDir<'a>,
    pex_info_dir: WheelDir<'a>,
    stash_dir: Option<PathBuf>,
    legacy_bin_dir: bool,
}

impl<'a> WheelDetails<'a> {
    fn new(
        project_name: &'a str,
        metadata_dirs: &'a MetadataDirs,
        layout: Option<WheelLayout>,
        legacy_bin_dir: bool,
    ) -> Self {
        let stash_dir = if let Some(layout) = layout {
            Some(layout.stash_dir)
        } else {
            None
        };
        Self {
            project_name,
            data_dir: metadata_dirs.data_dir(),
            pex_info_dir: metadata_dirs.pex_info_dir(),
            stash_dir,
            legacy_bin_dir,
        }
    }
}

fn calculate_spread_path(
    venv: &Virtualenv,
    wheel_details: &WheelDetails,
    dst_rel_path: &Path,
) -> anyhow::Result<Option<PathBuf>> {
    if let Ok(data_dir_relpath) = dst_rel_path.strip_prefix(wheel_details.data_dir.as_path()) {
        let mut components = data_dir_relpath.components();
        if let Some(paths_key) = components.next() {
            let key = paths_key.as_os_str().to_str().ok_or_else(|| {
                anyhow!(
                    "The first component of .data/ dir path {path} was not a UTF-8 key into \
                    sysconfig paths.",
                    path = wheel_details.data_dir
                )
            })?;
            if key == "headers" {
                // N.B.: You'd think sysconfig_paths["include"] would be the right answer here but
                // both `pip`, and by emulation, `uv pip`, use:
                //   `<venv>/include/site/pythonX.Y/<project name>/`.
                //
                // The "mess" is admitted and described at length here:
                // + https://discuss.python.org/t/clarification-on-a-wheels-header-data/9305
                // + https://discuss.python.org/t/deprecating-the-headers-wheel-data-key/23712
                //
                // Both discussions died out with no path resolved to clean up the mess.
                Ok(Some(
                    venv.prefix()
                        .join("include")
                        .join("site")
                        .join(format!(
                            "python{major}.{minor}",
                            major = venv.interpreter.raw().version.major,
                            minor = venv.interpreter.raw().version.minor
                        ))
                        .join(wheel_details.project_name)
                        .join(components.collect::<PathBuf>()),
                ))
            } else if let Some(spread_path) = venv.interpreter.raw().paths.get(key) {
                Ok(Some(spread_path.join(components.collect::<PathBuf>())))
            } else {
                bail!(
                    "Wheel for {project_name} has unknown .data dir entry {key}: \
                    {data_dir_relpath}",
                    project_name = wheel_details.project_name,
                    data_dir_relpath = data_dir_relpath.display()
                )
            }
        } else {
            Ok(None)
        }
    } else if let Some(stash_dir) = wheel_details.stash_dir.as_deref()
        && let Ok(stash_rel_path) = dst_rel_path.strip_prefix(stash_dir)
    {
        let mut components = stash_rel_path.components();
        if let Some(paths_key) = components.next() {
            let key = paths_key.as_os_str().to_str().ok_or_else(|| {
                anyhow!(
                    "The first component of {stash_dir} dir path {path} was not a UTF-8 key into \
                    sysconfig paths.",
                    stash_dir = stash_dir.display(),
                    path = wheel_details.data_dir
                )
            })?;
            if ["bin", "Scripts"].into_iter().any(|dir| key == dir) {
                Ok(Some(venv.script_path(components.collect::<PathBuf>())))
            } else if key == "include" {
                Ok(Some(
                    venv.prefix()
                        .components()
                        .chain(stash_rel_path.components())
                        .collect(),
                ))
            } else if let Some(spread_path) = venv.interpreter.raw().paths.get(key) {
                Ok(Some(spread_path.components().chain(components).collect()))
            } else {
                bail!(
                    "Wheel for {project_name} has unknown {stash_dir} dir entry {key}: \
                    {stash_rel_path}",
                    project_name = wheel_details.project_name,
                    stash_dir = stash_dir.display(),
                    stash_rel_path = stash_rel_path.display()
                )
            }
        } else {
            Ok(None)
        }
    } else if wheel_details.stash_dir.is_some()
        && (dst_rel_path == WheelLayout::file_name()
            || dst_rel_path
                .components()
                .next()
                .map(|first| wheel_details.pex_info_dir.as_path() == Path::new(first.as_os_str()))
                .unwrap_or_default())
    {
        Ok(None)
    } else if wheel_details.legacy_bin_dir
        && let Ok(script) = dst_rel_path.strip_prefix("bin")
    {
        Ok(Some(venv.script_path(script)))
    } else {
        Ok(Some(venv.site_packages_path(dst_rel_path)))
    }
}

fn populate_wheel_dir(
    venv: &Virtualenv,
    wheel: &Path,
    wheel_details: &WheelDetails,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let wheel_contents = walkdir::WalkDir::new(wheel)
        .min_depth(1)
        .into_iter()
        .filter_map(|entry| {
            match entry {
                Ok(entry) => {
                    let dst_rel_path = entry.path().strip_prefix(wheel).expect(
                        "Walked sub-paths of a wheel dir should be child paths of the wheel dir.",
                    );
                    // TODO: Experiment with creating parent dirs as needed here in this synchronous loop
                    //  just 1x to save on syscall overhead in the parallel loop over contained files below.
                    match calculate_spread_path(venv, wheel_details, dst_rel_path) {
                        Ok(dst) => dst.map(|dst| Ok((entry, dst))),
                        Err(err) => Some(Err(err)),
                    }
                }
                Err(err) => Some(Err(anyhow!("{err}"))),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    wheel_contents.into_par_iter().try_for_each(|(src, dst)| {
        if src.path().is_dir() {
            fs::create_dir_all(&dst)?;
        } else {
            if let Some(parent_dir) = dst.parent() {
                fs::create_dir_all(parent_dir)?;
            }
            match File::create_new(&dst) {
                Ok(mut dst_file) => {
                    provenance.record(src.path().display(), dst);
                    let mut src = File::open(src.path())?;
                    io::copy(&mut src, &mut dst_file)?;
                    platform::set_permissions(
                        dst_file.file_mut(),
                        Perms::Perms(src.metadata()?.permissions()),
                    )?;
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    let (size, fingerprint) = fingerprint_file(src.path(), default_digest())?;
                    provenance.record_collision(src.path().display(), fingerprint, size, dst);
                }
                Err(err) => bail!("{err}"),
            }
        }
        Ok(())
    })
}

fn populate_from_packed_pex<'a>(
    venv: &Virtualenv,
    packed_pex: &'a Pex<'a>,
    resolved_wheels: &IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    scope: InstallScope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    if matches!(scope, InstallScope::All | InstallScope::Deps) {
        collect_wheels_from_directory_pex(packed_pex, resolved_wheels)?
            .into_par_iter()
            .try_for_each(|(project_name, wheel_paths)| {
                populate_whl_zip(
                    venv,
                    &wheel_paths.path,
                    project_name,
                    wheel_paths.metadata_dirs,
                    provenance.clone(),
                )
            })?;
    }
    if matches!(scope, InstallScope::All | InstallScope::Srcs) {
        populate_user_code_from_directory_pex(packed_pex, venv, populate_pex_info, provenance)?;
    }
    Ok(())
}

fn populate_whl_zip(
    venv: &Virtualenv,
    wheel: &Path,
    project_name: &str,
    metadata_dirs: &MetadataDirs,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let mut whl_zip = ZipArchive::new(File::open(wheel)?.into_file())?;
    let metadata = whl_zip.metadata();
    let layout = if let Ok(layout_file) = whl_zip.by_name(WheelLayout::file_name()) {
        Some(WheelLayout::read(layout_file)?)
    } else {
        None
    };
    let record_name = format!(
        "{dist_info_dir}/RECORD",
        dist_info_dir = metadata_dirs.dist_info_dir()
    );
    let record = Record::read(Cursor::new(io::read_to_string(
        whl_zip.by_name(&record_name)?,
    )?))?;
    let wheel_details = WheelDetails::new(
        project_name,
        metadata_dirs,
        layout,
        record.wheel_has_bin_dir(),
    );
    (0..whl_zip.len()).into_par_iter().try_for_each(|index| {
        let zip_fp = File::open(wheel)?;
        let mut zip = unsafe { ZipArchive::unsafe_new_with_metadata(zip_fp, metadata.clone()) };
        extract_whl_idx(
            venv,
            &wheel_details,
            index,
            &mut zip,
            wheel.display(),
            provenance.clone(),
        )
    })
}

fn populate_user_code_from_directory_pex<'a>(
    directory_pex: &'a Pex<'a>,
    venv: &Virtualenv,
    populate_pex_info: bool,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let user_code = collect_loose_user_source(directory_pex.path)?;
    user_code.into_par_iter().try_for_each(|entry| {
        let dst_path =
            venv.site_packages_path(entry.path().strip_prefix(directory_pex.path).expect(
                "Walked directory PEX paths should be child paths of the directory PEX root dir.",
            ));
        if entry.file_type().is_dir() {
            fs::create_dir_all(dst_path)?;
        } else {
            if let Some(parent) = dst_path.parent() {
                fs::create_dir_all(parent)?;
            }
            match File::create_new(&dst_path) {
                Ok(mut dst) => {
                    provenance.record(entry.path().display(), dst_path);
                    io::copy(&mut File::open(entry.path())?, &mut dst)?;
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    let (size, fingerprint) = fingerprint_file(entry.path(), default_digest())?;
                    provenance.record_collision(
                        entry.path().display(),
                        fingerprint,
                        size,
                        dst_path,
                    );
                }
                Err(err) => bail!("{err}"),
            }
        }
        Ok(())
    })?;
    if populate_pex_info {
        fs::copy(
            directory_pex.path.join("PEX-INFO"),
            venv.prefix().join("PEX-INFO"),
        )?;
    }
    Ok(())
}

fn populate_from_zip_app_with_whl_deps<'a>(
    venv: &Virtualenv,
    zip_app_pex: &'a Pex<'a>,
    resolved_wheels: &IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    scope: InstallScope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let pex_zip = ZipArchive::new(File::open(zip_app_pex.path)?)?;
    let metadata = pex_zip.metadata();
    if matches!(scope, InstallScope::All | InstallScope::Deps) {
        let wheel_file_names = resolved_wheels.into_iter().collect::<Vec<_>>();
        wheel_file_names
            .into_par_iter()
            .try_for_each(|(wheel_file_name, wheel)| {
                let zip_fp = File::open(zip_app_pex.path)?;
                let mut zip =
                    unsafe { ZipArchive::unsafe_new_with_metadata(zip_fp, metadata.clone()) };
                let whl_name = [".deps", wheel_file_name].join("/");
                let whl_file = zip.by_name_seek(&whl_name)?;
                let mut whl_zip = ZipArchive::new(whl_file)?;
                let whl_zip_metadata = whl_zip.metadata();
                let layout = if let Ok(layout_file) = whl_zip.by_name(WheelLayout::file_name()) {
                    Some(WheelLayout::read(layout_file)?)
                } else {
                    None
                };
                let record_name = format!(
                    "{dist_info_dir}/RECORD",
                    dist_info_dir = wheel.dist_info_dir()
                );
                let record = Record::read(Cursor::new(io::read_to_string(
                    whl_zip.by_name(&record_name)?,
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
                        &wheel_details,
                        index,
                        &mut whl_zip,
                        format!("{zip}/{whl_name}", zip = zip_app_pex.path.display()),
                        provenance.clone(),
                    )
                })
            })?;
    }
    if matches!(scope, InstallScope::All | InstallScope::Srcs) {
        let extract_indexes = collect_zipped_user_source_indexes(&pex_zip);
        extract_indexes
            .into_par_iter()
            .try_for_each(|index| -> anyhow::Result<()> {
                let zip_fp = File::open(zip_app_pex.path)?;
                let mut zip =
                    unsafe { ZipArchive::unsafe_new_with_metadata(zip_fp, metadata.clone()) };
                extract_idx(
                    venv,
                    index,
                    None,
                    &mut zip,
                    zip_app_pex.path.display(),
                    provenance.clone(),
                )?;
                Ok(())
            })?;
        if populate_pex_info {
            let mut pex_zip = ZipArchive::new(File::open(zip_app_pex.path)?)?;
            let mut pex_info_src_fp = pex_zip.by_name("PEX-INFO")?;
            let mut pex_info_dst_fp = File::create_new(venv.prefix().join("PEX-INFO"))?;
            io::copy(&mut pex_info_src_fp, &mut pex_info_dst_fp)?;
        }
    }
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

fn populate_from_zip_app<'a>(
    venv: &Virtualenv,
    zip_app_pex: &'a Pex<'a>,
    resolved_wheels: &IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    scope: InstallScope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let mut pex_zip = ZipArchive::new(File::open(zip_app_pex.path)?)?;
    let metadata = pex_zip.metadata();
    let dep_filter = DepFilter(resolved_wheels);
    if matches!(scope, InstallScope::All | InstallScope::Deps) {
        let data_dirs = resolved_wheels
            .iter()
            .map(|(file_name, wheel)| {
                let layout: Option<WheelLayout> = if let Ok(layout_file) =
                    pex_zip.by_name(&format!(
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
                    pex_zip.by_name(&record_name)?,
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
                    wheel_details,
                    index,
                    &mut zip,
                    zip_app_pex.path.display(),
                    provenance.clone(),
                )?;
                Ok(())
            },
        )?;
    }
    if matches!(scope, InstallScope::All | InstallScope::Srcs) {
        let extract_indexes = pex_zip
            .file_names()
            .enumerate()
            .filter_map(|(idx, name)| {
                if filter_zipped_user_source(name) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect::<Vec<_>>();
        extract_indexes
            .into_par_iter()
            .try_for_each(|index| -> anyhow::Result<()> {
                let zip_fp = File::open(zip_app_pex.path)?;
                let mut zip =
                    unsafe { ZipArchive::unsafe_new_with_metadata(zip_fp, metadata.clone()) };
                extract_idx(
                    venv,
                    index,
                    None,
                    &mut zip,
                    zip_app_pex.path.display(),
                    provenance.clone(),
                )?;
                Ok(())
            })?;
    }
    if populate_pex_info && matches!(scope, InstallScope::All | InstallScope::Srcs) {
        let mut pex_info_src_fp = pex_zip.by_name("PEX-INFO")?;
        let mut pex_info_dst_fp = File::create_new(venv.prefix().join("PEX-INFO"))?;
        io::copy(&mut pex_info_src_fp, &mut pex_info_dst_fp)?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[time("debug", "{}")]
pub fn populate_user_code_and_wheels<'a>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    pex: &'a Pex<'a>,
    resolved_wheels: IndexMap<&'a str, ResolvedWheel<'a>>,
    populate_pex_info: bool,
    scope: InstallScope,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    match pex.layout {
        Layout::Loose => populate_from_loose_pex(
            venv,
            pex,
            &resolved_wheels,
            populate_pex_info,
            scope,
            provenance.clone(),
        )?,
        Layout::Packed => populate_from_packed_pex(
            venv,
            pex,
            &resolved_wheels,
            populate_pex_info,
            scope,
            provenance.clone(),
        )?,
        Layout::ZipApp => {
            if pex.info.raw().deps_are_wheel_files {
                populate_from_zip_app_with_whl_deps(
                    venv,
                    pex,
                    &resolved_wheels,
                    populate_pex_info,
                    scope,
                    provenance.clone(),
                )?
            } else {
                populate_from_zip_app(
                    venv,
                    pex,
                    &resolved_wheels,
                    populate_pex_info,
                    scope,
                    provenance.clone(),
                )?
            }
        }
    }
    if matches!(scope, InstallScope::All | InstallScope::Deps) {
        resolved_wheels
            .into_values()
            .collect::<Vec<_>>()
            .into_par_iter()
            .try_for_each(|resolved_wheel| {
                spread(
                    pex,
                    resolved_wheel,
                    venv,
                    shebang_interpreter,
                    shebang_arg,
                    provenance.clone(),
                )
            })?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
#[time("debug", "{}")]
pub fn populate<'a>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    pex: &'a Pex<'a>,
    resolved_wheels: IndexMap<&'a str, ResolvedWheel<'a>>,
    scripts: &mut Scripts,
    bin_path_override: Option<BinPath>,
    scope: InstallScope,
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
        scope,
        provenance,
    )?;
    if matches!(scope, InstallScope::All | InstallScope::Srcs) {
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

fn extract_whl_idx<R>(
    venv: &Virtualenv,
    wheel_details: &WheelDetails,
    index: usize,
    zip: &mut ZipArchive<R>,
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
        calculate_spread_path(venv, wheel_details, &dst_rel_path)?
    };
    if dst_path.is_some() {
        extract_idx(venv, index, dst_path, zip, source, provenance)
    } else {
        Ok(())
    }
}

fn extract_idx<R>(
    venv: &Virtualenv,
    index: usize,
    dst_path: Option<PathBuf>,
    zip: &mut ZipArchive<R>,
    source: impl Display,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()>
where
    R: Read + Seek,
{
    let mut zip_file = zip.by_index(index)?;
    let dst_path = dst_path.unwrap_or_else(|| {
        venv.site_packages_path(zip_file.name().split("/").collect::<PathBuf>())
    });
    if zip_file.is_dir() {
        fs::create_dir_all(dst_path)?;
    } else {
        if let Some(parent_dir) = dst_path.parent() {
            fs::create_dir_all(parent_dir)?;
        }
        match File::create_new(&dst_path) {
            Ok(mut dst_file) => {
                provenance.record(format!("{source}/{name}", name = zip_file.name()), dst_path);
                io::copy(&mut zip_file, &mut dst_file)?;
                if let Some(mode) = zip_file.unix_mode() {
                    platform::set_permissions(dst_file.file_mut(), Perms::Mode(mode))?;
                }
            }
            Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                let size = usize::try_from(zip_file.size())?;
                let name = zip_file.name().to_string();
                let fingerprint = Fingerprint::try_from(BufReader::new(zip_file))?;
                provenance.record_collision(
                    format!("{source}/{name}"),
                    fingerprint,
                    size,
                    dst_path,
                );
            }
            Err(err) => bail!("{err}"),
        }
    }
    Ok(())
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

struct PythonListStr<'a>(&'a Vec<&'a str>);

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

struct PythonListTupleStrStr<'a>(Option<&'a IndexMap<&'a str, &'a str>>);

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
    let pex_extra_sys_path_pth = venv.site_packages_path("PEX_EXTRA_SYS_PATH.pth");
    let mut pex_extra_sys_path_pth_fp = File::create_new(&pex_extra_sys_path_pth)?;
    for env_var in ["PEX_EXTRA_SYS_PATH", "__PEX_EXTRA_SYS_PATH__"].into_iter() {
        // # N.B.: .pth import lines must be single lines:
        // https://docs.python.org/3/library/site.html
        writeln!(
            pex_extra_sys_path_pth_fp,
            "import os, sys; \
            sys.path.extend(\
            entry \
            for entry in os.environ.get('{env_var}', '').split(os.pathsep) \
            if entry\
            )",
        )?;
    }

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
            entry_point = OptionalPythonStr(pex_info.entry_point),
            script = OptionalPythonStr(pex_info.script),
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
        requirements: &'a Vec<&'a str>,
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
