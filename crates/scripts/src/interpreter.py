# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

import json
import sys
import sysconfig

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import (  # noqa: F401
        Any,
        Dict,
        Optional,
        Tuple,
        Type,
        Union,
        cast,
    )
else:

    def cast(
        _type,  # type: Union[str, Type]
        value,  # type: Any
    ):
        return value


def cpython_abi_info(sys_config_vars):
    # type: (Dict[str, str]) -> Dict[str, Any]

    try:
        # N.B.: There is no importlib.machinery prior to ~3.3.
        from importlib.machinery import EXTENSION_SUFFIXES  # type: ignore[import-not-found]
    except ImportError:
        # N.B.: There is no imp from 3.12 on.
        import imp  # type: ignore[import-not-found]

        # N.B.: MyPy: Cannot assign to final name "EXTENSION_SUFFIXES"
        EXTENSION_SUFFIXES = [x[0] for x in imp.get_suffixes()]  # type: ignore[misc]
        del imp

    free_threaded = None  # type: Optional[bool]
    debug = False  # type: bool
    pymalloc = None  # type: Optional[bool]
    ucs4 = None  # type: Optional[bool]

    with_debug = sys_config_vars.get("Py_DEBUG")
    has_refcount = hasattr(sys, "gettotalrefcount")
    # Windows doesn't set Py_DEBUG, so checking for support of debug-compiled
    # extension modules is the best option.
    # https://github.com/pypa/pip/issues/3383#issuecomment-173267692
    has_ext = "_d.pyd" in EXTENSION_SUFFIXES
    if with_debug or (with_debug is None and (has_refcount or has_ext)):
        debug = True

    if sys.version_info >= (3, 13):
        free_threaded = bool(sys_config_vars.get("Py_GIL_DISABLED"))

    if sys.version_info < (3, 8):
        with_pymalloc = sys_config_vars.get("WITH_PYMALLOC")
        pymalloc = with_pymalloc or with_pymalloc is None

        if sys.version_info < (3, 3):
            unicode_size = sys_config_vars.get("Py_UNICODE_SIZE")
            ucs4 = unicode_size == 4 or (unicode_size is None and sys.maxunicode == 0x10FFFF)

    return {"free_threaded": free_threaded, "debug": debug, "pymalloc": pymalloc, "ucs4": ucs4}


def identify(sys_config_vars):
    # type: (Dict[str, str]) -> Dict[str, Any]

    has_ensurepip = True
    try:
        import ensurepip  # noqa: F401
    except ImportError:
        has_ensurepip = False

    pypy_version = cast(
        "Optional[Tuple[int, int, int]]",
        tuple(getattr(sys, "pypy_version_info", ())[:3]) or None,
    )

    abi_info = None  # type: Optional[Dict[str, bool]]
    if pypy_version is None:
        abi_info = cpython_abi_info(sys_config_vars)

    return {
        "path": sys.executable,
        "prefix": sys.prefix,
        "base_prefix": getattr(sys, "base_prefix", None),
        "version": {
            "major": sys.version_info.major,
            "minor": sys.version_info.minor,
            "micro": sys.version_info.micro,
            "releaselevel": sys.version_info.releaselevel,
            "serial": sys.version_info.serial,
        },
        "pypy_version": pypy_version,
        "cpython_abi_info": abi_info,
        "paths": sysconfig.get_paths(),
        "has_ensurepip": has_ensurepip,
    }


if __name__ == "__main__":
    json.dump(identify(sysconfig.get_config_vars()), sys.stdout)
