"""Streamlit 3D embeddings viewer -- thumbnails ARE the points.

    streamlit run crates/scripts/mesh_search/viz.py

Reads `.corpus/corpus.safetensors`, PCA-reduces the mean-pooled CLIP
features to 3D, encodes each chosen thumbnail as a PNG data URL on its
point, and renders them as billboard sprites in a deck.gl OrbitView.

Why we embed deck.gl from a CDN via st.components.v1.html instead of
using pydeck: streamlit's bundled deck.gl JSON converter only registers
MapView and a few enums, so OrbitView's controller throws assertion
failures on every interaction. Going to raw deck.gl gives us the full
API (OrbitView, COORDINATE_SYSTEM.CARTESIAN, IconLayer auto-packing).

Defaults to a stratified 8000-mesh sample so the JSON payload (~8 MB
at 32x32 PNGs per point) is reasonable.
"""
from __future__ import annotations

import base64
import io
import json
import sys
from collections import defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))

import numpy as np
import streamlit as st
import streamlit.components.v1 as components
import torch
from PIL import Image
from safetensors import safe_open
from sklearn.decomposition import PCA

MODEL_ID = "openai/clip-vit-base-patch32"
CORPUS_DIR = HERE / ".corpus"
SAMPLE = 8_000
SPRITE = 32


# ---------- corpus IO -------------------------------------------------

@st.cache_resource(show_spinner="loading corpus")
def load_corpus():
    """Returns dict with: mean [N, 512] np, thumbs [N, 32, 32, 4] uint8 np,
    views [N, 12, 512] torch on `device`, bbox_diag [N] float32 np | None,
    paths list[str], device str, bg_rgb tuple[int,int,int].
    """
    device = "cuda" if torch.cuda.is_available() else "cpu"
    with safe_open(str(CORPUS_DIR / "corpus.safetensors"),
                   framework="pt", device="cpu") as f:
        mean   = f.get_tensor("mean").numpy()
        thumbs = f.get_tensor("thumbs").numpy()
        bbox_diag = f.get_tensor("bbox_diag").numpy() if "bbox_diag" in f.keys() else None
        meta   = json.loads(f.metadata()["meta"])
    # Per-view tensor stays in torch (CLIP scoring uses einsum on it).
    with safe_open(str(CORPUS_DIR / "corpus.safetensors"),
                   framework="pt", device=device) as f:
        views = f.get_tensor("views")
    bg_rgb = tuple(meta.get("bg_rgb", [128, 128, 128]))
    return dict(mean=mean, thumbs=thumbs, views=views,
                bbox_diag=bbox_diag, paths=meta["paths"],
                device=device, bg_rgb=bg_rgb)


# ---------- CLIP search ----------------------------------------------

@st.cache_resource(show_spinner="loading CLIP")
def load_clip(model_id: str, device: str):
    # transformers v5 spams "[transformers] Accessing __path__ from
    # .models.<x>.image_processing_<y>" deprecation messages on lazy
    # discovery of every image processor. Quiet it before importing.
    from transformers.utils import logging as hf_logging
    hf_logging.set_verbosity_error()
    from transformers import CLIPModel, CLIPProcessor
    model = CLIPModel.from_pretrained(model_id).to(device).eval()
    proc  = CLIPProcessor.from_pretrained(model_id)
    return model, proc


@st.cache_resource(show_spinner="loading rembg")
def load_rembg():
    from rembg import new_session
    return new_session("u2net")


def encode_text(prompt: str, device: str) -> torch.Tensor:
    model, proc = load_clip(MODEL_ID, device)
    inp = proc(text=[prompt], return_tensors="pt", padding=True).to(device)
    with torch.no_grad():
        f = model.get_text_features(**inp).pooler_output
    return (f / f.norm(dim=-1, keepdim=True))[0]


def encode_image(img: Image.Image, device: str) -> torch.Tensor:
    model, proc = load_clip(MODEL_ID, device)
    inp = proc(images=img.convert("RGB"), return_tensors="pt").to(device)
    with torch.no_grad():
        f = model.get_image_features(**inp).pooler_output
    return (f / f.norm(dim=-1, keepdim=True))[0]


def remove_bg_to_corpus_grey(img: Image.Image, bg_rgb: tuple[int, int, int]) -> Image.Image:
    from rembg import remove
    cut = remove(img.convert("RGBA"), session=load_rembg())
    bg = Image.new("RGB", cut.size, bg_rgb)
    bg.paste(cut, mask=cut.split()[3])
    return bg


