// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::path::PathBuf;

use clap::{ArgAction, Args};
use cli::{Json, Output};
use interpreter::{
    Interpreter,
    InterpreterConstraint,
    InterpreterConstraints,
    SearchPath,
    SelectionStrategy,
};
use owo_colors::OwoColorize;
use scripts::{IdentifyInterpreter, Scripts};

#[derive(Args)]
#[group(skip)]
pub struct List {
    /// Output discovered Python paths in JSON.
    #[arg(long, default_value_t = false, help_heading = "Output")]
    json: bool,

    #[command(flatten)]
    json_serializer: Json,

    #[command(flatten)]
    output: Output,

    /// Constrain the interpreters to those meeting any of the given constraints.
    ///
    /// Interpreter constraints are composed of implementation names and a version specifiers in
    /// one of the following forms:
    /// + `<implementation name>`
    /// + `<version specifier>`
    /// + `<implementation name><version specifier>`
    ///
    /// Implementation names are:
    /// + `CPython`: any CPython interpreter
    /// + `CPython+t` or `CPython[free-threaded]`: a free-threaded CPython interpreter
    /// + `CPython-t` or `CPython[gil]`: a traditional GIL-enabled CPython interpreter
    /// + `PyPy`: any PyPy interpreter
    ///
    /// Version specifiers are PEP-440 [^1] version specifiers [^2].
    ///
    /// Some examples:
    /// + `PyPy`: any PyPy interpreter
    /// + `==3.14.*`: any Python interpreter version 3.14
    /// + `CPython>=3.12`: any CPython interpreter version 3.12 or greater
    ///
    /// [^1]: https://peps.python.org/pep-0440/
    /// [^2]: https://packaging.python.org/specifications/version-specifiers/#version-specifiers
    #[arg(
        short = 'c',
        long = "constraint",
        visible_aliases = ["ic", "interpreter-constraint"],
        value_name = "CONSTRAINT",
        help_heading = "Constraints",
        action = ArgAction::Append,
        value_parser = InterpreterConstraint::parse,
        verbatim_doc_comment,
    )]
    constraints: Vec<InterpreterConstraint>,
}

impl List {
    pub fn execute(self) -> anyhow::Result<()> {
        let ics = InterpreterConstraints::from(self.constraints);
        let pythons = ics.iter_possibly_compatible_python_exes(
            SelectionStrategy::Newest,
            SearchPath::from_env()?,
            false,
        )?;
        let pythons: Vec<PathBuf> = if !ics.is_empty() {
            let identification_script = IdentifyInterpreter::read(&mut Scripts::Embedded)?;
            pythons
                .filter_map(|python| {
                    if let Some(interpreter) =
                        Interpreter::load(&python, &identification_script).ok()
                        && ics.contains(&interpreter)
                    {
                        Some(python)
                    } else {
                        None
                    }
                })
                .collect()
        } else {
            pythons.collect()
        };

        let mut out = self.output.writer()?;
        if self.json {
            self.json_serializer.serialize(&mut out, &pythons)?;
        } else {
            for python in pythons {
                anstream::println!("{}", python.display().blue())
            }
        }
        Ok(())
    }
}
