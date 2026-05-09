"""Shared mesh -> renderable-submeshes pipeline.

Turns a parsed PAM/PAMLOD/PAC mesh + the live VFS into a list of dicts
ready for `raster.MeshRenderer.render_views`. Identical resolution
order to the original `render_corpus_fast.py` so any embeddings produced
via this lib are bit-comparable to the existing corpus.

Texture-resolution per submesh:
    1. Sidecar XML (.pac_xml for PAC, .pami for PAM/PAMLOD) -- the
       authoritative material->texture wiring used by the engine.
    2. Heuristic <dir>/<material>.dds with suffix probes.
"""
from __future__ import annotations

import math
from typing import Any

import numpy as np

import cdcore

from .sidecar import sidecar_textures_for_pac, sidecar_textures_for_pam


PARSERS = {
    "pam":    cdcore.parse_pam,
    "pamlod": cdcore.parse_pamlod,
    "pac":    cdcore.parse_pac,
}

TEXTURE_SUFFIX_PROBES = ("", "_d", "_diffuse", "_albedo", "_col", "_color", "_base", "_basecolor")

PLACEHOLDER_NAMES = {"shader", "uncooked", "default", "missing", ""}

# Stems ending in any of these are NOT diffuse; never use as the
# heuristic's base-color match. Pearl Abyss naming convention:
#   _n / _normal -- tangent-space normal map
#   _sp / _spec  -- specular / gloss
#   _ma / _mg    -- material / "grime" mask channels
#   _mr          -- metallic+roughness
#   _disp / _h   -- displacement / height
#   _ao          -- ambient occlusion
#   _f / _f01    -- "feature" channels (rare)
NON_DIFFUSE_SUFFIXES = (
    "_n", "_normal",
    "_sp", "_spec", "_specular",
    "_ma", "_mg", "_mr",
    "_disp", "_h", "_height",
    "_ao",
    "_f",
)


def _has_non_diffuse_suffix(stem: str) -> bool:
    s = stem.lower()
    return any(s.endswith(suf) for suf in NON_DIFFUSE_SUFFIXES)


def resolve_diffuse(vfs, mesh_path: str, material: str, sm_texture: str = "") -> str | None:
    """Find the best diffuse DDS in the VFS for one submesh.

    Order of attempts:
      1. sm.texture directly, if it's already a resolvable path.
      2. material/sm_texture as an absolute path (handles leading '/' from
         leveldata e.g. '/leveldata/rootlevel/proxylod/foo.dds').
      3. material as a basename in the same dir as the mesh, with the
         shipping suffix probe set.
      4. material as a basename in known texture dirs (character/, object/,
         leveldata/, texture/).
    """
    def try_lookup(p):
        if not p:
            return None
        p = p.lstrip("/")
        return p if vfs.lookup(p) is not None else None

    if sm_texture:
        hit = try_lookup(sm_texture)
        if hit:
            return hit

    raw = (sm_texture or material).strip()
    raw_l = raw.lower()
    if not raw_l or raw_l in PLACEHOLDER_NAMES:
        return None

    if "/" in raw or raw_l.endswith(".dds"):
        candidate = raw_l if raw_l.endswith(".dds") else f"{raw_l}.dds"
        hit = try_lookup(candidate)
        if hit:
            return hit

    stem = raw_l.lstrip("/")
    if stem.endswith(".dds"):
        stem = stem[:-4]

    if _has_non_diffuse_suffix(stem):
        return None

    mesh_dir = mesh_path.rsplit("/", 1)[0] if "/" in mesh_path else ""
    if mesh_dir:
        for suffix in TEXTURE_SUFFIX_PROBES:
            hit = try_lookup(f"{mesh_dir}/{stem}{suffix}.dds")
            if hit:
                return hit

    for d in ("character", "object", "leveldata", "texture"):
        if d == mesh_dir:
            continue
        for suffix in TEXTURE_SUFFIX_PROBES:
            hit = try_lookup(f"{d}/{stem}{suffix}.dds")
            if hit:
                return hit

    return None


def load_texture(vfs, dds_path: str, dds_cache: dict):
    """VFS-path -> np.uint8 [H,W,4]. Cached. Returns None on decode error."""
    cached = dds_cache.get(dds_path)
    if cached is not None:
        return cached if isinstance(cached, np.ndarray) else None
    entry = vfs.lookup(dds_path)
    if entry is None:
        dds_cache[dds_path] = False
        return None
    try:
        tex_bytes = vfs.read_entry(entry)
        w, h, rgba = cdcore.decode_dds_to_rgba(tex_bytes)
        arr = np.frombuffer(bytes(rgba), dtype=np.uint8).reshape(h, w, 4).copy()
    except Exception:
        dds_cache[dds_path] = False
        return None
    dds_cache[dds_path] = arr
    return arr


def build_submeshes(vfs, mesh_path: str, fmt: str, parsed_mesh, dds_cache: dict) -> tuple[list[dict[str, Any]], bool]:
    """Return (renderable_submeshes, had_any_texture).

    Each submesh dict is shaped for raster.MeshRenderer.render_views:
        vertices, faces, uvs, normals, texture, tint
    """
    submeshes = parsed_mesh.submeshes
    n = len(submeshes)

    if fmt == "pac":
        sidecar = sidecar_textures_for_pac(vfs, mesh_path, n)
    else:
        sidecar = sidecar_textures_for_pam(vfs, mesh_path, [sm.material for sm in submeshes])

    out: list[dict[str, Any]] = []
    had_any_texture = False
    for i, sm in enumerate(submeshes):
        if sm.vertex_count == 0 or sm.face_count == 0:
            continue
        v = np.array(sm.vertices, dtype=np.float32)
        f = np.array(sm.faces, dtype=np.uint32).reshape(-1, 3)
        if v.size == 0 or f.size == 0:
            continue
        uvs = np.array(sm.uvs, dtype=np.float32) if sm.uvs and len(sm.uvs) == len(v) else None
        normals = np.array(sm.normals, dtype=np.float32) if sm.normals and len(sm.normals) == len(v) else None

        sc = sidecar[i] if i < len(sidecar) else {}
        chosen = sc.get("path") if sc else None
        tint = sc.get("tint") if sc else None
        if chosen is None:
            chosen = resolve_diffuse(vfs, mesh_path, sm.material, sm.texture)
        tex = load_texture(vfs, chosen, dds_cache) if chosen else None
        if tex is not None:
            had_any_texture = True

        out.append({
            "vertices": v, "faces": f, "uvs": uvs,
            "normals": normals, "texture": tex, "tint": tint,
        })
    return out, had_any_texture


def fibonacci_sphere(n: int) -> np.ndarray:
    pts = []
    phi = math.pi * (3.0 - math.sqrt(5.0))
    for i in range(n):
        y = 1.0 - (i / max(n - 1, 1)) * 2.0
        r = math.sqrt(max(1.0 - y * y, 0.0))
        theta = phi * i
        pts.append([math.cos(theta) * r, y, math.sin(theta) * r])
    return np.array(pts, dtype=np.float32)


def mesh_bbox(parsed_mesh) -> tuple[list[float], float]:
    """(center, diag) of the mesh bbox; diag clamped to >= 1e-6."""
    bb_min = np.array(parsed_mesh.bbox_min, dtype=np.float32)
    bb_max = np.array(parsed_mesh.bbox_max, dtype=np.float32)
    center = ((bb_min + bb_max) / 2).tolist()
    diag = float(np.linalg.norm(bb_max - bb_min)) or 1.0
    return center, diag
