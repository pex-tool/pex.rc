# Release Notes

## 0.19.0

This release fixes Windows `pexrc` and Windows `PEX_TOOLS` to output paths Posix-style when running
in a Posix shell like git bash. A new `--path-style {auto,posix,windows}` option is added to the
relevant commands on Windows to override the path output style auto-detection.

## 0.18.0

This release adds support for the `platforms` `PEX_TOOL` for listing the platforms the PEX is able
to run on.

## 0.17.0

This release adds the `-X` flag to `pexrc` to enable experimental sub-commands for use at your
own risk. The `pexrc build` subcommand is the first such experiment.

## 0.16.5

This release fixes another bug in Python release candidate handling that the 0.16.3 release missed.

## 0.16.4

This release fixes handling of macOS Python Framework builds.

## 0.16.3

This release fixes Python release candidate handling.

## 0.16.2

This release fixes `pexrc inject` target platform detection to handle wheels with compressed tag
sets.

## 0.16.1

This release fixes interpreter detection for `pyenv` shims on unix. Previously the cache of
interpreter information for the shimmed Python was based on the shim and not the Python executable
it resolved to.

In addition, interpreter discovery on Windows is fixed to only discover `python.exe` / `pypy.exe`.
The windowed versions are resolved based off the console versions just in time when needed.

## 0.16.0

This release fixes interpreter discovery for Windows. The [PEP-514 spec](
https://peps.python.org/pep-0514/) is now implemented while also searching further on the 
`PEX_PYTHON_PATH` or `PATH` as appropriate.

In addition, the new `pexrc python {list,inspect}` tools allow insight into the `pexrc` interpreter
discovery mechanism.
 
## 0.15.0

This release adds defaults for Linux and Windows for the `platform_release` environment marker when
generating platform details via `pexrc platform python <spec>`. Now the only `<unknown>` marker
when using a spec is the `platform_version` which is actively antagonistic to any likely real-world
use.

This release also fixes injected windowed PEXes launched with `pythonw.exe` on Windows. Previously
these would incorrectly re-exec with `python.exe` and display a console.

## 0.14.0

This release adds `pexrc platform {info,python}` for displaying both local and foreign platform
information. The machinery powering `pexrc python platform` was ported from Python to Rust, yielding
a little more than 10% cold cache startup time improvement for injected PEXes.

## 0.13.2

This release has injected PEXes using PEP-829 `.start` files to affect `PEX_EXTRA_SYS_PATH`
`sys.path` mutation instead of `.pth` import lines. Although the `.pth` file is still emitted for
maximum compatibility with third party code, the `PEX_EXTRA_SYS_PATH.pth` file will no longer be
emitted for PEX venvs using Python 3.20 and greater. See [PEP-829](https://peps.python.org/pep-0829)
and the [3.15 notes](https://docs.python.org/3.15/whatsnew/3.15.html#whatsnew315-startup-files).

## 0.13.1

This release eliminates an erroneous application starting cursor that would persist for several
seconds when a gui was launched via python-proxyw on Windows.

## 0.13.0

This release brings support for `gui-scripts` and `pythonw.exe` on Windows.

## 0.12.6

This release fixes generation of `--sh-boot` headers on Windows.

## 0.12.5

This release fixes wheel metadata discovery to be robust to non-normalized wheel names, metadata
directory names, or any combination of the two.

## 0.12.4

This release fixes extras handling when resolving a PEX's wheels.

## 0.12.3

This release fixes platform tag detection for macOS arm64 and Windows arm64 and amd64.

## 0.12.2

This release fixes the `venv` PEX tool from trampling Pip provided by PEX deps when `--pip` is
specified. At parity with Pex, a warning is issued if `--collisions-ok`; otherwise the tool exits
with an error message explaining the conflict and the remedies.

Additionally, the `repository extract` tool is changed to wait forever when `--serve`ing instead
of timing out at 5 seconds if the server fails to come up. This is, again, at parity with Pex. In
this case however, a `--timeout` option is added to control this.

## 0.12.1

This release fixes the `venv` PEX tool `--pip` option for Python 2.7.

## 0.12.0

This release introduces `pexrc inject --jobs` to control maximum parallelism when injecting PEXes
with native runtimes bringing parity with the equivalent Pex feature.

Additionally, this release fixes `pexrc inject` target detection for `linux_*` wheels; previously
only `{many,musl}linux` wheels were handled.

Finally, `--source` extraction from directory PEXes and for console scripts is fixed for the
`repository extract` PEX tool.

## 0.11.2

This release fixes another `repository info` `PEX_TOOLS` bug, fixes an inconsistency in interpreter
constraint rendering for CPython threaded and non-threaded implementations and fixes PEXrc venvs
missing `PEX_EXTRA_SYS_PATH` handling.

## 0.11.1

This release fixes bugs in the `repository {info,extract}` `PEX_TOOLS`.

## 0.11.0

This release adds support for PEX-INFO `overridden` and `excluded` dependencies.

## 0.10.0

This release adds support for `PEX_ROOT`. When `PEXRC_ROOT` is set in the environment, it is still
preferred, but if not, a subdir of `PEX_ROOT` will be used to house the pexrc cache.

Additionally, if the final calculated pexrc cache root is not writable, a temporary cache dir will
be established and a warning issued just as is the case for Pex.

## 0.9.2

This release fixes injected `--sh-boot` PEXes to have the same interpreter selection logic as PEX.

## 0.9.1

This release fixes injected PEXes to properly resolve from legacy PEXes on the PEX_PATH at runtime
when those legacy PEXes expose items from the wheel .data/ dir in wheel chroot stashes.

## 0.9.0

This release adds support for auto-scoping the clibs and python-proxies injected into PEXes when
the PEX contains native wheels. For pure-Python PEXes, you still need to pare down manually using
`--target`.

## 0.8.0

This release adds support for "un-spreading" legacy PEX wheel chroots when injecting a PEXrc and
also for proper spreading of injected wheels at runtime. This covers all content delivered via
wheel .data/ dirs that was previously not handled by `pexrc`.

## 0.7.1

This release fixes injected `--sh-boot` PEXes to honor `PEX_TOOLS=1` and be robust to underlying
venv breaks due to system Python upgrades or uninstalls.

## 0.7.0

This release adds support for PEX_TOOLS when `pexrc` is built with the `tools` feature; e.g.:
```console
PEXRC_CLIB_FEATURES=tools cargo build ...
```

Releases now ship with this feature enabled.

## 0.6.0

This release adds support for installing venv console scripts.

## 0.5.0

This release wires PEX_VERBOSE to logging levels for both the `pexrc` tool and the runtime of the
injected PEXes it creates.

## 0.4.1

This release fixes user code support for `--no-pre-install-wheels` injected PEXes.

## 0.4.0

This release adds support for injecting `--no-pre-install-wheels` PEXes of all layout types.

## 0.3.1

This release fixes user code support for `--layout {loose,packed}` injected PEXes.

## 0.3.0

Add support for injecting `--layout loose` PEXes.

## 0.2.0

Add support for injecting `--layout packed` PEXes.

## 0.1.0 

Initial release.

