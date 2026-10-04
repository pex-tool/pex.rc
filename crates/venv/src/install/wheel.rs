// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};
use std::io::{BufReader, Cursor, ErrorKind, Read, Seek, Write as _};
use std::ops::Deref;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::{env, io};

use anyhow::{anyhow, bail};
use cache::{Fingerprint, default_digest, fingerprint_file};
use fs_err as fs;
use fs_err::File;
use platform::{Perms, mark_executable, path_as_str};
use python_platform::PythonVersion;
use python_proxy::ProxySource;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use wheel::{EntryPoint, EntryPoints, MetadataDirs, Record, WheelDir, WheelLayout};
use zip::ZipArchive;
use zip_ext::ZipArchiveExt;

use crate::script::PythonScript;
use crate::{Provenance, Virtualenv};

pub(crate) fn install_scripts(
    entry_points_txt: &Path,
    virtualenv: &Virtualenv,
    shebang_interpreter: &Path,
    shebang_arg: Option<&str>,
    proxy_source: &ProxySource,
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
        let script_path = virtualenv.script_path(name);
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
            proxy_source,
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
    _proxy_source: &ProxySource,
    _shebang_interpreter: &Path,
    mut script_file: File,
    script_contents: impl AsRef<[u8]>,
    _is_gui: bool,
) -> anyhow::Result<()> {
    script_file.write_all(script_contents.as_ref())?;
    mark_executable(script_file.file_mut())?;
    Ok(())
}

