// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::{Display, Formatter};
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail};
use cache::{CacheDir, HashOptions, atomic_file, hash_file};
use fs_err as fs;
use logging_timer::time;
use ouroboros::self_referencing;
use pep440_rs::Version;
use pep508_rs::MarkerEnvironment;
use python_platform::{NonEmptyVec, PlatformDetails, PythonPlatform};
use scripts::{IdentifyInterpreter, Scripts};
use serde::{Deserialize, Serialize};

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct PythonVersion<'a> {
    pub major: u8,
    pub minor: u8,
    pub micro: u8,
    pub releaselevel: &'a str,
    pub serial: u8,
}

impl<'a> Display for PythonVersion<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{major}.{minor}.{micro}",
            major = self.major,
            minor = self.minor,
            micro = self.micro
        ))?;

        // N.B.: Using this for possible strings reference:
        // https://peps.python.org/pep-0739/#implementation-version-releaselevel

        if let Some(level_abbrev) = match self.releaselevel {
            "alpha" => Some("a"),
            "beta" => Some("b"),
            "candidate" => Some("rc"),
            _ => None,
        } {
            f.write_fmt(format_args!("{level_abbrev}{serial}", serial = self.serial))?;
        }
        Ok(())
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct PyPyVersion(u8, u8, u8);

impl Display for PyPyVersion {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!(
            "{major}.{minor}.{patch}",
            major = self.0,
            minor = self.1,
            patch = self.2
        ))
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct CPythonAbiInfo {
    pub free_threaded: Option<bool>,
    pub debug: bool,
    pub pymalloc: Option<bool>,
    pub ucs4: Option<bool>,
}

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct RawInterpreter<'a> {
    #[serde(borrow)]
    pub path: Cow<'a, Path>,
    #[serde(borrow)]
    pub prefix: Cow<'a, Path>,
    #[serde(borrow)]
    pub base_prefix: Option<Cow<'a, Path>>,
    #[serde(borrow)]
    pub version: PythonVersion<'a>,
    pub pypy_version: Option<PyPyVersion>,
    pub cpython_abi_info: Option<CPythonAbiInfo>,
    pub paths: BTreeMap<String, PathBuf>,
    pub has_ensurepip: bool,
}

#[cfg(target_os = "linux")]
static LINUX_INFO: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[self_referencing]
pub struct Interpreter {
    data: Vec<u8>,
    realpath: PathBuf,
    platform_details: PlatformDetails,
    #[borrows(data)]
    #[covariant]
    interpreter: RawInterpreter<'this>,
}

impl Eq for Interpreter {}

impl PartialEq for Interpreter {
    fn eq(&self, other: &Self) -> bool {
        self.raw().eq(other.raw())
    }
}

impl Hash for Interpreter {
    fn hash<H>(&self, state: &mut H)
    where
        H: Hasher,
    {
        self.raw().hash(state)
    }
}

impl Clone for Interpreter {
    fn clone(&self) -> Self {
        Self::new(
            self.borrow_data().clone(),
            self.borrow_realpath().clone(),
            self.borrow_platform_details().clone(),
            |data| {
                serde_json::from_slice(data)
                    .expect("We've already parsed out data successfully once.")
            },
        )
    }
}

impl Interpreter {
    fn identify(
        python_exe: impl AsRef<Path>,
        identification_script: &IdentifyInterpreter,
    ) -> anyhow::Result<Vec<u8>> {
        let mut script = tempfile::Builder::new()
            .prefix("virtualenv.")
            .suffix(".py")
            .tempfile()?;
        script.write_all(identification_script.contents().as_bytes())?;
        let mut command = Command::new(python_exe.as_ref());
        command.arg("-sE").arg(script.path());
        #[cfg(target_os = "linux")]
        {
            use log::debug;

            let mut linux_info = LINUX_INFO
                .lock()
                .map_err(|err| anyhow!("Failed to obtain lock on Linux platform info: {err}"))?;
            let json = if let Some(json) = linux_info.as_ref() {
                debug!("Using cached Linux info.");
                json
            } else {
                let info = python_platform::LinuxInfo::parse(python_exe.as_ref())?;
                let json = serde_json::to_string(&info)?;
                debug!(
                    "Caching Linux info derived from {path}.",
                    path = python_exe.as_ref().display()
                );
                linux_info.insert(json)
            };
            command.arg("--linux-info").arg(json);
        }
        let result = command
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?
            .wait_with_output()?;
        if !result.status.success() {
            bail!(
                "Failed to identify Python interpreter at {path}.\n\
                Exit status {status} with STDERR:\n{stderr}",
                path = python_exe.as_ref().display(),
                status = result.status,
                stderr = String::from_utf8_lossy(result.stderr.as_slice())
            )
        }
        Ok(result.stdout)
    }

