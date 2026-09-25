// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::io::{BufReader, Read, Write};
use std::path::Path;
use std::str::FromStr;

use anyhow::{anyhow, bail};
use cache::Fingerprint;
use indexmap::IndexMap;
use interpreter::SelectionStrategy;
use ouroboros::self_referencing;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::instrument;
use wheel::WheelFile;

#[derive(Copy, Clone, Debug, Deserialize, Serialize)]
pub enum BinPath {
    #[serde(rename = "false")]
    False,
    #[serde(rename = "append")]
    Append,
    #[serde(rename = "prepend")]
    Prepend,
}

impl BinPath {
    pub fn as_str(&self) -> &'static str {
        match self {
            BinPath::False => "false",
            BinPath::Append => "append",
            BinPath::Prepend => "prepend",
        }
    }
}

#[derive(Copy, Clone, Debug, Deserialize, Serialize)]
pub enum InheritPath {
    #[serde(rename = "false")]
    False,
    #[serde(rename = "prefer")]
    Prefer,
    #[serde(rename = "fallback")]
    Fallback,
}

impl FromStr for InheritPath {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "false" => Ok(Self::False),
            "prefer" => Ok(Self::Prefer),
            "fallback" => Ok(Self::Fallback),
            _ => bail!(
                "Invalid value for InheritPath: {s}.\n\
                Must be one of: false, prefer or fallback"
            ),
        }
    }
}

#[derive(Copy, Clone, Debug, Deserialize, Serialize)]
pub enum InterpreterSelectionStrategy {
    #[serde(rename = "oldest")]
    Oldest,
    #[serde(rename = "newest")]
    Newest,
}

impl From<InterpreterSelectionStrategy> for SelectionStrategy {
    fn from(value: InterpreterSelectionStrategy) -> Self {
        match value {
            InterpreterSelectionStrategy::Oldest => SelectionStrategy::Oldest,
            InterpreterSelectionStrategy::Newest => SelectionStrategy::Newest,
        }
    }
}

impl From<SelectionStrategy> for InterpreterSelectionStrategy {
    fn from(value: SelectionStrategy) -> Self {
        match value {
            SelectionStrategy::Oldest => Self::Oldest,
            SelectionStrategy::Newest => Self::Newest,
        }
    }
}

impl FromStr for InterpreterSelectionStrategy {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> anyhow::Result<Self> {
        match s {
            "oldest" => Ok(Self::Oldest),
            "newest" => Ok(Self::Newest),
            _ => bail!(
                "Invalid value for InterpreterSelectionStrategy: {s}.\n\
                Must be one of: oldest or newest"
            ),
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RawPexInfo<'a> {
    pub bind_resource_paths: Option<IndexMap<Cow<'a, str>, Cow<'a, str>>>,
    pub build_properties: IndexMap<&'a str, Value>,
    pub code_hash: &'a str,
    pub deps_are_wheel_files: bool,
    #[serde(borrow)]
    pub distributions: IndexMap<Cow<'a, str>, Cow<'a, str>>,
    pub emit_warnings: bool,
    #[serde(borrow)]
    pub entry_point: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pub excluded: Vec<Cow<'a, str>>,
    pub ignore_errors: bool,
    pub inherit_path: Option<InheritPath>,
    #[serde(borrow)]
    pub inject_args: Vec<Cow<'a, str>>,
    #[serde(borrow)]
    pub inject_env: Option<IndexMap<Cow<'a, str>, Cow<'a, str>>>,
    #[serde(borrow)]
    pub inject_python_args: Vec<Cow<'a, str>>,
    #[serde(borrow)]
    pub interpreter_constraints: Vec<Cow<'a, str>>,
    pub interpreter_selection_strategy: Option<InterpreterSelectionStrategy>,
    #[serde(borrow)]
    pub overridden: Vec<Cow<'a, str>>,
    #[serde(borrow)]
    pub pex_hash: Cow<'a, str>,
    #[serde(borrow)]
    pub pex_path: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pub pex_paths: Vec<Cow<'a, Path>>,
    #[serde(borrow)]
    pub pex_root: Option<Cow<'a, str>>,
    #[serde(borrow)]
    pub requirements: Vec<Cow<'a, str>>,
    #[serde(borrow)]
    pub script: Option<Cow<'a, str>>,
    pub strip_pex_env: Option<bool>,
    pub venv: bool,
    pub venv_bin_path: Option<BinPath>,
    pub venv_hermetic_scripts: bool,
    pub venv_system_site_packages: bool,
}

impl<'a> RawPexInfo<'a> {
    pub fn finalize_pex_hash(&mut self) -> anyhow::Result<&'_ str> {
        self.pex_hash = Cow::Borrowed("");
        // N.B.: If this PEX-INFO is from an injected PEX, the Pex version used to create that PEX
        // should not perturb the hash of the injected PEX since we do not use the Pex runtime code.
        let pex_version = self.build_properties.insert("pex_version", json!("0.0.0"));

        let bytes = serde_json::to_vec(&self)?;
        let mut digest = Sha256::new();
        digest.update(&bytes);

        self.pex_hash = Cow::Owned(Fingerprint::new(digest).hex_digest());
        if let Some(pex_version) = pex_version {
            self.build_properties.insert("pex_version", pex_version);
        } else {
            self.build_properties.shift_remove("pex_version");
        }

        Ok(&self.pex_hash)
    }

    pub fn write(&self, writer: impl Write) -> anyhow::Result<()> {
        Ok(serde_json::to_writer(writer, self)?)
    }

    pub fn has_entry_point(&self) -> bool {
        self.entry_point.is_some() || self.script.is_some()
    }
}

#[self_referencing]
pub struct PexInfo {
    data: Vec<u8>,
    #[borrows(data)]
    #[covariant]
    info: RawPexInfo<'this>,
}

impl PexInfo {
    #[instrument(level = "debug", skip_all)]
    pub fn parse<'a>(
        contents: impl Read,
        size: u64,
        source: Option<impl FnOnce() -> Cow<'a, str>>,
    ) -> anyhow::Result<PexInfo> {
        let mut data = Vec::with_capacity(usize::try_from(size)?);
        BufReader::new(contents).read_to_end(&mut data)?;
        Self::try_new(data, |data| {
            serde_json::from_slice(data).map_err(|err| {
                anyhow!(
                    "Failed to parse PEX-INFO from {source}: {err}",
                    source = source.map(|f| f()).unwrap_or(Cow::Borrowed("<string>"))
                )
            })
        })
    }

    pub fn parse_distributions(&self) -> impl Iterator<Item = anyhow::Result<WheelFile<'_>>> {
        self.borrow_info()
            .distributions
            .keys()
            .map(|file_name| WheelFile::parse_file_name(file_name.as_ref()))
    }

    pub fn write(&self, writer: impl Write) -> anyhow::Result<()> {
        self.borrow_info().write(writer)
    }

    #[inline]
    pub fn raw(&self) -> &RawPexInfo<'_> {
        self.borrow_info()
    }

    #[inline]
    pub fn with_raw_mut<R>(&mut self, func: impl FnOnce(&mut RawPexInfo) -> R) -> R {
        self.with_info_mut(func)
    }
}
