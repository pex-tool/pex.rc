# -*- coding: utf-8 -*-
# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

from __future__ import print_function

import ctypes
import functools
import importlib
import os
import os.path
import pkgutil
import platform
import shutil
import sys
import tempfile
import time
import warnings
from ctypes import cdll

_BOOT_START = time.time()

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from types import ModuleType  # noqa: F401
    from typing import (  # noqa: F401
        Any,
        Callable,
        List,
        Mapping,
        NoReturn,
        Optional,
        Protocol,
        Sequence,
        Tuple,
        Type,
    )
else:

    class Protocol(object):  # type: ignore[no-redef]
        pass


_PEX_VERBOSE = False
try:
    _PEX_VERBOSE = int(os.environ.get("PEX_VERBOSE", "0")) >= 3
except ValueError:
    pass


if sys.version_info >= (3, 10):

    def orig_argv():
        # type: () -> List[str]
        return sys.orig_argv

else:
    try:
        import ctypes

        # N.B.: None of the PyPy versions we support <3.10 supports the pythonapi.
        from ctypes import pythonapi

        def orig_argv():
            # type: () -> List[str]

            # Under MyPy for Python 3.5, ctypes.POINTER is incorrectly typed. This code is tested
            # to work correctly in practice on all Pythons Pex supports.
            argv = ctypes.POINTER(  # type: ignore[call-arg]
                ctypes.c_char_p if sys.version_info[0] == 2 else ctypes.c_wchar_p
            )()

            argc = ctypes.c_int()
            pythonapi.Py_GetArgcArgv(ctypes.byref(argc), ctypes.byref(argv))

            # Under MyPy for Python 3.5, argv[i] has its type incorrectly evaluated. This code
            # is tested to work correctly in practice on all Pythons Pex supports.
            return [argv[i] for i in range(argc.value)]  # type: ignore[misc]

    except ImportError:
        # N.B.: This handles the older PyPy case.
        def orig_argv():
            # type: () -> List[str]
            return []


class OperatingSystem(object):
    @classmethod
    def current(cls):
        # type: () -> OperatingSystem

        operating_system = platform.system().lower()
        if operating_system == "linux":
            return LINUX
        elif operating_system == "darwin":
            return MACOS
        elif operating_system == "windows":
            return WINDOWS
        else:
            raise ValueError("Unsupported OS: {os}".format(os=operating_system))

    def __init__(
        self,
        name,
        lib_extension,  # type: str
        lib_prefix="",  # type: str
    ):
        # type: (...) -> None
        self.name = name
        self._lib_prefix = lib_prefix
        self._lib_extension = lib_extension

    def target_triple(
        self,
        arch,  # type: Arch
        abi=None,  # type: Optional[ABI]
    ):
        # type: (...) -> str
        return "{arch}-{os}{abi}".format(arch=arch, os=self.name, abi="-" + abi.name if abi else "")

    def library_file_name(self, lib_name):
        # type: (str) -> str
        return "{lib_prefix}{name}.{lib_extension}".format(
            lib_prefix=self._lib_prefix, name=lib_name, lib_extension=self._lib_extension
        )

    def __str__(self):
        # type: () -> str
        return self.name


LINUX = OperatingSystem("linux", lib_prefix="lib", lib_extension="so")
MACOS = OperatingSystem("macos", lib_prefix="lib", lib_extension="dylib")
WINDOWS = OperatingSystem("windows", lib_extension="dll")

CURRENT_OS = OperatingSystem.current()


class Arch(object):
    @classmethod
    def current(cls):
        # type: () -> Arch

        machine = platform.machine().lower()
        if machine in ("aarch64", "arm64"):
            return ARM64
        elif machine in ("armv7l", "armv8l"):
            return ARM32
        elif machine == "ppc64le":
            return PPC64LE
        elif machine in ("amd64", "x86_64"):
            return X86_64
        else:
            raise ValueError("Unsupported chip architecture: {arch}".format(arch=machine))

    def __init__(self, name):
        # type: (str) -> None
        self.name = name

    def __str__(self):
        # type: () -> str
        return self.name


ARM64 = Arch("aarch64")
ARM32 = Arch("arm")
PPC64LE = Arch("powerpc64le")
X86_64 = Arch("x86_64")