    pub fn load_uncached(
        python_exe: impl AsRef<Path>,
        identification_script: &IdentifyInterpreter,
        platform_details: PlatformDetails,
    ) -> anyhow::Result<Self> {
        let json_bytes = Self::identify(python_exe.as_ref(), identification_script)?;
        Self::try_new(
            json_bytes,
            python_exe.as_ref().canonicalize()?,
            platform_details,
            |data| {
                serde_json::from_slice(data).map_err(|err| {
                    anyhow!(
                        "Failed to identify Python interpreter {exe}: {err}",
                        exe = python_exe.as_ref().display()
                    )
                })
            },
        )
    }

    #[cfg(unix)]
    pub fn most_specific_exe_name(&self) -> String {
        let interpreter = self.raw();
        let name = if interpreter.pypy_version.is_some() {
            "pypy"
        } else {
            "python"
        };
        format!(
            "{name}{major}.{minor}",
            major = interpreter.version.major,
            minor = interpreter.version.minor
        )
    }

    pub fn prefix_rel_paths(&self) -> Vec<Cow<'_, Path>> {
        let interpreter = self.raw();
        Self::candidate_rel_paths(&interpreter.version, interpreter.pypy_version.is_some())
    }

    #[cfg(unix)]
    fn candidate_rel_paths<'a>(version: &PythonVersion, is_pypy: bool) -> Vec<Cow<'a, Path>> {
        let mut candidates: Vec<Cow<'_, Path>> = Vec::with_capacity(if is_pypy { 6 } else { 3 });
        if is_pypy {
            candidates.push(Cow::Owned(PathBuf::from(format!(
                "bin/pypy{major}.{minor}",
                major = version.major,
                minor = version.minor
            ))));
            candidates.push(Cow::Owned(PathBuf::from(format!(
                "bin/pypy{major}",
                major = version.major
            ))));
            candidates.push(Cow::Borrowed(Path::new("bin/pypy")));
        }
        candidates.push(Cow::Owned(PathBuf::from(format!(
            "bin/python{major}.{minor}",
            major = version.major,
            minor = version.minor
        ))));
        candidates.push(Cow::Owned(PathBuf::from(format!(
            "bin/python{major}",
            major = version.major
        ))));
        candidates.push(Cow::Borrowed(Path::new("bin/python")));
        candidates
    }

    #[cfg(windows)]
    fn candidate_rel_paths<'a>(version: &PythonVersion, is_pypy: bool) -> Vec<Cow<'a, Path>> {
        if is_pypy {
            vec![
                Cow::Borrowed(Path::new("pypy.exe")),
                Cow::Borrowed(Path::new("python.exe")),
                Cow::Owned(PathBuf::from(format!(
                    "Scripts\\pypy{major}.{minor}.exe",
                    major = version.major,
                    minor = version.minor
                ))),
                Cow::Owned(PathBuf::from(format!(
                    "Scripts\\pypy{major}.exe",
                    major = version.major
                ))),
                Cow::Borrowed(Path::new("Scripts\\pypy.exe")),
                Cow::Owned(PathBuf::from(format!(
                    "Scripts\\python{major}.{minor}.exe",
                    major = version.major,
                    minor = version.minor
                ))),
                Cow::Owned(PathBuf::from(format!(
                    "Scripts\\python{major}.exe",
                    major = version.major
                ))),
                Cow::Borrowed(Path::new("Scripts\\python.exe")),
            ]
        } else {
            vec![
                Cow::Borrowed(Path::new("python.exe")),
                Cow::Borrowed(Path::new("Scripts\\python.exe")),
            ]
        }
    }

    fn at_prefix(
        prefix: impl AsRef<Path>,
        version: PythonVersion,
        pypy_version: Option<PyPyVersion>,
        scripts: &mut Scripts,
        re_cache_version_mismatch: bool,
    ) -> anyhow::Result<Self> {
        let check_pypy_version = |interpreter: &Interpreter| match (
            pypy_version.as_ref(),
            interpreter.raw().pypy_version.as_ref(),
        ) {
            (Some(expected_pypy_version), Some(actual_pypy_version))
                if expected_pypy_version == actual_pypy_version =>
            {
                true
            }
            (None, None) => true,
            _ => false,
        };
        let identification_script = IdentifyInterpreter::read(scripts)?;
        let candidate_rel_paths = Self::candidate_rel_paths(&version, pypy_version.is_some());
        let mut re_cache_candidates: Vec<Self> = Vec::with_capacity(candidate_rel_paths.len());
        for rel_path in candidate_rel_paths {
            let candidate_path = prefix.as_ref().join(rel_path);
            if let Ok(interpreter) = Self::load(&candidate_path, &identification_script) {
                if interpreter.raw().version != version {
                    if re_cache_version_mismatch
                        && (
                            interpreter.raw().version.major,
                            interpreter.raw().version.minor,
                        ) == (version.major, version.minor)
                    {
                        re_cache_candidates.push(interpreter)
                    }
                    continue;
                }
                if check_pypy_version(&interpreter) {
                    return Ok(interpreter);
                }
            }
        }
        for interpreter in re_cache_candidates {
            let interpreter = interpreter.reload(&identification_script)?;
            if interpreter.raw().version == version && check_pypy_version(&interpreter) {
                return Ok(interpreter);
            }
        }
        if let Some(pypy_version) = pypy_version {
            bail!(
                "Failed to find a Python interpreter matching version {version} \
                (PyPy {pypy_version})"
            )
        } else {
            bail!("Failed to find a Python interpreter matching version {version}")
        }
    }

    const INTERPRETER_HASH_CONFIG: HashOptions =
        HashOptions::new().path(true).mtime(true).size(true);

    fn interpreter_info(python_exe: impl AsRef<Path>) -> anyhow::Result<PathBuf> {
        let hash = hash_file(python_exe.as_ref(), &Self::INTERPRETER_HASH_CONFIG)?;
        Ok(CacheDir::Interpreter.path()?.join(hash.base64_digest()))
    }

    #[time("debug", "Interpreter.{}")]
    pub fn load(
        python_exe: &Path,
        identification_script: &IdentifyInterpreter,
    ) -> anyhow::Result<Self> {
        let interpreter_info = Self::interpreter_info(python_exe)?;
        let platform_details = PlatformDetails::spawn(python_exe)?;
        Self::load_internal(
            &interpreter_info,
            python_exe,
            identification_script,
            platform_details,
        )
    }

    fn load_internal(
        interpreter_info: &Path,
        python_exe: &Path,
        identification_script: &IdentifyInterpreter,
        platform_details: impl FnOnce() -> anyhow::Result<PlatformDetails>,
    ) -> anyhow::Result<Self> {
        let file = atomic_file(interpreter_info, |file| {
            let json_bytes = Self::identify(python_exe, identification_script)?;
            BufWriter::new(file).write_all(&json_bytes)?;
            Ok(())
        })?;
        let size = file.metadata()?.len();
        let mut data = Vec::with_capacity(usize::try_from(size)?);
        BufReader::new(file).read_to_end(&mut data)?;
        Self::try_new(
            data,
            python_exe.canonicalize()?,
            platform_details()?,
            |data| {
                serde_json::from_slice(data).map_err(|err| {
                    anyhow!(
                        "Failed to identify Python interpreter {exe}: {err}",
                        exe = python_exe.display()
                    )
                })
            },
        )
    }

    fn reload(self, identification_script: &IdentifyInterpreter) -> anyhow::Result<Self> {
        let python_exe = self.raw().path.as_ref();
        let interpreter_info = Self::interpreter_info(python_exe)?;
        fs::remove_file(&interpreter_info)?;
        let platform_details = PlatformDetails::spawn(python_exe)?;
        Self::load_internal(
            &interpreter_info,
            python_exe,
            identification_script,
            platform_details,
        )
    }

    #[time("debug", "Interpreter.{}")]
    pub fn store(&self) -> anyhow::Result<()> {
        let hash = hash_file(self.raw().path.as_ref(), &Self::INTERPRETER_HASH_CONFIG)?;
        let interpreter_info = CacheDir::Interpreter.path()?.join(hash.base64_digest());
        atomic_file(&interpreter_info, |file| {
            serde_json::to_writer(BufWriter::new(file), self.raw())?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn hermetic_args(&self) -> &'static str {
        if self.raw().version.major == 3 && self.raw().version.minor >= 4 {
            "-I"
        } else {
            "-sE"
        }
    }

    #[time("debug", "Interpreter.{}")]
    pub fn resolve_base_interpreter(self, scripts: &mut Scripts) -> anyhow::Result<Interpreter> {
        if let Some(base_prefix) = self.raw().base_prefix.as_ref()
            && base_prefix != &self.raw().prefix
        {
            let resolved = Self::at_prefix(
                base_prefix,
                self.raw().version,
                self.raw().pypy_version,
                scripts,
                true,
            )?;
            return resolved.resolve_base_interpreter(scripts);
        }
        Ok(self)
    }

    pub fn is_venv(&self) -> bool {
        if let Some(base_prefix) = self.raw().base_prefix.as_deref()
            && base_prefix != self.raw().prefix
        {
            true
        } else {
            false
        }
    }

    #[inline]
    pub fn raw(&self) -> &RawInterpreter<'_> {
        self.borrow_interpreter()
    }

    #[inline]
    pub fn with_raw_mut<R>(&mut self, func: impl FnOnce(&mut RawInterpreter) -> R) -> R {
        self.with_interpreter_mut(func)
    }

    pub fn realpath(&self) -> &Path {
        self.borrow_realpath()
    }

    pub fn set_realpath(&mut self, value: &Path) {
        self.with_realpath_mut(|realpath| {
            realpath.clear();
            realpath.push(value)
        })
    }

    pub fn platform_details(&self) -> &PlatformDetails {
        self.borrow_platform_details()
    }
}

