// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

mod pex;
pub(crate) mod wheel;

use std::io::Write;

use fs_err::File;
pub use pex::{Scope, populate, populate_user_code_and_wheels};
use python_platform::PythonVersion;
use scripts::{Scripts, VenvPexExtraSysPathPth, VenvPexExtraSysPathPy, VenvPexExtraSysPathStart};
pub use wheel::populate_whl_zip;

use crate::Virtualenv;

pub fn write_pex_extra_sys_path_support_files(
    venv: &Virtualenv,
    scripts: &mut Scripts,
) -> anyhow::Result<()> {
    // See: https://peps.python.org/pep-0829/
    let mut pex_extra_sys_path_py_fp =
        File::create_new(venv.site_packages_path("PEX_EXTRA_SYS_PATH.py"))?;
    pex_extra_sys_path_py_fp
        .write_all(VenvPexExtraSysPathPy::read(scripts)?.contents().as_bytes())?;

    // Starting with Python 3.15 .start files trump import lines in .pth files.
    // See: https://peps.python.org/pep-0829/#abstract
    if venv.interpreter.details.version >= PythonVersion::simple(3, 15) {
        let mut pex_extra_sys_path_start_fp =
            File::create_new(venv.site_packages_path("PEX_EXTRA_SYS_PATH.start"))?;
        pex_extra_sys_path_start_fp.write_all(
            VenvPexExtraSysPathStart::read(scripts)?
                .contents()
                .as_bytes(),
        )?;
    }

    // After ~Python 3.20 .pth import lines will start to raise warnings; so we no longer emit a
    // .pth compatibility bridge. See: https://peps.python.org/pep-0829/#abstract
    if venv.interpreter.details.version < PythonVersion::simple(3, 20) {
        let mut pex_extra_sys_path_pth_fp =
            File::create_new(venv.site_packages_path("PEX_EXTRA_SYS_PATH.pth"))?;
        pex_extra_sys_path_pth_fp
            .write_all(VenvPexExtraSysPathPth::read(scripts)?.contents().as_bytes())?;
    }

    Ok(())
}
