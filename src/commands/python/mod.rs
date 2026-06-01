// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use clap::Subcommand;

use crate::commands::python::inspect::Inspect;
use crate::commands::python::list::List;

mod inspect;
mod list;

#[derive(Subcommand)]
#[group(skip)]
pub enum Python {
    /// Inspect Python installation.
    Inspect(Inspect),
    /// List supported Python installations.
    List(List),
}