CURRENT_ARCH = Arch.current()


class ELFFile(object):
    class Invalid(ValueError):
        pass

    def __init__(self, path):
        # type: (str) -> None

        import struct

        self._f = open(path, "rb")

        try:
            ident = self._read("16B")
        except struct.error as e:
            raise self.Invalid("unable to parse identification: {err}".format(err=e))
        magic = ident[:4]
        if magic != (127, 69, 76, 70):  # DEL (0x7F), E, L, F
            raise self.Invalid("invalid magic: {magic!r}".format(magic=magic))

        self.capacity = ident[4]  # Format for program header (bitness).
        self.encoding = ident[5]  # Data structure encoding (endianness).

        try:
            # e_fmt: Format for program header.
            # p_fmt: Format for section header.
            # p_idx: Indexes to find p_type, p_offset, and p_filesz.
            e_fmt, self._p_fmt, self._p_idx = {
                (1, 1): ("<HHIIIIIHHH", "<IIIIIIII", (0, 1, 4)),  # 32-bit LSB.
                (1, 2): (">HHIIIIIHHH", ">IIIIIIII", (0, 1, 4)),  # 32-bit MSB.
                (2, 1): ("<HHIQQQIHHH", "<IIQQQQQQ", (0, 2, 5)),  # 64-bit LSB.
                (2, 2): (">HHIQQQIHHH", ">IIQQQQQQ", (0, 2, 5)),  # 64-bit MSB.
            }[(self.capacity, self.encoding)]
        except KeyError as e:
            raise self.Invalid(
                "unrecognized capacity ({capacity}) or encoding ({encoding}): {err}".format(
                    capacity=self.capacity, encoding=self.encoding, err=e
                )
            )

        try:
            (
                _,
                _,
                _,
                _,
                self._e_phoff,  # Offset of program header.
                _,
                _,
                _,
                self._e_phentsize,  # Size of section.
                self._e_phnum,  # Number of sections.
            ) = self._read(e_fmt)
        except struct.error as e:
            raise self.Invalid(
                "unable to parse machine and section information: {err}".format(err=e)
            )

    def __enter__(self):
        # type: () -> ELFFile
        return self

    def __exit__(self, exc_type, exc_val, exc_tb):
        # type: (...) -> None
        if self._f:
            self._f.close()

    def _read(self, fmt):
        # type: (str) -> Tuple[int, ...]

        import struct

        return struct.unpack(fmt, self._f.read(struct.calcsize(fmt)))

    def interpreter(self):
        # type: () -> Optional[bytes]
        """The path recorded in the ``PT_INTERP`` section header."""

        import struct

        for idx in range(self._e_phnum):
            self._f.seek(self._e_phoff + self._e_phentsize * idx)
            try:
                data = self._read(self._p_fmt)
            except struct.error:
                continue
            if data[self._p_idx[0]] != 3:  # Not PT_INTERP.
                continue
            self._f.seek(data[self._p_idx[1]])
            return self._f.read(data[self._p_idx[2]]).rstrip(b"\0")
        return None


def is_musl(executable):
    # type: (str) -> bool
    try:
        with ELFFile(executable) as elffile:
            interpreter = elffile.interpreter()
            if interpreter:
                if _PEX_VERBOSE:
                    print(
                        "pex: Parsed {exe} as using interpreter: {interp!r}.".format(
                            exe=executable, interp=interpreter
                        ),
                        file=sys.stderr,
                    )

                # Via: https://www.musl-libc.org/doc/1.0.0/manual.html
                # > The interpreter will be: $(syslibdir)/ld-musl-$(ARCH).so.1
                # Crucially, we can rely on matching `musl` in the interpreter path.
                return b"musl" in interpreter
            return False
    except ELFFile.Invalid as e:
        print(
            "pex: Failed to parse {exe} as an ELF file to determine abi; "
            "assuming gnu: {err}".format(exe=executable, err=e),
            file=sys.stderr,
        )
        return False


