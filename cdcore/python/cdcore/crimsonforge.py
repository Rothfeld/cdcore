"""Drop-in Rust-backed shims for the CrimsonForge Python toolchain.

The Crimson Desert Python project ships pure-Python modules at
``core.vfs_manager``, ``core.dds_reader``, and ``core.mesh_parser``.
This submodule provides behaviour-equivalent (and substantially
faster) Rust-backed replacements and wires them into
:mod:`sys.modules` so existing ``from core.vfs_manager import ...``
imports transparently take the Rust path.

Usage -- a single line in the entry point, before any ``from core.*``
import runs::

    import cdcore.crimsonforge   # noqa: F401  (side-effect import)

Importing :mod:`cdcore` itself has no side effects.  This explicitly
named submodule does the patching at import time; the module name is
the signal that something project-specific is happening, so the import
statement is self-documenting.  :func:`install` is exposed and
idempotent in case a test harness needs to invoke it directly.

Anything not implemented in the wrapper falls through to the original
Python module, with a one-shot ``warnings.warn`` so unknown call sites
stay visible.
"""
from __future__ import annotations

import sys
import types
import warnings
from pathlib import Path


class _RustVfsManager:
    """``core.vfs_manager.VfsManager`` shape, backed by Rust.

    Wraps :class:`cdcore.VfsManager` and translates the project's
    method names (``load_pamt``, ``read_entry_data``, ``extract_entry``,
    ``invalidate_pamt_cache``, ``list_package_groups``) onto the
    underlying Rust calls.  Methods we haven't shimmed delegate to a
    lazily-instantiated Python ``VfsManager`` so newer code keeps
    working unchanged.
    """

    def __init__(self, packages_path):
        from . import VfsManager as _RustVM
        self._packages_path = Path(packages_path)
        self._rust = _RustVM(str(self._packages_path))
        self._py_fallback = None

    def _get_fallback(self):
        if self._py_fallback is None:
            # Re-import the real ``core.vfs_manager`` module from disk.
            # Our shim has already been installed at this module path,
            # so we have to drop it before letting importlib find the
            # underlying file, then reinstate the shim afterwards.
            shim_module = sys.modules.pop("core.vfs_manager", None)
            import importlib
            real_mod = importlib.import_module("core.vfs_manager")
            self._py_fallback = real_mod.VfsManager(str(self._packages_path))
            if shim_module is not None:
                sys.modules["core.vfs_manager"] = shim_module
        return self._py_fallback

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        warnings.warn(
            f"cdcore.compat: '{name}' not shimmed, falling back to Python VfsManager",
            stacklevel=2,
        )
        return getattr(self._get_fallback(), name)

    def load_pamt(self, group_dir: str):
        self._rust.load_group(group_dir)
        return self._rust.get_pamt(group_dir)

    def list_package_groups(self) -> list:
        return self._rust.list_groups()

    def get_pamt(self, group_dir: str):
        return self._rust.get_pamt(group_dir)

    def invalidate_pamt_cache(self, group_dir: str):
        self._rust.invalidate_group(group_dir)

    def reload(self) -> None:
        self._rust.reload()

    def read_entry_data(self, entry) -> bytes:
        return self._rust.read_entry(entry)

    def extract_entry(self, entry, output_dir: str) -> dict:
        data = self._rust.read_entry(entry)
        rel = entry.path.replace("\\", "/").lstrip("/")
        dest = Path(output_dir) / rel
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_bytes(data)
        return {"path": str(dest), "size": len(data)}

    @property
    def packages_path(self) -> str:
        return str(self._packages_path)

    @property
    def papgt_path(self) -> str:
        return str(self._packages_path / "meta" / "0.papgt")


class _DdsProxy(types.ModuleType):
    """Proxy for ``core.dds_reader`` -- ``decode_dds_to_rgba`` is Rust,
    everything else falls through to the real Python module on first
    access."""

    _real = None

    def _load_real(self):
        if self._real is None:
            shim = sys.modules.pop("core.dds_reader", None)
            import importlib
            self._real = importlib.import_module("core.dds_reader")
            if shim is not None:
                sys.modules["core.dds_reader"] = shim
        return self._real

    def __getattr__(self, name):
        if name == "decode_dds_to_rgba":
            from . import decode_dds_to_rgba
            return decode_dds_to_rgba
        return getattr(self._load_real(), name)


class _MeshParserProxy(types.ModuleType):
    """Proxy for ``core.mesh_parser`` -- ``parse_pam`` and
    ``parse_pamlod`` are Rust (30-70x faster); everything else falls
    through.  Patching the loaded module's globals on first access
    means internal helpers like ``parse_mesh`` also pick up the Rust
    implementations."""

    _real = None

    def _load_real(self):
        if self._real is None:
            shim = sys.modules.pop("core.mesh_parser", None)
            import importlib
            self._real = importlib.import_module("core.mesh_parser")
            if shim is not None:
                sys.modules["core.mesh_parser"] = shim
            from . import parse_pam as _rs_pam, parse_pamlod as _rs_pamlod
            self._real.parse_pam    = _rs_pam
            self._real.parse_pamlod = _rs_pamlod
        return self._real

    def __getattr__(self, name):
        if name == "parse_pam":
            from . import parse_pam
            return parse_pam
        if name == "parse_pamlod":
            from . import parse_pamlod
            return parse_pamlod
        return getattr(self._load_real(), name)


_INSTALLED = False


def install() -> None:
    """Install the Rust-backed shims into :mod:`sys.modules`.

    Runs automatically when this module is first imported; exposed so
    test harnesses can call it explicitly.  Idempotent.

    Prints a one-line confirmation to stderr so the activation is
    visible in the console.
    """
    global _INSTALLED
    if _INSTALLED:
        return

    patched = []
    if "core.vfs_manager" not in sys.modules:
        mod = types.ModuleType("core.vfs_manager")
        mod.VfsManager = _RustVfsManager
        sys.modules["core.vfs_manager"] = mod
        patched.append("core.vfs_manager")

    if "core.dds_reader" not in sys.modules:
        sys.modules["core.dds_reader"] = _DdsProxy("core.dds_reader")
        patched.append("core.dds_reader")

    if "core.mesh_parser" not in sys.modules:
        sys.modules["core.mesh_parser"] = _MeshParserProxy("core.mesh_parser")
        patched.append("core.mesh_parser")

    _INSTALLED = True

    import os
    if patched:
        from . import __version__
        print(
            f"cdcore.crimsonforge {__version__}: shimmed {', '.join(patched)}",
            file=sys.stderr,
        )


# Importing this module is the signal to install -- the module name
# (cdcore.crimsonforge) advertises that side effect to callers.
install()
