// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use clap::{ArgAction, Args};
use cli::{Json, Output};
use interpreter::{
    Interpreter,
    InterpreterConstraint,
    InterpreterConstraints,
    SearchPath,
    SelectionStrategy,
};
use log::debug;
use owo_colors::OwoColorize;
use rayon::iter::{IntoParallelIterator, ParallelIterator};
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

    /// Limit the interpreters to those meeting any of the given constraints.
    ///
    /// Interpreter constraints are composed of implementation names and version specifiers in
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
    /// + `>=3.12`: any Python interpreter version 3.12 or greater
    /// + `CPython[free-threaded]==3.14.*`: any free-threaded CPython 3.14 interpreter
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
        let mut pythons = ics
            .iter_possibly_compatible_python_exes(
                SelectionStrategy::Newest,
                SearchPath::from_env()?,
                false,
            )?
            .collect::<Vec<_>>();

        if !ics.is_empty() {
            let identification_script = IdentifyInterpreter::read(&mut Scripts::Embedded)?;
            pythons = pythons
                .into_par_iter()
                .filter_map(
                    |python| match Interpreter::load(&python, &identification_script) {
                        Ok(interpreter)
                            if ics.contains(interpreter.details.python_implementation()) =>
                        {
                            Some(python)
                        }
                        Err(err) => {
                            debug!("Failed to load {python}: {err}", python = python.display());
                            None
                        }
                        _ => None,
                    },
                )
                .collect()
        }

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
