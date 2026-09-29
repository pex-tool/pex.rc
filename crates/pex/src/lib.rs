// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

mod pex;
mod pex_info;
mod pex_path;

pub use pex::{
    DEPS_DIR,
    DEPS_ZIP_DIR,
    Layout,
    PEX_INFO_FILE,
    Pex,
    ResolveError,
    ResolvedWheels,
    SRCS_DIR,
    SRCS_ZIP_DIR,
    collect_loose_user_source,
    collect_zipped_user_source_indexes,
};
pub use pex_info::{BinPath, InheritPath, InterpreterSelectionStrategy, PexInfo, RawPexInfo};
pub use pex_path::PexPath;
