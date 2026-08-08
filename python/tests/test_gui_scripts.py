# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import os.path
import subprocess
import sys
import time
from textwrap import dedent

import psutil
import pytest
from testing import IS_CI, IS_WINDOWS, pexrc_inject

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Any  # noqa: F401


def find_and_kill(
    process_name,  # type: str
    timeout=5.0,  # type: float
):
    # type: (...) -> None

    found = False
    start = time.time()
    while True:
        wait_time = time.time() - start
        assert wait_time <= timeout, (
            "Waited past {timeout} seconds (wait_time) for gui to appear.".format(
                timeout=timeout,
            )
        )

        for ps in psutil.process_iter(attrs=["name", "cmdline"]):
            attrs = ps.as_dict(attrs=["name", "cmdline"])
            if process_name in (attrs.get("name") or "") or any(
                process_name in arg for arg in (attrs.get("cmdline") or "")
            ):
                ps.kill()
                found = True
        if found:
            return


@pytest.mark.skipif(IS_CI, reason="This test pops up windows and expects a display is present.")
def test_gui_scripts(tmpdir):
    # type: (Any) -> None

    pex_root = os.path.join(str(tmpdir), "pex-root")
    pex = os.path.join(str(tmpdir), "psgdemos.pex")
    subprocess.check_call(
        args=["pex", "--runtime-pex-root", pex_root, "psgdemos", "-c", "psgdemos", "-o", pex]
    )
    injected_pex = pexrc_inject(pex)
    subprocess.check_call(
        args=[
            sys.executable,
            injected_pex,
            "-c",
            dedent(
                """\
                import os
                import sys

                import PySimpleGUI as sg


                settings = sg.user_settings_filename(filename="psgdemos.json")
                if os.path.exists(settings):
                    os.remove(settings)
                    print(f"Removed settings at {settings}", file=sys.stderr)
                else:
                    print(f"Settings not present at {settings}", file=sys.stderr)
                """
            ),
        ],
        env={**os.environ, "PEX_INTERPRETER": "1"},
    )
    process = subprocess.Popen(args=[sys.executable, injected_pex])
    time.sleep(
        # Windows is slow, at least in my vm. This allows time for the UI to come up before psutil
        # kills it.
        5 if IS_WINDOWS else 1
    )
    find_and_kill("psgdemos")
    process.wait(timeout=1.0)
