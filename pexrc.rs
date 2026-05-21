// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::fmt::Display;
use std::path::PathBuf;
use std::sync::LazyLock;

use clap::builder::PossibleValue;
use clap::{ArgAction, Parser, Subcommand, ValueEnum};
use indexmap::{Equivalent, IndexSet};
use pex::{Pex, WheelOptions};
use pexrc::commands::{extract, info, inject, script};
use pexrc::embeds::{CLIB_BY_TARGET, PROXY_BY_TARGET, PROXYW_BY_TARGET};
use pexrc::source;
use target::Target;

/// Pex Runtime Control.
#[derive(Parser)]
#[command(version, about, long_about = None, styles = cli::STYLES)]
struct Cli {
    #[command(flatten)]
    verbosity: Option<clap_verbosity_flag::Verbosity>,

    #[command(flatten)]
    color: colorchoice_clap::Color,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, ValueEnum)]
enum CompressionMethod {
    Deflated,
    Zstd,
}

impl From<CompressionMethod> for zip::CompressionMethod {
    fn from(val: CompressionMethod) -> Self {
        match val {
            CompressionMethod::Deflated => zip::CompressionMethod::Deflated,
            CompressionMethod::Zstd => zip::CompressionMethod::Zstd,
        }
    }
}

#[derive(Clone, Hash, Eq, PartialEq)]
struct SimplifiedTarget(target::SimplifiedTarget);

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

#[derive(Subcommand)]
enum Commands {
    /// Extract PEX dependencies as wheels.
    Extract {
        /// The maximum number of parallel jobs to use when extracting wheels.
        #[arg(short = 'j', long)]
        jobs: Option<usize>,

        #[arg(short = 'Z', long, value_enum, default_value_t = CompressionMethod::Zstd)]
        compression_method: CompressionMethod,

        #[arg(long)]
        compression_level: Option<i64>,

        /// The directory to extract the wheels to.
        #[arg(short = 'd', long)]
        dest_dir: PathBuf,

        /// The PEX to extract dependency wheels from. Can be a path or URL.
        #[arg(value_name = "PEX")]
        pex: String,
    },
    /// Inject traditional PEXes with native runtimes.
    Inject {
        /// The maximum number of parallel jobs to use when injecting PEXes.
        #[arg(short = 'j', long)]
        jobs: Option<usize>,

        #[arg(short = 'Z', long, value_enum, default_value_t = CompressionMethod::Zstd)]
        compression_method: CompressionMethod,

        #[arg(long)]
        compression_level: Option<i64>,

        #[arg(long = "target")]
        #[arg(action=ArgAction::Append)]
        targets: Vec<SimplifiedTarget>,

        #[arg(short = 'p', long)]
        preferred_python: Option<PathBuf>,

        /// The PEXes to inject with a native runtime. Can be paths or URLs.
        #[arg(value_name = "PEX", required = true)]
        pexes: Vec<String>,
    },
    /// Provide information about the supported target runtimes.
    Info,
    /// Create a Windows-style Python venv console script executable.
    Script {
        #[arg(long)]
        target: Option<SimplifiedTarget>,

        #[arg(short = 'p', long)]
        python: PathBuf,

        #[arg(short = 'o', long)]
        output_file: PathBuf,

        #[arg(long, default_value_t = false)]
        gui: bool,

        #[arg(value_name = "SCRIPT")]
        script: PathBuf,
    },
}

fn configure_jobs(jobs: Option<usize>) -> anyhow::Result<()> {
    if let Some(jobs) = jobs {
        rayon::ThreadPoolBuilder::default()
            .num_threads(jobs)
            .build_global()?;
    }
    Ok(())
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    logging::init(cli.verbosity.map(|verbosity| verbosity.log_level_filter()))?;
    cli.color.write_global();

    match cli.command {
        Commands::Extract {
            jobs,
            compression_method,
            compression_level,
            dest_dir,
            pex,
        } => {
            configure_jobs(jobs)?;
            let pex = source::to_path(pex, Some(&dest_dir))?;
            extract::to_dir(
                &dest_dir,
                Pex::load(&pex)?,
                &WheelOptions::new(compression_method.into(), compression_level, None),
            )
        }
        Commands::Inject {
            jobs,
            compression_method,
            compression_level,
            targets,
            preferred_python,
            pexes,
        } => {
            configure_jobs(jobs)?;
            let (clibs, proxies) = if !targets.is_empty() {
                (
                    targets
                        .iter()
                        .map(|target| {
                            CLIB_BY_TARGET.get(target).expect(
                                "The allowed --target values are all keys in CLIB_BY_TARGET.",
                            )
                        })
                        .collect::<Vec<_>>(),
                    targets
                        .iter()
                        .map(|target| {
                            PROXY_BY_TARGET.get(target).expect(
                                "The allowed --target values are all keys in PROXY_BY_TARGET.",
                            )
                        })
                        .chain(
                            targets
                                .iter()
                                .filter_map(|target| PROXYW_BY_TARGET.get(target)),
                        )
                        .collect::<Vec<_>>(),
                )
            } else {
                (
                    CLIB_BY_TARGET.values().collect::<Vec<_>>(),
                    PROXY_BY_TARGET
                        .values()
                        .chain(PROXYW_BY_TARGET.values())
                        .collect::<Vec<_>>(),
                )
            };
            let pexes = pexes
                .into_iter()
                .map(|source| source::to_path(source, None))
                .collect::<anyhow::Result<Vec<_>>>()?;
            inject::inject_all(
                pexes,
                &WheelOptions::new(compression_method.into(), compression_level, None),
                clibs.as_slice(),
                proxies.as_slice(),
                preferred_python.as_deref(),
            )
        }
        Commands::Info => info::display(),
        Commands::Script {
            target,
            python,
            script,
            output_file,
            gui: is_gui,
        } => {
            if let Some(target) = target {
                script::create(target.into(), &python, &script, &output_file, is_gui)
            } else {
                let current_target = Target::current()?;
                script::create(
                    current_target.simplified_target_triple()?,
                    &python,
                    &script,
                    &output_file,
                    is_gui,
                )
            }
        }
    }
}
