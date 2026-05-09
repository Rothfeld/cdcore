# mesh_search -- CLIP-based search over the CD mesh corpus

Two scripts. Tunables are constants at the top of each file.

## Install

```bash
pip install -r crates/scripts/mesh_search/requirements.txt
```

Also needs the in-tree `cdcore` python module and the Crimson Desert
package directory mounted somewhere on the host. In this dev container
it's at `/cd`; substitute your own path everywhere `/cd` appears below.

## Build the corpus

```bash
python3 crates/scripts/mesh_search/build_corpus.py /cd
```

Output goes to `.corpus/` next to `build_corpus.py` (`CORPUS_DIR` constant).
Walks `<game_install_dir>` (`/cd` in the example, but it's whatever path
you pass as the CLI argument), parses every PAM/PAMLOD/PAC, renders 12
fibonacci-sphere views (224x224, moderngl) plus 1 hand-picked thumbnail
angle (front-3/4 catalog pose, `THUMB_DIR`), CLIP-encodes the 12 views,
writes:

  - `corpus.safetensors`
    - tensor `views`     `[N, 12, 512]`    -- L2-normed per-view CLIP features
    - tensor `mean`      `[N, 512]`        -- mean-pool of `views`, L2-normed
    - tensor `thumbs`    `[N, 32, 32, 4]`  -- uint8 RGBA, alpha-aware (corpus
                                              grey punched to alpha 0 at full
                                              res then LANCZOS-downsampled)
    - tensor `bbox_diag` `[N]`             -- mesh bbox diagonal, used as 3D
                                              axis source in viz.py
    - `metadata["meta"]` -- JSON: paths, game_install_dir, model_id,
                            n_views, resolution, thumb_size, bg_rgb,
                            thumb_format
  - `thumbnails/<vfs_path>.png` -- one 224x224 PNG per mesh, VFS-tree layout

~31 min on RTX-class GPU for the ~96k mesh corpus.

## Visualize + search (Streamlit UI)

```bash
streamlit run crates/scripts/mesh_search/viz.py
```

Single app: 3D embedding explorer + text/image CLIP search.

**Sidebar layout:**

  - **search**:
      - radio: `off` / `text` / `image`
      - text mode: prompt input
      - image mode: upload + "remove background" toggle (rembg + composite
        onto the corpus grey so the query matches the rendering distribution)
      - **top-K** slider: when search is on, K becomes the 3D pool size
  - **filter** (always applied; search runs *within* the filter):
      - category multiselect (`character/`, `object/`, `leveldata/`, ...)
      - path substring (`bucket`, `horse`, `sword`, ...)
  - **3D**:
      - sample size (stratified per category within the pool)
      - sprite size (slider value x 2 -> CARTESIAN world units)
      - X/Y/Z axis source: `PCA-1`, `PCA-2`, `PCA-3`, `bbox`, `bbox (log)`
      - "reroll sample" -- different random seed for the stratified sample

**Scoring**: `max_v cos(query, view_v)` over the per-view tensor -- a
single angle of the corpus mesh can match the query directly instead of
being diluted into the mean-pooled prototype.

**Top-10 grid** under the 3D view: full-resolution disk thumbnails of
the top-10 search results (with corpus-grey punched to alpha so the
streamlit theme bg shows through), each captioned with its VFS path and
cosine score.

**Render details**: deck.gl `IconLayer` in an `OrbitView` (CARTESIAN
coords, billboard sprites). Sprites in world units with min/max-pixel
caps so they grow on zoom-in but stay legible at any zoom. Nearest-
neighbour texture filtering for sharp pixel-art-style sprites.

First image query downloads `~/.u2net/u2net.onnx` (~170 MB) on demand.

## Files

```
crates/scripts/mesh_search/
├── build_corpus.py      # corpus builder
├── viz.py               # streamlit search + 3D explorer
├── requirements.txt
└── _lib/
    ├── raster.py        # moderngl rasterizer (EGL + GLSL 330)
    ├── sidecar.py       # .pac_xml / .pami texture resolver
    └── mesh_pipeline.py # build_submeshes + mesh_bbox + fibonacci_sphere
```
