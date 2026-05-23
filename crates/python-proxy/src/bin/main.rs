// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::path::PathBuf;
use std::process::exit;
use std::{env, io};

use python_proxy::read_proxy;

#[cfg(unix)]
fn proxy_path() -> io::Result<PathBuf> {
    env::args()
        .next()
        .ok_or_else(|| io::Error::other("No argv0 was present; python-proxy cannot run."))
        .map(PathBuf::from)
}

#[cfg(windows)]
fn proxy_path() -> io::Result<PathBuf> {
    env::current_exe()
}

fn main() {
    let python_proxy = match proxy_path().and_then(read_proxy) {
        Ok(python) => python,
        Err(err) => {
            eprintln!("Failed to determine python executable path: {err}");
            exit(1);
        }
    };
    let (mut command, pexrc_cache_read_lock) = match python_proxy.prepare_command() {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("Failed to prepare python proxy command: {err}");
            exit(1);
        }
    };
    match platform::exec(&mut command, &[pexrc_cache_read_lock]) {
        Ok(status) => exit(status),
        Err(err) => {
            eprintln!(
                "Failed to spawn {python}: {err}",
                python = python_proxy.target.display()
            );
            exit(1);
        }
    }
}