#[cfg(windows)]
fn write_script(
    proxy_source: &ProxySource,
    shebang_interpreter: &Path,
    script_file: File,
    script_contents: impl AsRef<[u8]>,
    is_gui: bool,
) -> anyhow::Result<()> {
    python_proxy::create(
        proxy_source,
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

pub(crate) struct WheelDetails<'a> {
    project_name: &'a str,
    data_dir: WheelDir<'a>,
    pex_info_dir: WheelDir<'a>,
    stash_dir: Option<PathBuf>,
    legacy_bin_dir: bool,
}

impl<'a> WheelDetails<'a> {
    pub(crate) fn new(
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

pub(crate) enum Spread {
    Move(PathBuf),
    Script(PathBuf),
}

impl From<Spread> for PathBuf {
    fn from(value: Spread) -> Self {
        match value {
            Spread::Move(path) | Spread::Script(path) => path,
        }
    }
}

impl AsRef<Path> for Spread {
    fn as_ref(&self) -> &Path {
        match self {
            Self::Move(path) | Self::Script(path) => path,
        }
    }
}

impl Deref for Spread {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.as_ref()
    }
}

pub(crate) fn calculate_spread(
    venv: &Virtualenv,
    wheel_details: &WheelDetails,
    dst_rel_path: &Path,
) -> anyhow::Result<Option<Spread>> {
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
                Ok(Some(Spread::Move(
                    venv.headers_prefix()
                        .join(wheel_details.project_name)
                        .join(components.collect::<PathBuf>()),
                )))
            } else if let Some(spread_path) = venv.interpreter.details.paths.get(key) {
                if key == "scripts" {
                    Ok(Some(Spread::Script(
                        spread_path.join(
                            components
                                .collect::<PathBuf>()
                                .with_extension(env::consts::EXE_EXTENSION),
                        ),
                    )))
                } else {
                    Ok(Some(Spread::Move(
                        spread_path.join(components.collect::<PathBuf>()),
                    )))
                }
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
                Ok(Some(Spread::Script(
                    venv.script_path(components.collect::<PathBuf>()),
                )))
            } else if key == "include" {
                Ok(Some(Spread::Move(
                    venv.prefix()
                        .components()
                        .chain(stash_rel_path.components())
                        .collect(),
                )))
            } else if let Some(spread_path) = venv.interpreter.details.paths.get(key) {
                if key == "scripts" {
                    Ok(Some(Spread::Script(
                        spread_path
                            .components()
                            .chain(components)
                            .collect::<PathBuf>()
                            .with_extension(env::consts::EXE_EXTENSION),
                    )))
                } else {
                    Ok(Some(Spread::Move(
                        spread_path.components().chain(components).collect(),
                    )))
                }
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
        Ok(Some(Spread::Script(venv.script_path(script))))
    } else {
        Ok(Some(Spread::Move(venv.site_packages_path(dst_rel_path))))
    }
}

#[allow(clippy::too_many_arguments)]
pub fn populate_whl_zip(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    wheel: &Path,
    whl_zip: Option<ZipArchive<std::fs::File>>,
    project_name: &str,
    metadata_dirs: &MetadataDirs,
    proxy_source: &ProxySource,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()> {
    let mut whl_zip = if let Some(whl_zip) = whl_zip {
        whl_zip
    } else {
        ZipArchive::new(File::open(wheel)?.into_file())?
    };
    let metadata = whl_zip.metadata();
    let layout = if let Ok(layout_file) = whl_zip.by_name_ex(WheelLayout::file_name()) {
        Some(WheelLayout::read(layout_file)?)
    } else {
        None
    };
    let record_name = format!(
        "{dist_info_dir}/RECORD",
        dist_info_dir = metadata_dirs.dist_info_dir()
    );
    let record = Record::read(Cursor::new(io::read_to_string(
        whl_zip.by_name_ex(&record_name)?,
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
        let file_name = zip
            .name_for_index(index)
            .expect("Each wheel entry has a name");
        let spread_dest = calculate_spread(venv, &wheel_details, Path::new(file_name))?;
        extract_idx(
            venv,
            shebang_interpreter,
            index,
            spread_dest,
            &mut zip,
            proxy_source,
            wheel.display(),
            provenance.clone(),
        )
    })
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn extract_idx<R>(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    index: usize,
    spread_dst: Option<Spread>,
    zip: &mut ZipArchive<R>,
    proxy_source: &ProxySource,
    source: impl Display,
    provenance: Arc<Provenance>,
) -> anyhow::Result<()>
where
    R: Read + Seek,
{
    let mut zip_file = zip.by_index(index)?;
    let spread_dst = spread_dst.unwrap_or_else(|| {
        Spread::Move(venv.site_packages_path(zip_file.name().split("/").collect::<PathBuf>()))
    });
    if zip_file.is_dir() {
        fs::create_dir_all(spread_dst)?;
    } else {
        if let Some(parent_dir) = spread_dst.parent() {
            fs::create_dir_all(parent_dir)?;
        }
        match File::create_new(spread_dst.as_ref()) {
            Ok(mut dst_file) => {
                let source = format!("{source}/{name}", name = zip_file.name());
                let perms = zip_file.unix_mode().map(Perms::Mode);
                match spread_dst {
                    Spread::Move(dst) => {
                        provenance.record(source, dst);
                        io::copy(&mut zip_file, &mut dst_file)?;
                        if let Some(perms) = perms {
                            platform::set_permissions(dst_file.file_mut(), perms)?;
                        }
                    }
                    Spread::Script(dst) => {
                        provenance.record(source, dst);
                        let python_version = venv.interpreter.details.version;
                        let size = zip_file.size();
                        let mut script_contents = tempfile::spooled_tempfile(10 * 1_024);
                        io::copy(&mut zip_file, &mut script_contents)?;
                        script_contents.rewind()?;
                        reify_script(
                            shebang_interpreter,
                            proxy_source,
                            dst_file,
                            python_version,
                            &mut script_contents,
                            perms,
                            size,
                        )?;
                    }
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
                    spread_dst.into(),
                );
            }
            Err(err) => bail!("{err}"),
        }
    }
    Ok(())
}

pub(crate) fn populate_wheel_dir(
    venv: &Virtualenv,
    shebang_interpreter: &Path,
    wheel: &Path,
    wheel_details: &WheelDetails,
    proxy_source: &ProxySource,
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
                    match calculate_spread(venv, wheel_details, dst_rel_path) {
                        Ok(dst) => dst.map(|dst| Ok((entry, dst))),
                        Err(err) => Some(Err(err)),
                    }
                }
                Err(err) => Some(Err(anyhow!("{err}"))),
            }
        })
        .collect::<Result<Vec<_>, _>>()?;
    wheel_contents.into_par_iter().try_for_each(|(src, dst)| {
        if src.file_type().is_dir() {
            fs::create_dir_all(&dst)?;
        } else {
            if let Some(parent_dir) = dst.parent() {
                fs::create_dir_all(parent_dir)?;
            }
            match File::create_new(dst.as_ref()) {
                Ok(mut dst_file) => {
                    let python_version = venv.interpreter.details.version;
                    let source = src.path().display();
                    let mut src = File::open(src.path())?;
                    let metadata = src.metadata()?;
                    let perms = Some(Perms::Perms(metadata.permissions()));
                    let size = metadata.len();
                    match dst {
                        Spread::Move(dst) => {
                            provenance.record(source, dst);
                            io::copy(&mut src, &mut dst_file)?;
                            if let Some(perms) = perms {
                                platform::set_permissions(dst_file.file_mut(), perms)?;
                            }
                        }
                        Spread::Script(dst) => {
                            provenance.record(source, dst);
                            reify_script(
                                shebang_interpreter,
                                proxy_source,
                                dst_file,
                                python_version,
                                &mut src,
                                perms,
                                size,
                            )?;
                        }
                    };
                }
                Err(err) if err.kind() == ErrorKind::AlreadyExists => {
                    let (size, fingerprint) = fingerprint_file(src.path(), default_digest())?;
                    provenance.record_collision(
                        src.path().display(),
                        fingerprint,
                        size,
                        dst.into(),
                    );
                }
                Err(err) => bail!("{err}"),
            }
        }
        Ok(())
    })
}

fn reify_script(
    shebang_interpreter: &Path,
    proxy_source: &ProxySource,
    mut dst_file: File,
    python_version: PythonVersion,
    src: &mut (impl Read + Seek),
    perms: Option<Perms>,
    size: u64,
) -> anyhow::Result<()> {
    if let Some(python_script) = PythonScript::detect(src, size, python_version)? {
        let reified_script = python_script.reified_contents(shebang_interpreter, src)?;
        write_script(
            proxy_source,
            shebang_interpreter,
            dst_file,
            reified_script,
            python_script.is_windowed,
        )?;
    } else {
        src.rewind()?;
        io::copy(src, &mut dst_file)?;
        if let Some(perms) = perms {
            platform::set_permissions(dst_file.file_mut(), perms)?;
        }
    }
    Ok(())
}