class ABI(object):
    @classmethod
    def current(cls):
        # type: () -> Optional[ABI]
        if CURRENT_OS is not LINUX:
            return None
        if CURRENT_ARCH is ARM32:
            return None
        return MUSL if is_musl(sys.executable) else GNU

    def __init__(self, name):
        # type: (str) -> None
        self.name = name

    def __str__(self):
        # type: () -> str
        return self.name


GNU = ABI("gnu")
MUSL = ABI("musl")

CURRENT_ABI = ABI.current()


class TimeUnit(object):
    def __init__(
        self,
        name,  # type: str
        multiplier,  # type: int
    ):
        # type: (...) -> None
        self._name = name
        self._multiplier = multiplier

    def elapsed(self, start):
        # type: (float) -> float
        return (time.time() - start) * self._multiplier

    def __str__(self):
        # type: () -> str
        return self._name


MS = TimeUnit("ms", 1000)
US = TimeUnit("µs", 1000 * 1000)


def timed(unit):
    # type: (TimeUnit) -> Callable[[Callable], Callable]
    def wrapper(func):
        if _PEX_VERBOSE:

            @functools.wraps(func)
            def wrapped(*args, **kwargs):
                start = time.time()
                try:
                    return func(*args, **kwargs)
                finally:
                    print(
                        "pex: {func}(*{args!r}, **{kwargs!r}) took {elapsed:.4}{unit}".format(
                            func=func.__name__,
                            args=args,
                            kwargs=kwargs,
                            elapsed=unit.elapsed(start),
                            unit=unit,
                        ),
                        file=sys.stderr,
                    )

            return wrapped
        else:
            return func

    return wrapper


_unload_dll = None  # type: Optional[Callable[[str, Optional[Pexrc]], None]]
if CURRENT_OS is WINDOWS:
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS = 0x00000004
    MAX_UNLOAD_WAIT_SECS = 0.05

    import gc
    from ctypes import WinError, windll  # type: ignore[attr-defined]
    from ctypes.wintypes import HMODULE  # type: ignore[attr-defined]
    from os.path import dirname, exists
    from time import time as now

    def _unload_dll(
        path,  # type: str
        dll,  # type: Optional[Pexrc]
    ):
        # type: (...) -> None
        handle = None  # type: Optional[HMODULE]  # type: ignore[name-defined]
        if dll is not None:
            module_handle = HMODULE()  # type: ignore[attr-defined]
            if not windll.kernel32.GetModuleHandleExW(  # type: ignore[attr-defined]
                GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, dll.boot, ctypes.pointer(module_handle)
            ):
                warnings.warn(
                    "Failed to clean up extracted dll resource at {path}: {err}".format(
                        path=path,
                        err=WinError(),  # type: ignore[attr-defined]
                    )
                )
            else:
                handle = module_handle
            del dll

        count = 0
        freed = False
        start = now()
        while exists(path):
            if handle is not None:
                if windll.kernel32.FreeLibrary(handle):  # type: ignore[attr-defined]
                    freed = True
                elif not freed:
                    raise WinError()  # type: ignore[attr-defined]
                else:
                    gc.collect()
            shutil.rmtree(dirname(path), ignore_errors=True)
            if not handle:
                break
            elapsed = now() - start
            if elapsed > MAX_UNLOAD_WAIT_SECS:
                warnings.warn(
                    "Failed to clean up extracted dll resource at {path} after {count} attempts "
                    "spanning {elapsed:.2}s".format(path=path, count=count, elapsed=elapsed)
                )
                break
            count += 1


class Pexrc(Protocol):
    def boot(
        self,
        python_exe,  # type: bytes
        pex_file,  # type: bytes
    ):
        # type: (...) -> int
        pass


SHOULD_EXECUTE = __name__ == "__main__"


