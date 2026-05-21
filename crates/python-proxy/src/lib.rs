// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(string_from_utf8_lossy_owned)]

use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::LazyLock;
use std::{env, io};

use anyhow::anyhow;
use pex::{Layout, Pex};
use target::Target;
use zip::write::SimpleFileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

pub const SHEBANG_PREFIX: &str = "\n#!";
const SHEBANG_SUFFIX: &str = "\n";

const PATH_MAX: usize = 4096;

pub struct PythonProxy {
    pub proxy: PathBuf,
    pub target: PathBuf,
    pub has_script: bool,
}

impl PythonProxy {
    pub fn prepare_command(&self) -> io::Result<(Command, File)> {
        let mut command = if self.target.is_absolute() {
            Command::new(&self.target)
        } else if let Some(proxy_dir) = self.proxy.parent() {
            Command::new(proxy_dir.join(&self.target))
        } else {
            return Err(io::Error::other(format!(
                "The proxy target {target} is relative but the python-proxy at {proxy} has no \
                    parent directory to base that relative path in",
                target = self.target.display(),
                proxy = self.proxy.display()
            )));
        };
        if self.has_script {
            command.arg(self.proxy.as_os_str());
        }
        command.args(env::args_os().skip(1));
        command.env("__PYVENV_LAUNCHER__", &self.proxy);

        // N.B.: For Mac Python Framework builds (and Windows Python builds) __PYVENV_LAUNCHER__ is
        // deleted from the env on launch. We need to know about the launcher in the venv `pex`
        // script; so we duplicate that knowledge in our own env var.
        command.env("__PEXRC_PYVENV_LAUNCHER__", &self.proxy);

        let lock = match cache::read_lock() {
            Ok(lock) => lock,
            Err(err) => {
                return Err(io::Error::other(format!(
                    "Failed to obtain PEXRC cache read lock: {err}"
                )));
            }
        };
        Ok((command, lock))
    }
}

pub fn read_proxy(proxy: PathBuf) -> io::Result<PythonProxy> {
    let mut buf = vec![0u8; PATH_MAX];
    let mut exe_fp = BufReader::new(File::open(&proxy)?);
    exe_fp.seek(SeekFrom::End(-(buf.len() as i64)))?;
    exe_fp.read_to_end(&mut buf)?;
    match buf
        .windows(SHEBANG_PREFIX.len())
        .rposition(|chunk| SHEBANG_PREFIX.as_bytes() == chunk)
    {
        Some(index) => {
            const EOCD_MAGIC: &[u8] = b"PK\x05\x06";
            let eocd_start = index - 22;
            let has_script = &buf[eocd_start..(eocd_start + EOCD_MAGIC.len())] == EOCD_MAGIC;
            buf.drain(..index + SHEBANG_PREFIX.len());
            buf.truncate(buf.trim_ascii_end().len());
            let target = String::from_utf8(buf).map(PathBuf::from).map_err(|err| {
                io::Error::other(format!(
                    "Python shebang footer contained a non-UTF-8 path: {buf}",
                    buf = err.into_utf8_lossy()
                ))
            })?;
            Ok(PythonProxy {
                proxy,
                target,
                has_script,
            })
        }
        None => Err(io::Error::other("Failed to find Python shebang footer.")),
    }
}

pub enum ProxySource<'a> {
    Pex(&'a Pex<'a>),
    Read(Box<dyn Read + 'a>),
}

#[cfg(windows)]
pub fn create(
    proxy_source: ProxySource,
    interpreter: &Path,
    target_python: File,
    script: Option<String>,
    is_gui: bool,
) -> anyhow::Result<()> {
    use std::borrow::Cow;
    let interpreter = if is_gui {
        Cow::Owned(interpreter.with_file_name("pythonw.exe"))
    } else {
        Cow::Borrowed(interpreter)
    };
    create_proxy(
        proxy_source,
        interpreter.as_ref(),
        target_python,
        script,
        is_gui,
    )
}

