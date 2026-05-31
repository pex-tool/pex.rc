// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use chrono::{DateTime, Utc};
use clap::{Args, ValueEnum};

use crate::package::WheelOptions;

#[derive(Clone, ValueEnum)]
pub enum CompressionMethod {
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

#[derive(Args)]
pub struct CompressionArgs {
    #[arg(short = 'Z', long, value_enum, default_value_t = CompressionMethod::Zstd)]
    compression_method: CompressionMethod,

    #[arg(long)]
    compression_level: Option<i64>,
}

impl CompressionArgs {
    pub fn into_wheel_options(self, timestamp: Option<DateTime<Utc>>) -> WheelOptions {
        WheelOptions::new(
            self.compression_method.into(),
            self.compression_level,
            timestamp,
        )
    }
}
