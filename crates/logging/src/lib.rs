// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

mod profiling;

use std::any::Any;
use std::str::FromStr;
use std::{env, io};

use anyhow::anyhow;
use log::LevelFilter;
use tracing_subscriber::Layer;
use tracing_subscriber::filter::LevelFilter as TracingLevelFiler;
use tracing_subscriber::fmt::format::FmtSpan;
use tracing_subscriber::fmt::time::Uptime;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;

const DEFAULT_LEVEL: LevelFilter = LevelFilter::Warn;

#[derive(Default)]
pub struct FlushGuard {
    _guards: Vec<Box<dyn Any>>,
}

pub fn init_default() -> anyhow::Result<FlushGuard> {
    init(None, None)
}

pub fn init(level: Option<LevelFilter>, ansi: Option<bool>) -> anyhow::Result<FlushGuard> {
    let mut layers = Vec::new();
    let mut log_layer = tracing_subscriber::fmt::layer()
        .with_writer(io::stderr)
        .with_thread_names(true)
        .with_span_events(FmtSpan::CLOSE)
        .with_timer(Uptime::default())
        .compact();
    if let Some(ansi) = ansi {
        log_layer = log_layer.with_ansi(ansi)
    }
    let level_filter = if let Some(level_filter) = level {
        level_filter
    } else {
        calculate_level()?
    };
    layers.push(
        log_layer
            .with_filter(as_tracing_level_filter(level_filter))
            .boxed(),
    );

    let flush_guard = profiling::configure(&mut layers)?;
    tracing_subscriber::registry().with(layers).init();
    Ok(flush_guard)
}

fn as_tracing_level_filter(level_filter: LevelFilter) -> TracingLevelFiler {
    match level_filter {
        LevelFilter::Off => TracingLevelFiler::OFF,
        LevelFilter::Error => TracingLevelFiler::ERROR,
        LevelFilter::Warn => TracingLevelFiler::WARN,
        LevelFilter::Info => TracingLevelFiler::INFO,
        LevelFilter::Debug => TracingLevelFiler::DEBUG,
        LevelFilter::Trace => TracingLevelFiler::TRACE,
    }
}

fn calculate_level() -> anyhow::Result<LevelFilter> {
    parse_pex_level().map(|pex_filter| {
        pex_filter
            .iter()
            .copied()
            .chain(parse_rust_level().iter().copied())
            .max()
            .unwrap_or(DEFAULT_LEVEL)
    })
}

fn parse_pex_level() -> anyhow::Result<Option<LevelFilter>> {
    let level = if let Some(level_var) = env::var_os("PEX_VERBOSE") {
        let level_str = level_var.into_string().map_err(|raw_value| {
            anyhow!(
                "PEX_VERBOSE must be an un-signed integer. Given non-UTF-8 value: {value}",
                value = raw_value.display()
            )
        })?;
        match u8::from_str(&level_str).map_err(|err| {
            anyhow!("PEX_VERBOSE must be an un-signed integer. Given {level_str}: {err}")
        })? {
            0 => DEFAULT_LEVEL,
            1 => DEFAULT_LEVEL.increment_severity(),
            2 => DEFAULT_LEVEL.increment_severity().increment_severity(),
            _ => DEFAULT_LEVEL
                .increment_severity()
                .increment_severity()
                .increment_severity(),
        }
    } else {
        return Ok(None);
    };
    Ok(Some(level))
}

fn parse_rust_level() -> Option<LevelFilter> {
    if let Ok(level_var) = env::var("RUST_LOG")
        && let Ok(filter_builder) = env_filter::Builder::new().try_parse(&level_var)
    {
        Some(filter_builder.build().filter())
    } else {
        None
    }
}
