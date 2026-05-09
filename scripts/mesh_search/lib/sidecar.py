"""Parse `.pac_xml` (skinned-mesh) and `.pami` (static-mesh) sidecars to
recover per-submesh diffuse textures.

Why we need this: Pearl Abyss's filename heuristic (`<dir>/<material>.dds`)
finds A texture, but often it's a normal map or specular map -- the real
diffuse lives at a different path that's only spelled out in the XML.

Examples in the wild:

  .pac_xml has:
    <SkinnedMeshMaterialWrapper _subMeshName="cd_phm_00_hair_base_0022">
      <Material _materialName="SkinnedMeshHair">
        <Vector Name="_parameters">
          <MaterialParameterTexture _name="_baseColorTexture">
            <ResourceReferencePath_ITexture _path="character/texture/cd_phm_00_hair_base_0001.dds"/>
          ...

  .pami has:
    <Material PrimitiveName="cd_nail_01">
      <Parameters>
        <MaterialParameterTexture Name="_baseColorTexture"
                                  Value="object/texture/cd_nail_01.dds"/>
        ...

The XML's path is "canonical" (`character/texture/...` / `object/texture/...`)
but actual VFS paths often live at `character/...` / `object/...` -- one
extra `texture/` segment that needs stripping. The resolver below tries
the literal path first, then a stripped variant.

`nonetexture<hex>.dds` is a known placeholder; treat as None.
"""
from __future__ import annotations

import re
from xml.etree import ElementTree as ET


_NONETEXTURE_RE = re.compile(r"nonetexture0x[0-9a-f]+", re.I)

# Color-multiplier uniform names found across PA shaders, in priority order.
# `_baseColor` / `_tintColor` are the dominant ones; the rest are
# shader-specific (hair dye, terrain blend, etc).
_TINT_PARAM_NAMES = (
    "_baseColor",
    "_tintColor",
    "_tintColorR",       # PA's two-region dye uses _tintColorR (helmets, armor)
    "_hairDyeingColor",  # hair shader
    "_dyeingColor",
    "_dyeingColorMaskR",
    "_baseHeightTintColor",
)


def _is_placeholder(path: str) -> bool:
    if not path:
        return True
    return bool(_NONETEXTURE_RE.search(path))


def _path_variants(p: str) -> list[str]:
    """Return literal + sensible munged forms to try in VFS lookup."""
    if not p:
        return []
    p = p.replace("\\", "/").lstrip("/")
    variants = [p]
    # Strip a `texture/` infix if present right after the top-level dir.
    parts = p.split("/", 2)
    if len(parts) == 3 and parts[1] == "texture":
        variants.append(f"{parts[0]}/{parts[2]}")
    return variants


def resolve_texture_path(vfs, raw_path: str) -> str | None:
    """Given a path as written in a PA XML, return the actual VFS path or
    None. Skips known placeholder paths."""
    if _is_placeholder(raw_path):
        return None
    for v in _path_variants(raw_path):
        if vfs.lookup(v) is not None:
            return v
    return None


def _parse_color(value: str) -> tuple[float, float, float, float] | None:
    """Parse a color string from PA XML. Two formats seen:
        '#aabbccdd'              (hex sRGB-ish, RGBA)
        '0.500000 0.300000 0.4'  (space-separated linear RGB)
        '0.5 0.3 0.4 1.0'        (space-separated linear RGBA)
    """
    if not value:
        return None
    v = value.strip()
    if v.startswith("#"):
        v = v[1:]
        if len(v) == 6: v += "ff"
        if len(v) != 8: return None
        try:
            r = int(v[0:2], 16) / 255.0
            g = int(v[2:4], 16) / 255.0
            b = int(v[4:6], 16) / 255.0
            a = int(v[6:8], 16) / 255.0
            return (r, g, b, a)
        except ValueError:
            return None
    parts = v.split()
    try:
        nums = [float(p) for p in parts[:4]]
    except ValueError:
        return None
    if len(nums) < 3:
        return None
    if len(nums) == 3:
        nums.append(1.0)
    return tuple(nums)


def _byte4_to_rgba(packed: int) -> tuple[float, float, float, float]:
    """PA's MaterialParameterByte4 packs 4 bytes into a uint32. Layout
    (little-endian): byte0=R, byte1=G, byte2=B, byte3=A."""
    return (
        ( packed        & 0xff) / 255.0,
        ((packed >>  8) & 0xff) / 255.0,
        ((packed >> 16) & 0xff) / 255.0,
        ((packed >> 24) & 0xff) / 255.0,
    )


def _find_tint(scope) -> tuple[float, float, float, float] | None:
    """Walk MaterialParameterColor children of `scope` looking for one of
    the known tint param names (in priority order). Returns the first hit
    as RGBA floats. Falls back to MaterialParameterByte4 _grimeBlendingParameterR
    when none of the named color slots exist -- that's how PA's
    SkinnedMeshCloth_Ver2 shader stores its base color (no _baseColorTexture
    in the XML, the colour is shader-uniform encoded as a Byte4)."""
    by_name: dict[str, tuple] = {}
    for c in scope.iter("MaterialParameterColor"):
        name = c.get("_name") or c.get("Name") or ""
        raw = c.get("_value") or c.get("Value") or ""
        col = _parse_color(raw)
        if col is None:
            continue
        by_name[name] = col
    for n in _TINT_PARAM_NAMES:
        if n in by_name:
            return by_name[n]
    # Byte4 fallback: grime/cloth base color packed as uint32 RGBA.
    for c in scope.iter("MaterialParameterByte4"):
        name = c.get("_name") or c.get("Name") or ""
        if name != "_grimeBlendingParameterR":
            continue
        raw = c.get("_value") or c.get("Value") or ""
        try:
            packed = int(raw)
        except (TypeError, ValueError):
            continue
        return _byte4_to_rgba(packed)
    return None


