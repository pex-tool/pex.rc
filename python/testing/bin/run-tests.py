# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

import atexit
import os.path
import shutil
import subprocess
import sys
import sysconfig
import tempfile
from argparse import ArgumentParser, RawTextHelpFormatter

PYTHONPATH = os.path.abspath("python")
sys.path.append(PYTHONPATH)

# We can only import testing after sys.path ammendment.
from testing import IS_MAC  # noqa: E402

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Any, List, Optional  # noqa: F401


def ensure_pexrc(release_mode):
    # type: (Optional[bool]) -> str

    profile = "release" if release_mode else os.environ.pop("PEXRC_PROFILE", "dev")
    env = os.environ.copy()
    env.update(PEXRC_CLIB_FEATURES="tools")
    subprocess.check_call(args=["cargo", "build", "--profile", profile], env=env)
    profile_dir = "debug" if profile == "dev" else profile
    return os.path.abspath(
        os.path.join("target", profile_dir, "pexrc" + sysconfig.get_config_vars()["EXE"])
    )


def seed_pexrc_root(
    session_dir,  # type: str
    pexrc,  # type: str
):
    # type: (...) -> str

    pexrc_root = os.path.join(session_dir, "pexrc-root")
    if IS_MAC:
        # See the conftest.py pexrc_root fixture for details about why this is needed on Mac only.
        pex = os.path.join(pexrc_root, "seed.pex")
        subprocess.check_call(args=["pex", "-o", pex])
        subprocess.check_call(args=[pexrc, "inject", pex])
        subprocess.check_call(
            args=[sys.executable, pex + "rc", "-c", "print('Seeded!')"],
            env=dict(PEXRC_ROOT=pexrc_root),
        )
    else:
        os.makedirs(pexrc_root)
    return pexrc_root


def run_tests():
    # type: () -> Any

    arg_parser = ArgumentParser(
        description=(
            "Runs pexrc integration tests using pytest.\n"
            "\n"
            "Any options not documented below are passed through to pytest.\n"
            "To explicitly pass options to pytest, separate them with an additional `--`; i.e.:\n"
            "+ `uv run dev-cmd test -- -h` will display this help.\n"
            "+ `uv run dev-cmd test -- -- -h` will display pytest help"
        ),
        formatter_class=RawTextHelpFormatter,
    )
    arg_parser.add_argument(
        "--release",
        default=None,
        action="store_true",
        help="Build pexrc for use in tests in release mode.",
    )

    run_test_args = []
    explicit_pytest_args = None  # type: Optional[List[str]]
    for arg in sys.argv[1:]:
        if explicit_pytest_args is None:
            if arg == "--":
                explicit_pytest_args = []
            else:
                run_test_args.append(arg)
        else:
            explicit_pytest_args.append(arg)
    options, pytest_args = arg_parser.parse_known_args(run_test_args)
    if explicit_pytest_args:
        pytest_args.extend(explicit_pytest_args)

    pexrc = ensure_pexrc(options.release)
    env = os.environ.copy()
    session_dir = tempfile.mkdtemp(prefix="pexrc-pytest.", suffix=".session")
    atexit.register(shutil.rmtree, session_dir)
    env.update(
        _PEXRC_TEST_PEXRC_BINARY=pexrc,
        _PEXRC_TEST_SESSION_DIR=session_dir,
        _PEXRC_TEST_SESSION_PEXRC_ROOT=seed_pexrc_root(session_dir, pexrc),
        PYTHONPATH=PYTHONPATH,
    )
    return subprocess.call(
        args=["pytest", "-n", "auto"] + pytest_args,
        cwd=os.path.abspath(os.path.join("python", "tests")),
        env=env,
    )


if __name__ == "__main__":
    sys.exit(run_tests())
