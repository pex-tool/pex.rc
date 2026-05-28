// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]
#![feature(const_convert)]
#![feature(const_trait_impl)]

mod json;
mod output;

use clap::builder::Styles;
use clap::builder::styling::{AnsiColor, Effects};
pub use json::Json;
pub use output::Output;

pub const STYLES: Styles = Styles::styled()
    .header(AnsiColor::Blue.on_default().effects(Effects::BOLD))
    .usage(AnsiColor::Blue.on_default().effects(Effects::BOLD))
    .literal(AnsiColor::Cyan.on_default().effects(Effects::BOLD))
    .placeholder(AnsiColor::Yellow.on_default());
