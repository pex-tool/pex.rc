// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail};
use fs_err as fs;
use fs_err::File;
use interpreter::Interpreter;
use logging_timer::time;
use platform::symlink_or_link_or_copy;
use python_platform::PythonPlatform;
use scripts::{IdentifyInterpreter, Scripts, VendoredVirtualenv};
use target_lexicon::{HOST, OperatingSystem};

#[cfg(unix)]
const SCRIPTS_DIR: &str = "bin";

#[cfg(unix)]
fn executable_rel_path(interpreter: &Interpreter) -> Cow<'static, str> {
    let interpreter = interpreter.raw();
    if interpreter.pypy_version.is_some() {
        Cow::Owned(format!(
            "{SCRIPTS_DIR}/pypy{major}.{minor}",
            major = interpreter.version.major,
            minor = interpreter.version.minor
        ))
    } else {
        Cow::Owned(format!(
            "{SCRIPTS_DIR}/python{major}.{minor}",
            major = interpreter.version.major,
            minor = interpreter.version.minor
        ))
    }
}

#[cfg(windows)]
const SCRIPTS_DIR: &str = "Scripts";

#[cfg(windows)]
fn executable_rel_path(interpreter: &Interpreter) -> Cow<'static, str> {
    if interpreter.raw().pypy_version.is_some() {
        Cow::Owned(format!(
            "{SCRIPTS_DIR}\\pypy{major}.{minor}.exe",
            major = interpreter.raw().version.major,
            minor = interpreter.raw().version.minor
        ))
    } else {
        Cow::Borrowed("Scripts\\python.exe")
    }
}

pub trait Linker {
    fn link(&self, dest: &Path, interpreter: Option<&Path>, is_gui: bool) -> anyhow::Result<()>;
}

pub struct FileSystemLinker();

impl Linker for FileSystemLinker {
    fn link(&self, dest: &Path, interpreter: Option<&Path>, _is_gui: bool) -> anyhow::Result<()> {
        if let Some(interpreter) = interpreter {
            symlink_or_link_or_copy(interpreter, dest, false)?;
        }
        Ok(())
    }
}

pub struct Virtualenv<'a> {
    pub interpreter: Interpreter,
    pub bin_dir_relpath: &'a str,
    pub site_packages_relpath: Cow<'a, Path>,
}

impl<'a> Virtualenv<'a> {
    #[time("debug", "Virtualenv.{}")]
    pub fn enclosing(interpreter: Interpreter) -> anyhow::Result<Self> {
        let site_packages_relpath = site_packages_relpath(&interpreter);
        Ok(Self {
            interpreter,
            bin_dir_relpath: SCRIPTS_DIR,
            site_packages_relpath,
        })
    }

    #[time("debug", "Virtualenv.{}")]
    pub fn load(path: Cow<'a, Path>, scripts: &mut Scripts) -> anyhow::Result<Self> {
        let pyvenv_cfg = PyVenvCfg::read(path.as_ref())?;
        let identification_script = IdentifyInterpreter::read(scripts)?;
        let interpreter = Interpreter::load(
            &path.as_ref().join(pyvenv_cfg.executable_rel_path),
            &identification_script,
        )?;
        Self::enclosing(interpreter)
    }

    pub fn host_interpreter(
        venv_dir: &Path,
        interpreter: &Interpreter,
    ) -> anyhow::Result<Interpreter> {
        let executable_relpath = executable_rel_path(interpreter);
        let path = venv_dir.join(executable_relpath.as_ref());

        let mut venv_interpreter = interpreter.clone();
        if HOST.operating_system == OperatingSystem::Windows {
            venv_interpreter.set_realpath(&path);
        }
        venv_interpreter.with_raw_mut(|venv_interpreter| {
            if venv_interpreter.base_prefix.is_none() {
                venv_interpreter.base_prefix = Some(venv_interpreter.prefix.clone());
            }
            venv_interpreter.prefix = Cow::Owned(venv_dir.to_path_buf());
            venv_interpreter.path = Cow::Owned(path);
            for path in venv_interpreter.paths.values_mut() {
                if let Ok(prefix_rel_path) = path.strip_prefix(&interpreter.raw().prefix) {
                    *path = venv_interpreter.prefix.join(prefix_rel_path);
                }
            }
        });
        Ok(venv_interpreter)
    }

