from .cdcore import *
from .cdcore import __version__

import sys
import types
from pathlib import Path


class _RustVfsManager:
    """VfsProtocol-conforming wrapper around the Rust VfsManager.

    Injected into sys.modules as core.vfs_manager so every existing
    'from core.vfs_manager import VfsManager' gets the Rust backend
    without any changes to the Python project.

    Unknown attributes fall through to a lazily-loaded instance of the
    original Python VfsManager so newer CrimsonForge code that calls
    methods not yet implemented here keeps working.
    """

    def __init__(self, packages_path):
        from .cdcore import VfsManager as _RustVM
        self._packages_path = Path(packages_path)
        self._rust = _RustVM(str(self._packages_path))
        self._py_fallback = None

    def _get_fallback(self):
        if self._py_fallback is None:
            sys.modules.pop("core.vfs_manager", None)
            import importlib
            _real_mod = importlib.import_module("core.vfs_manager")
            self._py_fallback = _real_mod.VfsManager(str(self._packages_path))
            sys.modules["core.vfs_manager"] = sys.modules.get("cdcore.vfs_shim", self)
        return self._py_fallback

    def __getattr__(self, name):
        if name.startswith("_"):
            raise AttributeError(name)
        import warnings
        warnings.warn(
            f"cdcore._RustVfsManager: '{name}' not implemented, falling back to Python",
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


def _rust_decode_dds_to_rgba(data: bytes):
    from .cdcore import decode_dds_to_rgba
    return decode_dds_to_rgba(data)


class _DdsProxy(types.ModuleType):
    """Proxy for core.dds_reader that replaces decode_dds_to_rgba with
    the Rust implementation while delegating all other attributes to the
    real Python module loaded from disk."""

    _real = None

    def _load_real(self):
        if self._real is None:
            sys.modules.pop("core.dds_reader", None)
            import importlib
            self._real = importlib.import_module("core.dds_reader")
            sys.modules["core.dds_reader"] = self
        return self._real

    def __getattr__(self, name):
        if name == "decode_dds_to_rgba":
            return _rust_decode_dds_to_rgba
        return getattr(self._load_real(), name)


# Inject core.vfs_manager — any 'from core.vfs_manager import VfsManager'
# gets _RustVfsManager without the Python project needing to know.
if "core.vfs_manager" not in sys.modules:
    _vfs_mod = types.ModuleType("core.vfs_manager")
    _vfs_mod.VfsManager = _RustVfsManager
    sys.modules["core.vfs_manager"] = _vfs_mod

# Inject core.dds_reader — decode_dds_to_rgba is backed by Rust; all
# other attributes (read_dds_info, validate_dds_payload_size, etc.)
# fall through to the real Python module on first access.
if "core.dds_reader" not in sys.modules:
    sys.modules["core.dds_reader"] = _DdsProxy("core.dds_reader")
