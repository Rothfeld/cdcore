"""Headless OpenGL textured-mesh rasterizer via moderngl + EGL.

Replaces the Blender batched render. We don't need PBR shading, shadow
maps, or world environment -- we need a textured raster. moderngl gives
us that path with a few hundred lines and ~100-1000x the throughput.

Public surface:

    r = MeshRenderer(width=224, height=224)
    rgb_array = r.render_views(
        vertices=np.float32 [V, 3],
        faces=np.uint32   [F, 3],
        uvs=np.float32    [V, 2] | None,
        normals=np.float32 [V, 3] | None,
        texture_rgba=np.uint8 [H, W, 4] | None,
        view_directions=np.float32 [N, 3],
        bbox_center=(cx, cy, cz),
        bbox_diag=float,
        distance_factor=1.6,
        fov_deg=45.0,
    )
    # rgb_array: np.uint8 [N, height, width, 3]
"""
from __future__ import annotations

import math
import numpy as np
import moderngl


VERT_SHADER = """
#version 330

uniform mat4 mvp;
uniform mat4 model;

in vec3 in_position;
in vec3 in_normal;
in vec2 in_uv;

out vec3 v_normal_ws;
out vec2 v_uv;

void main() {
    gl_Position = mvp * vec4(in_position, 1.0);
    // Pass world-space normal to fragment for cheap directional shading.
    v_normal_ws = mat3(model) * in_normal;
    v_uv = in_uv;
}
"""

# Shader does N.L Lambert + soft ambient, all in linear space, then
# applies a sRGB encode on the way out so the resulting PNG looks like
# what AgX-tonemapped Blender produced on a low-light scene.
FRAG_SHADER = """
#version 330

uniform sampler2D u_tex;
uniform int u_has_texture;
uniform vec3 u_light_dir;       // unit, world space
uniform float u_ambient;        // 0..1
uniform vec3 u_default_color;
uniform vec3 u_tint;            // multiplied with albedo (linear), default (1,1,1)

in vec3 v_normal_ws;
in vec2 v_uv;

out vec4 frag_color;

vec3 srgb_decode(vec3 c) {
    return mix(c / 12.92,
               pow((c + 0.055) / 1.055, vec3(2.4)),
               step(0.04045, c));
}

vec3 srgb_encode(vec3 linear) {
    return mix(linear * 12.92,
               1.055 * pow(linear, vec3(1.0 / 2.4)) - 0.055,
               step(0.0031308, linear));
}

void main() {
    vec3 albedo;
    if (u_has_texture == 1) {
        // No V flip: cdcore.decode_dds_to_rgba returns rows top-to-bottom
        // and moderngl's ctx.texture stores them in the same order, so
        // (u, v=0) reads from the top of the DDS image -- which matches
        // Pearl Abyss's own UV convention. A V flip here scrambles the
        // texture into a patchwork; verified via debug_uv.py.
        vec4 t = texture(u_tex, v_uv);
        albedo = srgb_decode(t.rgb);
    } else {
        albedo = u_default_color;
    }
    // Tint comes from the .pac_xml / .pami uniform (e.g. _hairDyeingColor
    // for hair, _baseColor / _tintColor for most others). It's the
    // dominant signal that turns grey hair-card textures into actual
    // hair colour, brown leather into brown leather, etc.
    albedo *= u_tint;

    vec3 n = normalize(v_normal_ws);
    float ndotl = max(dot(n, normalize(u_light_dir)), 0.0);
    float lambert = u_ambient + (1.0 - u_ambient) * ndotl;
    vec3 lit = albedo * lambert;
    frag_color = vec4(srgb_encode(lit), 1.0);
}
"""


def look_at(eye, center, up=(0.0, 1.0, 0.0)):
    eye = np.asarray(eye, dtype=np.float32)
    center = np.asarray(center, dtype=np.float32)
    up = np.asarray(up, dtype=np.float32)
    f = center - eye
    f /= np.linalg.norm(f) + 1e-8
    s = np.cross(f, up)
    s_norm = np.linalg.norm(s)
    if s_norm < 1e-6:
        # eye direction parallel to up; pick a different up.
        up = np.array([1.0, 0.0, 0.0], dtype=np.float32)
        s = np.cross(f, up)
        s_norm = np.linalg.norm(s)
    s /= s_norm
    u = np.cross(s, f)
    m = np.eye(4, dtype=np.float32)
    m[0, :3] = s
    m[1, :3] = u
    m[2, :3] = -f
    m[0, 3] = -np.dot(s, eye)
    m[1, 3] = -np.dot(u, eye)
    m[2, 3] = np.dot(f, eye)
    return m


def perspective(fov_deg, aspect, znear, zfar):
    f = 1.0 / math.tan(math.radians(fov_deg) / 2.0)
    m = np.zeros((4, 4), dtype=np.float32)
    m[0, 0] = f / aspect
    m[1, 1] = f
    m[2, 2] = (zfar + znear) / (znear - zfar)
    m[2, 3] = (2.0 * zfar * znear) / (znear - zfar)
    m[3, 2] = -1.0
    return m


def compute_smooth_normals(vertices, faces):
    """Per-vertex normals by summing adjacent face normals."""
    normals = np.zeros_like(vertices, dtype=np.float32)
    v0 = vertices[faces[:, 0]]
    v1 = vertices[faces[:, 1]]
    v2 = vertices[faces[:, 2]]
    fn = np.cross(v1 - v0, v2 - v0)
    flen = np.linalg.norm(fn, axis=1, keepdims=True)
    fn = np.where(flen > 1e-8, fn / flen, fn)
    np.add.at(normals, faces[:, 0], fn)
    np.add.at(normals, faces[:, 1], fn)
    np.add.at(normals, faces[:, 2], fn)
    nlen = np.linalg.norm(normals, axis=1, keepdims=True)
    return np.where(nlen > 1e-8, normals / nlen, np.array([0, 1, 0], dtype=np.float32))


