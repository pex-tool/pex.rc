// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;
use std::{env, mem};

use anyhow::{anyhow, bail};
use cache::{CacheDir, CacheRoot, HashOptions, Key, atomic_dir};
use fs_err as fs;
use interpreter::SearchPath;
use itertools::Itertools;
use log::{info, warn};
use logging_timer::time;
use pex::{InheritPath, Pex, PexPath};
use python_proxy::ProxySource;
use regex::bytes::Regex;
use venv::{InstallScope, Linker, Provenance, Virtualenv, populate, populate_user_code_and_wheels};

struct PythonProxyLinker<'a>(&'a Pex<'a>);

impl<'a> Linker for PythonProxyLinker<'a> {
    #[cfg(unix)]
    fn link(&self, dest: &Path, interpreter: Option<&Path>, is_gui: bool) -> anyhow::Result<()> {
        let file_name = dest.file_name().ok_or_else(|| {
            anyhow!(
                "The destination for the python-proxy doesn't have a file name: {path}",
                path = dest.display()
            )
        })?;
        let venv_python_file_name = format!(
            ".{file_name}",
            file_name = file_name.to_str().ok_or_else(|| anyhow!(
                "The destination for the python-proxy is not a UTF-8 file name: {file_name}",
                file_name = file_name.display()
            ))?
        );

        let mut key = Key::default();
        key.property("proxied-python", &venv_python_file_name);
        let fingerprint = key.fingerprint();
        let python_proxy = CacheDir::PythonProxy
            .path()?
            .join(fingerprint.base64_digest());

        cache::atomic_file(&python_proxy, |file| {
            python_proxy::create(
                ProxySource::Pex(self.0),
                venv_python_file_name.as_ref(),
                file.into_file(),
                None,
                is_gui,
            )
        })?;

        if let Some(interpreter) = interpreter {
            platform::symlink_or_link_or_copy(
                interpreter,
                dest.with_file_name(&venv_python_file_name),
                false,
            )?;
        } else {
            let orig_python = dest.with_file_name(&venv_python_file_name);
            fs::rename(dest, &orig_python)?;
        }
        platform::symlink_or_link_or_copy(python_proxy, dest, true)?;
        Ok(())
    }

    #[cfg(windows)]
    fn link(&self, dest: &Path, interpreter: Option<&Path>, is_gui: bool) -> anyhow::Result<()> {
        python_proxy::create(
            ProxySource::Pex(self.0),
            interpreter
                .ok_or_else(|| anyhow!("Windows venvs require an interpreter to link to."))?,
            fs::File::create(dest)?.into_file(),
            None,
            is_gui,
        )
    }
}

pub fn boot(
    python: impl AsRef<Path>,
    python_args: Vec<String>,
    pex: impl AsRef<Path>,
    argv: Vec<String>,
) -> anyhow::Result<i32> {
    #[cfg(feature = "tools")]
    if let Ok(tools) = env::var("PEX_TOOLS")
        && tools == "1"
    {
        if let Err(err) = tools::main(python.as_ref(), pex.as_ref(), argv) {
            eprintln!("{err}");
            std::process::exit(1);
        }
        std::process::exit(0);
    }
    logging::init_default()?;
    let lock = match cache::read_lock() {
        Ok(lock) => lock,
        Err(err) => bail!("Failed to obtain PEXRC cache read lock: {err}"),
    };
    let mut command = prepare_boot(python, python_args, pex, argv)?;
    info!(
        "Booting with {exe} {args}",
        exe = command.get_program().to_string_lossy(),
        args = command.get_args().map(OsStr::to_string_lossy).join(" ")
    );
    if let Ok(CacheRoot::TempDir(temp_dir)) = CacheDir::root() {
        command.env("PEXRC_ROOT", temp_dir.path());
        Ok(platform::spawn(&mut command)?)
    } else {
        Ok(platform::exec(&mut command, &[lock])?)
    }
}

fn prepare_boot(
    python: impl AsRef<Path>,
    python_args: Vec<String>,
    pex: impl AsRef<Path>,
    argv: Vec<String>,
) -> anyhow::Result<Command> {
    let venv = prepare_venv(
        python.as_ref(),
        pex.as_ref(),
        #[cfg(unix)]
        env::var_os("_PEXRC_SH_BOOT_SEED_DIR").map(PathBuf::from),
    )?;

    let mut command = {
        #[cfg(windows)]
        {
            let python_exe = {
                if let Some(file_name) = python.as_ref().file_name().and_then(OsStr::to_str)
                    // N.B.: This could be either pythonw.exe or pypyw.exe (or pypy<version>w.exe).
                    && file_name.ends_with("w.exe")
                {
                    // Either way, our venv machinery ensures "pythonw.exe" exists.
                    Cow::Owned(venv.interpreter.details.path.with_file_name("pythonw.exe"))
                } else {
                    Cow::Borrowed(&venv.interpreter.details.path)
                }
            };
            Command::new(python_exe.as_ref())
        }
        #[cfg(unix)]
        Command::new(&venv.interpreter.details.path)
    };
    command
        .args(python_args)
        .arg(venv.prefix().as_os_str())
        .args(argv);
    Ok(command)
}

