# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import os.path
import subprocess

import pytest
from testing import IS_WINDOWS
from testing.compare import compare

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Any, Iterator, List, Text, Tuple  # noqa: F401

    from testing.compare import ProcessResult  # noqa: F401


def create_sqlalchemy_pex(
    tmpdir,  # type: Any
    extra_pex_args,  # type: List[str]
    set_runtime_pex_root=True,  # type: bool
):
    # type: (...) -> str

    venv = os.path.join(str(tmpdir), "venv")
    subprocess.check_call(args=["uv", "venv", "--no-project", venv])
    subprocess.check_call(args=["uv", "pip", "install", "--python", venv, "sqlalchemy"])

    pex = os.path.join(str(tmpdir), "pex")
    args = [
        "pex",
        "--venv-repository",
        venv,
        "sqlalchemy",
        "-o",
        pex,
    ] + extra_pex_args

    if set_runtime_pex_root:
        pex_root = os.path.join(str(tmpdir), "pex-root")
        args.extend(
            [
                "--runtime-pex-root",
                pex_root,
            ]
        )

    subprocess.check_call(args)
    return pex


@pytest.mark.parametrize(
    "layout_args",
    [pytest.param(["--layout", layout], id=layout) for layout in ("zipapp", "loose", "packed")],
)
@pytest.mark.parametrize(
    "deps_are_wheel_files_args",
    [pytest.param(["--no-pre-install-wheels"], id="whls"), pytest.param([], id="chroots")],
)
@pytest.mark.parametrize(
    "boot_args", [pytest.param(["--sh-boot"], id="sh-boot"), pytest.param([], id="py-boot")]
)
def test_issue_103(
    tmpdir,  # type: Any
    pexrc_root,  # type: str
    layout_args,  # type: List[str]
    deps_are_wheel_files_args,  # type: List[str]
    boot_args,  # type: List[str]
):
    # type: (...) -> None

    # N.B.: Traditional Pex does not set up a valid --sh-boot shebang header on Windows; so we can
    # inject such PEXes and get a valid, runnable PEXrc, but we cannot run the original PEX.
    # Likewise, traditional --no-pre-install-wheels PEXes fail to run on Windows, but we can inject
    # them and get a runnable PEXrc.
    only_inject = IS_WINDOWS and (
        "--sh-boot" in boot_args or "--no-pre-install-wheels" in deps_are_wheel_files_args
    )

    compare(
        create_sqlalchemy_pex(
            tmpdir,
            layout_args + deps_are_wheel_files_args + boot_args,
            set_runtime_pex_root=not only_inject,
        ),
        args=["-c", "import sqlalchemy"],
        env=dict(PEXRC_ROOT=pexrc_root),
        only_inject=only_inject,
        assert_faster=False,
    )
