// Copyright 2026 Pex project contributors.
// SPDX-License-Identifier: Apache-2.0

use std::path::Path;

use interpreter::Interpreter;
use pretty_assertions::assert_eq;
use python_platform::{
    Arch,
    Libc,
    Os,
    PlatformDetails,
    PlatformRelease,
    PlatformVersion,
    PythonPlatform,
    parse,
};
use rstest::rstest;
use scripts::IdentifyInterpreter;
use testing::{interpreter_identification_script, python_exe};

#[rstest]
fn test_abbreviated_platform(
    python_exe: &Path,
    interpreter_identification_script: IdentifyInterpreter,
) {
    let platform_details = PlatformDetails::python(python_exe).unwrap();
    let interpreter = Interpreter::load_uncached(
        python_exe,
        &interpreter_identification_script,
        platform_details,
    )
    .unwrap();

    let raw_interpreter = interpreter.raw();
    let spec = format!(
        "cpython-{major}.{minor}.{patch}-{os}-{arch}",
        major = raw_interpreter.version.major,
        minor = raw_interpreter.version.minor,
        patch = raw_interpreter.version.micro,
        os = match Os::current().unwrap() {
            Os::Linux(libc) => match libc {
                Libc::Gnu(libc_version) => format!(
                    "manylinux_{major}_{minor}",
                    major = libc_version.major,
                    minor = libc_version.minor
                ),
                Libc::Musl(libc_version) => format!(
                    "musllinux_{major}_{minor}",
                    major = libc_version.major,
                    minor = libc_version.minor
                ),
            },
            Os::Mac(mac_version) => {
                format!(
                    "macos_{major}_{minor}",
                    major = mac_version.major,
                    minor = mac_version.minor
                )
            }
            Os::Windows(release) =>
                if let Some(release) = release {
                    format!("windows_{release}")
                } else {
                    "windows".to_owned()
                },
        },
        arch = Arch::current().unwrap(),
    );
    let platform_details = parse(
        &spec,
        Some(PlatformRelease::new(
            interpreter.marker_env().platform_release(),
        )),
        Some(PlatformVersion::new(
            interpreter.marker_env().platform_version(),
        )),
    )
    .unwrap();

    assert_eq!(interpreter.marker_env(), &platform_details.marker_env);
    assert_eq!(
        interpreter.supported_tags().iter().collect::<Vec<_>>(),
        platform_details.supported_tags().iter().collect::<Vec<_>>()
    );
}