pub fn mount(python: impl AsRef<Path>, pex: impl AsRef<Path>) -> anyhow::Result<PathBuf> {
    logging::init_default()?;
    match cache::read_lock() {
        Ok(lock) => {
            // N.B.: We're being called from a Python program that lives longer than us via an
            // import hook. We want the cache read lock to survive for the lifetime of that process.
            // To prevent unlock when the lock goes out of scope, we forget it to disable its
            // destructor. This will keep the fd open which will keep the read_lock read-locked.
            mem::forget(lock);
        }
        Err(err) => bail!("Failed to obtain PEXRC cache read lock: {err}"),
    };
    prepare_venv(
        python.as_ref(),
        pex.as_ref(),
        #[cfg(unix)]
        None,
    )
    .map(|venv| venv.prefix().join(&venv.site_packages_relpath))
}

#[time("debug", "{}")]
fn prepare_venv<'a>(
    python: &Path,
    pex: &Path,
    #[cfg(unix)] sh_boot_seed_dir: Option<PathBuf>,
) -> anyhow::Result<Virtualenv<'a>> {
    let pex = Pex::load(pex)?;
    let pex_path = PexPath::from_pex_info(&pex.info, true);
    let pex_info = pex.info.raw();
    let additional_pexes = pex_path.load_pexes()?;
    let search_path = SearchPath::from_env()?;
    let venv_dir = venv_dir(Some(python), &pex, &search_path, &additional_pexes)?;
    if let Some(venv_interpreter) = atomic_dir(&venv_dir, |work_dir| {
        let mut resolve = pex.resolve(Some(python), additional_pexes.iter(), search_path, None)?;
        let venv = Virtualenv::create(
            resolve.interpreter,
            Cow::Borrowed(work_dir),
            PythonProxyLinker(&pex),
            &mut resolve.scripts,
            pex_info.venv_system_site_packages,
            false,
            None,
        )?;

        let interpreter_relpath = venv
            .interpreter
            .details
            .path
            .strip_prefix(&venv.interpreter.details.prefix)?;
        let shebang_interpreter = venv_dir.join(interpreter_relpath);
        let shebang_arg = if (pex_info.venv && pex_info.venv_hermetic_scripts)
            || (!pex_info.venv
                && pex_info.inherit_path.unwrap_or(InheritPath::False) == InheritPath::False)
        {
            Some(venv.interpreter.hermetic_args())
        } else {
            None
        };
        let provenance = Arc::new(Provenance::new(format!(
            "populating venv for {pex}",
            pex = pex.path.display()
        )));
        populate(
            &venv,
            &shebang_interpreter,
            shebang_arg,
            &pex,
            resolve.wheels,
            &mut resolve.scripts,
            None,
            InstallScope::All,
            provenance.clone(),
        )?;
        for (additional_pex, resolved_wheels) in resolve.additional_wheels {
            populate_user_code_and_wheels(
                &venv,
                &shebang_interpreter,
                shebang_arg,
                additional_pex,
                resolved_wheels,
                false,
                InstallScope::All,
                provenance.clone(),
            )?;
        }
        if let Some(collision_report) = Arc::try_unwrap(provenance)
            .expect("Provenance use is complete")
            .into_collision_report()?
        {
            warn!("{collision_report}");
        }
        Ok(venv.interpreter)
    })? {
        info!("Built venv at {path}", path = venv_dir.display());
        let venv_interpreter = Virtualenv::host_interpreter(&venv_dir, &venv_interpreter)?;
        venv_interpreter.store()?;
        #[cfg(unix)]
        if let Some(sh_boot_seed_dir) = sh_boot_seed_dir {
            fs_err::create_dir_all(&sh_boot_seed_dir)?;
            let python = venv_interpreter.most_specific_exe_name();
            // N.B.: This symlink is probed by the --sh-boot script to confirm the venv is still
            // linked to an existing base Python (no uninstalls or upgrades).
            platform::unix::symlink(
                &venv_interpreter
                    .clone()
                    .resolve_base_interpreter(&mut pex.scripts()?)?
                    .details
                    .path,
                sh_boot_seed_dir.join(format!("base-{python}")),
                false,
            )?;
            // N.B.: This is what the --sh-boot script executes after the probe for venv viability
            // succeeds.
            platform::unix::symlink(
                venv_dir.join("pex"),
                sh_boot_seed_dir.join(format!("pex-{python}")),
                true,
            )?;
        }
        Virtualenv::enclosing(venv_interpreter)
    } else {
        info!("Loading cached venv at {path}", path = venv_dir.display());
        let mut scripts = pex.scripts()?;
        Virtualenv::load(Cow::Owned(venv_dir), &mut scripts)
    }
}