    #[time("debug", "Virtualenv.{}")]
    pub fn create(
        interpreter: Interpreter,
        path: Cow<'a, Path>,
        linker: impl Linker,
        scripts: &mut Scripts,
        include_system_site_packages: bool,
        pip: bool,
        prompt: Option<&'a str>,
    ) -> anyhow::Result<Self> {
        let venv_interpreter = Self::host_interpreter(path.as_ref(), &interpreter)?;

        let site_packages_relpath =
            if interpreter.raw().version.major == 3 && interpreter.raw().version.minor >= 3 {
                create_pep_405_venv(
                    interpreter,
                    path.as_ref(),
                    linker,
                    include_system_site_packages,
                    scripts,
                    pip,
                    prompt,
                )?
            } else {
                let virtualenv_script = VendoredVirtualenv::read(scripts)?;
                create_virtualenv_venv(
                    &interpreter,
                    path.as_ref(),
                    linker,
                    virtualenv_script,
                    include_system_site_packages,
                    pip,
                    prompt,
                )?
            };

        Ok(Self {
            interpreter: venv_interpreter,
            bin_dir_relpath: SCRIPTS_DIR,
            site_packages_relpath,
        })
    }

    pub fn prefix(&self) -> &Path {
        &self.interpreter.raw().prefix
    }

    pub fn script_path(&self, rel_path: impl AsRef<Path>) -> PathBuf {
        self.interpreter
            .raw()
            .prefix
            .join(self.bin_dir_relpath)
            .join(rel_path)
    }

    pub fn site_packages_path(&self, rel_path: impl AsRef<Path>) -> PathBuf {
        self.interpreter
            .raw()
            .prefix
            .join(self.site_packages_relpath.as_ref())
            .join(rel_path)
    }

    pub fn create_additional_pythons(&self) -> anyhow::Result<()> {
        for rel_path in self.interpreter.prefix_rel_paths() {
            let dest = self.interpreter.raw().prefix.join(rel_path.as_ref());
            if !dest.exists() {
                symlink_or_link_or_copy(self.interpreter.raw().path.as_ref(), dest, true)?;
            }
        }
        Ok(())
    }
}

struct PyVenvCfg<'a> {
    home: Cow<'a, Path>,
    include_system_site_packages: bool,
    version: Option<Cow<'a, str>>,
    prompt: Option<Cow<'a, str>>,
    executable: Option<Cow<'a, Path>>,
    executable_rel_path: Cow<'a, Path>,
}

impl<'a> PyVenvCfg<'a> {
    fn read(dir: &Path) -> anyhow::Result<Self> {
        let pyvenv_cfg = BufReader::new(File::open(dir.join("pyvenv.cfg"))?);
        let mut home: Option<PathBuf> = None;
        let mut include_system_site_packages: Option<bool> = None;
        let mut version: Option<Cow<'a, str>> = None;
        let mut prompt: Option<Cow<'a, str>> = None;
        let mut executable: Option<Cow<'a, Path>> = None;
        let mut executable_rel_path: Option<PathBuf> = None;
        for line in pyvenv_cfg.lines() {
            let line = line?;
            let mut components = line.splitn(2, " = ");
            match components.next() {
                Some("home") => home = components.next().map(str::trim_end).map(PathBuf::from),
                Some("include-system-site-packages") => {
                    include_system_site_packages = components
                        .next()
                        .map(str::trim_end)
                        .map(|value| value == "true")
                }
                Some("version") => {
                    version = components
                        .next()
                        .map(str::trim_end)
                        .map(str::to_string)
                        .map(Cow::Owned)
                }
                Some("prompt") => {
                    prompt = components
                        .next()
                        .map(str::trim_end)
                        .map(|prompt| {
                            if prompt.starts_with("'") {
                                prompt.trim_prefix("'").trim_suffix("'")
                            } else if prompt.starts_with("\"") {
                                prompt.trim_prefix("\"").trim_suffix("\"")
                            } else {
                                prompt
                            }
                        })
                        .map(str::to_string)
                        .map(Cow::Owned)
                }
                Some("executable") => {
                    executable = components
                        .next()
                        .map(str::trim_end)
                        .map(PathBuf::from)
                        .map(Cow::Owned)
                }
                Some("executable-rel-path") => {
                    executable_rel_path = components.next().map(str::trim_end).map(PathBuf::from)
                }
                _ => {}
            }
        }
        if let Some(home) = home
            && let Some(executable_rel_path) = executable_rel_path
        {
            Ok(Self {
                home: Cow::Owned(home),
                include_system_site_packages: include_system_site_packages.unwrap_or_default(),
                version,
                prompt,
                executable,
                executable_rel_path: Cow::Owned(executable_rel_path),
            })
        } else {
            bail!(
                "The pyvenv.cfg in {dir} is not valid. It must contain both a home entry and a executable entry.",
                dir = dir.display()
            )
        }
    }

