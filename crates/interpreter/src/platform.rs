// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::{Display, Formatter};

use python_platform::{PythonPlatform, PythonVersion};

use crate::{Interpreter, Tag};

pub struct Platform<'a> {
    implementation: &'a str,
    version: PythonVersion,
    abi: &'a str,
    platform: &'a str,
}

impl<'a> Platform<'a> {
    pub fn of(interpreter: &'a Interpreter) -> anyhow::Result<Self> {
        let tag = Tag::parse(interpreter.supported_tags().first())?;
        Ok(Self {
            implementation: &tag.python[0..2],
            version: interpreter.details.version,
            abi: tag.abi,
            platform: tag.platform,
        })
    }
}

impl<'a> Display for Platform<'a> {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{platform}-{implementation}-{version}-{abi}",
            platform = self.platform,
            implementation = self.implementation,
            version = self.version,
            abi = self.abi
        )
    }
}
