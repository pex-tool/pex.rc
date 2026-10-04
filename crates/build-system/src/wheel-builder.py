# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0

import argparse
import importlib
import json
import os
import sys

TYPE_CHECKING = False
if TYPE_CHECKING:
    # Ruff doesn't understand Python 2 and thus the type comment usages.
    from typing import Any, Callable, List, Optional  # noqa: F401


class BackendError(Exception):
    pass


def load_backend(backend):
    # type: (str) -> Any
    module_path, _, object_path = backend.partition(":")
    backend_object = importlib.import_module(module_path)
    if object_path:
        for attribute in object_path.split("."):
            backend_object = getattr(backend_object, attribute)
    return backend_object


def backend_func(
    backend,  # type: str
    func,  # type: str
):
    # type: (...) -> Optional[Callable]
    try:
        backend_object = load_backend(backend)
    except (ImportError, AttributeError) as e:
        raise BackendError(
            "Failed to load backend `{backend}`: {err}".format(backend=backend, err=e)
        )
    backend_path = os.environ.get("PEX_EXTRA_SYS_PATH")
    if backend_path:
        if not any(
            backend_object.__file__.startswith(entry + os.sep)
            for entry in backend_path.split(os.pathsep)
        ):
            raise BackendError(
                "The build backend for the Python project at {project_dir} was loaded from {file} "
                "which is not within the configured backend-path {backend_path} as required by "
                "PEP-517: https://peps.python.org/pep-0517/#in-tree-build-backends".format(
                    project_dir=os.getcwd(), file=backend_object.__file__, backend_path=backend_path
                )
            )
    return getattr(backend_object, func, None)


def get_requires_for_build_wheel(backend):
    # type: (str) -> List[str]
    func = backend_func(backend, "get_requires_for_build_wheel")
    if func:
        return func() or []
    return []


def build_wheel(
    backend,  # type: str
    wheel_directory,  # type: str
):
    func = backend_func(backend, "build_wheel")
    if not func:
        raise BackendError(
            "The `{backend}` backend is missing the `build_wheel` function required by "
            "PEP-517.".format(backend=backend)
        )
    return func(wheel_directory)


def main():
    # type: () -> Any

    parser = argparse.ArgumentParser()
    parser.add_argument("--verbose", default=False, action="store_true")
    parser.add_argument("--backend", default="setuptools.build_meta:__legacy__")
    commands = parser.add_subparsers()

    get_requires_for_build_wheel_parser = commands.add_parser("get_requires_for_build_wheel")
    get_requires_for_build_wheel_parser.set_defaults(func=get_requires_for_build_wheel)

    build_wheel_parser = commands.add_parser("build_wheel")
    build_wheel_parser.add_argument("wheel_directory")
    build_wheel_parser.set_defaults(func=build_wheel)

    args = vars(parser.parse_args())
    verbose = args.pop("verbose")
    func = args.pop("func")
    backend = args.pop("backend")

    if verbose:
        print(
            "[{file}] Executing {func}({args}) against backend {backend!r}.".format(
                file=__file__,
                func=func.__name__,
                args=", ".join(
                    "{name}={value!r}".format(name=name, value=value)
                    for name, value in args.items()
                ),
                backend=backend,
            ),
            file=sys.stderr,
        )

    # N.B.: Some backends (setuptools for one) can pollute stdout with log lines; so we re-direct
    # stdout since we need it to communicate our result.
    orig_stdout = sys.stdout
    sys.stdout = sys.stderr

    result = func(backend, **args)
    if verbose:
        print(
            "[{file}] Result was: {result!r}".format(file=__file__, result=result), file=sys.stderr
        )

    json.dump(result, orig_stdout)


if __name__ == "__main__":
    try:
        sys.exit(main())
    except BackendError as err:
        sys.exit(str(err))