#[cfg(unix)]
pub fn create(
    proxy_source: ProxySource,
    interpreter: &Path,
    target_python: File,
    script: Option<String>,
    is_gui: bool,
) -> anyhow::Result<()> {
    create_proxy(proxy_source, interpreter, target_python, script, is_gui)
}

fn create_proxy(
    proxy_source: ProxySource,
    interpreter: &Path,
    mut target_python: File,
    script: Option<String>,
    is_gui: bool,
) -> anyhow::Result<()> {
    match proxy_source {
        ProxySource::Pex(pex) => match pex.layout {
            Layout::Loose | Layout::Packed => {
                let mut python_proxy = read_python_proxy_from_dir(pex.path, is_gui)?;
                io::copy(&mut python_proxy, &mut target_python)?;
            }
            Layout::ZipApp => {
                let mut pex_zip = ZipArchive::new(File::open(pex.path)?)?;
                let mut python_proxy = read_python_proxy_from_zip(&mut pex_zip, is_gui)?;
                io::copy(&mut python_proxy, &mut target_python)?;
            }
        },
        ProxySource::Read(mut bytes) => {
            io::copy(&mut bytes, &mut target_python)?;
        }
    }
    let shebang_python = interpreter.as_os_str();
    if let Some(script) = script {
        let mut script_zip = ZipWriter::new(&target_python);
        script_zip.start_file(
            "__main__.py",
            SimpleFileOptions::default().compression_method(CompressionMethod::Deflated),
        )?;
        script_zip.write_all(script.as_bytes())?;
        script_zip.set_comment(format!(
            "{SHEBANG_PREFIX}{shebang_python}{SHEBANG_SUFFIX}",
            shebang_python = shebang_python.to_str().ok_or_else(|| anyhow!(
                "The shebang python path is not UTF-8: {shebang_python}",
                shebang_python = shebang_python.display()
            ))?
        ))?;
        script_zip.finish()?;
    } else {
        target_python.write_all(SHEBANG_PREFIX.as_bytes())?;
        target_python.write_all(shebang_python.as_encoded_bytes())?;
        target_python.write_all(SHEBANG_SUFFIX.as_bytes())?;
    }

    platform::mark_executable(&mut target_python)?;
    Ok(())
}

static PYTHON_PROXY_FILE_NAME: LazyLock<anyhow::Result<String>> = LazyLock::new(|| {
    let current_target = Target::current()?;
    current_target.fully_qualified_binary_name("python-proxy", None)
});

static PYTHON_PROXY_GUI_FILE_NAME: LazyLock<anyhow::Result<String>> = LazyLock::new(|| {
    let current_target = Target::current()?;
    current_target.fully_qualified_binary_name("python-proxyw", None)
});

fn python_proxy_file_name<'a>(is_gui: bool) -> anyhow::Result<&'a str> {
    if is_gui {
        PYTHON_PROXY_GUI_FILE_NAME
            .as_deref()
            .map_err(|err| anyhow!("{err}"))
    } else {
        PYTHON_PROXY_FILE_NAME
            .as_deref()
            .map_err(|err| anyhow!("{err}"))
    }
}

fn read_python_proxy_from_dir(pex_dir: &Path, is_gui: bool) -> anyhow::Result<impl Read> {
    Ok(File::open(
        pex_dir
            .join("__pex__")
            .join(".proxies")
            .join(python_proxy_file_name(is_gui)?),
    )?)
}

fn read_python_proxy_from_zip(
    pex_zip: &mut ZipArchive<impl Read + Seek>,
    is_gui: bool,
) -> anyhow::Result<impl Read> {
    Ok(pex_zip.by_name(&format!(
        "__pex__/.proxies/{python_proxy_name}",
        python_proxy_name = python_proxy_file_name(is_gui)?
    ))?)
}
