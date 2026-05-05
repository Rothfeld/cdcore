"""Rust-backed building blocks for the Crimson Desert toolchain.

This module re-exports every type, parser, and helper compiled by the
``cdcore`` Rust crate.  It deliberately has *no* import-time side
effects: nothing in ``sys.modules`` is mutated, no other package's
attributes are patched.  Importing this library is now boring, the way
a library import should be.

Projects that want the ``core.vfs_manager`` / ``core.dds_reader`` /
``core.mesh_parser`` drop-in replacements opt in with a single
side-effect import in their entry point:

    import cdcore.crimsonforge   # noqa: F401

Do this before any ``from core.X import ...`` statement runs.  See
:mod:`cdcore.crimsonforge` for what the shim covers.
"""
from .cdcore import *
from .cdcore import __version__
