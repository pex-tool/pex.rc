// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::sync::LazyLock;

use time::Month;
use tracing::warn;

pub static LATEST_STABLE: LazyLock<(u8, u8)> = LazyLock::new(|| {
    let minor = {
        // N.B.: This computes the maximum CPython minor version assuming CPython sticks to ~semver
        // and does not switch to calver.
        // + Release Schedule: https://peps.python.org/pep-0602/
        // + Rejected calver proposal: https://peps.python.org/pep-2026/
        //
        // Given PyPy history and the structure of the project, this max should always be greater
        // than the PyPy max minor.
        //
        // The calibration point: 3.14.0 was released on 2025-10-07 and there are yearly releases.
        let today = time::UtcDateTime::now().date();
        let years_since_pi_release = today.year() - 2025;
        let minor = 14 + years_since_pi_release;
        let mut minor = u8::try_from(minor).unwrap_or_else(|err| {
            warn!(
                "Failed to guess the current production release of CPython using the baseline \
                release of 3.14 ion 2025-10-07.\n\
                At a yearly release cadence incrementing the minor version number, \
                {minor} has overflowed a u8: {err}\n\
                Continuing with assumed max CPython production release of 3.255"
            );
            u8::MAX
        });
        if today.month() < Month::October {
            minor -= 1;
        }
        minor
    };
    (3, minor)
});

pub static OLDEST_SUPPORTED_STABLE: LazyLock<(u8, u8)> = LazyLock::new(|| {
    let minor = {
        // N.B.: This computes the minimum officially supported CPython minor version assuming
        // CPython sticks to ~semver and does not switch to calver.
        // + Release Schedule: https://peps.python.org/pep-0602/
        // + Rejected calver proposal: https://peps.python.org/pep-2026/
        //
        // The calibration point: 3.9 became end of life on 2025-10-31 and there are yearly advances
        // in the CPython minor version to next become EOL.
        let today = time::UtcDateTime::now().date();
        let years_since_39_eol = today.year() - 2025;
        let minor = 9 + years_since_39_eol;
        let mut minor = u8::try_from(minor).unwrap_or_else(|err| {
            warn!(
                "Failed to guess the oldest supported production release of CPython using the \
                baseline EOL of 3.9 ion 2025-10-31.\n\
                At a yearly release EOL cadence incrementing the last minor version number \
                supported, {minor} has overflowed a u8: {err}\n\
                Continuing with assumed oldest supported CPython production release of 3.255"
            );
            u8::MAX
        });
        if today.month() > Month::October {
            minor += 1;
        }
        minor
    };
    (3, minor)
});
