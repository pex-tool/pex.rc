// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::env;
use std::path::PathBuf;

use fs_err as fs;

fn chrome_profile(create_dirs: bool) -> anyhow::Result<Option<PathBuf>> {
    profile_path("PEXRC_JSON_PROFILE", "profile.json", create_dirs)
}

fn flame_profile(create_dirs: bool) -> anyhow::Result<Option<PathBuf>> {
    profile_path("PEXRC_FLAME_PROFILE", "profile.flame", create_dirs)
}

fn profile_path(
    env_var: &str,
    default_filename: &str,
    create_dirs: bool,
) -> anyhow::Result<Option<PathBuf>> {
    if let Some(value) = env::var_os(env_var) {
        let path = if value.is_empty() {
            PathBuf::from(default_filename)
        } else {
            let specified = PathBuf::from(value);
            if create_dirs && let Some(parent) = specified.parent() {
                fs::create_dir_all(parent)?;
            }
            specified
        };
        Ok(Some(path))
    } else {
        Ok(None)
    }
}

pub(crate) use layers::configure;

#[cfg(feature = "profiling")]
pub(crate) mod layers {
    use std::any::Any;

    use tracing::level_filters::LevelFilter;
    use tracing_chrome::{ChromeLayerBuilder, TraceStyle};
    use tracing_flame::FlameLayer;
    use tracing_subscriber::{Layer, Registry};

    use crate::FlushGuard;

    pub(crate) fn configure(
        layers: &mut Vec<Box<dyn Layer<Registry> + Send + Sync>>,
    ) -> anyhow::Result<FlushGuard> {
        let mut guards: Vec<Box<dyn Any>> = vec![];
        if let Some(profile_path) = super::chrome_profile(true)? {
            let (chrome_layer, flush_guard) = ChromeLayerBuilder::new()
                .file(profile_path)
                .include_args(true)
                .trace_style(TraceStyle::Threaded)
                .build();
            layers.push(chrome_layer.with_filter(LevelFilter::TRACE).boxed());
            guards.push(Box::new(flush_guard));
        }
        if let Some(profile_path) = super::flame_profile(true)? {
            let (flame_layer, flush_guard) = FlameLayer::with_file(profile_path)?;
            layers.push(
                flame_layer
                    .with_threads_collapsed(true)
                    .with_filter(LevelFilter::TRACE)
                    .boxed(),
            );
            guards.push(Box::new(flush_guard));
        }
        Ok(FlushGuard { _guards: guards })
    }
}

#[cfg(not(feature = "profiling"))]
pub(crate) mod layers {
    use std::fmt::{Display, Formatter};
    use std::path::PathBuf;

    use owo_colors::OwoColorize;
    use tracing_subscriber::{Layer, Registry};

    use crate::FlushGuard;

    pub(crate) fn configure(
        _layers: &mut Vec<Box<dyn Layer<Registry> + Send + Sync>>,
    ) -> anyhow::Result<FlushGuard> {
        let profile_paths = super::chrome_profile(false)?
            .into_iter()
            .chain(super::flame_profile(false)?)
            .collect::<Vec<_>>();
        if !profile_paths.is_empty() {
            struct Message(Vec<PathBuf>);
            impl Display for Message {
                fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
                    write!(
                        f,
                        "The profiling feature is not enabled in this build of pexrc.\n\
                        Will not generate {profiles} to:",
                        profiles = if self.0.len() == 1 {
                            "profile"
                        } else {
                            "profiles"
                        },
                    )?;
                    for (index, profile_path) in self.0.iter().enumerate() {
                        if index > 0 {
                            f.write_str(" or")?;
                        }
                        write!(f, " {}", profile_path.display())?;
                    }
                    Ok(())
                }
            }
            anstream::eprintln!("{}", Message(profile_paths).yellow())
        }
        Ok(FlushGuard::default())
    }
}
