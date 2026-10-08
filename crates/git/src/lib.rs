// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(exit_status_error)]

use std::borrow::Cow;
use std::env;
use std::ffi::OsStr;
use std::fmt::Display;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::LazyLock;

use anyhow::anyhow;
use url::Url;
use which::which;

static GIT_EXECUTABLE: LazyLock<anyhow::Result<PathBuf>> = LazyLock::new(|| Ok(which("git")?));

pub struct GitCmd(Command);

impl GitCmd {
    pub fn arg(mut self, arg: impl AsRef<OsStr>) -> Self {
        self.0.arg(arg);
        self
    }

    pub fn args(mut self, args: impl IntoIterator<Item = impl AsRef<OsStr>>) -> Self {
        self.0.args(args);
        self
    }

    pub fn execute<D: Display>(mut self, action: impl Fn() -> D) -> anyhow::Result<Vec<u8>> {
        let result = self
            .0
            .spawn()
            .and_then(|child| child.wait_with_output())
            .map_err(|err| anyhow!("Failed to {action}: {err}", action = action()))?;
        result.status.exit_ok().map_err(|err| {
            anyhow!(
                "Failed to {action}: {err}\nGit stderr:\n{stderr}",
                stderr = String::from_utf8_lossy(&result.stderr),
                action = action()
            )
        })?;
        Ok(result.stdout)
    }
}

pub struct Git<'a> {
    executable: &'a Path,
    working_dir: Cow<'a, Path>,
}

impl<'a> Git<'a> {
    pub fn clone(remote: &Url, clone_dir: &'a Path) -> anyhow::Result<Self> {
        let git = Self {
            executable: GIT_EXECUTABLE.as_deref().map_err(|err| {
                anyhow!(
                    "A git executable is required to clone {remote} but none was found on the \
                    PATH: {err}",
                )
            })?,
            working_dir: Cow::Borrowed(clone_dir),
        };
        git.command()
            .arg("clone")
            .arg(remote.to_string())
            .execute(|| format!("clone {remote}"))?;
        Ok(git)
    }

    pub fn enclosing() -> anyhow::Result<Self> {
        let cwd = env::current_dir()?;
        Ok(Self {
            executable: GIT_EXECUTABLE.as_deref().map_err(|err| {
                anyhow!(
                    "A git executable is required to interact with the repo enclosing {cwd} but \
                    none was found on the PATH: {err}",
                    cwd = cwd.display()
                )
            })?,
            working_dir: Cow::Owned(cwd),
        })
    }

    pub fn command(&self) -> GitCmd {
        let mut command = Command::new(self.executable);
        command.current_dir(self.working_dir.as_ref());
        command.stdout(Stdio::piped());
        command.stderr(Stdio::piped());
        GitCmd(command)
    }
}
