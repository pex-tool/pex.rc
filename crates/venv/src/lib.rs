// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(exit_status_error)]
#![feature(slice_split_once)]
#![feature(trim_prefix_suffix)]
extern crate core;

mod provenance;
mod resolver;
mod script;
pub mod venv_pex;
pub mod virtualenv;

pub use provenance::{Collision, CollisionReport, Provenance};
pub use resolver::{InstallPaths, collect_installed_wheels};
pub use venv_pex::{InstallScope, populate, populate_user_code_and_wheels};
pub use virtualenv::{Linker, Virtualenv};