def _parse_xml(data: bytes):
    """xml.etree handles UTF-8 BOM cleanly if we strip it first."""
    if data.startswith(b"\xef\xbb\xbf"):
        data = data[3:]
    # Some files are XML fragments without a single root element. Wrap.
    text = data.decode("utf-8", errors="replace")
    text = f"<__cdroot__>{text}</__cdroot__>"
    return ET.fromstring(text)


def parse_pac_xml(data: bytes) -> list[dict]:
    """Return list of {submesh_name, base_color_path, tint}, one per
    SkinnedMeshMaterialWrapper, in document order. `tint` is RGBA floats
    or None."""
    root = _parse_xml(data)
    out = []
    for wrapper in root.iter("SkinnedMeshMaterialWrapper"):
        name = wrapper.get("_subMeshName", "")
        diffuse_raw = ""
        for tex in wrapper.iter("MaterialParameterTexture"):
            tname = tex.get("_name") or tex.get("Name") or ""
            if tname.lower() == "_basecolortexture":
                ref = tex.find("ResourceReferencePath_ITexture")
                if ref is not None:
                    diffuse_raw = ref.get("_path") or ref.get("Value") or ""
                    break
        tint = _find_tint(wrapper)
        out.append({"submesh_name": name, "base_color_path": diffuse_raw, "tint": tint})
    return out


def parse_pami(data: bytes) -> list[dict]:
    """Return list of {primitive_name, base_color_path, tint}, one per
    <Material> block, in document order. `tint` is RGBA floats or None."""
    root = _parse_xml(data)
    out = []
    for mat in root.iter("Material"):
        # Skip <Material Name="_resourceMaterial"> -- those are nested
        # parameter blocks, not top-level materials.
        if not mat.get("PrimitiveName"):
            continue
        prim = mat.get("PrimitiveName", "")
        diffuse_raw = ""
        params = mat.find("Parameters")
        params_iter = params.iter("MaterialParameterTexture") if params is not None else mat.iter("MaterialParameterTexture")
        for tex in params_iter:
            tname = tex.get("Name") or tex.get("_name") or ""
            if tname.lower() == "_basecolortexture":
                diffuse_raw = tex.get("Value") or ""
                if not diffuse_raw:
                    ref = tex.find("ResourceReferencePath_ITexture")
                    if ref is not None:
                        diffuse_raw = ref.get("_path") or ref.get("Value") or ""
                break
        tint = _find_tint(mat)
        out.append({"primitive_name": prim, "base_color_path": diffuse_raw, "tint": tint})
    return out


def sidecar_textures_for_pac(vfs, pac_path: str, n_submeshes: int) -> list[dict]:
    """Return a list of length n_submeshes, one entry per submesh:
        {"path": str|None, "tint": (r,g,b,a) | None}
    """
    xml_path = pac_path + "_xml"
    entry = vfs.lookup(xml_path)
    if entry is None:
        return [{"path": None, "tint": None} for _ in range(n_submeshes)]
    try:
        data = vfs.read_entry(entry)
        wrappers = parse_pac_xml(data)
    except Exception:
        return [{"path": None, "tint": None} for _ in range(n_submeshes)]
    out = []
    for i in range(n_submeshes):
        if i >= len(wrappers):
            out.append({"path": None, "tint": None})
            continue
        raw = wrappers[i].get("base_color_path", "")
        out.append({
            "path": resolve_texture_path(vfs, raw),
            "tint": wrappers[i].get("tint"),
        })
    return out


def _pami_path_for_pam(pam_path: str) -> str:
    # foo.pam -> foo.pami ; foo.pamlod -> foo.pami (shares the sidecar)
    for ext in (".pamlod", ".pam"):
        if pam_path.endswith(ext):
            return pam_path[: -len(ext)] + ".pami"
    return pam_path + ".pami"


def sidecar_textures_for_pam(vfs, pam_path: str,
                             materials: list[str]) -> list[dict]:
    """Per-submesh entry: {"path": str|None, "tint": (r,g,b,a)|None}."""
    pami_path = _pami_path_for_pam(pam_path)
    entry = vfs.lookup(pami_path)
    none = [{"path": None, "tint": None} for _ in materials]
    if entry is None:
        return none
    try:
        data = vfs.read_entry(entry)
        mats = parse_pami(data)
    except Exception:
        return none

    by_name = {m["primitive_name"].lower(): m for m in mats if m["primitive_name"]}

    out = []
    for i, mat_name in enumerate(materials):
        m = by_name.get((mat_name or "").lower())
        if m is None and i < len(mats):
            m = mats[i]
        if m is None:
            out.append({"path": None, "tint": None})
            continue
        out.append({
            "path": resolve_texture_path(vfs, m["base_color_path"]),
            "tint": m.get("tint"),
        })
    return out
