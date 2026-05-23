# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import os
import subprocess

from testing import testing_cache_root

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Text, Tuple  # noqa: F401


def ensure_python(
    version,  # type: Tuple[int, int]
    install_if_missing=True,  # type: bool
):
    # type: (...) -> Text

    version_spec = ".".join(map(str, version))
    env = dict(os.environ, UV_PYTHON_INSTALL_DIR=os.path.join(testing_cache_root(), "interpreters"))
    try:
        return (
            subprocess.check_output(args=["uv", "python", "find", version_spec], env=env)
            .decode("utf-8")
            .strip()
        )
    except subprocess.CalledProcessError:
        if not install_if_missing:
            raise
        subprocess.check_call(
            args=["uv", "python", "install", "--managed-python", "--force", version_spec], env=env
        )
        return ensure_python(version, install_if_missing=False)
