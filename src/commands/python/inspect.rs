// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use anyhow::anyhow;
use clap::Args;
use cli::{Json, Output};
use interpreter::{Interpreter, InterpreterConstraints, SearchPath, SelectionStrategy};
use python_platform::PythonPlatform;
use scripts::IdentifyInterpreter;
use scripts::Scripts::Embedded;
use serde_json::{Value, json};

#[derive(Args)]
#[group(skip)]
pub struct Inspect {
    #[command(flatten)]
    json_serializer: Json,

    #[command(flatten)]
    output: Output,

    #[arg()]
    python: Option<PathBuf>,
}

impl Inspect {
    pub fn execute(self) -> anyhow::Result<()> {
        self.output.configure()?;

        let identification_script = IdentifyInterpreter::read(&mut Embedded)?;
        let interpreter = self
            .python
            .and_then(|python| {
                Interpreter::load(&python, &identification_script)
                    .ok()
                    .map(Ok)
            })
            .unwrap_or_else(|| {
                InterpreterConstraints::EMPTY
                    .iter_possibly_compatible_python_exes(
                        SelectionStrategy::Newest,
                        SearchPath::from_env()?,
                        false,
                    )?
                    .filter_map(|python| Interpreter::load(&python, &identification_script).ok())
                    .next()
                    .ok_or_else(|| anyhow!("No Python installations could be found on the system."))
            })?;

        let mut out = self.output.writer()?;

        let mut object = serde_json::Map::new();
        object.insert("realpath".to_string(), json!(interpreter.realpath));
        // N.B.: This inlines the details as top-level keys.
        object.append(
            json!(interpreter.details).as_object_mut().expect(
                "Interpreter details is a struct which always equates to a json Object Map",
            ),
        );
        object.insert("env_markers".to_string(), json!(interpreter.marker_env()));
        object.insert(
            "supported_tags".to_string(),
            Value::from(interpreter.supported_tags().collect::<Vec<_>>()),
        );

        self.json_serializer
            .serialize(&mut out, &serde_json::Value::Object(object))?;
        Ok(())
    }
}