@st.cache_data(show_spinner=False)
def disk_thumb_alpha(vfs_path: str, bg_rgb: tuple[int, int, int]) -> Image.Image:
    """Load <CORPUS_DIR>/thumbnails/<vfs_path>.png and return an RGBA copy
    with corpus-grey pixels (within tolerance 1) punched to alpha 0.
    Cached per path so the grid doesn't redo this every rerun.
    """
    rgb = np.asarray(Image.open(CORPUS_DIR / "thumbnails" / f"{vfs_path}.png").convert("RGB"))
    bg = np.array(bg_rgb, dtype=np.int16)
    diff = np.abs(rgb.astype(np.int16) - bg).max(axis=-1)
    alpha = np.where(diff <= 1, 0, 255).astype(np.uint8)
    return Image.fromarray(np.concatenate([rgb, alpha[..., None]], axis=-1), mode="RGBA")


def top_k_indices(views: torch.Tensor, q: torch.Tensor, k: int,
                  pool: np.ndarray | None = None) -> tuple[np.ndarray, np.ndarray]:
    """views [N, V, 512], q [512] -> max over V -> top-K (corpus indices, scores).

    If `pool` is given, scoring is restricted to those corpus indices and
    the returned indices are still in the corpus's own coordinate system
    (caller doesn't need to re-map).
    """
    q = q.to(views.device)
    if pool is not None and pool.size > 0:
        idx_t = torch.as_tensor(pool, dtype=torch.long, device=views.device)
        sims = (views[idx_t] @ q).amax(dim=-1).cpu()        # [|pool|]
        vals, local = torch.topk(sims, min(k, sims.shape[0]))
        return pool[local.numpy()], vals.numpy()
    sims = (views @ q).amax(dim=-1).cpu()                   # [N]
    vals, idxs = torch.topk(sims, min(k, sims.shape[0]))
    return idxs.numpy(), vals.numpy()


def category_of(path: str) -> str:
    return path.split("/", 1)[0] if "/" in path else "_root"


@st.cache_data(show_spinner=False)
def all_categories(_paths: list[str]) -> list[str]:
    return sorted({category_of(p) for p in _paths})


def filter_pool(paths: list[str], cats_keep: tuple[str, ...], substr: str) -> np.ndarray:
    """Indices of paths matching the user's category + substring filter."""
    s = substr.lower()
    keep = []
    for i, p in enumerate(paths):
        if category_of(p) not in cats_keep:
            continue
        if s and s not in p.lower():
            continue
        keep.append(i)
    return np.array(keep, dtype=np.int64)


# Per-axis source: PCA component on the CLIP `mean` features, or the
# mesh bbox diagonal (linear or log-scaled). Each axis is independently
# choosable so you can mix shape-similarity (PCA) with size (bbox).
AXIS_PCA_1, AXIS_PCA_2, AXIS_PCA_3 = "PCA-1", "PCA-2", "PCA-3"
AXIS_BBOX     = "bbox"
AXIS_BBOX_LOG = "bbox (log)"
AXIS_OPTIONS  = [AXIS_PCA_1, AXIS_PCA_2, AXIS_PCA_3, AXIS_BBOX, AXIS_BBOX_LOG]
AXIS_BBOX_OPTS = (AXIS_BBOX, AXIS_BBOX_LOG)


