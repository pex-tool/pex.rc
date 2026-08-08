// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::borrow::Cow;
use std::ffi::{OsStr, OsString};
use std::io;
use std::io::{Read, Seek, Write};
use std::iter::Iterator;
use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use const_format::concatcp;
use fs_err as fs;
use fs_err::File;
use include_dir::{Dir, include_dir};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;
use zip::write::{FileOptionExtension, FileOptions, SimpleFileOptions};
use zip::{ZipArchive, ZipWriter};

#[derive(Copy, Clone, EnumIter)]
pub enum Script {
    IdentifyInterpreter,
    VendoredVirtualenv,
    VenvPex,
    VenvPexExtraSysPathPth,
    VenvPexExtraSysPathPy,
    VenvPexExtraSysPathStart,
    VenvPexRepl,
}

impl Script {
    pub const fn file_name(&self) -> &'static str {
        match self {
            Script::IdentifyInterpreter => "interpreter.py",
            Script::VendoredVirtualenv => "virtualenv.py",
            Script::VenvPex => "venv-pex.py",
            Script::VenvPexExtraSysPathPth => "PEX_EXTRA_SYS_PATH.pth",
            Script::VenvPexExtraSysPathPy => "PEX_EXTRA_SYS_PATH.py",
            Script::VenvPexExtraSysPathStart => "PEX_EXTRA_SYS_PATH.start",
            Script::VenvPexRepl => "venv-pex-repl.py",
        }
    }
}

pub enum Scripts {
    #[cfg(feature = "embedded")]
    Embedded,
    Loose(PathBuf),
    Zipped(ZipArchive<File>),
}

const ZIP_REL_PATH: &str = "__pex__/.scripts";
static HOST_REL_PATH: LazyLock<PathBuf> = LazyLock::new(|| ZIP_REL_PATH.split("/").collect());

#[cfg(feature = "embedded")]
const ACTIVATION_SCRIPTS_DIR: Dir<'static> = include_dir!("$ACTIVATION_SCRIPTS_DIR");

pub struct ActivationScript {
    pub file_name: Cow<'static, OsStr>,
    pub contents: Cow<'static, str>,
}

const ZIP_ACTIVATION_SCRIPTS_REL_PATH: &str = concatcp!(ZIP_REL_PATH, "/venv-activation");
static HOST_ACTIVATION_SCRIPTS_REL_PATH: LazyLock<PathBuf> =
    LazyLock::new(|| ZIP_ACTIVATION_SCRIPTS_REL_PATH.split("/").collect());

