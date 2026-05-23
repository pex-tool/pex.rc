// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::fmt::Display;
use std::sync::LazyLock;

use clap::ValueEnum;
use clap::builder::PossibleValue;
use indexmap::{Equivalent, IndexSet};

use crate::embeds::{CLIB_BY_TARGET, PROXY_BY_TARGET};

#[derive(Clone, Hash, Eq, PartialEq)]
pub struct SimplifiedTarget(target::SimplifiedTarget);

impl Display for SimplifiedTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.write_str(self.0.as_str())
    }
}

impl Equivalent<target::SimplifiedTarget> for SimplifiedTarget {
    fn equivalent(&self, key: &target::SimplifiedTarget) -> bool {
        &self.0 == key
    }
}

static AVAILABLE_TARGETS: LazyLock<Vec<SimplifiedTarget>> = LazyLock::new(|| {
    CLIB_BY_TARGET
        .keys()
        .chain(PROXY_BY_TARGET.keys())
        .collect::<IndexSet<_>>()
        .into_iter()
        .map(|target| SimplifiedTarget(*target))
        .collect()
});

impl ValueEnum for SimplifiedTarget {
    fn value_variants<'a>() -> &'a [Self] {
        AVAILABLE_TARGETS.as_slice()
    }

    fn to_possible_value(&self) -> Option<PossibleValue> {
        Some(PossibleValue::new(self.0.as_str()))
    }
}

impl From<SimplifiedTarget> for target::SimplifiedTarget {
    fn from(value: SimplifiedTarget) -> Self {
        value.0
    }
}
