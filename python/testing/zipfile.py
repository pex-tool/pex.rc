# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import absolute_import

import zipfile
from collections import defaultdict

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import DefaultDict, Dict, Set  # noqa: F401

    from typing_extensions import Literal  # noqa: F401


# The method ID of the zstd compression type in injected PEX zips. This was determined by
# observation but also matches newer Python support.
#
# N.B.: We can't rely on the zipfile.ZIP_ZSTANDARD constant being available since its Python 3.14+
# only. See: https://docs.python.org/3/library/zipfile.html#zipfile.ZIP_ZSTANDARD
ZIP_ZSTANDARD = 93


def inventory_zip_modes(path):
    # type: (str) -> Dict[Literal["dir", "file"], Set[int]]
    with zipfile.ZipFile(path) as zip_fp:
        inventory = defaultdict(set)  # type: DefaultDict[Literal["dir", "file"], Set[int]]
        for zip_info in zip_fp.infolist():
            inventory["dir" if zip_info.filename.endswith("/") else "file"].add(
                zip_info.external_attr >> 16
            )
        return inventory