impl Scripts {
    pub fn read(&mut self, script: Script) -> anyhow::Result<Cow<'static, str>> {
        match self {
            #[cfg(feature = "embedded")]
            Scripts::Embedded => Ok(Cow::Borrowed(match script {
                Script::IdentifyInterpreter => include_str!("interpreter.py"),
                Script::VendoredVirtualenv => include_str!(env!("VIRTUALENV_PY")),
                Script::VenvPex => include_str!("venv-pex.py"),
                Script::VenvPexExtraSysPathPth => include_str!("PEX_EXTRA_SYS_PATH.pth"),
                Script::VenvPexExtraSysPathPy => include_str!("PEX_EXTRA_SYS_PATH.py"),
                Script::VenvPexExtraSysPathStart => include_str!("PEX_EXTRA_SYS_PATH.start"),
                Script::VenvPexRepl => include_str!("venv-pex-repl.py"),
            })),
            Scripts::Loose(base_dir) => {
                let resource_path = base_dir
                    .join(HOST_REL_PATH.as_path())
                    .join(script.file_name());
                Ok(Cow::Owned(fs::read_to_string(resource_path)?))
            }
            Scripts::Zipped(zip) => {
                let resource_path =
                    format!("{ZIP_REL_PATH}/{file_name}", file_name = script.file_name());
                let entry = zip.by_name(&resource_path)?;
                Ok(Cow::Owned(io::read_to_string(entry)?))
            }
        }
    }

    pub fn activation_scripts(&mut self) -> anyhow::Result<Vec<ActivationScript>> {
        match self {
            #[cfg(feature = "embedded")]
            Scripts::Embedded => Ok(ACTIVATION_SCRIPTS_DIR
                .files()
                .map(|file| ActivationScript {
                    file_name: Cow::Borrowed(
                        file.path()
                            .file_name()
                            .expect("The embedded activation scripts always have a file name."),
                    ),
                    contents: Cow::Borrowed(file.contents_utf8().expect(
                        "The embedded activations scripts always have valid UTF-8 content.",
                    )),
                })
                .collect()),
            Scripts::Loose(base_dir) => {
                let listing =
                    fs::read_dir(base_dir.join(HOST_ACTIVATION_SCRIPTS_REL_PATH.as_path()))?
                        .collect::<Result<Vec<_>, _>>()?;
                let mut activation_scripts = Vec::with_capacity(listing.len());
                for entry in listing {
                    let contents = fs::read_to_string(entry.path())?;
                    activation_scripts.push(ActivationScript {
                        file_name: Cow::Owned(entry.file_name()),
                        contents: Cow::Owned(contents),
                    });
                }
                Ok(activation_scripts)
            }
            Scripts::Zipped(zip) => {
                // N.B.: These names will include the dir name, which we want to skip and for which
                // we compensate below.
                let activation_script_file_names = zip
                    .file_names()
                    .filter(|file_name| file_name.starts_with(ZIP_ACTIVATION_SCRIPTS_REL_PATH))
                    .map(String::from)
                    .collect::<Vec<_>>();
                let mut activation_scripts =
                    Vec::with_capacity(activation_script_file_names.len() - 1);
                for file_name in activation_script_file_names {
                    let mut entry = zip.by_name(&file_name)?;
                    if !entry.is_file() {
                        continue;
                    }

                    let mut contents = String::with_capacity(usize::try_from(entry.size())?);
                    entry.read_to_string(&mut contents)?;
                    activation_scripts.push(ActivationScript {
                        file_name: Cow::Owned(OsString::from(file_name.rsplit("/").next().expect(
                            "We ensured the activation script was nested under a directory above.",
                        ))),
                        contents: Cow::Owned(contents),
                    });
                }
                Ok(activation_scripts)
            }
        }
    }

    pub fn inject<'a, T: FileOptionExtension + Copy>(
        &mut self,
        zip: &'a mut ZipWriter<impl Write + Seek>,
        file_options: FileOptions<'a, T>,
    ) -> anyhow::Result<()> {
        let directory_options = SimpleFileOptions::default();
        zip.add_directory(ZIP_REL_PATH, directory_options)?;
        for resource_path in Script::iter() {
            let text = self.read(resource_path)?;
            let script_path = format!(
                "{ZIP_REL_PATH}/{script}",
                script = resource_path.file_name()
            );
            zip.start_file(script_path, file_options)?;
            zip.write_all(text.as_bytes())?;
        }
        zip.add_directory(ZIP_ACTIVATION_SCRIPTS_REL_PATH, directory_options)?;
        for activation_script in self.activation_scripts()? {
            let activation_script_path = format!(
                "{ZIP_ACTIVATION_SCRIPTS_REL_PATH}/{script}",
                script = activation_script.file_name.display()
            );
            zip.start_file(activation_script_path, file_options)?;
            zip.write_all(activation_script.contents.as_ref().as_bytes())?;
        }
        Ok(())
    }

    pub fn write(&mut self, dest_dir: &Path) -> anyhow::Result<()> {
        let scripts_dir = dest_dir.join(HOST_REL_PATH.as_path());
        fs::create_dir_all(&scripts_dir)?;
        for resource_path in Script::iter() {
            let text = self.read(resource_path)?;
            let mut file = File::create_new(scripts_dir.join(resource_path.file_name()))?;
            file.write_all(text.as_bytes())?;
        }
        let activation_scripts_dir = dest_dir.join(HOST_ACTIVATION_SCRIPTS_REL_PATH.as_path());
        fs::create_dir_all(&activation_scripts_dir)?;
        for activation_script in self.activation_scripts()? {
            let mut file =
                File::create_new(activation_scripts_dir.join(activation_script.file_name))?;
            file.write_all(activation_script.contents.as_ref().as_bytes())?;
        }
        Ok(())
    }
}

macro_rules! generate_script_type {
    ( $script_type:ident ) => {
        pub struct $script_type<'a>(Cow<'a, str>);

        impl<'a> $script_type<'a> {
            pub fn read(scripts: &mut Scripts) -> anyhow::Result<$script_type<'a>> {
                let text = scripts.read(Script::$script_type)?;
                Ok($script_type(text))
            }

            pub fn contents(&self) -> &str {
                self.0.as_ref()
            }
        }
    };
}

generate_script_type!(IdentifyInterpreter);
generate_script_type!(VendoredVirtualenv);
generate_script_type!(VenvPex);
generate_script_type!(VenvPexExtraSysPathPth);
generate_script_type!(VenvPexExtraSysPathPy);
generate_script_type!(VenvPexExtraSysPathStart);
generate_script_type!(VenvPexRepl);
