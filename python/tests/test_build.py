# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import json
import os.path
import subprocess
import sys
from textwrap import dedent

from testing import pexrc

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Any  # noqa: F401


def test_pex_issue_3314(
    tmpdir,  # type: Any
    pexrc_root,  # type: str
):
    # type: (...) -> None

    marker_dir = os.path.join(str(tmpdir), "marker")
    os.mkdir(marker_dir)
    open(os.path.join(marker_dir, ".rules_py_marker_file"), "w").close()

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

    env = dict(os.environ, PEXRC_ROOT=pexrc_root)
    env.pop("VIRTUAL_ENV", None)

    pex = os.path.join(str(tmpdir), "pex")
    subprocess.check_call(
        args=[
            pexrc(),
            "-X",
            "build",
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
            fp.name,
            "-o",
            pex,
        ],
        env=env,
    )

    data = json.loads(subprocess.check_output(args=[sys.executable, pex], env=env))
    assert "1/137" == data.pop("_main")
    assert "42" == data.pop("_main/src")
    assert data.pop("tests.py-dir") == data.pop("RUNFILES_DIR")
    assert not data
