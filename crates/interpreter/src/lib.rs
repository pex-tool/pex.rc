// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(coroutines)]
#![cfg_attr(windows, feature(coroutine_trait))]
#![cfg_attr(windows, feature(iter_from_coroutine))]
#![feature(stmt_expr_attributes)]
#![feature(str_as_str)]

mod constraints;
mod interpreter;

mod platform;
mod pyenv;
mod search_path;
mod tag;
mod version;

pub use constraints::unix::calculate_compatible_binary_names as calculate_compatible_unix_binary_names;
pub use constraints::{
    InterpreterConstraint,
    InterpreterConstraints,
    SelectionStrategy,
    VersionSpec,
};
pub use interpreter::{Interpreter, InterpreterDetails};
pub use platform::Platform;
pub use search_path::SearchPath;
pub use tag::Tag;
pub use version::{LATEST_STABLE, OLDEST_SUPPORTED_STABLE};