@st.cache_data(show_spinner="stratified sample + PCA")
def project_3d(_mean: np.ndarray, _paths: list[str], _bbox_diag: np.ndarray | None,
               pool_key: tuple[int, ...], sample_size: int | None,
               seed: int, axes: tuple[str, str, str]) -> tuple[np.ndarray, np.ndarray]:
    """Sample (stratified per category) within `pool_key`, project to 3D.

    Each output axis is built from the source named in `axes`: a PCA
    component (computed once over the sampled subset) or log(bbox_diag).
    All axes are individually centered + scaled so they share the same
    visible range (~[-100, 100]) regardless of source.
    """
    pool = np.array(pool_key, dtype=np.int64)
    if pool.size == 0:
        return np.zeros((0, 3), dtype=np.float32), pool

    if sample_size is None or sample_size >= pool.size:
        idx = pool
    else:
        cats: dict[str, list[int]] = defaultdict(list)
        for i in pool.tolist():
            cats[category_of(_paths[i])].append(i)
        rng = np.random.default_rng(seed)
        n_pool = pool.size
        quota = {c: max(1, round(sample_size * len(v) / n_pool)) for c, v in cats.items()}
        picked: list[int] = []
        for c, ids in cats.items():
            k = min(quota[c], len(ids))
            picked.extend(rng.choice(ids, size=k, replace=False).tolist())
        idx = np.array(sorted(picked[:sample_size]))

    # Sources: build only what the user asked for so we don't pay for
    # PCA when no PCA axis is selected, and don't blow up when bbox is
    # missing but only PCA axes are selected.
    pcs_needed = max(
        (int(a.split("-")[1]) for a in axes if a.startswith("PCA-")),
        default=0,
    )
    pcs = (PCA(n_components=pcs_needed, random_state=0).fit_transform(_mean[idx])
           if pcs_needed else None)

    bbox_lin = bbox_log = None
    if any(a in AXIS_BBOX_OPTS for a in axes):
        assert _bbox_diag is not None, (
            "a 'bbox' axis was selected but corpus has no bbox_diag tensor; "
            "run add_bbox.py."
        )
        diag = _bbox_diag[idx].astype(np.float32)
        # A small fraction of meshes have non-finite or zero bboxes from
        # degenerate parses. Clamp to the [0.1%, 99.9%] range of the
        # finite positives so the axis isn't squashed by a single inf
        # outlier (and log() stays bounded).
        finite_pos = diag[np.isfinite(diag) & (diag > 0)]
        lo, hi = (np.percentile(finite_pos, 0.1), np.percentile(finite_pos, 99.9)) \
            if finite_pos.size else (1e-3, 1.0)
        diag = np.where(np.isfinite(diag) & (diag > 0), diag, lo)
        diag = np.clip(diag, lo, hi)
        bbox_lin = diag
        bbox_log = np.log(diag)

    cols: list[np.ndarray] = []
    for a in axes:
        if a.startswith("PCA-"):
            cols.append(pcs[:, int(a.split("-")[1]) - 1])  # 1-indexed in label
        elif a == AXIS_BBOX:
            cols.append(bbox_lin)
        elif a == AXIS_BBOX_LOG:
            cols.append(bbox_log)
        else:
            raise ValueError(f"unknown axis: {a}")

    xyz = np.stack(cols, axis=-1).astype(np.float32)
    # Per-axis center+scale -> same visible range so all combinations
    # frame nicely in the orbit camera.
    xyz -= xyz.mean(0)
    scale = xyz.std(0)
    xyz /= np.where(scale > 1e-6, scale, 1.0)
    xyz *= 50.0
    return xyz, idx


@st.cache_data(show_spinner="encoding thumbnails")
def thumb_urls(_thumbs: np.ndarray, idx_key: tuple[int, ...]) -> list[str]:
    """Encode each [32, 32, 4] uint8 RGBA thumb as a PNG data URL.

    The corpus's `thumbs` tensor is already alpha-aware (built by
    refine_thumbs.py / build_corpus.py from a full-resolution alpha
    mask, then LANCZOS-downsampled with the alpha channel along for
    the ride). No bg masking needed here -- transparency is intrinsic.
    """
    assert _thumbs.shape[-1] == 4, (
        f"viz.py expects RGBA thumbs; got shape {_thumbs.shape}. "
        f"Run refine_thumbs.py to upgrade the corpus."
    )
    urls: list[str] = []
    for i in idx_key:
        buf = io.BytesIO()
        Image.fromarray(_thumbs[i], mode="RGBA").save(buf, format="PNG", optimize=False)
        urls.append("data:image/png;base64," + base64.b64encode(buf.getvalue()).decode("ascii"))
    return urls


# ---------- HTML/JS template -----------------------------------------

