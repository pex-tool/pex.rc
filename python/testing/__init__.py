# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import os
import platform
import subprocess

import platformdirs

IS_CI = os.environ.get("CI", "false") == "true"

IS_LINUX = platform.system().lower() == "linux"
IS_MAC = platform.system().lower() == "darwin"
IS_WINDOWS = platform.system().lower() == "windows"

IS_ARM64 = platform.machine().lower() in ("aarch64", "arm64")
IS_X86_64 = platform.machine().lower() in ("amd64", "x86_64")


def pexrc():
    # type: () -> str
    return os.environ["_PEXRC_TEST_PEXRC_BINARY"]


def pexrc_inject(pex):
    # type: (str) -> str

    subprocess.check_call(args=[pexrc(), "inject", pex])
    injected_pex = pex + "rc" if pex.endswith(".pex") else pex + ".pexrc"
    assert (os.path.isfile(pex) and os.path.isfile(injected_pex)) or (
        os.path.isdir(pex) and os.path.isdir(injected_pex)
    )
    return injected_pex


def session_dir():
    # type: () -> str
    return os.environ["_PEXRC_TEST_SESSION_DIR"]


def session_pexrc_root():
    # type: () -> str
    return os.environ["_PEXRC_TEST_SESSION_PEXRC_ROOT"]


def testing_cache_root():
    # type: () -> str
    return platformdirs.user_cache_dir("pexrc-dev", opinion=False)
