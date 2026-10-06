// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(exit_status_error)]

use std::borrow::Cow;
use std::fmt::Display;
use std::io::Cursor;
use std::ops::Deref;
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::str::FromStr;
use std::{env, io};

use anyhow::{anyhow, bail};
use cache::{CacheDir, Key, atomic_dir};
use fs_err as fs;
use pep508_rs::Requirement;
use scripts::Scripts;
use serde::Deserialize;
use sha2::Sha256;
use tracing::{Level, debug_span, enabled, instrument};
use url::Url;
use venv::Virtualenv;

const WHEEL_BUILDER: &[u8] = include_bytes!("wheel-builder.py");

#[derive(Deserialize)]
struct BuildSystem<'a> {
    #[serde(rename = "build-backend")]
    build_backend: Option<&'a str>,
    #[serde(rename = "backend-path")]
    backend_path: Option<Vec<&'a Path>>,
    requires: Vec<Requirement<Url>>,
}

impl<'a> Default for BuildSystem<'a> {
    fn default() -> Self {
        Self {
            build_backend: Some("setuptools.build_meta:__legacy__"),
            backend_path: None,
            requires: vec![
                Requirement::from_str("setuptools").expect("This is a known valid requirement."),
            ],
        }
    }
}

#[derive(Deserialize)]
struct PyProject<'a> {
    #[serde(borrow, rename = "build-system")]
    build_system: BuildSystem<'a>,
}

pub trait VenvBuilder {
    fn create_at(
        &self,
        subject: &impl Display,
        venv_dir: PathBuf,
        requirements: &[Requirement<Url>],
    ) -> anyhow::Result<Virtualenv<'_>>;
}

#[instrument(level = "debug", skip_all, fields(subject = %subject))]
fn ensure_venv<'a>(
    subject: &impl Display,
    venv_builder: &'a impl VenvBuilder,
    requirements: &[Requirement<Url>],
    scripts: &mut Scripts,
) -> anyhow::Result<Virtualenv<'a>> {
    let venv_dir = {
        let mut key = Key::<Sha256>::new();
        let mut hashable_requirements = requirements
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        hashable_requirements.sort();
        key.list("requirements", hashable_requirements.iter());
        let fingerprint = key.fingerprint();

        CacheDir::BuildSystems
            .path()?
            .join(fingerprint.base64_digest())
    };
    if let Some(venv) = atomic_dir(&venv_dir, |venv_dir| {
        venv_builder.create_at(subject, venv_dir.to_path_buf(), requirements)
    })? {
        let venv_interpreter = Virtualenv::host_interpreter(&venv_dir, &venv.interpreter)?;
        venv_interpreter.store()?;
        Virtualenv::enclosing(venv_interpreter)
    } else {
        Virtualenv::load(Cow::Owned(venv_dir), scripts)
    }
}

pub struct ProjectDir(PathBuf);

impl ProjectDir {
    pub fn new(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let dir = path.as_ref().canonicalize()?;
        if !dir.is_dir() {
            bail!(
                "The given project path is not a directory: {}",
                dir.display()
            );
        }
        Ok(Self(dir))
    }

    pub fn push_sub_dir(&mut self, sub_dir: impl AsRef<Path>) -> anyhow::Result<()> {
        let sub_dir = sub_dir.as_ref();
        if sub_dir
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
        {
            bail!(
                "Sub-dirs can only consist of normal components; given: {}",
                sub_dir.display()
            )
        }
        self.0.push(sub_dir);
        Ok(())
    }
}

impl AsRef<Path> for ProjectDir {
    fn as_ref(&self) -> &Path {
        &self.0
    }
}

