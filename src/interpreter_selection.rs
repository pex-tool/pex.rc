// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::ffi::OsString;
use std::path::PathBuf;

use clap::Args;
use const_format::concatcp;
use interpreter::{
    Interpreter,
    InterpreterConstraint,
    InterpreterConstraints,
    SearchPath,
    SelectionStrategy,
};
use pex::InterpreterSelectionStrategy;

const PYTHON_PATH_HELP: &str = concatcp!(
    "A '",
    platform::PATH_SEP,
    "' separated list of paths to search for interpreters in (default: `$PATH`)."
);

const PYTHON_PATH_LONG_HELP: &str = concatcp!(
    PYTHON_PATH_HELP,
    r#"

Each element can be the absolute path of an interpreter binary or a directory containing
interpreter binaries.
"#
);

#[derive(Args, Debug)]
#[command(next_help_heading = "Interpreter Selection")]
pub struct InterpreterSelectionArgs {
    #[cfg_attr(
        // N.B.: This prevents doctest from attempting to analyze the code blocks. Otherwise;
        // fenced code blocks annotated with something like `text` would be needed, the problem
        // being the `text` fence pollutes the CLI console output.
        not(doctest),
        doc = r#"Constrain the selected Python interpreter.

You can constrain the implementation of the Python interpreter via:
+ CPython                             : Any CPython interpreter
+ CPython+t or CPython[free-threaded] : A free-threaded CPython interpreter
+ CPython-t or CPython[gil]           : A traditional GIL-enabled CPython interpreter
+ PyPy                                : Any PyPy interpreter

If the implementation of the Python interpreter is not important, a version specifier set
can be used to constrain the range of Python versions. For example:
+ >=3          : Any Python 3.x
+ >=3.11,<3.14 : Any Python 3.11, 3.12 or 3.13
+ ==3.14.*     : Any Python 3.14

You can also combine an implementation restriction with a version specifier set; e.g.:
+ CPython+t==3.14.* : Any CPython free threaded interpreter with version 3.14.x
+ PyPy>=3.11        : Any PyPy interpreter with version 3.11 or newer

If you specify multiple constraints, they will be logically ORed together such that selected
interpreters will meet at least one of the constraints.

To find the exact interpreter constraints of a local python, try:

    pexrc python inspect .venv/bin/python | jq -r .requirement

To find out the interpreter constraints of all Python interpreters on the `$PATH`, use:

    target/release/pexrc python inspect --all | jq -c '{path: .path, ic: .requirement}'

"#
    )]
    #[arg(long = "interpreter-constraint", verbatim_doc_comment)]
    interpreter_constraints: Vec<InterpreterConstraint>,

    /// Use this strategy to select between interpreters of differing major or minor version.
    ///
    /// N.B.: Whatever selection strategy is chosen, the highest available patch version is always
    /// selected as a tie-breaker when there is more than one compatible interpreter available.
    #[arg(long, verbatim_doc_comment)]
    interpreter_selection_strategy: Option<InterpreterSelectionStrategy>,

    #[arg(long, help=PYTHON_PATH_HELP, long_help=PYTHON_PATH_LONG_HELP)]
    python_path: Option<OsString>,
}

impl InterpreterSelectionArgs {
    pub fn finalize(self) -> InterpreterSelection {
        let constraints = if self.interpreter_constraints.is_empty() {
            InterpreterConstraints::EMPTY
        } else {
            self.interpreter_constraints.into()
        };

        let search_path = self
            .python_path
            .map(|pex_python_path| SearchPath::from_pex_python_path(pex_python_path, None));

        InterpreterSelection {
            constraints,
            selection_strategy: self
                .interpreter_selection_strategy
                .map(InterpreterSelectionStrategy::into),
            search_path,
        }
    }
}

pub struct InterpreterSelection {
    pub constraints: InterpreterConstraints,
    pub selection_strategy: Option<SelectionStrategy>,
    pub search_path: Option<SearchPath>,
}

impl InterpreterSelection {
    pub fn iter_possibly_compatible_python_exes(
        &self,
        include_pex_compatible: bool,
        default_selection_strategy: SelectionStrategy,
    ) -> anyhow::Result<impl Iterator<Item = PathBuf>> {
        let search_path = if let Some(search_path) = self.search_path.as_ref() {
            search_path.clone()
        } else {
            SearchPath::from_env()?
        };
        self.constraints.iter_possibly_compatible_python_exes(
            self.selection_strategy
                .unwrap_or(default_selection_strategy),
            search_path,
            include_pex_compatible,
        )
    }

    pub fn contains(&self, interpreter: &Interpreter) -> bool {
        if let Some(search_path) = &self.search_path
            && !search_path.contains(&interpreter.details.path)
            && !search_path.contains(&interpreter.realpath)
        {
            return false;
        }
        self.constraints
            .contains(interpreter.details.python_implementation())
    }
}
