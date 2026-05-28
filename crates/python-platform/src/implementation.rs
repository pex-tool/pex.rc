// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use anyhow::bail;

#[derive(Copy, Clone)]
pub(crate) enum Implementation {
    CPython,
    PyPy,
}

impl Implementation {
    pub(crate) fn parse(value: &str) -> anyhow::Result<Self> {
        if value.eq_ignore_ascii_case("cpython") {
            Ok(Self::CPython)
        } else if value.eq_ignore_ascii_case("pypy") {
            Ok(Self::PyPy)
        } else {
            bail!("Un-supported Python implementation: {value}")
        }
    }
}
