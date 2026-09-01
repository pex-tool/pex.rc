use std::borrow::Cow;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

use interpreter::Interpreter;
use repackage::WheelOptions;
use rstest::rstest;
use scripts::{IdentifyInterpreter, Scripts};
use testing::{embedded_scripts, interpreter_identification_script, python_exe, tmp_dir};
use venv::virtualenv::FileSystemLinker;
use venv::{InstallPaths, Virtualenv, collect_installed_wheels};
use zip::CompressionMethod;

#[rstest]
fn test_collect_installed_wheels_empty(
    python_exe: &Path,
    interpreter_identification_script: IdentifyInterpreter<'static>,
    tmp_dir: PathBuf,
    mut embedded_scripts: Scripts,
) {
    let venv = Virtualenv::create(
        Interpreter::load(python_exe, &interpreter_identification_script).unwrap(),
        Cow::Owned(tmp_dir),
        FileSystemLinker(),
        &mut embedded_scripts,
        false,
        false,
        None,
    )
    .unwrap();

    assert!(collect_installed_wheels(&venv).unwrap().is_empty());
}

#[rstest]
fn test_collect_installed_wheels_uv(tmp_dir: PathBuf, mut embedded_scripts: Scripts) {
    assert!(
        Command::new("uv")
            .args(["venv", "--no-project"])
            .arg(&tmp_dir)
            .spawn()
            .unwrap()
            .wait()
            .unwrap()
            .success()
    );
    assert!(
        Command::new("uv")
            .args(["pip", "install", "--python"])
            .arg(&tmp_dir)
            .args(["greenlet", "dill", "cowsay"])
            .spawn()
            .unwrap()
            .wait()
            .unwrap()
            .success()
    );

    let venv = Virtualenv::load(Cow::Owned(tmp_dir), &mut embedded_scripts).unwrap();
    let installed_wheels = collect_installed_wheels(&venv).unwrap();
    let install_paths = InstallPaths::for_venv(&venv).unwrap();
    let wheel_options = WheelOptions::new(CompressionMethod::Zstd, None, None);
    for installed_wheel in &installed_wheels {
        installed_wheel
            .pack(&install_paths, &wheel_options, Cursor::new(vec![]))
            .unwrap();
    }
}

#[rstest]
fn test_collect_installed_wheels_pex(tmp_dir: PathBuf, mut embedded_scripts: Scripts) {
    assert!(
        Command::new("uvx")
            .args(["--from", "pex", "pex3", "venv", "create", "--force", "-d"])
            .arg(&tmp_dir)
            .args(["greenlet", "dill", "cowsay"])
            .spawn()
            .unwrap()
            .wait()
            .unwrap()
            .success()
    );

    let venv = Virtualenv::load(Cow::Owned(tmp_dir), &mut embedded_scripts).unwrap();
    let installed_wheels = collect_installed_wheels(&venv).unwrap();
    let install_paths = InstallPaths::for_venv(&venv).unwrap();
    let wheel_options = WheelOptions::new(CompressionMethod::Zstd, None, None);
    for installed_wheel in &installed_wheels {
        installed_wheel
            .pack(&install_paths, &wheel_options, Cursor::new(vec![]))
            .unwrap();
    }
}
