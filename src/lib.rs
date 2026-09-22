// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(trim_prefix_suffix)]

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

pub mod commands;
pub mod compression_method;
pub mod embeds;
pub mod simplified_target;
pub mod source;
pub mod target;
