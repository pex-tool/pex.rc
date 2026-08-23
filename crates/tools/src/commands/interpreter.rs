// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::io::Write;
use std::path::Path;

use clap::Args;
use cli::{Json, Output};
use indexmap::indexset;
use interpreter::{
    Interpreter,
    InterpreterConstraint,
    InterpreterConstraints,
    Platform,
    SearchPath,
};
use pex::{Pex, ResolvedWheels};
use python_platform::PythonPlatform;
use rayon::iter::ParallelIterator;
use scripts::IdentifyInterpreter;
use serde_json::json;

#[derive(Args)]
pub(crate) struct InterpreterArgs {
    /// Print all compatible interpreters, preferred first.
    #[arg(short = 'a', long, default_value_t = false)]
    all: bool,

    /// Provide more information about the interpreter in JSON format.
    ///
    /// Once: include the interpreter requirement and platform in addition to its path.
    /// Twice: include the interpreter's supported tags.
    /// Thrice: include the interpreter's environment markers and its venv affiliation, if any.
    #[arg(short = 'v', long, action = clap::ArgAction::Count, verbatim_doc_comment)]
    verbose: u8,

    #[command(flatten)]
    json: Json,

    #[command(flatten)]
    output: Output,
}

pub(crate) fn display(
    python: Option<&Path>,
    pex: Pex,
    args: InterpreterArgs,
) -> anyhow::Result<()> {
    args.output.configure()?;

    let mut out = args.output.writer()?;
    for interpreter in compatible_interpreters(python, &pex, args.all)? {
        match args.verbose {
            0 => writeln!(
                &mut out,
                "{path}",
                path = interpreter.details.path.display()
            )?,
            1 => args.json.serialize(
                &mut out,
                &json!({
                    "path": interpreter.details.path,
                    "requirement": InterpreterConstraint::exact_version(&interpreter).to_string(),
                    "platform": Platform::of(&interpreter)?.to_string()
                }),
            )?,
            2 => args.json.serialize(
                &mut out,
                &json!({
                    "path": interpreter.details.path,
                    "requirement": InterpreterConstraint::exact_version(&interpreter).to_string(),
                    "platform": Platform::of(&interpreter)?.to_string(),
                    "supported_tags": interpreter.supported_tags().collect::<Vec<_>>()
                }),
            )?,
            _ => {
                if interpreter.is_venv() {
                    let mut scripts = pex.scripts()?;
                    let base_interpreter =
                        interpreter.clone().resolve_base_interpreter(&mut scripts)?;
                    args.json.serialize(
                        &mut out,
                        &json!({
                            "path": interpreter.details.path,
                            "requirement": InterpreterConstraint::exact_version(&interpreter).to_string(),
                            "platform": Platform::of(&interpreter)?.to_string(),
                            "supported_tags": interpreter.supported_tags().collect::<Vec<_>>(),
                            "env_markers": interpreter.marker_env(),
                            "venv": true,
                            "base_interpreter": base_interpreter.details.path
                        }),
                    )?
                } else {
                    args.json.serialize(
                        &mut out,
                        &json!({
                            "path": interpreter.details.path,
                            "requirement": InterpreterConstraint::exact_version(&interpreter).to_string(),
                            "platform": Platform::of(&interpreter)?.to_string(),
                            "supported_tags": interpreter.supported_tags().collect::<Vec<_>>(),
                            "env_markers": interpreter.marker_env(),
                            "venv": false
                        }),
                    )?
                }
            }
        }
    }
    Ok(())
}

fn compatible_interpreters(
    python: Option<&Path>,
    pex: &Pex,
    all: bool,
) -> anyhow::Result<impl IntoIterator<Item = Interpreter>> {
    let search_path = SearchPath::from_env()?;
    if all {
        let mut interpreters = indexset![
            pex.resolve(python, [].iter(), search_path.clone(), None)?
                .interpreter
        ];
        let mut scripts = pex.scripts()?;
        let dependency_configuration = pex.dependency_configuration()?;
        let identification_script = IdentifyInterpreter::read(&mut scripts)?;
        let interpreter_constraints =
            InterpreterConstraints::try_from(&pex.info.raw().interpreter_constraints)?;
        let resolved = pex.resolve_all(
            &identification_script,
            &interpreter_constraints,
            search_path,
            &dependency_configuration,
            None,
        )?;
        let filter = |result| match result {
            Ok(ResolvedWheels { interpreter, .. }) => Some(interpreter),
            Err(_) => None,
        };
        for interpreter in resolved.filter_map(filter).collect::<Vec<_>>() {
            interpreters.insert(interpreter);
        }
        Ok(interpreters)
    } else {
        Ok(indexset![
            pex.resolve(python, [].iter(), search_path, None)?
                .interpreter
        ])
    }
}
