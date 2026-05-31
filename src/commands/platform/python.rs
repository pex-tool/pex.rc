// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use clap::Args;
use cli::{Json, Output};
use interpreter::Interpreter;
use scripts::{IdentifyInterpreter, Scripts};

use crate::target::{PYTHON_PLATFORM_LONG_HELP, PythonPlatform};

#[derive(Args)]
#[group(skip)]
pub struct Python {
    #[command(flatten)]
    json: Json,

    #[command(flatten)]
    output: Output,

    /// The Python platform to inspect.
    #[arg(value_parser = PythonPlatform::parse, long_help=PYTHON_PLATFORM_LONG_HELP)]
    python_platform: PythonPlatform,
}

impl Python {
    pub fn execute(&self) -> anyhow::Result<()> {
        let mut out = self.output.writer()?;
        match &self.python_platform {
            PythonPlatform::Spec(spec) => {
                let platform = python_platform::parse(spec, None, None)?;
                self.json.serialize(&mut out, &platform)
            }
            PythonPlatform::Interpreter(path) => {
                let identification_script = IdentifyInterpreter::read(&mut Scripts::Embedded)?;
                let interpreter = Interpreter::load(path, &identification_script)?;
                self.json
                    .serialize(&mut out, interpreter.platform_details())
            }
        }
    }
}