    fn write(&self, dir: &Path) -> anyhow::Result<()> {
        let mut pyvenv_cfg = File::create(dir.join("pyvenv.cfg"))?;
        pyvenv_cfg.write_all(b"home = ")?;
        pyvenv_cfg.write_all(self.home.as_os_str().as_encoded_bytes())?;
        pyvenv_cfg.write_all(b"\n")?;

        pyvenv_cfg.write_all(b"include-system-site-packages = ")?;
        pyvenv_cfg.write_all(if self.include_system_site_packages {
            b"true"
        } else {
            b"false"
        })?;
        pyvenv_cfg.write_all(b"\n")?;

        if let Some(version) = self.version.as_deref() {
            pyvenv_cfg.write_all(b"version = ")?;
            pyvenv_cfg.write_all(version.as_bytes())?;
            pyvenv_cfg.write_all(b"\n")?;
        }

        if let Some(prompt) = self.prompt.as_deref() {
            // TODO: This is flawed escaping in general. To match the Python venv module, the value
            //  should be equivalent to the output of `repr(prompt)` in Python.
            // See: https://github.com/python/cpython/blob/88e378cc1cd55429e08268a8da17e54ede104fb5/Lib/venv/__init__.py#L235-L236
            let quote_char = if prompt.contains('\'') { "\"" } else { "'" };
            pyvenv_cfg.write_all(b"prompt = ")?;
            pyvenv_cfg.write_all(quote_char.as_bytes())?;
            pyvenv_cfg.write_all(prompt.as_bytes())?;
            pyvenv_cfg.write_all(quote_char.as_bytes())?;
            pyvenv_cfg.write_all(b"\n")?;
        }

        if let Some(executable) = self.executable.as_deref() {
            pyvenv_cfg.write_all(b"executable = ")?;
            pyvenv_cfg.write_all(executable.as_os_str().as_encoded_bytes())?;
            pyvenv_cfg.write_all(b"\n")?;
        }

        pyvenv_cfg.write_all(b"executable-rel-path = ")?;
        pyvenv_cfg.write_all(self.executable_rel_path.as_os_str().as_encoded_bytes())?;
        pyvenv_cfg.write_all(b"\n")?;

        Ok(())
    }
}

fn create_pep_405_venv<'a>(
    interpreter: Interpreter,
    path: &Path,
    linker: impl Linker,
    include_system_site_packages: bool,
    scripts: &mut Scripts,
    pip: bool,
    prompt: Option<&'a str>,
) -> anyhow::Result<Cow<'a, Path>> {
    // See: https://peps.python.org/pep-0405/
    let base_interpreter = interpreter.resolve_base_interpreter(scripts)?;
    let raw_base_interpreter = base_interpreter.raw();
    let home = base_interpreter.realpath().parent().ok_or_else(|| {
        anyhow!(
            "Failed to calculate the home dir of venv base python {path}",
            path = base_interpreter.realpath().display()
        )
    })?;
    let executable_rel_path = executable_rel_path(&base_interpreter);
    let executable_rel_path = Path::new(executable_rel_path.as_ref());
    let pyvenv_cfg = PyVenvCfg {
        home: Cow::Borrowed(home),
        include_system_site_packages,
        version: Some(Cow::Owned(raw_base_interpreter.version.to_string())),
        prompt: prompt.map(Cow::Borrowed),
        executable: Some(Cow::Borrowed(base_interpreter.realpath())),
        executable_rel_path: Cow::Borrowed(executable_rel_path),
    };
    pyvenv_cfg.write(path)?;

    let venv_python = path.join(executable_rel_path);
    if let Some(parent) = venv_python.parent() {
        fs::create_dir_all(parent)?;
    }
    linker.link(&venv_python, Some(base_interpreter.realpath()), false)?;
    #[cfg(windows)]
    linker.link(
        &venv_python.with_file_name("pythonw.exe"),
        Some(&base_interpreter.realpath().with_file_name("pythonw.exe")),
        true,
    )?;
    let site_packages_relpath = site_packages_relpath(&base_interpreter);
    fs::create_dir_all(path.join(site_packages_relpath.as_ref()))?;
    if pip {
        ensure_pip(raw_base_interpreter, &venv_python)?;
    }
    Ok(site_packages_relpath)
}

