"""Walk the CD VFS, render each PAM/PAMLOD/PAC in 12 views, CLIP-encode, save.

Usage:
    python3 crates/scripts/mesh_search/build_corpus.py <game_install_dir>

    game_install_dir : path to the CD package directory, e.g. /cd

Output dir is `.corpus/` next to this script (CORPUS_DIR), containing:
    corpus.safetensors        -- {"views": [N, 12, 512], "mean": [N, 512]} L2-normed,
                                 plus metadata["meta"] = JSON-encoded build params + paths
    thumbnails/<vfs_path>.png -- one PNG per mesh, mirroring the VFS tree
"""
import sys
assert len(sys.argv) == 2, __doc__
game_install_dir = sys.argv[1]

from __future__ import annotations
import json
import traceback
from pathlib import Path
sys.path.insert(0, str(Path(__file__).resolve().parent))

import numpy as np
import torch
from PIL import Image
from safetensors.torch import save_file
from tqdm import tqdm

import cdcore
from lib.mesh_pipeline import (
    PARSERS, build_submeshes, fibonacci_sphere, mesh_bbox,
)
from lib.raster import MeshRenderer

MODEL_ID = "openai/clip-vit-base-patch32"
N_VIEWS = 12
# The 12 fibonacci-sphere views are for CLIP encoding (uniform sampling
# of the sphere is what matters there). For the human-readable thumbnail
# we render a separate, hand-chosen direction: front-3/4 at moderate
# elevation, the canonical catalog/illustration pose.
THUMB_DIR = (-0.542, 0.455, -0.707)
RES = 224
THUMB_SIZE = 32     # inline thumb for the embeddings visualizer (viz.py)
# Linear RGBA the rasterizer clears to behind every mesh.  In the 8-bit
# framebuffer it ends up as (128, 128, 128) -- which is what query.py
# composites bg-removed images onto, kept in sync via metadata["bg_rgb"].
BG_COLOR = (0.5, 0.5, 0.5, 1.0)
CORPUS_DIR = Path(__file__).resolve().parent / ".corpus"


CORPUS_DIR.mkdir(exist_ok=True)
device = "cuda" if torch.cuda.is_available() else "cpu"

print(f"VFS {game_install_dir}")
vfs = cdcore.VfsManager(game_install_dir); vfs.load_all_groups()

print(f"CLIP {MODEL_ID} on {device}")
# Silence transformers v5's "[transformers] Accessing __path__ ..."
# deprecation spam, emitted on lazy-loading every image processor.
from transformers.utils import logging as hf_logging
hf_logging.set_verbosity_error()
from transformers import CLIPModel, CLIPProcessor
model = CLIPModel.from_pretrained(MODEL_ID).to(device).eval()
proc  = CLIPProcessor.from_pretrained(MODEL_ID)

print(f"renderer @ {RES}x{RES}, {N_VIEWS} views")
renderer = MeshRenderer(width=RES, height=RES)
view_dirs = fibonacci_sphere(N_VIEWS)

meshes: list[tuple[str, str]] = []
for grp in vfs.list_groups():
    pamt = vfs.get_pamt(grp)
    if pamt is None: continue
    for entry in pamt.file_entries:
        tail = entry.path.rsplit("/", 1)[-1]
        ext = tail.rsplit(".", 1)[1].lower() if "." in tail else ""
        if ext in PARSERS:
            meshes.append((entry.path, ext))
print(f"{len(meshes)} candidate meshes")

dds_cache: dict = {}
views_per_mesh: list[torch.Tensor] = []
thumbs: list[np.ndarray] = []
bbox_diags: list[float] = []
paths: list[str] = []
n_fail = 0

