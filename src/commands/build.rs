// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::borrow::Cow;
use std::io::BufReader;
use std::path::PathBuf;

use clap::{ArgAction, Args};
use fs_err::File;
use pep508_rs::Requirement;
use pex::{PexInfo, RawPexInfo};
use resolver::dependency_configuration::DependencyConfiguration;
use scripts::Scripts;
use url::Url;

use crate::target::{PYTHON_PLATFORM_LONG_HELP, PythonPlatform};

#[derive(Args, Debug)]
#[group(skip)]
pub struct Build {
    /// Requirements to include in the PEX.
    ///
    /// If no requirements are specified, an empty hermetic PEX will be generated.
    #[arg(
        value_name = "REQUIREMENT",
        help_heading = "Contents",
        verbatim_doc_comment
    )]
    requirements: Vec<Requirement<Url>>,

    /// Wheels (or directories containing wheels) to include in the PEX.
    ///
    /// There must be at least one wheel satisfying each direct requirement. If no targets are
    /// specified, that is the only check performed; otherwise a full transitive closure is
    /// confirmed for each specified target.
    #[arg(
        long,
        visible_alias = "wheel",
        value_name = "PATH",
        action = ArgAction::Append,
        help_heading = "Contents",
        verbatim_doc_comment
    )]
    wheels: Vec<PathBuf>,

    /// The Python platforms the built PEX will target at runtime.
    ///
    /// If specified, the targets will be used to resolve any specified requirements from the
    /// configured wheels. If required wheels are not present, the build will error.
    #[arg(
        long = "target",
        action = ArgAction::Append,
        help_heading = "Targets",
        value_parser = PythonPlatform::parse,
        long_help=PYTHON_PLATFORM_LONG_HELP,
        verbatim_doc_comment
    )]
    targets: Vec<PythonPlatform>,

    /// Existing PEX-INFO to use for the built PEX.
    ///
    /// If the PEX-INFO is from a traditional PEX it may be edited minimally to conform to the PEXrc
    /// runtime and any specified requirements. If no PEX-INFO is supplied, it will be created from
    /// the other given inputs.
    #[arg(long, help_heading = "Contents", verbatim_doc_comment)]
    pex_info: Option<PathBuf>,

    /// Instead of building a zipapp PEX, build a packed PEX.
    ///
    /// A Packed PEX is a directory containing a top-level `pex` script / `__main__.py` with wheels
    /// and other needed assets as-is under that. This can be useful in situations where using
    /// rsync-style transfer to ship incremental updates to large PEXes as opposed to having to ship
    /// the whole PEX.
    #[arg(
        long,
        help_heading = "Layout",
        default_value_t = false,
        verbatim_doc_comment
    )]
    packed: bool,

    /// Instead of booting via a Python shebang, boot via a Posix `sh` shebang.
    ///
    /// When running the PEX file directly (on Unix), instead of using a `#!/usr/bin/env python`
    /// style shebang, use a specially crafted `#!/bin/sh ...` shebang header that performs initial
    /// boot interpreter discovery smartly. If your PEX will target systems with a Posix shell at
    /// `/bin/sh` (overwhelmingly common on unix systems), this is the most robust and
    /// lowest-latency boot mode for repeated runs (at ~O(1ms)).
    ///
    /// N.B.: Both the Python and `sh` shebang headers are safe, but ignored on Windows systems.
    /// For those, you must run the PEX via Python (`python PEX`, `py PEX`, etc.) or else use an
    /// extension scheme you register with windows (Setting up a `.pyz` association is common).
    #[arg(
        long,
        help_heading = "Boot Mode",
        default_value_t = false,
        verbatim_doc_comment
    )]
    sh_boot: bool,

    /// The name of the generated PEX file.
    ///
    /// Omitting this will run PEX immediately and not save it to a file.
    ///
    /// If the name contains the {platform} placeholder, the most-specific platform tags supported
    /// by the PEX will be substituted. For example, for a multi-platform Linux x86-64, Mac ARM PEX
    /// containing platform-specific wheels, `-o 'example-{platform}.pex'` might expand to a PEX
    /// filename of `example-cp314-cp314-macosx_11_0_arm64.manylinux2014_x86_64.pex`.
    #[arg(
        short = 'o',
        long,
        visible_alias = "output-file",
        help_heading = "Output",
        verbatim_doc_comment
    )]
    output: Option<PathBuf>,
}

impl Build {
    pub fn execute(self) -> anyhow::Result<()> {
        if let Some(pex_info) = self.pex_info {
            let pex_info_file = File::open(&pex_info)?;
            let size = pex_info_file.metadata()?.len();
            let pex_info = PexInfo::parse(
                BufReader::new(pex_info_file),
                size,
                Some(|| Cow::Owned(pex_info.display().to_string())),
            )?;
            let requirements = if self.requirements.is_empty() {
                pex_info
                    .raw()
                    .requirements
                    .iter()
                    .map(|requirement| Ok(requirement.parse::<Requirement<Url>>()?))
                    .collect::<anyhow::Result<Vec<_>>>()?
            } else {
                self.requirements
            };
            create_pex(
                self.targets,
                requirements.as_slice(),
                self.wheels,
                pex_info.raw(),
                self.packed,
                self.sh_boot,
                self.output,
            )
        } else {
            let pex_info = RawPexInfo {
                requirements: self
                    .requirements
                    .iter()
                    .map(ToString::to_string)
                    .map(Cow::Owned)
                    .collect(),
                ..Default::default()
            };

            create_pex(
                self.targets,
                self.requirements.as_slice(),
                self.wheels,
                &pex_info,
                self.packed,
                self.sh_boot,
                self.output,
            )
        }
    }
}

fn create_pex(
    _targets: Vec<PythonPlatform>,
    _requirements: &[Requirement<Url>],
    _wheels: Vec<PathBuf>,
    pex_info: &RawPexInfo,
    _packed: bool,
    _sh_boot: bool,
    _output: Option<PathBuf>,
) -> anyhow::Result<()> {
    let _dependency_configuration = DependencyConfiguration::parse(
        pex_info.excluded.as_slice(),
        pex_info.overridden.as_slice(),
    )?;
    // 1. Resolve wheels (targets, requirements, wheels, dependency_configuration)
    let _scripts = Scripts::Embedded;
    // 2. Call create_packed_pex or create_zipapp (resolve, pex_info, scripts, sh_boot, output)
    todo!("Creating a PEX from sources and requirements is coming soon.")
}