@timed(MS)
def _load_pexrc():
    # type: () -> Pexrc

    prefix = ".clibs" if __name__ == "__pex__" else os.path.join("__pex__", ".clibs")
    target_triple = CURRENT_OS.target_triple(arch=CURRENT_ARCH, abi=CURRENT_ABI)
    library_file_name = CURRENT_OS.library_file_name(lib_name="pexrc")
    library_file_relpath = os.path.join(
        prefix,
        "{target_triple}.{library_file_name}".format(
            target_triple=target_triple, library_file_name=library_file_name
        ),
    )
    if __file__ and os.path.isfile(__file__):
        # We're in a either a loose or packed PEX.
        library_file_path = os.path.join(os.path.dirname(__file__), library_file_relpath)
        if not os.path.exists(library_file_path):
            raise RuntimeError(
                "Pexrc is not supported on {target_triple}: no pexrc library found.".format(
                    target_triple=target_triple,
                )
            )
        try:
            return cdll.LoadLibrary(library_file_path)  # type: Pexrc
        except OSError as e:
            raise RuntimeError(
                "Failed to load pexrc library from {library_file_path}: {err}".format(
                    library_file_path=library_file_path, err=e
                )
            )

    dll = None  # type: Optional[Pexrc]
    tmp_dir = tempfile.mkdtemp()
    library_file_path = os.path.join(tmp_dir, library_file_name)
    try:
        pexrc_data = pkgutil.get_data(__name__, library_file_relpath)
        if pexrc_data is None:
            raise RuntimeError(
                "Pexrc is not supported on {target_triple}: no pexrc library found.".format(
                    target_triple=target_triple,
                )
            )
        with open(library_file_path, "wb") as fp:
            fp.write(pexrc_data)
        try:
            pexrc = cdll.LoadLibrary(library_file_path)  # type: Pexrc
        except OSError as e:
            raise RuntimeError(
                "Failed to load pexrc library from {library_file_path}: {err}".format(
                    library_file_path=library_file_path, err=e
                )
            )
        dll = pexrc
        return pexrc
    finally:
        if CURRENT_OS is WINDOWS:
            import atexit

            # N.B.: Once the library is loaded on Windows, it can't be deleted without jumping
            # through extra hoops:
            # PermissionError: [WinError 5] Access is denied: 'C:...\\Temp\\tmpbyxvw46f\\pexrc.dll'
            assert _unload_dll is not None
            atexit.register(_unload_dll, library_file_path, dll)
        else:

            def warn_extracted_lib_leak(err):
                warnings.warn(
                    "Failed to clean up extracted library resource at {path}: {err}".format(
                        path=library_file_path, err=err
                    )
                )

            if sys.version_info[:2] < (3, 12):

                def onerror(_func, _path, exec_info):
                    _, err, _ = exec_info
                    warn_extracted_lib_leak(err)

                shutil.rmtree(tmp_dir, ignore_errors=False, onerror=onerror)
            else:

                def onexc(_func, _path, err):
                    warn_extracted_lib_leak(err)

                shutil.rmtree(tmp_dir, ignore_errors=False, onexc=onexc)  # type: ignore[call-arg]


_pexrc = _load_pexrc()


def to_cstr(value):
    # type: (str) -> bytes

    return value.encode("utf-8") + b"\x00"


def to_array_of_cstr(values):
    # type: (Sequence[str]) -> ctypes.Array[ctypes.c_char_p]

    array_type = ctypes.c_char_p * (len(values) + 1)
    array_of_cstr = array_type()
    for index, value in enumerate(values):
        array_of_cstr[index] = to_cstr(value)
    array_of_cstr[len(values)] = None
    return array_of_cstr


# N.B.: pexrc uses this to indicate an internal oot error (vs the return code from executing the
# booted PEX).
BOOT_ERROR_CODE = 75


@timed(MS)
def boot(
    pex,  # type: str
    python_args,  # type: Sequence[str]
    args,  # type: Sequence[str]
):
    # type: (...) -> NoReturn

    python_exe = to_cstr(sys.executable)
    python_argv = to_array_of_cstr(python_args)
    pex_file = to_cstr(pex)
    argv = to_array_of_cstr(args)

    if _PEX_VERBOSE:
        print(
            "pex: total time to _pexrc.boot {elapsed:.4}{unit}".format(
                elapsed=MS.elapsed(_BOOT_START), unit=MS
            ),
            file=sys.stderr,
        )
    sys.exit(
        _pexrc.boot(
            python_exe,
            ctypes.pointer(python_argv),
            ctypes.c_int(len(python_argv) - 1),
            pex_file,
            ctypes.pointer(argv),
            ctypes.c_int(len(argv) - 1),
        )
    )