HTML_TEMPLATE = """\
<!DOCTYPE html>
<html><head>
<meta charset="utf-8">
<!-- vh/vw units need this in srcdoc iframes -- without it 100vh resolves to 0,
     #deck-root collapses, deck.gl gets a 300x150 default canvas and luma.gl
     fails to initialise. -->
<meta name="viewport" content="width=device-width, initial-scale=1">
<style>
  html, body { margin: 0; padding: 0; height: 100%; width: 100%; overflow: hidden; background: #1a1a1a; }
  #deck-root { position: absolute; inset: 0; }
  canvas { display: block; }
  .deck-tooltip {
    background: rgba(0,0,0,0.85) !important; color: #ddd !important;
    font: 12px monospace !important; padding: 4px 8px !important;
    border-radius: 3px !important; pointer-events: none;
  }
</style></head><body><div id="deck-root"></div>
<script src="https://unpkg.com/deck.gl@9.1.0/dist.min.js"></script>
<script>
  const DATA = __DATA__;
  const SPRITE_SCALE = __SPRITE_SCALE__;
  const __H = __HEIGHT__;
  const root = document.getElementById('deck-root');

  // Poll until the iframe layout has settled and the root div has real
  // pixel dimensions. Hardcoding via __H is the belt: deck still gets
  // a sane size even if the layout race never resolves.
  function ready() { return root.clientWidth > 50 && root.clientHeight > 50; }

  function start() {
    // Poll until layout has assigned the iframe a non-zero size. In headless
    // chromium the layout race can take several frames after `load` before
    // window.innerWidth/innerHeight resolve to anything but 0.
    const w = Math.max(window.innerWidth, root.clientWidth, 100);
    const h = Math.max(window.innerHeight, root.clientHeight, __H);
    if (w < 50 || h < 50) {
      requestAnimationFrame(start);
      return;
    }
    root.style.width  = w + 'px';
    root.style.height = h + 'px';
    console.log('viz: starting Deck at', w, 'x', h);

    new deck.Deck({
      parent: root,
      width: w,
      height: h,
      views: new deck.OrbitView({orbitAxis: 'Y', fov: 50}),
      controller: true,
      initialViewState: {target: [0, 0, 0], zoom: 1, rotationX: 20, rotationOrbit: 30, minZoom: -3, maxZoom: 6},
      layers: [
        new deck.IconLayer({
          id: 'icons',
          data: DATA,
          getPosition: d => d.position,
          getIcon: d => d.icon,
          // Sprites in CARTESIAN world units, NOT screen pixels: zoom in
          // -> sprites grow with the rest of the cloud (proper 3D feel).
          // Min/max-pixel caps keep them legible at extreme zoom.
          getSize: SPRITE_SCALE,
          sizeUnits: 'common',
          sizeMinPixels: 6,
          sizeMaxPixels: 96,
          billboard: true,
          coordinateSystem: deck.COORDINATE_SYSTEM.CARTESIAN,
          pickable: true,
          // 32x32 source upscaled with linear filter is mushy. Nearest
          // gives sharp pixel-art-style sprites at any zoom level.
          textureParameters: {
            minFilter: 'nearest',
            magFilter: 'nearest',
            mipmapFilter: 'nearest',
          },
        }),
      ],
      getTooltip: ({object}) => object && {text: object.path, className: 'deck-tooltip'},
    });
  }

  if (document.readyState === 'complete') start();
  else window.addEventListener('load', start);
</script></body></html>
"""


# ---------- UI --------------------------------------------------------

st.set_page_config(page_title="CD mesh embeddings 3D", layout="wide")
st.title("Crimson Desert -- mesh embeddings 3D")

if not (CORPUS_DIR / "corpus.safetensors").is_file():
    st.error(f"no corpus at `{CORPUS_DIR}`. Run build_corpus.py first.")
    st.stop()

c = load_corpus()
mean, thumbs, views_tensor, bbox_diag, paths = (
    c["mean"], c["thumbs"], c["views"], c["bbox_diag"], c["paths"])
device, bg_rgb = c["device"], c["bg_rgb"]
N = mean.shape[0]
cats = all_categories(paths)

if "viz_seed" not in st.session_state:
    st.session_state["viz_seed"] = 0

axis_choices = AXIS_OPTIONS if bbox_diag is not None else AXIS_OPTIONS[:3]

with st.sidebar:
    st.markdown(f"**corpus** {N:,} meshes  (device `{device}`)")

    st.markdown("**search**  (top-K becomes the 3D pool)")
    mode = st.radio("query type", ["off", "text", "image"], horizontal=True,
                    label_visibility="collapsed")
    text_q = ""; img_q_bytes = None; rm_bg = True
    if mode == "text":
        text_q = st.text_input("prompt", value="a wooden bucket")
    elif mode == "image":
        f = st.file_uploader("image", type=["png", "jpg", "jpeg", "webp", "bmp"])
        rm_bg = st.checkbox("remove background", value=True)
        if f is not None:
            img_q_bytes = f.getvalue()
    top_k = st.slider("top-K", 50, 5000, 500, step=50)

    st.markdown("---")
    st.markdown("**filter**  (applied when search is off)")
    cats_keep = st.multiselect("categories", cats, default=cats)
    substr = st.text_input("path contains", value="",
                           placeholder="e.g. bucket, horse, sword")

    st.markdown("---")
    st.markdown("**3D**")
    sample = st.slider("sample size", 500, min(N, 30_000), SAMPLE, step=500)
    sprite_scale = st.slider("sprite size on screen", 1, 30, 6)
    ax_x = st.selectbox("X axis", axis_choices, index=0, key="ax_x")
    ax_y = st.selectbox("Y axis", axis_choices, index=1, key="ax_y")
    default_z = AXIS_BBOX_LOG if bbox_diag is not None else AXIS_PCA_3
    ax_z = st.selectbox("Z axis", axis_choices,
                        index=axis_choices.index(default_z), key="ax_z")
    if st.button("reroll sample"):
        st.session_state["viz_seed"] += 1
    if bbox_diag is None:
        st.caption("(no bbox_diag yet -- run `add_bbox.py` to enable the bbox axes)")


