// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::collections::BTreeMap;
use std::fmt::Display;
use std::hash::{Hash, Hasher};
use std::io::{BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{anyhow, bail};
use cache::{CacheDir, HashOptions, atomic_dir, hash_file};
use fs_err as fs;
use fs_err::File;
use logging_timer::time;
use ouroboros::self_referencing;
use pep508_rs::MarkerEnvironment;
use python_platform::{
    CPythonAbiInfo,
    CPythonImplementation,
    PlatformDetails,
    PyPyImplementation,
    PyPyVersion,
    PythonImplementation,
    PythonPlatform,
    PythonVersion,
};
use scripts::{IdentifyInterpreter, Scripts};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Eq, PartialEq, Hash, Deserialize, Serialize)]
pub struct InterpreterDetails {
    pub path: PathBuf,
    pub prefix: PathBuf,
    pub base_prefix: Option<PathBuf>,
    pub version: PythonVersion,
    pub pypy_version: Option<PyPyVersion>,
    pub cpython_abi_info: Option<CPythonAbiInfo>,
    pub paths: BTreeMap<String, PathBuf>,
    pub has_ensurepip: bool,
}

impl InterpreterDetails {
    pub fn python_implementation(&self) -> PythonImplementation {
        if let Some(cpython_abi_info) = self.cpython_abi_info {
            PythonImplementation::CPython(CPythonImplementation {
                version: self.version,
                abi_info: cpython_abi_info,
            })
        } else {
            PythonImplementation::PyPy(PyPyImplementation {
                version: self.version,
                pypy_version: self.pypy_version,
            })
        }
    }
}

// N.B. The extra complexity of the JsonPlatformDetails container for parsing PlatformDetails nets
// a ~2.5x perf increase on warm cache loads as compared to storing a PlatformDetails using String
// tags directly.
#[self_referencing]
struct JsonPlatformDetails {
    contents: String,
    #[borrows(contents)]
    #[covariant]
    platform_details: PlatformDetails<'this>,
}

impl JsonPlatformDetails {
    fn load(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let contents = fs::read_to_string(path)?;
        Self::try_new(contents, |contents| Ok(serde_json::from_str(contents)?))
    }
}

impl Clone for JsonPlatformDetails {
    fn clone(&self) -> Self {
        Self::new(self.borrow_contents().clone(), |contents| {
            serde_json::from_str(contents)
                .expect("We've already successfully parsed our JSON contents")
        })
    }
}

impl Eq for JsonPlatformDetails {}

impl PartialEq for JsonPlatformDetails {
    fn eq(&self, other: &Self) -> bool {
        self.borrow_platform_details()
            .eq(other.borrow_platform_details())
    }
}

impl Hash for JsonPlatformDetails {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.borrow_platform_details().hash(state)
    }
}

#[cfg(target_os = "linux")]
static LINUX_INFO: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);

