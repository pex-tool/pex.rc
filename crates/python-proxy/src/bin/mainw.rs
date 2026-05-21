// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![cfg(windows)]
#![deny(clippy::all)]
#![windows_subsystem = "windows"]

use std::env;
use std::os::windows::process::CommandExt;
use std::process::{Child, exit};

use python_proxy::{PythonProxy, read_proxy};

// See: https://learn.microsoft.com/en-us/windows/win32/procthread/process-creation-flags
const DETACHED_PROCESS: u32 = 0x00000008;

fn main() {
    let python_proxy = match env::current_exe().and_then(read_proxy) {
        Ok(python) => python,
        Err(err) => {
            eprintln!("Failed to determine python executable path: {err}");
            exit(1);
        }
    };

    let (mut command, _pexrc_cache_read_lock) = match python_proxy.prepare_command() {
        Ok(proxy) => proxy,
        Err(err) => {
            eprintln!("Failed to prepare python proxy command: {err}");
            exit(1);
        }
    };
    command.creation_flags(DETACHED_PROCESS);

    match command.spawn() {
        Ok(mut child) => match child.wait() {
            Ok(exit_status) => {
                if !exit_status.success() {
                    exit(exit_status.code().unwrap_or(1))
                }
            }
            Err(err) => {
                eprintln!(
                    "Failed to wait for python proxy child process {id} to complete: {err}",
                    id = child.id()
                );
                exit(1)
            }
        },
        Err(err) => {
            eprintln!("{err}");
            exit(1)
        }
    }
}
