# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

import os
import sys


def _extend_sys_path(path_env_var):
    path = os.environ.get(path_env_var)
    if path:
        entries = path.split(os.pathsep)
        sys.path.extend(entries)
        return entries
    return []


def extend_sys_path(legacy=False):
    entries = _extend_sys_path("PEX_EXTRA_SYS_PATH")
    entries.extend(_extend_sys_path("__PEX_EXTRA_SYS_PATH__"))
    debug = os.environ.pop("__PEX_EXTRA_SYS_PATH_DEBUG__", "")
    if debug:
        import json

        with open(debug, "w") as fp:
            json.dump({"entries": entries, "legacy": legacy}, fp)