#[derive(Clone, Eq, PartialEq, Hash)]
pub struct Interpreter {
    pub realpath: PathBuf,
    pub details: InterpreterDetails,
    platform_details: JsonPlatformDetails,
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
    ) -> anyhow::Result<Self> {
        let data = Self::identify(python_exe.as_ref(), identification_script)?;
        let details: InterpreterDetails =
            serde_json::from_slice(data.as_slice()).map_err(|err| {
                anyhow!(
                    "Failed to identify Python interpreter {exe}: {err}",
                    exe = python_exe.as_ref().display()
                )
            })?;
        let platform_details =
            PlatformDetails::python(python_exe.as_ref(), details.python_implementation())?;
        Ok(Self {
            realpath: python_exe.as_ref().canonicalize()?,
            details,
            platform_details: JsonPlatformDetails::new(String::new(), |_| platform_details),
        })
    }

    #[cfg(unix)]
    pub fn most_specific_exe_name(&self) -> String {
        let name = if self.details.pypy_version.is_some() {
            "pypy"
        } else {
            "python"
        };
        format!(
            "{name}{major}.{minor}",
            major = self.details.version.major,
            minor = self.details.version.minor
        )
    }

    pub fn prefix_rel_paths(&self) -> Vec<Cow<'_, Path>> {
        Self::candidate_rel_paths(&self.details.version, self.details.pypy_version.is_some())
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
            interpreter.details.pypy_version.as_ref(),
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
                if interpreter.details.version != version {
                    if re_cache_version_mismatch
                        && (
                            interpreter.details.version.major,
                            interpreter.details.version.minor,
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
            if interpreter.details.version == version && check_pypy_version(&interpreter) {
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
        Self::load_internal(&interpreter_info, python_exe, identification_script)
    }

    fn load_internal(
        interpreter_info: &Path,
        python_exe: &Path,
        identification_script: &IdentifyInterpreter,
    ) -> anyhow::Result<Self> {
        if let Some((details, platform_details)) = atomic_dir(interpreter_info, |path| {
            let json_bytes = Self::identify(python_exe, identification_script)?;
            let details: InterpreterDetails = serde_json::from_slice(json_bytes.as_slice())
                .map_err(|err| {
                    anyhow!(
                        "Failed to identify Python interpreter {exe}: {err}",
                        exe = python_exe.display()
                    )
                })?;

            let implementation_details = File::create_new(path.join("interpreter-details.json"))?;
            BufWriter::new(implementation_details).write_all(&json_bytes)?;

            let platform_details =
                PlatformDetails::python(python_exe, details.python_implementation())?;
            let platform_details_json = path.join("platform-details.json");
            serde_json::to_writer(
                BufWriter::new(File::create_new(&platform_details_json)?),
                &platform_details,
            )?;
            Ok((details, JsonPlatformDetails::load(platform_details_json)?))
        })? {
            Ok(Self {
                realpath: python_exe.canonicalize()?,
                details,
                platform_details,
            })
        } else {
            let details: InterpreterDetails = serde_json::from_reader(BufReader::new(File::open(
                interpreter_info.join("interpreter-details.json"),
            )?))?;
            let platform_details =
                JsonPlatformDetails::load(interpreter_info.join("platform-details.json"))?;
            Ok(Self {
                realpath: python_exe.canonicalize()?,
                details,
                platform_details,
            })
        }
    }

    fn reload(self, identification_script: &IdentifyInterpreter) -> anyhow::Result<Self> {
        let python_exe = self.details.path.as_ref();
        let interpreter_info = Self::interpreter_info(python_exe)?;
        fs::remove_dir_all(&interpreter_info)?;
        Self::load_internal(&interpreter_info, python_exe, identification_script)
    }

    #[time("debug", "Interpreter.{}")]
    pub fn store(&self) -> anyhow::Result<()> {
        let hash = hash_file(self.details.path.as_ref(), &Self::INTERPRETER_HASH_CONFIG)?;
        let interpreter_info = CacheDir::Interpreter.path()?.join(hash.base64_digest());
        atomic_dir(&interpreter_info, |path| {
            serde_json::to_writer(
                BufWriter::new(File::create_new(path.join("interpreter-details.json"))?),
                &self.details,
            )?;
            serde_json::to_writer(
                BufWriter::new(File::create_new(path.join("platform-details.json"))?),
                &self.platform_details(),
            )?;
            Ok(())
        })?;
        Ok(())
    }

    pub fn hermetic_args(&self) -> &'static str {
        if self.details.version.major == 3 && self.details.version.minor >= 4 {
            "-I"
        } else {
            "-sE"
        }
    }

    #[time("debug", "Interpreter.{}")]
    pub fn resolve_base_interpreter(self, scripts: &mut Scripts) -> anyhow::Result<Interpreter> {
        if let Some(base_prefix) = self.details.base_prefix.as_ref()
            && base_prefix != &self.details.prefix
        {
            let resolved = Self::at_prefix(
                base_prefix,
                self.details.version,
                self.details.pypy_version,
                scripts,
                true,
            )?;
            return resolved.resolve_base_interpreter(scripts);
        }
        Ok(self)
    }

    pub fn is_venv(&self) -> bool {
        if let Some(base_prefix) = self.details.base_prefix.as_deref()
            && base_prefix != self.details.prefix
        {
            true
        } else {
            false
        }
    }

    pub fn platform_details(&self) -> &PlatformDetails<'_> {
        self.platform_details.borrow_platform_details()
    }
}

impl<'a> PythonPlatform<'a> for Interpreter {
    fn description(&self) -> impl Display {
        format!(
            "interpreter at {python_exe}",
            python_exe = self.details.path.display()
        )
    }

    fn marker_env(&self) -> &MarkerEnvironment {
        self.platform_details().marker_env()
    }

    fn supported_tags(&self) -> impl Iterator<Item = &'_ str> {
        self.platform_details().supported_tags()
    }

    fn primary_tag(&self) -> &str {
        self.platform_details().primary_tag()
    }
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::{Command, Stdio};

    use anyhow::Context;
    use pretty_assertions::assert_eq;
    use python_platform::PythonPlatform;
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

        let interpreter =
            Interpreter::load_uncached(&venv_python_exe, &interpreter_identification_script)
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
                .map(ToOwned::to_owned)
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
                .details
                .path
        )
    }
}
