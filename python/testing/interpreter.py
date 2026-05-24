# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import os
import platform
import subprocess

from testing import IS_MAC, IS_WINDOWS, testing_cache_root

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Text, Tuple  # noqa: F401


def get_os():
    # type: () -> str

    if IS_WINDOWS:
        return "windows"
    elif IS_MAC:
        return "macos"
    else:
        return "linux"


def get_arch():
    # type: () -> str

    if IS_WINDOWS:
        arch = os.environ.get("PROCESSOR_ARCHITECTURE", platform.machine()).lower()
    else:
        arch = platform.machine().lower()

    if arch in ("aarch64", "arm64"):
        return "aarch64"
    elif arch in ("x86_64", "amd64"):
        return "x86_64"
    else:
        return arch


def ensure_python(
    version,  # type: Tuple[int, int]
    install_if_missing=True,  # type: bool
):
    # type: (...) -> Text

    # N.B.: We force arch to get arm64 PBS builds for Windows arm64 machines.
    # See: https://github.com/astral-sh/uv/issues/12906
    version_spec = "cpython-{major}.{minor}-{os}-{arch}".format(
        major=version[0], minor=version[1], os=get_os(), arch=get_arch()
    )
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
