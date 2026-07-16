# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import os.path
import subprocess

import pytest
from testing import IS_WINDOWS, IS_X86_64, pexrc_inject
from testing.interpreter import ensure_python

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Any  # noqa: F401


@pytest.mark.skipif(
    not (IS_WINDOWS and IS_X86_64),
    reason=(
        "The test requires the pythonnet dependency under test to be consumed as a win-amd64 wheel."
    ),
)
def test_issue_135(tmpdir):
    # type: (Any) -> None

    pex = os.path.join(tmpdir, "pex")
    subprocess.check_call(
        args=["pex", "--platform", "win-amd64-cp-314-cp314", "pythonnet==3.1.0", "-o", pex]
    )
    injected_pex = pexrc_inject(pex)
    python = ensure_python(version=(3, 14))
    subprocess.check_call(args=[python, injected_pex, "-c", "import pythonnet"])
