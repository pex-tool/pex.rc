#!/bin/sh
# -*- coding: utf-8 -*-
# Copyright 2026 Pex project contributors.
# SPDX-License-Identifier: Apache-2.0
# --- split --- #

# N.B.: This script should stick to syntax defined for POSIX `sh` and avoid non-builtins.
# See: https://pubs.opengroup.org/onlinepubs/9699919799/idx/shell.html
set -eu

# --- split --- #
# N.B.: These vars are templated in by pexrc when it injects a PEX with its runtime.
RAW_DEFAULT_PEXRC_ROOT="{pexrc_root}"
VENV_RELPATH="{venv_relpath}"
PYTHONS="{pythons}"
PYTHON_ARGS="{python_args}"
# --- split --- #

# N.B.: The SC2116 warning suppressions below are in place to ensure tilde-expansion of the
# DEFAULT_PEX_ROOT value which is necessary for the -x check of the venv pex to succeed when it
# should.
if [ -n "${RAW_DEFAULT_PEXRC_ROOT}" ]; then
    # shellcheck disable=SC2116
    DEFAULT_PEXRC_ROOT="$(echo ${RAW_DEFAULT_PEXRC_ROOT})"
else
    if uname -s | grep -iE 'mac|darwin' > /dev/null; then
        # shellcheck disable=SC2116
        DEFAULT_PEXRC_ROOT="$(echo ~/Library/Caches/pexrc)"
    else
        # shellcheck disable=SC2116
        DEFAULT_PEXRC_ROOT="$(echo ~/.cache/pexrc)"
    fi
fi

PEXRC_ROOT="${PEXRC_ROOT:-${DEFAULT_PEXRC_ROOT}}"
VENV="${PEXRC_ROOT}/${VENV_RELPATH}"

on_fast_path() {
    [ -z "${PEX_IGNORE_RCFILES:-}" ] \
      && [ -z "${PEX_PYTHON:-}" ] \
      && [ -z "${PEX_PYTHON_PATH:-}" ] \
      && [ -z "${PEX_PATH:-}" ] \
      && [ -z "${PEX_TOOLS:-}" ]
}

if on_fast_path; then
  for python in ${PYTHONS}; do
      if [ -x "${VENV}/sh-boot/base-${python}" ] && [ -x "${VENV}/sh-boot/pex-${python}" ]; then
          # The fast path: We're installed under the PEXRC_ROOT and the venv interpreter to use is
          # embedded in the shebang of our venv pex script; so just execute that script directly.
          export PEX="$0"

          exec "${VENV}/sh-boot/pex-${python}" "$@"
      fi
  done
fi

find_python() {
    for python in ${PYTHONS}; do
        if command -v "${python}" 2>/dev/null; then
            return
        fi
    done
}

# The slow path: This PEX zipapp is not installed yet. Run the PEX zipapp so it can install itself,
# rebuilding its fast path layout under the PEXRC_ROOT.
python_exe="$(find_python)"
if [ -n "${python_exe}" ]; then
    if [ -n "${PEX_VERBOSE:-}" ]; then
        echo >&2 "$0 used /bin/sh boot to select python: ${python_exe} for re-exec..."
        if [ -n "${PEX_TOOLS:-}" ]; then
          echo >&2 "Running pex to invoke PEX_TOOLS."
        else
          echo >&2 "Running pex to lay itself out under PEXRC_ROOT."
        fi
    fi
    export _PEXRC_SH_BOOT_SEED_DIR="${VENV}/sh-boot"
    exec "${python_exe}" "${PYTHON_ARGS}" "$0" "$@"
fi

echo >&2 "Failed to find any of these python binaries on the PATH:"
for python in ${PYTHONS} ; do
    echo >&2 "${python}"
done
echo >&2 "Either adjust your PATH, which is currently:"
echo >&2 "${PATH}"
echo >&2 "Or else install an appropriate Python that provides one of the binaries in this list."
exit 1