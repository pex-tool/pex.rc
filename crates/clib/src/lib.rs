// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

#![deny(clippy::all)]

use std::borrow::Cow;
use std::ffi::{CStr, CString, c_char, c_int};
use std::path::Path;
use std::ptr;

use pexrs::{boot as rust_boot, mount as rust_mount};

#[inline]
unsafe fn as_path(value: *const c_char, subject: &str) -> Result<&Path, String> {
    if value.is_null() {
        return Err(format!("{subject} path is a null pointer."));
    }

    // SAFETY: value must be a null-terminated character array.
    let cstr = unsafe { CStr::from_ptr(value) };
    cstr.to_str()
        .map(Path::new)
        // TODO: Handle valid WTF-8 on Windows but not valid UTF-8?
        .map_err(|e| format!("{subject} path is not valid UTF-8: {e}"))
}

#[inline]
unsafe fn as_argv(
    argv: *const *const c_char,
    argc: usize,
) -> Result<Vec<String>, Cow<'static, str>> {
    if argv.is_null() {
        return Err(Cow::Borrowed("The argv passed is a null pointer"));
    }

    // SAFETY: argv must be an array of length argc of null-terminated character arrays.
    let c_argv = unsafe { std::slice::from_raw_parts(argv, argc) };

    let mut argv = Vec::with_capacity(argc);
    for (idx, c_arg) in c_argv.iter().take(argc).enumerate() {
        if c_arg.is_null() {
            return Err(Cow::Owned(format!(
                "Command line arg {idx} is a null pointer."
            )));
        }
        // SAFETY: value must be a null-terminated character array.
        let cstr = unsafe { CStr::from_ptr(*c_arg) };
        let arg = cstr.to_str().map(str::to_string).map_err(|e| {
            // TODO: Handle valid WTF-8 on Windows but not valid UTF-8?
            Cow::Owned(format!("Command line arg {idx} is not valid UTF-8: {e}"))
        })?;
        argv.push(arg)
    }
    Ok(argv)
}

/// # Safety
///
/// The caller must ensure `python_exe` is a null-terminated character array.
/// The caller must ensure `python_argv` is an array of length `python_argc` of null-terminated
/// character arrays.
/// The caller must ensure `pex_file` is a null-terminated character array.
/// The caller must ensure `argv` is an array of length `argc` of null-terminated character arrays.
#[unsafe(export_name = "boot")]
pub unsafe extern "C" fn boot(
    python_exe: *const c_char,
    python_argv: *const *const c_char,
    python_argc: usize,
    pex_file: *const c_char,
    argv: *const *const c_char,
    argc: usize,
) -> c_int {
    // SAFETY: python_exe must be a null-terminated character array.
    let python_exe_path = match unsafe { as_path(python_exe, "Python executable") } {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };

    // SAFETY: python_argv must be an array of length python_argc of null-terminated character
    // arrays.
    let python_argv = match unsafe { as_argv(python_argv, python_argc) } {
        Ok(argv) => argv,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };

    // SAFETY: pex_file must be a null-terminated character array.
    let pex_path = match unsafe { as_path(pex_file, "PEX file") } {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };

    // SAFETY: argv must be an array of length argc of null-terminated character arrays.
    let argv = match unsafe { as_argv(argv, argc) } {
        Ok(argv) => argv,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };

    match rust_boot(Some(python_exe_path), python_argv, pex_path, argv, true) {
        Ok(code) => code,
        Err(err) => {
            eprintln!(
                "Problem booting PEX at {pex_path}: {err}",
                pex_path = pex_path.display()
            );
            1
        }
    }
}

/// # Safety
///
/// The caller must ensure `python_exe` and `pex_file` are null-terminated character arrays.
/// Additionally, `sys_path_entry` must be mutable and large enough to hold a null-terminated file
/// path on the system.
#[unsafe(export_name = "mount")]
pub unsafe extern "C" fn mount(
    python_exe: *const c_char,
    pex_file: *const c_char,
    sys_path_entry: *mut c_char,
) -> c_int {
    // SAFETY: python_exe must be a null-terminated character array.
    let python_exe_path = match unsafe { as_path(python_exe, "Python executable") } {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };

    // SAFETY: pex_file must be a null-terminated character array.
    let pex_path = match unsafe { as_path(pex_file, "PEX file") } {
        Ok(path) => path,
        Err(err) => {
            eprintln!("{err}");
            return 1;
        }
    };

    if sys_path_entry.is_null() {
        eprintln!("The sys_path_entry is a null pointer.");
        return 1;
    }

    match rust_mount(python_exe_path, pex_path) {
        Ok(path) => match CString::new(path.into_os_string().as_encoded_bytes()) {
            Ok(c_path) => {
                let c_path_bytes = c_path.as_bytes_with_nul();

                // SAFETY: sys_path_entry must be at least as long as c_path_bytes.
                unsafe {
                    ptr::copy_nonoverlapping(
                        c_path_bytes.as_ptr() as *const c_char,
                        sys_path_entry,
                        c_path_bytes.len(),
                    )
                }
                0
            }
            Err(err) => {
                eprintln!(
                    "Problem copying mount point for PEX at {pex_path}: {err}",
                    pex_path = pex_path.display()
                );
                1
            }
        },
        Err(err) => {
            eprintln!(
                "Problem mounting PEX at {pex_path}: {err}",
                pex_path = pex_path.display()
            );
            1
        }
    }
}
