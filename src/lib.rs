// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(exit_status_error)]
#![feature(normalize_lexically)]
#![feature(os_str_split_at)]
#![feature(trim_prefix_suffix)]

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod commands;
pub mod compression_method;
pub mod embeds;
mod interpreter_selection;
pub mod simplified_target;
pub mod source;
pub mod target;
