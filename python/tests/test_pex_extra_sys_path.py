# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import json
import os
import subprocess
import sys

import pytest
from testing import pexrc_inject
from testing.interpreter import ensure_python

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Any, Text, Tuple  # noqa: F401


@pytest.fixture
def find_pythons():
    # type: () -> Tuple[Text, Text]

    if sys.version_info[:2] < (3, 15):
        return sys.executable, ensure_python(version=(3, 15))
    else:
        return ensure_python(version=(3, 14)), sys.executable


@pytest.fixture
def python_pth(find_pythons):
    # type: (Tuple[Text, Text]) -> Text
    python_pth, _ = find_pythons
    return python_pth


@pytest.fixture
def python_start(find_pythons):
    # type: (Tuple[Text, Text]) -> Text
    _, python_start = find_pythons
    return python_start


def test_pep_829(
    tmpdir,  # type: Any
    python_pth,  # type: Text
    python_start,  # type: Text
):
    # type: (...) -> None

    pex_root = os.path.join(str(tmpdir), "pex-root")
    pex = os.path.join(str(tmpdir), "pex")
    subprocess.check_call(args=["pex", "--runtime-pex-root", pex_root, "-o", pex])

    one = os.path.join(str(tmpdir), "entry-one")
    two = os.path.join(str(tmpdir), "entry-two")
    three = os.path.join(str(tmpdir), "entry-three")
    debug_file = os.path.join(str(tmpdir), "debug.json")

    injected_pex = pexrc_inject(pex)

    def assert_px_extra_sys_path(
        python,  # type: Text
        expected_legacy,  # type: bool
    ):
        # type: (...) -> None
        output = subprocess.check_output(
            args=[
                python,
                injected_pex,
                "-c",
                "import json, sys; json.dump(sys.path, sys.stdout)",
            ],
            env=dict(
                os.environ,
                PEX_EXTRA_SYS_PATH=os.pathsep.join((two, one)),
                __PEX_EXTRA_SYS_PATH__=three,
                __PEX_EXTRA_SYS_PATH_DEBUG__=debug_file,
            ),
        )
        assert [two, one, three] == json.loads(output)[-3:]
        with open(debug_file) as fp:
            data = json.load(fp)
        assert data["legacy"] == expected_legacy
        assert [two, one, three] == data["entries"]

    assert_px_extra_sys_path(python_pth, expected_legacy=True)
    assert_px_extra_sys_path(python_start, expected_legacy=False)