if sys.version_info[:2] >= (3, 4):
    from importlib.abc import Loader as Loader
    from importlib.abc import MetaPathFinder as Finder
    from importlib.machinery import ModuleSpec as ModuleSpec
else:

    class Loader(Protocol):
        pass

    class ModuleSpec(Protocol):
        pass

    class Finder(Protocol):
        pass


class PexImporter(Finder, Loader):
    def find_module(
        self,
        fullname,  # type: str
        path,  # type: Any
    ):
        # type: (...) -> Optional[Loader]
        return self if fullname.startswith("__pex__.") else None

    def find_spec(
        self,
        fullname,  # type: str
        path,  # type: Any
        target=None,  # type: Optional[ModuleType]
    ):
        # type: (...) -> Optional[ModuleSpec]
        # Python 2.7 does not know about this API and does not use it.
        from importlib.util import spec_from_loader  # type: ignore[import]

        return spec_from_loader(fullname, self) if fullname.startswith("__pex__.") else None

    @staticmethod
    def load_module(fullname):
        # type: (str) -> ModuleType
        root_package, _, module_name = fullname.partition(".")
        if root_package != "__pex__":
            raise ImportError(
                "Cannot import sub-modules for {root}: {module}".format(
                    root=root_package, module=module_name
                )
            )
        return sys.modules.setdefault(fullname, importlib.import_module(module_name))


@timed(MS)
def mount(
    pex,  # type: str
    python=None,  # type: Optional[str]
):
    # type: (...) -> None

    pex_file = to_cstr(pex)

    boot_python = python or sys.executable
    python_exe = to_cstr(boot_python)

    sys_path_entry = ctypes.create_string_buffer(8096)
    result = _pexrc.mount(python_exe, pex_file, sys_path_entry)
    if result != 0:
        raise RuntimeError("Could not mount PEX!")
    entry = (
        sys_path_entry.value.decode("utf-8") if sys.version_info[0] >= 3 else sys_path_entry.value
    )
    # N.B.: The sys.path list contains str on both Python 2.7 and Python 3. MyPy can't track the
    # conditional above though and worries unicode gets appended to the list under Python 2.7.
    sys.path.append(entry)  # type: ignore[arg-type]
    sys.meta_path.append(PexImporter())
    if _PEX_VERBOSE:
        print(
            "pex: Mounted PEX {entry} on sys.path.".format(
                entry=entry  # type: ignore[str-bytes-safe]
            ),
            sys.stderr,
        )


def entry_point_from_filename(filename):
    # type: (str) -> str

    # Either the entry point is "__main__" and we're in execute mode or "__pex__/__init__.py"
    # and we're in import hook mode.
    ep = os.path.dirname(filename)
    if SHOULD_EXECUTE:
        return ep
    return os.path.dirname(ep)


def find_entry_point():
    # type: () -> Optional[str]
    file = globals().get("__file__")
    if file is not None and os.path.exists(file):
        return entry_point_from_filename(file)

    loader = globals().get("__loader__")
    if loader is not None:
        if hasattr(loader, "archive"):
            return loader.archive

        if hasattr(loader, "get_filename"):
            # The source of the loader interface has changed over the course of Python history
            # from `pkgutil.ImpLoader` to `importlib.abc.Loader`, but the existence and
            # semantics of `get_filename` has remained constant; so we just check for the
            # method.
            return entry_point_from_filename(loader.get_filename())

    return None


if __name__ == "__main__":
    entry_point = find_entry_point()
    if entry_point is None:
        sys.exit("Could not launch python executable!\n")
    os.environ["PEX"] = entry_point

# --- split --- #
    python_args = ['{inject_python_args}']  # type: List[str]
# --- split --- #
    orig_args = orig_argv()
    if orig_args is not None:
        orig_python_args = []  # type: List[str]
        for index, arg in enumerate(orig_args[1:], start=1):
            if os.path.exists(arg) and os.path.samefile(entry_point, arg):
                orig_python_args = orig_args[1:index]
                break
        python_args.extend(orig_python_args)

    boot(entry_point, python_args=python_args, args=sys.argv[1:])
elif __name__ == "__pex__":
    entry_point = find_entry_point()
    if entry_point is None:
        raise RuntimeError("Could not mount PEX!")
    mount(entry_point)
