# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import os.path
import sysconfig

EXE_EXTENSION = sysconfig.get_config_var("EXE") or ""


def script_path(path):
    # type: (str) -> str
    if EXE_EXTENSION:
        path, _ = os.path.splitext(path)
        return path + EXE_EXTENSION
    return path