impl PythonPlatform for Interpreter {
    fn description(&self) -> impl Display {
        format!(
            "interpreter at {python_exe}",
            python_exe = self.raw().path.as_ref().display()
        )
    }

    fn marker_env(&self) -> &MarkerEnvironment {
        self.borrow_platform_details().marker_env()
    }

    fn supported_tags(&self) -> &NonEmptyVec<String> {
        self.borrow_platform_details().supported_tags()
    }

    fn version(&self) -> Cow<'_, Version> {
        let version = self.raw().version;
        Cow::Owned(Version::new([
            u64::from(version.major),
            u64::from(version.minor),
            u64::from(version.micro),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use anyhow::Context;
    use pretty_assertions::assert_eq;
    use python_platform::{PlatformDetails, PythonPlatform};
    use rstest::rstest;
    use scripts::{IdentifyInterpreter, Scripts};
    use testing::{
        embedded_scripts,
        interpreter_identification_script,
        python_exe,
        venv_python_exe,
    };
    use textwrap::dedent;

    use crate::Interpreter;

    #[rstest]
    fn test_tags_same_as_packaging(
        venv_python_exe: PathBuf,
        interpreter_identification_script: IdentifyInterpreter,
    ) {
        assert!(
            Command::new(&venv_python_exe)
                .args([
                    "-m",
                    "pip",
                    "install",
                    // N.B.: This commit includes two unreleased fixes:
                    // + Fixed linux / manylinux tag ordering.
                    // + Fixed fat32 -> fat3 for macOS abi tags.
                    // TODO: Revert to just "packaging" (latest) once these fixes are released.
                    "packaging @ git+https://github.com/pypa/packaging@45d15309ac4a2411196e800378f2"
                ])
                .spawn()
                .unwrap()
                .wait()
                .unwrap()
                .success()
        );
        let output = Command::new(&venv_python_exe)
            .arg("-c")
            .arg(dedent(
                "
                import json
                import sys

                from packaging import tags

                json.dump(list(map(str, tags.sys_tags())), sys.stdout)
                ",
            ))
            .stdout(Stdio::piped())
            .spawn()
            .unwrap()
            .wait_with_output()
            .unwrap();
        assert!(output.status.success());
        let expected_tags: Vec<String> =
            serde_json::from_str(String::from_utf8(output.stdout).unwrap().as_str()).unwrap();

        let platform_details = PlatformDetails::python(&venv_python_exe).unwrap();
        let interpreter = Interpreter::load_uncached(
            &venv_python_exe,
            &interpreter_identification_script,
            platform_details,
        )
        .with_context(|| {
            format!(
                "Failed to load interpreter info for {python}",
                python = venv_python_exe.display()
            )
        })
        .unwrap();
        assert_eq!(
            expected_tags,
            interpreter
                .supported_tags()
                .iter()
                .cloned()
                .collect::<Vec<_>>()
        );
    }

    #[rstest]
    fn test_resolve_base_interpreter(
        python_exe: &Path,
        venv_python_exe: PathBuf,
        mut embedded_scripts: Scripts,
    ) {
        let identification_script = IdentifyInterpreter::read(&mut embedded_scripts).unwrap();
        let venv_interpreter = Interpreter::load(&venv_python_exe, &identification_script)
            .with_context(|| {
                format!(
                    "Failed to load interpreter info for {python}",
                    python = venv_python_exe.display()
                )
            })
            .unwrap();
        assert_eq!(
            python_exe,
            venv_interpreter
                .resolve_base_interpreter(&mut embedded_scripts)
                .unwrap()
                .raw()
                .path
                .as_ref()
        )
    }
}