fn create_virtualenv_venv<'a>(
    interpreter: &Interpreter,
    path: &Path,
    linker: impl Linker,
    virtualenv_script: VendoredVirtualenv<'a>,
    include_system_site_packages: bool,
    pip: bool,
    prompt: Option<&'a str>,
) -> anyhow::Result<Cow<'a, Path>> {
    let mut script = tempfile::Builder::new()
        .prefix("virtualenv.")
        .suffix(".py")
        .tempfile()?;
    script.write_all(virtualenv_script.contents().as_bytes())?;
    let raw_interpreter = interpreter.raw();
    let mut command = Command::new(raw_interpreter.path.as_ref());
    command
        .arg(interpreter.hermetic_args())
        .arg(script.path())
        .args(["--no-pip", "--no-setuptools", "--no-wheel"]);
    if include_system_site_packages {
        command.arg("--system-site-packages");
    }
    if let Some(prompt) = prompt {
        command.arg("--prompt").arg(prompt);
    }
    let child = command
        .arg(path)
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "Failed to create a venv at {workdir} using {python_implementation} {python_version} \
            interpreter {python_exe}:\n{stderr}",
            workdir = path.display(),
            python_implementation = interpreter.marker_env().platform_python_implementation(),
            python_version = interpreter.marker_env().python_full_version(),
            python_exe = raw_interpreter.path.as_ref().display(),
            stderr = String::from_utf8_lossy(&output.stderr)
        )
    }

    let home = interpreter.realpath().parent().ok_or_else(|| {
        anyhow!(
            "Failed to calculate the home dir of venv base python {path}",
            path = interpreter.realpath().display()
        )
    })?;
    let executable_rel_path = executable_rel_path(interpreter);
    let executable_rel_path = Path::new(executable_rel_path.as_ref());
    let venv_python = path.join(executable_rel_path);
    assert!(venv_python.is_file());
    let pyvenv_cfg = PyVenvCfg {
        home: Cow::Borrowed(home),
        include_system_site_packages,
        version: Some(Cow::Owned(raw_interpreter.version.to_string())),
        prompt: prompt.map(Cow::Borrowed),
        executable: Some(Cow::Borrowed(interpreter.realpath())),
        executable_rel_path: Cow::Borrowed(executable_rel_path),
    };
    pyvenv_cfg.write(path.as_ref())?;

    linker.link(&venv_python, None, false)?;
    #[cfg(windows)]
    linker.link(&venv_python.with_file_name("pythonw.exe"), None, true)?;
    if pip {
        ensure_pip(raw_interpreter, &venv_python)?;
    }
    Ok(site_packages_relpath(interpreter))
}

fn ensure_pip(base_interpreter: &interpreter::RawInterpreter, python: &Path) -> anyhow::Result<()> {
    if !base_interpreter.has_ensurepip {
        // TODO: Consider embedding or fetching: https://bootstrap.pypa.io/pip/
        bail!(
            "Cannot install Pip since the selected interpreter does not have the `ensurepip` \
            module: {interpreter}",
            interpreter = base_interpreter.path.as_ref().display()
        )
    }

    Command::new(python)
        .args(["-m", "ensurepip", "--default-pip"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?
        .wait()?
        .exit_ok()?;
    Ok(())
}

fn site_packages_relpath<'a>(interpreter: &Interpreter) -> Cow<'a, Path> {
    if HOST.operating_system == OperatingSystem::Windows {
        // TODO: XXX: Confirm venv layouts for PyPy under Windows.
        return Cow::Borrowed(Path::new("Lib\\site-packages"));
    }
    let raw_interpreter = interpreter.raw();
    if interpreter.marker_env().platform_python_implementation() == "PyPy"
        && (raw_interpreter.version.major, raw_interpreter.version.minor) < (3, 8)
    {
        Cow::Borrowed(Path::new("site-packages"))
    } else {
        Cow::Owned(
            PathBuf::from("lib")
                .join(format!(
                    "{implementation}{major}.{minor}",
                    implementation =
                        if interpreter.marker_env().platform_python_implementation() == "PyPy" {
                            "pypy"
                        } else {
                            "python"
                        },
                    major = raw_interpreter.version.major,
                    minor = raw_interpreter.version.minor
                ))
                .join("site-packages"),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::path::PathBuf;

    use interpreter::Interpreter;
    use rstest::rstest;
    use scripts::{IdentifyInterpreter, Scripts};
    use testing::{embedded_scripts, python_exe, tmp_dir};

    use crate::virtualenv::{FileSystemLinker, Path, Virtualenv};

    #[rstest]
    fn test_create(python_exe: &Path, tmp_dir: PathBuf, mut embedded_scripts: Scripts) {
        let identification_script = IdentifyInterpreter::read(&mut embedded_scripts).unwrap();
        let interpreter = Interpreter::load(python_exe, &identification_script).unwrap();
        let expected_prefix = interpreter
            .raw()
            .base_prefix
            .as_deref()
            .unwrap_or(interpreter.raw().prefix.as_ref())
            .to_owned();
        let venv = Virtualenv::create(
            interpreter,
            Cow::Owned(tmp_dir),
            FileSystemLinker(),
            &mut embedded_scripts,
            false,
            false,
            None,
        )
        .unwrap();
        assert_eq!(
            expected_prefix,
            venv.interpreter.raw().base_prefix.as_deref().unwrap()
        )
    }
}
