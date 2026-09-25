// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashSet;
use std::path::PathBuf;

use anyhow::anyhow;
use clap::Args;
use cli::{Json, Output};
use interpreter::{Interpreter, InterpreterConstraint, SelectionStrategy};
use python_platform::PythonPlatform;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
use scripts::IdentifyInterpreter;
use scripts::Scripts::Embedded;
use serde_json::{Value, json};

use crate::interpreter_selection::{InterpreterSelection, InterpreterSelectionArgs};

#[derive(Args)]
#[group(skip)]
pub struct Inspect {
    #[command(flatten)]
    json_serializer: Json,

    #[command(flatten)]
    output: Output,

    #[command(flatten)]
    interpreter_selection_args: InterpreterSelectionArgs,

    #[arg(long, conflicts_with = "python")]
    all: bool,

    #[arg(conflicts_with = "all")]
    python: Option<PathBuf>,
}

impl Inspect {
    pub fn execute(self) -> anyhow::Result<()> {
        let interpreter_selection = self.interpreter_selection_args.finalize();
        let interpreters = collect_interpreters(
            interpreter_selection,
            self.python,
            self.all,
            SelectionStrategy::Newest,
        )?;

        self.output.configure()?;
        let mut out = self.output.writer()?;
        let mut seen = HashSet::new();
        for interpreter in &interpreters {
            if !seen.insert(&interpreter.realpath) {
                continue;
            }
            let mut object = serde_json::Map::new();
            object.insert(
                "requirement".to_string(),
                json!(InterpreterConstraint::exact_version(interpreter).to_string()),
            );
            object.insert("realpath".to_string(), json!(interpreter.realpath));
            // N.B.: This inlines the details as top-level keys.
            object.append(json!(interpreter.details).as_object_mut().expect(
                "Interpreter details is a struct which always equates to a json Object Map",
            ));
            object.insert("env_markers".to_string(), json!(interpreter.marker_env()));
            object.insert(
                "supported_tags".to_string(),
                Value::from(interpreter.supported_tags().collect::<Vec<_>>()),
            );

            self.json_serializer
                .serialize(&mut out, &serde_json::Value::Object(object))?;
        }
        Ok(())
    }
}

fn collect_interpreters(
    interpreter_selection: InterpreterSelection,
    python: Option<PathBuf>,
    all: bool,
    default_selection_strategy: SelectionStrategy,
) -> anyhow::Result<Vec<Interpreter>> {
    let identification_script = IdentifyInterpreter::read(&mut Embedded)?;
    if all {
        let interpreters = interpreter_selection
            .iter_possibly_compatible_python_exes(false, default_selection_strategy)?
            .collect::<Vec<_>>();
        Ok(interpreters
            .into_par_iter()
            .filter_map(|python| Interpreter::load(&python, &identification_script).ok())
            .filter(|interpreter| interpreter_selection.contains(interpreter))
            .collect())
    } else {
        let interpreter = python
            .as_deref()
            .and_then(|python| {
                Interpreter::load(python, &identification_script)
                    .ok()
                    .map(Ok)
            })
            .unwrap_or_else(|| {
                interpreter_selection
                    .iter_possibly_compatible_python_exes(false, default_selection_strategy)?
                    .filter_map(|python| Interpreter::load(&python, &identification_script).ok())
                    .find(|interpreter| interpreter_selection.contains(interpreter))
                    .ok_or_else(|| anyhow!("No Python installations could be found on the system."))
            })?;
        Ok(vec![interpreter])
    }
}