class MeshRenderer:
    def __init__(self, width=224, height=224):
        self.width = width
        self.height = height
        self.ctx = moderngl.create_context(standalone=True, require=330, backend="egl")
        self.ctx.enable(moderngl.DEPTH_TEST)
        self.ctx.front_face = "ccw"
        # Disable backface culling -- many CD meshes have inconsistent
        # winding; we'd lose polys with cull on.
        self.prog = self.ctx.program(vertex_shader=VERT_SHADER, fragment_shader=FRAG_SHADER)
        # Default uniforms.
        self.prog["u_light_dir"].value = (0.5, 0.7, 0.5)
        self.prog["u_ambient"].value = 0.35
        self.prog["u_default_color"].value = (0.55, 0.55, 0.55)
        self.prog["u_tint"].value = (1.0, 1.0, 1.0)
        # Frame buffer.
        self.color_tex = self.ctx.texture((width, height), 4)
        self.depth_buf = self.ctx.depth_renderbuffer((width, height))
        self.fbo = self.ctx.framebuffer(color_attachments=[self.color_tex],
                                        depth_attachment=self.depth_buf)

    def release(self):
        self.fbo.release()
        self.color_tex.release()
        self.depth_buf.release()
        self.prog.release()
        self.ctx.release()

    def render_views(self,
                     submeshes: list[dict],
                     view_directions: np.ndarray,
                     bbox_center,
                     bbox_diag: float,
                     distance_factor: float = 1.6,
                     fov_deg: float = 45.0,
                     bg_color=(0.5, 0.5, 0.5, 1.0)) -> np.ndarray:
        """Render `submeshes` from each direction in `view_directions`.

        Each entry in `submeshes` is a dict:
            vertices : np.float32 [V, 3]    required
            faces    : np.uint32  [F, 3]    required
            uvs      : np.float32 [V, 2]    optional
            normals  : np.float32 [V, 3]    optional (computed if None)
            texture  : np.uint8   [H, W, 4] optional (flat grey if None)

        Each submesh becomes its own draw call. That's the only sane way
        to give a multi-material mesh (e.g. a fence with separate wood,
        nail, and rope textures) the right texture per part.
        """
        ctx = self.ctx
        prog = self.prog

        # Build per-submesh GL state once; reused across all views.
        gl_subs = []
        for sm in submeshes:
            v = np.asarray(sm["vertices"], dtype=np.float32)
            f = np.asarray(sm["faces"], dtype=np.uint32).reshape(-1, 3)
            if v.size == 0 or f.size == 0:
                continue
            uvs = sm.get("uvs")
            uvs = np.asarray(uvs, dtype=np.float32) if uvs is not None and len(uvs) == len(v) else np.zeros((len(v), 2), np.float32)
            n = sm.get("normals")
            n = np.asarray(n, dtype=np.float32) if n is not None and len(n) == len(v) else compute_smooth_normals(v, f)
            interleaved = np.concatenate([v, n, uvs], axis=1)
            vbo = ctx.buffer(interleaved.tobytes())
            ibo = ctx.buffer(f.tobytes())
            vao = ctx.vertex_array(prog, [(vbo, "3f 3f 2f", "in_position", "in_normal", "in_uv")], ibo)

            tex = sm.get("texture")
            gl_tex = None
            if tex is not None:
                tex = np.ascontiguousarray(tex, dtype=np.uint8)
                h, w = tex.shape[:2]
                gl_tex = ctx.texture((w, h), 4, tex.tobytes())
                gl_tex.repeat_x = True
                gl_tex.repeat_y = True
                try:
                    gl_tex.build_mipmaps()
                except Exception:
                    pass
            tint = sm.get("tint") or (1.0, 1.0, 1.0, 1.0)
            tint_rgb = (float(tint[0]), float(tint[1]), float(tint[2]))
            gl_subs.append((vao, vbo, ibo, gl_tex, tint_rgb))

        diag = max(float(bbox_diag), 1e-3)
        distance = diag * distance_factor
        proj = perspective(fov_deg, self.width / self.height, distance * 0.05, distance * 4.0)
        center = np.asarray(bbox_center, dtype=np.float32)

        out = np.zeros((len(view_directions), self.height, self.width, 3), dtype=np.uint8)
        model = np.eye(4, dtype=np.float32)

        for i, direction in enumerate(view_directions):
            d = np.asarray(direction, dtype=np.float32)
            d = d / (np.linalg.norm(d) + 1e-8)
            eye = center + d * distance
            view = look_at(eye, center)
            mvp = proj @ view @ model

            self.fbo.use()
            ctx.viewport = (0, 0, self.width, self.height)
            ctx.clear(*bg_color, depth=1.0)
            prog["mvp"].write(mvp.T.tobytes())
            prog["model"].write(model.T.tobytes())

            for vao, _, _, gl_tex, tint_rgb in gl_subs:
                if gl_tex is not None:
                    gl_tex.use(location=0)
                    prog["u_tex"].value = 0
                    prog["u_has_texture"].value = 1
                else:
                    prog["u_has_texture"].value = 0
                prog["u_tint"].value = tint_rgb
                vao.render(moderngl.TRIANGLES)

            data = self.fbo.read(components=3, alignment=1)
            img = np.frombuffer(data, dtype=np.uint8).reshape(self.height, self.width, 3)
            out[i] = img[::-1, :, :]

        for vao, vbo, ibo, gl_tex, _ in gl_subs:
            vao.release()
            vbo.release()
            ibo.release()
            if gl_tex is not None:
                gl_tex.release()

        return out