impl Deref for ProjectDir {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

const REPRODUCIBLE_BUILD_ENV: [(&str, &str); 2] = [
    ("PYTHONHASHSEED", "0"),
    // N.B.: This is Jan 1st 1980 00:00:00 UTC.
    ("SOURCE_DATE_EPOCH", "315532800"),
];

#[instrument(level = "debug", skip_all, fields(project = %project_dir.display()))]
pub fn build_wheel(
    project_dir: ProjectDir,
    dest_dir: &Path,
    venv_builder: &impl VenvBuilder,
    scripts: &mut Scripts,
) -> anyhow::Result<PathBuf> {
    let mut wheel_builder = tempfile::Builder::new()
        .prefix("wheel-builder")
        .suffix(".py")
        .tempfile_in(dest_dir)?;
    io::copy(&mut Cursor::new(WHEEL_BUILDER), &mut wheel_builder)?;

    let pyproject_content = {
        let pyproject_toml = project_dir.join("pyproject.toml");
        if pyproject_toml.is_file() {
            Some(fs::read_to_string(pyproject_toml)?)
        } else {
            None
        }
    };
    let mut build_system = if let Some(pyproject_content) = pyproject_content.as_deref() {
        let pyproject = toml::from_str::<PyProject>(pyproject_content)?;
        pyproject.build_system
    } else {
        BuildSystem::default()
    };
    let extra_sys_path = {
        if let Some(backend_path) = build_system.backend_path
            && !backend_path.is_empty()
        {
            let entry_count = backend_path.len();
            let mut entries = backend_path.into_iter().map(|entry_rel_path| {
                let entry = project_dir.join(entry_rel_path).canonicalize()?;
                if !entry.starts_with(&project_dir) {
                    bail!(
                        "The [build-system] `backend-path` for the Python project at {project_dir} \
                        contains entry \"{backend_path}\" which resolves to {entry} and is not a \
                        subdirectory of the project as required by PEP-517: \
                        https://peps.python.org/pep-0517/#in-tree-build-backends",
                        project_dir = project_dir.display(),
                        backend_path = entry_rel_path.display(),
                        entry = entry.display()
                    )
                }
                Ok(entry)
            });
            Some(if entry_count == 1 {
                entries.next().expect("We checked there was 1.")?.into()
            } else {
                env::join_paths(entries.collect::<anyhow::Result<Vec<_>>>()?)?
            })
        } else {
            None
        }
    };
    let create_wheel_builder_command = |python: &Path| -> anyhow::Result<Command> {
        let mut command = Command::new(python);
        command.arg(wheel_builder.path());
        if let Some(build_backend) = build_system.build_backend {
            command.arg("--backend").arg(build_backend);
        }
        if enabled!(Level::DEBUG) {
            command.arg("--verbose");
        }
        if let Some(extra_sys_path) = extra_sys_path.as_deref() {
            command.env("PEX_EXTRA_SYS_PATH", extra_sys_path);
        }
        command
            .current_dir(&project_dir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        Ok(command)
    };

    let subject = format!("Python project at {}", project_dir.display());
    let mut venv = ensure_venv(&subject, venv_builder, &build_system.requires, scripts)?;
    let requirements_for_build_wheel = {
        let span = debug_span!("Execute get_requires_for_build_wheel", project_dir = %project_dir.display());
        let _span = span.enter();
        let result = create_wheel_builder_command(&venv.interpreter.details.path)?
            .arg("get_requires_for_build_wheel")
            .spawn()
            .and_then(|process| process.wait_with_output())
            .map_err(|err| anyhow!("Failed to execute `get_requires_for_build_wheel`: {err}"))?;
        result.status.exit_ok().map_err(|err| {
            anyhow!(
                "Execution of `get_requires_for_build_wheel` failed with: {err}\n\
                Stderr from build backend:\n\
                {stderr}",
                stderr = String::from_utf8_lossy(&result.stderr).trim_end()
            )
        })?;
        serde_json::from_slice::<Vec<Requirement<Url>>>(&result.stdout)?
    };

    if !requirements_for_build_wheel.is_empty() {
        build_system.requires.extend(requirements_for_build_wheel);
        venv = ensure_venv(&subject, venv_builder, &build_system.requires, scripts)?;
    }

    let span = debug_span!("Execute build_wheel", project_dir = %project_dir.display());
    let _span = span.enter();
    let result = create_wheel_builder_command(&venv.interpreter.details.path)?
        .arg("build_wheel")
        .arg(dest_dir)
        .envs(REPRODUCIBLE_BUILD_ENV)
        .spawn()
        .and_then(|process| process.wait_with_output())
        .map_err(|err| anyhow!("Failed to execute `build_wheel`: {err}"))?;
    result.status.exit_ok().map_err(|err| {
        anyhow!(
            "Execution of `build_wheel` failed with: {err}\n\
            Stderr from build backend:\n\
            {stderr}",
            stderr = String::from_utf8_lossy(&result.stderr).trim_end()
        )
    })?;
    Ok(dest_dir.join(
        serde_json::from_slice::<&str>(&result.stdout).map_err(|err| {
            anyhow!(
                "Failed to parse output from `build_wheel`: {err}\nOutput:\n{}",
                String::from_utf8_lossy(&result.stdout)
            )
        })?,
    ))
}
