// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use std::str::FromStr;
use std::sync::LazyLock;

use anyhow::{anyhow, bail};
use indexmap::IndexSet;
use pep440_rs::Version;
use pep508_rs::{ExtraName, PackageName, Requirement, VersionOrUrl};
use python_platform::PythonPlatform;
use regex::{Regex, RegexBuilder};
use url::Url;
use version_ranges::Ranges;

use crate::PexInfo;

enum ExcludeConstraint {
    None,
    VersionRanges(Ranges<Version>),
    Url(Url),
}

pub struct DependencyConfiguration {
    excluded: HashMap<PackageName, ExcludeConstraint>,
    overridden: HashMap<PackageName, IndexSet<Requirement<Url>>>,
}

static OVERRIDE_REPLACE: LazyLock<Regex> = LazyLock::new(|| {
    RegexBuilder::new(
        r"^(?P<project>[A-Z0-9]|[A-Z0-9][A-Z0-9._-]*[A-Z0-9])\s*=\s*(?P<requirement>[A-Z0-9]|[A-Z0-9][A-Z0-9._-]*[A-Z0-9].*)$"
    ).case_insensitive(true).build().expect("This is a valid regex.")
});

impl DependencyConfiguration {
    pub(crate) fn load(pex_info: &PexInfo) -> anyhow::Result<Self> {
        let excluded = pex_info
            .raw()
            .excluded
            .iter()
            .map(|excluded| {
                match Requirement::<Url>::from_str(excluded).map_err(|err| anyhow!("{err}")) {
                    Ok(requirement) => {
                        let exclude_constraint = match requirement.version_or_url {
                            None => ExcludeConstraint::None,
                            Some(VersionOrUrl::VersionSpecifier(version_specifier)) => {
                                ExcludeConstraint::VersionRanges(Ranges::from(version_specifier))
                            }
                            Some(VersionOrUrl::Url(url)) => ExcludeConstraint::Url(url),
                        };
                        Ok((requirement.name, exclude_constraint))
                    }
                    Err(err) => Err(err),
                }
            })
            .collect::<anyhow::Result<HashMap<_, _>>>()?;

        let mut overridden: HashMap<PackageName, IndexSet<Requirement<Url>>> = HashMap::new();
        for override_spec in &pex_info.raw().overridden {
            let (name, requirement) = if let Some(captures) =
                OVERRIDE_REPLACE.captures(override_spec)
                && let Some(name) = captures.name("project")
                && let Some(requirement) = captures.name("requirement")
            {
                (
                    PackageName::new(name.as_str().to_string())?,
                    Requirement::from_str(requirement.as_str())?,
                )
            } else {
                let requirement = Requirement::from_str(override_spec)?;
                (requirement.name.clone(), requirement)
            };
            overridden.entry(name).or_default().insert(requirement);
        }

        Ok(Self {
            excluded,
            overridden,
        })
    }

    pub(crate) fn excluded(&self, requirement: &Requirement<Url>) -> bool {
        if let Some(constraint) = self.excluded.get(&requirement.name) {
            match constraint {
                ExcludeConstraint::None => true,
                ExcludeConstraint::VersionRanges(ranges) => match &requirement.version_or_url {
                    None => false,
                    Some(VersionOrUrl::VersionSpecifier(version_specifier)) => {
                        Ranges::from(version_specifier.clone()).subset_of(ranges)
                    }
                    Some(VersionOrUrl::Url(_)) => false,
                },
                ExcludeConstraint::Url(exclude_url) => {
                    matches!(&requirement.version_or_url, Some(VersionOrUrl::Url(req_url)) if req_url == exclude_url)
                }
            }
        } else {
            false
        }
    }

    pub(crate) fn overridden<'a>(
        &self,
        requirement: &Requirement<Url>,
        target: &impl PythonPlatform<'a>,
        extras: &[ExtraName],
    ) -> anyhow::Result<Option<Requirement<Url>>> {
        if let Some(overrides) = self.overridden.get(&requirement.name) {
            let marker_env = target.marker_env();
            let mut applicable_overrides = Vec::with_capacity(overrides.len());
            for requirement in overrides {
                if requirement.marker.evaluate(marker_env, extras) {
                    applicable_overrides.push(requirement)
                }
            }
            if applicable_overrides.len() > 1 {
                struct ApplicableOverrides<'a>(Vec<&'a Requirement<Url>>);
                impl<'a> Display for ApplicableOverrides<'a> {
                    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                        for (idx, requirement) in self.0.iter().copied().enumerate() {
                            writeln!(f, "{idx}. {requirement}", idx = idx + 1)?;
                        }
                        Ok(())
                    }
                }
                bail!(
                    "Invalid override configuration for {target}.\n\
                    More than one applicable override was found for {requirement}:\n\
                    {overrides}",
                    target = target.description(),
                    overrides = ApplicableOverrides(applicable_overrides)
                )
            }
            if !applicable_overrides.is_empty() {
                return Ok(Some(applicable_overrides[0].clone()));
            }
        }
        Ok(None)
    }
}
