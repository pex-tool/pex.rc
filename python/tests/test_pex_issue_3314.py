# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import json
import os.path
import stat
import subprocess
import sys
from textwrap import dedent

import pytest
from testing import pexrc, pexrc_inject
from testing.zipfile import inventory_zip_modes

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Any, Dict  # noqa: F401


@pytest.fixture
def marker_dir(tmpdir):
    #  type: (Any) -> str

    marker_dir = os.path.join(str(tmpdir), "marker")
    os.mkdir(marker_dir)
    open(os.path.join(marker_dir, ".rules_py_marker_file"), "w").close()
    return marker_dir


@pytest.fixture
def project_dir(tmpdir):
    #  type: (Any) -> str

    project_dir = os.path.join(str(tmpdir), "project")
    os.mkdir(project_dir)

    main_dir = os.path.join(project_dir, "_main")
    os.mkdir(main_dir)
    main_pkg_dir = os.path.join(main_dir, "main_pkg")
    os.mkdir(main_pkg_dir)
    open(os.path.join(main_pkg_dir, "__init__.py"), "w").close()
    with open(os.path.join(main_pkg_dir, "res"), "w") as fp:
        fp.write("1/137")

    src_dir = os.path.join(main_dir, "src")
    os.mkdir(src_dir)
    main_src_pkg_dir = os.path.join(src_dir, "main_src_pkg")
    os.mkdir(main_src_pkg_dir)
    open(os.path.join(main_src_pkg_dir, "__init__.py"), "w").close()
    with open(os.path.join(src_dir, "main_src_pkg", "res"), "w") as fp:
        fp.write("42")

    return project_dir


@pytest.fixture
def tests_py_exe(tmpdir):
    #  type: (Any) -> str

    # N.B.: The expected layout of first-party code in the PEX from marker_dir and project_dir is:
    #
    # .rules_py_marker_file
    # __pex_executable__.py  # This is `--exe tests.py`.
    # _main/
    # _main/main_pkg/
    # _main/main_pkg/__init__.py
    # _main/main_pkg/res
    # _main/src/
    # _main/src/main_src_pkg/
    # _main/src/main_src_pkg/__init__.py
    # _main/src/main_src_pkg/res
    #
    with open(os.path.join(str(tmpdir), "tests.py"), "w") as fp:
        fp.write(
            dedent(
                """\
                import json
                import os
                import pkgutil
                import sys


                json.dump(
                    {
                        "RUNFILES_DIR": os.environ["RUNFILES_DIR"],
                        "tests.py-dir": os.path.dirname(__file__),
                        "_main": pkgutil.get_data("main_pkg", "res").decode("utf-8"),
                        "_main/src": pkgutil.get_data("main_src_pkg", "res").decode("utf-8"),
                    },
                    sys.stdout
                )
                """
            )
        )
    return fp.name


def assert_pex_output(pex):
    # type: (str) -> None

    data = json.loads(subprocess.check_output(args=[sys.executable, pex]))
    assert "1/137" == data.pop("_main")
    assert "42" == data.pop("_main/src")
    assert data.pop("tests.py-dir") == data.pop("RUNFILES_DIR")
    assert not data


def test_build(
    tmpdir,  # type: Any
    pexrc_root,  # type: str
    marker_dir,  # type: str
    project_dir,  # type: str
    tests_py_exe,  # type: str
):
    # type: (...) -> None

    env = dict(os.environ, PEXRC_ROOT=pexrc_root)
    env.pop("VIRTUAL_ENV", None)

    pex = os.path.join(str(tmpdir), "pex")
    subprocess.check_call(
        args=[
            pexrc(),
            "-X",
            "build",
            "--runtime-pex-root",
            pexrc_root,
            "-D",
            marker_dir,
            "-D",
            project_dir,
            "--bind-resource-path",
            "RUNFILES_DIR=.rules_py_marker_file/..",
            "--inject-env",
            (
                "PEX_EXTRA_SYS_PATH="
                "{{pex.env.RUNFILES_DIR}}/_main{pathsep}{{pex.env.RUNFILES_DIR}}/_main/src".format(
                    pathsep=os.pathsep
                )
            ),
            "--modes",
            "755",
            "--exe",
            tests_py_exe,
            "-o",
            pex,
        ],
        env=env,
    )
    assert {"dir": {stat.S_IFDIR | 0o755}, "file": {stat.S_IFREG | 0o755}} == inventory_zip_modes(
        pex
    )
    assert_pex_output(pex)


def test_inject(
    tmpdir,  # type: Any
    pexrc_root,  # type: str
    marker_dir,  # type: str
    project_dir,  # type: str
    tests_py_exe,  # type: str
):
    # type: (...) -> None

    pex = os.path.join(str(tmpdir), "pex")
    subprocess.check_call(
        args=[
            "pex",
            "--runtime-pex-root",
            pexrc_root,
            "-D",
            marker_dir,
            "-D",
            project_dir,
            "--bind-resource-path",
            "RUNFILES_DIR=.rules_py_marker_file/..",
            "--inject-env",
            (
                "PEX_EXTRA_SYS_PATH="
                "{{pex.env.RUNFILES_DIR}}/_main{pathsep}{{pex.env.RUNFILES_DIR}}/_main/src".format(
                    pathsep=os.pathsep
                )
            ),
            "--exe",
            tests_py_exe,
            "-o",
            pex,
        ]
    )

    injected_pex = pexrc_inject(pex, "--modes", "755")
    assert {"dir": {stat.S_IFDIR | 0o755}, "file": {stat.S_IFREG | 0o755}} == inventory_zip_modes(
        injected_pex
    )
    assert_pex_output(injected_pex)