const INTERPRETER_HASH_OPTIONS: HashOptions = HashOptions::new().path(true).mtime(true).size(true);

pub fn venv_dir(
    ambient_python: Option<&Path>,
    pex: &Pex,
    search_path: &SearchPath,
    additional_pexes: &[Pex],
) -> anyhow::Result<PathBuf> {
    let mut key = Key::default();
    let pex_info = pex.info.raw();

    // The primary PEX hash covers its user code contents, distributions and ICs.
    key.property("pex_hash", pex_info.pex_hash);

    // We hash just the distributions of additional PEXes since those are the only items used from
    // PEX_PATH adjoined PEX files; i.e.: neither the entry_point nor any other PEX file data or
    // metadata is used.
    for additional_pex in additional_pexes {
        key.object(
            "additional_pex",
            additional_pex
                .info
                .raw()
                .distributions
                .iter()
                .map(|(file_name, fingerprint)| (file_name, fingerprint.as_ref())),
        );
    }

    let mut imprecise_pex_python: Option<&OsStr> = None;
    let mut imprecise_pex_python_path: Option<OsString> = None;

    // If there are no restrictions on interpreter, whatever we derive from the ambient python is
    // our opaque choice, which we can keep.
    if pex_info.interpreter_constraints.is_empty()
        && search_path.is_empty()
        && let Some(python_exe) = ambient_python
    {
        key.file(python_exe, &INTERPRETER_HASH_OPTIONS)?;
    } else if let Some(python_exe) = search_path.unique_interpreter() {
        // The user chose a unique interpreter (via PEX_PYTHON or PEX_PYTHON_PATH or a combination
        // of the two). It may not match the ICs, if any, but the choice is respected.
        key.file(python_exe, &INTERPRETER_HASH_OPTIONS)?;
    } else {
        // Otherwise, we do our best.
        if let Some(python) = search_path.pex_python() {
            let value = python.as_encoded_bytes();
            key.property("PEX_PYTHON", value);
            if pex_info.emit_warnings
                && !Regex::new(r"^(?:[Pp]ython|pypy)\d+\.\d+[^\d]?(?:\.exe)$")?.is_match(value)
            {
                imprecise_pex_python = Some(python);
            }
        }
        if let Some(path) = search_path.pex_python_path() {
            key.list(
                "PEX_PYTHON_PATH",
                path.iter().map(|path| path.as_os_str().as_encoded_bytes()),
            );
            if pex_info.emit_warnings {
                imprecise_pex_python_path = Some(env::join_paths(path)?);
            }
        }
    }

    let venv_dir = CacheDir::Venv.path()?.join(PathBuf::from(key));
    if let Some(pex_python) = imprecise_pex_python {
        warn!(
            "\
            Using a venv selected by PEX_PYTHON={pex_python}\n\
            for {pex_file}\n\
            at {venv_dir}.\n\
            \n\
            If `{pex_python}` is upgraded or downgraded at some later date, this venv will still\n\
            be used. To force re-creation of the venv using the upgraded or downgraded\n\
            `{pex_python}` you will need to delete it at that point in time.\n\
            \n\
            To avoid this warning, either specify a Python binary with major and minor version\n\
            in its name, like PEX_PYTHON=python3.14 or else re-build the PEX\n\
            with `--no-emit-warnings` or re-run the PEX with PEX_EMIT_WARNINGS=False.\n\
            ",
            pex_python = pex_python.display(),
            pex_file = pex.path.display(),
            venv_dir = venv_dir.display()
        )
    }
    if let Some(pex_python_path) = imprecise_pex_python_path {
        warn!(
            "\
            Using a venv restricted by PEX_PYTHON_PATH={ppp}\n\
            for {pex_file}\n\
            at {venv_dir}.\n\
            \n\
            If the contents of `{ppp}` changes at some later date, this venv and the interpreter\n\
            selected from `{ppp}` will still be used. To force re-creation of the venv using\n\
            the new pythons available on `{ppp}` you will need to delete it at that point in\n\
            time.\n\
            \n\
            To avoid this warning, re-build the PEX with `--no-emit-warnings` or re-run the PEX\n\
            with PEX_EMIT_WARNINGS=False.\n\
            ",
            ppp = pex_python_path.display(),
            pex_file = pex.path.display(),
            venv_dir = venv_dir.display()
        )
    }
    Ok(venv_dir)
}
