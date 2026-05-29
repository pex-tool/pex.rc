// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::str::FromStr;

use anyhow::bail;

#[derive(Copy, Clone)]
pub(crate) enum Implementation {
    CPython,
    PyPy,
}

impl FromStr for Implementation {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        if s.eq_ignore_ascii_case("cpython") {
            Ok(Self::CPython)
        } else if s.eq_ignore_ascii_case("pypy") {
            Ok(Self::PyPy)
        } else {
            bail!("Un-supported Python implementation: {s}")
        }
    }
}
