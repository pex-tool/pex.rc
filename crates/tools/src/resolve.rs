// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use indexmap::IndexMap;
use interpreter::{Interpreter, SearchPath};
use pep440_rs::{Version, VersionSpecifiers};
use pep508_rs::{PackageName, Requirement};
use pex::Pex;
use resolver::CollectWheelMetadata;
use url::Url;

pub(crate) struct WheeInfo<'a> {
    pub file_name: &'a str,
    pub raw_project_name: &'a str,
    pub raw_version: &'a str,
    pub version: Version,
    pub requires_dists: Vec<Requirement<Url>>,
    pub requires_python: Option<VersionSpecifiers>,
}

pub(crate) fn resolve<'a>(
    python: Option<&Path>,
    pex: &'a Pex<'a>,
    additional_pexes: &'a [Pex<'a>],
) -> anyhow::Result<(Interpreter, IndexMap<PackageName, WheeInfo<'a>>)> {
    let search_path = SearchPath::from_env()?;
    let collect_metadata = CollectWheelMetadata::new();
    let resolve = pex.resolve(
        python,
        additional_pexes.iter(),
        search_path,
        Some(collect_metadata.clone()),
    )?;

    let mut resolved_metadata = collect_metadata.into_collected()?;
    resolved_metadata.sort_by_key(|metadata| metadata.file_name);
    let wheel_info = resolved_metadata
        .into_iter()
        .map(|metadata| {
            (
                metadata.project_name,
                WheeInfo {
                    file_name: metadata.file_name,
                    raw_project_name: metadata.raw_project_name,
                    raw_version: metadata.raw_version,
                    version: metadata.version,
                    requires_dists: metadata.requires_dists,
                    requires_python: metadata.requires_python,
                },
            )
        })
        .collect();
    Ok((resolve.interpreter, wheel_info))
}
