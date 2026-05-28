// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

mod list;
mod python;

use clap::Subcommand;
pub use list::List;
pub use python::Python;

#[derive(Subcommand)]
#[group(skip)]
pub enum Platform {
    /// List supported platforms.
    List(List),
    /// Materialize Python platform information used in resolves.
    Python(Python),
}
