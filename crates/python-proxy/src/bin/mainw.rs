// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![cfg(windows)]
#![deny(clippy::all)]
#![windows_subsystem = "windows"]

use std::env;
use std::ffi::CString;
use std::fmt::Display;
use std::os::windows::process::CommandExt;
use std::process::{Child, exit};

use anyhow::bail;
use python_proxy::read_proxy;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExA,
    DestroyWindow,
    HWND_MESSAGE,
    MB_OK,
    MSG,
    MessageBoxA,
    PM_REMOVE,
    PeekMessageA,
    WINDOW_EX_STYLE,
    WINDOW_STYLE,
};
use windows::core::{PCSTR, s};

#[macro_export]
macro_rules! error {
    ($($tt:tt)*) => {{
        $crate::error_and_exit(format_args!($($tt)*));
    }}
}

fn error_and_exit(message: impl Display) -> ! {
    let message = unsafe { CString::new(message.to_string()).unwrap_unchecked() };
    let message = PCSTR::from_raw(message.as_ptr() as *const _);
    unsafe {
        MessageBoxA(
            None,        // hWnd (No owner window - this is a free-floating error dialog)
            message,     // lpText
            s!("Error"), // lpCaption
            MB_OK,       // uType (Just an Ok button to dismiss the error dialog)
        )
    };
    exit(1)
}

fn clear_app_starting_cursor_state() {
    let mut msg = MSG::default();
    unsafe {
        // Create a message-only (invisible) window.
        // See: https://learn.microsoft.com/en-us/windows/win32/winmsg/window-features#message-only-windows
        if let Ok(hwnd) = CreateWindowExA(
            WINDOW_EX_STYLE(0), // dwExStyle
            // (See https://learn.microsoft.com/en-us/windows/win32/winmsg/about-window-classes#system-classes
            // for the Static system window class)
            s!("STATIC"),              // lpClassName
            s!("pexrc python-proxyw"), // lpWindowName
            WINDOW_STYLE(0),           // dwStyle
            0,                         // X
            0,                         // Y
            0,                         // nWidth
            0,                         // nHeight
            Some(HWND_MESSAGE),        // hWndParent (This is what makes the window message-only)
            None,                      // hMenu
            None,                      // hInstance
            None,                      // lpParam
        ) {
            // Process all pending messages (remove them from the window message queue); this is
            // enough to clear the app starting cursor state.
            // See: https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-peekmessagea
            let _ = PeekMessageA(&mut msg, Some(hwnd), 0, 0, PM_REMOVE);
            let _ = DestroyWindow(hwnd);
        }
    }
}

fn main() {
    let python_proxy = match env::current_exe().and_then(read_proxy) {
        Ok(python) => python,
        Err(err) => error!("Failed to determine python executable path: {err}"),
    };
    let (mut command, _pexrc_cache_read_lock) = match python_proxy.prepare_command() {
        Ok(proxy) => proxy,
        Err(err) => error!("Failed to prepare python proxy command: {err}"),
    };
    match command.spawn() {
        Ok(mut child) => {
            // Now that we've launched the child asynchronously, perform invisible windowed
            // application activity to indicate we've started.
            clear_app_starting_cursor_state();
            match child.wait() {
                Ok(exit_status) => {
                    if !exit_status.success() {
                        exit(exit_status.code().unwrap_or(1))
                    }
                }
                Err(err) => {
                    error!(
                        "Failed to wait for python proxy child process {id} to complete: {err}",
                        id = child.id()
                    )
                }
            }
        }
        Err(err) => {
            error!("Failed to spawn python proxy child process: {err}");
        }
    }
}