pbar = tqdm(meshes, desc="render+encode", unit="mesh", smoothing=0.05)
for vfs_path, fmt in pbar:
    try:
        entry = vfs.lookup(vfs_path)
        data = vfs.read_entry(entry)
        mesh = PARSERS[fmt](data, vfs_path)
        subs, _ = build_submeshes(vfs, vfs_path, fmt, mesh, dds_cache)
        if not subs or mesh.total_vertices == 0:
            n_fail += 1; pbar.set_postfix(ok=len(paths), fail=n_fail); continue
        center, diag = mesh_bbox(mesh)
        # 12 uniform-sphere views for CLIP + 1 hand-picked thumb angle.
        all_dirs = np.concatenate([view_dirs, np.array([THUMB_DIR], dtype=np.float32)])
        rgb_all = renderer.render_views(submeshes=subs, view_directions=all_dirs,
                                        bbox_center=center, bbox_diag=diag,
                                        bg_color=BG_COLOR)
        rgb       = rgb_all[:N_VIEWS]
        thumb_rgb = rgb_all[N_VIEWS]
    except Exception:
        tqdm.write(f"skip {vfs_path}")
        traceback.print_exc()
        n_fail += 1; pbar.set_postfix(ok=len(paths), fail=n_fail); continue

    thumb_full = thumb_rgb                                         # [H, W, 3] uint8
    thumb_pil = Image.fromarray(thumb_full)
    thumb = CORPUS_DIR / "thumbnails" / f"{vfs_path}.png"
    thumb.parent.mkdir(parents=True, exist_ok=True)
    thumb_pil.save(thumb)
    # Tiny inline copy for the embeddings visualizer (viz.py). RGBA
    # with hard alpha mask at full res before LANCZOS downsample, so
    # silhouettes get a proper alpha-blended fade and bg pixels are
    # fully transparent (no grey halo around the mesh in viz.py).
    bg = np.array([round(c * 255) for c in BG_COLOR[:3]], dtype=np.uint8)
    is_bg = np.all(np.abs(thumb_full.astype(np.int16) - bg.astype(np.int16)) <= 1, axis=-1)
    alpha = np.where(is_bg, 0, 255).astype(np.uint8)
    rgba_full = np.concatenate([thumb_full, alpha[..., None]], axis=-1)
    rgba_small = np.asarray(Image.fromarray(rgba_full, mode="RGBA").resize(
        (THUMB_SIZE, THUMB_SIZE), Image.LANCZOS))
    thumbs.append(rgba_small)

    imgs = [Image.fromarray(im) for im in rgb]
    inp = proc(images=imgs, return_tensors="pt").to(device)
    with torch.no_grad():
        f = model.get_image_features(**inp).pooler_output
    f = f / f.norm(dim=-1, keepdim=True)            # [V, 512]
    views_per_mesh.append(f.float().cpu())
    bbox_diags.append(diag)                             # [N] -- viz.py uses this as a 3D axis
    paths.append(vfs_path)
    pbar.set_postfix(ok=len(paths), fail=n_fail)

assert views_per_mesh, "no embeddings produced"
V = torch.stack(views_per_mesh, 0).contiguous()       # [N, 12, 512]
M = V.mean(1); M = M / M.norm(dim=-1, keepdim=True)   # [N, 512]
T = torch.from_numpy(np.stack(thumbs)).contiguous()   # [N, 32, 32, 4] uint8 RGBA
B = torch.tensor(bbox_diags, dtype=torch.float32).contiguous()  # [N]
meta = {
    "paths":            paths,
    "game_install_dir": game_install_dir,
    "model_id":         MODEL_ID,
    "n_views":          N_VIEWS,
    "resolution":       RES,
    "thumb_size":       THUMB_SIZE,
    "thumb_format":     "RGBA",
    "bg_rgb":           [round(c * 255) for c in BG_COLOR[:3]],
}
save_file({"views": V, "mean": M, "thumbs": T, "bbox_diag": B},
          str(CORPUS_DIR / "corpus.safetensors"),
          metadata={"meta": json.dumps(meta)})
print(f"wrote {CORPUS_DIR}  ({len(paths)} meshes, {n_fail} failed)")