# The category+substring filter applies in both modes. When a search
# query is active, top-K runs *within* that filter so 'image of a horse'
# limited to category=character/ doesn't get diluted by similar-looking
# objects elsewhere.
filter_idx = filter_pool(paths, tuple(cats_keep), substr)
query_active = (mode == "text" and text_q.strip()) or (mode == "image" and img_q_bytes)
search_scores: dict[int, float] = {}
if query_active:
    if filter_idx.size == 0:
        st.warning("filter excludes everything; nothing to search.")
        st.stop()
    if mode == "text":
        q_vec = encode_text(text_q.strip(), device)
        st.sidebar.caption(f"text query: {text_q!r}")
    else:
        img = Image.open(io.BytesIO(img_q_bytes))
        if rm_bg:
            img = remove_bg_to_corpus_grey(img, bg_rgb)
        q_vec = encode_image(img, device)
    pool_idx, pool_scores = top_k_indices(views_tensor, q_vec, top_k, filter_idx)
    pool = pool_idx.astype(np.int64)
    search_scores = {int(i): float(s) for i, s in zip(pool_idx, pool_scores)}
    st.sidebar.caption(
        f"top-{len(pool)} of {filter_idx.size:,} filtered  --  best cos = {pool_scores[0]:.3f}"
    )
else:
    pool = filter_idx
    st.sidebar.caption(f"matches filter: {pool.size:,}")

if pool.size == 0:
    st.warning("no meshes match")
    st.stop()

xyz, idx = project_3d(mean, paths, bbox_diag, tuple(pool.tolist()),
                     min(sample, pool.size), st.session_state["viz_seed"],
                     (ax_x, ax_y, ax_z))
urls = thumb_urls(thumbs, tuple(idx.tolist()))

data = [
    {"position": xyz[k].tolist(),
     "icon": {"url": urls[k], "width": SPRITE, "height": SPRITE, "anchorY": SPRITE // 2},
     "path": paths[i]}
    for k, i in enumerate(idx.tolist())
]

VIZ_HEIGHT = 720
html = (HTML_TEMPLATE
        .replace("__DATA__", json.dumps(data))
        # 2x scale-factor: the slider value is doubled before being
        # handed to deck.gl, so a slider tick of 6 renders as 12 px.
        .replace("__SPRITE_SCALE__", str(sprite_scale * 2))
        .replace("__HEIGHT__", str(VIZ_HEIGHT)))

if query_active:
    if mode == "image" and img_q_bytes is not None:
        c1, c2 = st.columns([1, 5])
        with c1:
            st.image(img if rm_bg else Image.open(io.BytesIO(img_q_bytes)),
                     caption="query")
        with c2:
            components.html(html, height=VIZ_HEIGHT, scrolling=False)
    else:
        components.html(html, height=VIZ_HEIGHT, scrolling=False)
else:
    components.html(html, height=VIZ_HEIGHT, scrolling=False)

st.caption(
    f"showing {len(idx):,} of {pool.size:,} ({N:,} total)  --  "
    f"axes: X={ax_x}  Y={ax_y}  Z={ax_z}.  "
    f"left-drag to orbit, right-drag to pan, scroll to zoom."
)


# ---------- top-10 full-resolution thumbnail grid --------------------

if query_active:
    GRID_N, COLS = 10, 5
    top = pool[:GRID_N].tolist()
    st.markdown(f"### top-{len(top)} matches")
    for row in range(0, len(top), COLS):
        cols = st.columns(COLS)
        for col, i in zip(cols, top[row:row + COLS]):
            with col:
                full = CORPUS_DIR / "thumbnails" / f"{paths[i]}.png"
                if full.is_file():
                    st.image(disk_thumb_alpha(paths[i], bg_rgb),
                             use_container_width=True)
                else:
                    # Fall back to the in-corpus 32x32 RGBA thumb.
                    st.image(thumbs[i], use_container_width=True)
                st.caption(f"`{paths[i]}`  \ncos = {search_scores[int(i)]:.3f}")
