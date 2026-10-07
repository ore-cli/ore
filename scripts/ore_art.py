#!/usr/bin/env python3
"""
ore — a rotating ASCII crystal for your terminal.

A tiny software renderer with no dependencies: it builds a cluster of
hexagonal quartz points in 3D, spins them, and rasterizes to characters using
a glass shading model (edge-on fresnel + specular + traced facet edges), so
the thing reads as a crystal rather than a blob.

    scripts/ore_art.py                   # spin forever
    scripts/ore_art.py --once            # one frame, then exit
    scripts/ore_art.py --no-color        # plain ASCII
    scripts/ore_art.py --dump 24         # N frames, for baking into a spinner
    scripts/ore_art.py --label "" --ss 3 # crystal only, higher quality

Ctrl-C to quit.
"""

from __future__ import annotations

import argparse
import math
import os
import shutil
import signal
import sys
import time

RAMPS = {
    "blocks": " .:-=+*#%@",
    "fine": " .'`^\",:;!i~+?][}{1)(|/tfjrxnuvczXYUJCLQ0OZmwqpdbkhao*#MW&8%B@",
    "shade": " .:-=+*░▒▓█",
    "mineral": " .·:-=+≡*#%▓█",
}

# --------------------------------------------------------------------------
# vector / matrix helpers (plain tuples, no numpy)
# --------------------------------------------------------------------------


def sub(a, b):
    return (a[0] - b[0], a[1] - b[1], a[2] - b[2])


def cross(a, b):
    return (
        a[1] * b[2] - a[2] * b[1],
        a[2] * b[0] - a[0] * b[2],
        a[0] * b[1] - a[1] * b[0],
    )


def dot(a, b):
    return a[0] * b[0] + a[1] * b[1] + a[2] * b[2]


def norm(a):
    m = math.sqrt(dot(a, a)) or 1.0
    return (a[0] / m, a[1] / m, a[2] / m)


def matmul(m, n):
    return tuple(
        tuple(sum(m[i][k] * n[k][j] for k in range(3)) for j in range(3))
        for i in range(3)
    )


def apply(m, v):
    return (
        m[0][0] * v[0] + m[0][1] * v[1] + m[0][2] * v[2],
        m[1][0] * v[0] + m[1][1] * v[1] + m[1][2] * v[2],
        m[2][0] * v[0] + m[2][1] * v[1] + m[2][2] * v[2],
    )


def rot_x(t):
    c, s = math.cos(t), math.sin(t)
    return ((1, 0, 0), (0, c, -s), (0, s, c))


def rot_y(t):
    c, s = math.cos(t), math.sin(t)
    return ((c, 0, s), (0, 1, 0), (-s, 0, c))


def rot_z(t):
    c, s = math.cos(t), math.sin(t)
    return ((c, -s, 0), (s, c, 0), (0, 0, 1))


# --------------------------------------------------------------------------
# geometry: hexagonal prism + pyramidal termination = one quartz point
# --------------------------------------------------------------------------


def crystal_point(
    radius=1.0, body=2.0, tip=0.9, sides=6, phase=0.0, jitter=0.0, base_arris=False
):
    """Returns (triangles, edges) for one crystal, base at origin, growing +Y."""
    lo, hi = [], []
    for i in range(sides):
        a = phase + 2 * math.pi * i / sides
        r = radius * (1.0 + jitter * math.sin(a * 3.0 + phase * 2.0))
        lo.append((r * math.cos(a), 0.0, r * math.sin(a)))
        hi.append((r * math.cos(a), body, r * math.sin(a)))
    apex = (0.0, body + tip, 0.0)
    base = (0.0, 0.0, 0.0)

    tris, edges = [], []
    for i in range(sides):
        j = (i + 1) % sides
        tris += [
            (lo[i], lo[j], hi[j]),
            (lo[i], hi[j], hi[i]),  # prism wall
            (hi[i], hi[j], apex),  # termination
            (lo[j], lo[i], base),
        ]  # base cap
        edges += [
            (lo[i], hi[i]),  # vertical arris
            (hi[i], hi[j]),  # shoulder ring
            (hi[i], apex),
        ]  # termination arris
        if base_arris:
            edges.append((lo[i], lo[j]))
    return tris, edges


def splay(geo, bearing, lean, dist, y):
    """Seat a crystal on the matrix at `bearing`, leaning *outward* by `lean`.

    The lean direction is derived from the crystal's own position rather than
    being a free parameter, so a point can never end up leaning back across
    the cluster. tilt toward +X, then swing the whole thing to `bearing`.
    """
    tris, edges = geo
    m = matmul(rot_y(bearing), rot_z(-abs(lean)))
    ox, oz = dist * math.cos(bearing), -dist * math.sin(bearing)

    def xf(v):
        v = apply(m, v)
        return (v[0] + ox, v[1] + y, v[2] + oz)

    return ([tuple(xf(v) for v in t) for t in tris], [(xf(a), xf(b)) for a, b in edges])


# --------------------------------------------------------------------------
# geometry: the matrix — the craggy host rock the crystals grow out of
# --------------------------------------------------------------------------


def _noise(i, j, seed):
    """Cheap deterministic hash in [0,1); stable across frames, no RNG state."""
    x = math.sin(i * 12.9898 + j * 78.233 + seed * 37.719) * 43758.5453
    return x - math.floor(x)


def vein(p, seed=1.0, width=0.22, density=1.0):
    """Gold content at a point, 0..1.

    A sine field warped by a second, slower sine — the warp is what stops
    the veins reading as regular stripes. Thin bands of the field near zero
    become the seams. Evaluated in model space, so a vein runs continuously
    across facet boundaries instead of stopping at each edge.
    """
    x, y, z = p
    warp = (
        math.sin(1.9 * x + 0.8 * y + seed) * 0.42
        + math.sin(2.4 * z - 1.2 * y + seed * 1.7) * 0.34
    )
    field = math.sin(3.2 * x + 2.1 * z + 1.7 * y + 2.2 * warp)
    branch = math.sin(5.1 * z - 3.4 * x + 3.0 * warp) * 0.45
    d = min(abs(field), abs(field * 0.55 + branch))
    w = width * density
    if d >= w:
        return 0.0
    k = 1.0 - d / w
    return k * k * (3.0 - 2.0 * k)  # smoothstep, so seams have soft edges


def make_matrix(radius=1.28, rise=0.78, drop=0.70, rings=3, segs=13, seed=3.0):
    """A lumpy, flat-bottomed boulder.

    Returns ([(a, b, c, albedo), ...], [(p, q), ...]) — faces plus the ridge
    lines where facets meet, which is what makes broken rock read as broken.

    Built as a perturbed dome over a flat base — every vertex is pushed
    around by a stable hash, so the rock is irregular but identical frame
    to frame, and each facet gets its own albedo for a grainy, mineral read.
    """
    ringv = []
    for r in range(rings + 1):
        u = r / rings  # 0 at the crown, 1 at the rim
        ring = []
        for sgn in range(segs):
            a = 2 * math.pi * sgn / segs
            bump = _noise(r, sgn, seed)
            rad = radius * math.sin(u * math.pi / 2) * (0.52 + 0.92 * bump)
            hgt = (
                rise
                * math.cos(u * math.pi / 2)
                * (0.16 + 1.30 * _noise(r, sgn, seed + 5))
            )
            ring.append((rad * math.cos(a), hgt, rad * math.sin(a)))
        ringv.append(ring)

    crown = (0.0, rise * (0.86 + 0.2 * _noise(0, 0, seed + 9)), 0.0)
    rim = ringv[-1]
    foot = [
        (v[0] * 0.86, -drop * (0.72 + 0.5 * _noise(9, k, seed + 2)), v[2] * 0.86)
        for k, v in enumerate(rim)
    ]
    base = (0.0, -drop, 0.0)

    faces = []
    for sgn in range(segs):
        nxt = (sgn + 1) % segs
        faces.append((crown, ringv[0][sgn], ringv[0][nxt]))
        for r in range(rings):
            a, b = ringv[r][sgn], ringv[r][nxt]
            c, d = ringv[r + 1][sgn], ringv[r + 1][nxt]
            faces.append((a, b, d))
            faces.append((a, d, c))
        faces.append((rim[sgn], rim[nxt], foot[nxt]))  # sheared flank
        faces.append((rim[sgn], foot[nxt], foot[sgn]))
        faces.append((foot[nxt], base, foot[sgn]))  # underside

    # ridge lines: where facets meet. Broken rock reads as broken because of
    # these, the same way the crystal reads as faceted because of its arrises.
    ridges = []
    for sgn in range(segs):
        nxt = (sgn + 1) % segs
        ridges.append((crown, ringv[0][sgn]))
        for r in range(rings + 1):
            ridges.append((ringv[r][sgn], ringv[r][nxt]))
            if r < rings:
                ridges.append((ringv[r][sgn], ringv[r + 1][sgn]))
        ridges.append((rim[sgn], foot[sgn]))

    ys = [v[1] for f in faces for v in f]
    lo, hi = min(ys), max(ys)
    span = (hi - lo) or 1.0

    def ao(v):
        # light pools on the high ground and dies in the crevices
        return ((v[1] - lo) / span) ** 0.85

    out = []
    for k, (a, b, c) in enumerate(faces):
        albedo = 0.45 + 1.05 * _noise(k, k * 3, seed + 11)
        out.append((a, b, c, albedo, (ao(a), ao(b), ao(c))))
    return out, ridges


# --------------------------------------------------------------------------
# the `ore` mark: a dominant spire and outward-splayed points on a matrix
# --------------------------------------------------------------------------


def build_model(with_matrix=True, matrix_width=1.0):
    parts = [
        # (radius, body, tip, phase, bearing,      lean, dist,  y)
        (0.58, 2.30, 1.00, 0.15, 0.35, 0.05, 0.06, -0.62),
        (0.32, 1.30, 0.55, 0.55, 0.95, 0.42, 0.72, -0.60),
        (0.29, 1.45, 0.52, 0.20, 3.55, 0.36, 0.80, -0.55),
        (0.22, 0.90, 0.40, 2.30, 2.20, 0.62, 0.86, -0.50),
        (0.19, 0.75, 0.34, 1.10, 5.10, 0.66, 0.78, -0.45),
        (0.15, 0.55, 0.28, 3.00, 4.20, 0.80, 0.95, -0.42),
    ]
    tris, edges = [], []
    for rad, body, tip, ph, bearing, lean, dist, y in parts:
        ts, es = splay(
            crystal_point(radius=rad, body=body, tip=tip, phase=ph, jitter=0.06),
            bearing=bearing,
            lean=lean,
            dist=dist,
            y=y,
        )
        tris += ts
        edges += es

    rock, ridges = make_matrix(radius=1.28 * matrix_width) if with_matrix else ([], [])
    SINK = 0.88

    def down(v):
        return (v[0], v[1] - SINK, v[2])

    rock = [(down(a), down(b), down(c), al, ao) for a, b, c, al, ao in rock]
    ridges = [(down(p), down(q)) for p, q in ridges]

    # recentre on the whole model's bounding box, so it spins about its own axis
    pts = [v for t in tris for v in t] + [v for f in rock for v in f[:3]]
    cx = (min(p[0] for p in pts) + max(p[0] for p in pts)) / 2
    cy = (min(p[1] for p in pts) + max(p[1] for p in pts)) / 2
    cz = (min(p[2] for p in pts) + max(p[2] for p in pts)) / 2

    def c(v):
        return (v[0] - cx, v[1] - cy, v[2] - cz)

    tris = [tuple(c(v) for v in t) for t in tris]
    edges = [(c(a), c(b)) for a, b in edges]
    rock = [(c(f[0]), c(f[1]), c(f[2]), f[3], f[4]) for f in rock]
    ridges = [(c(p), c(q)) for p, q in ridges]

    pts = [c(p) for p in pts]
    ry = max(abs(p[1]) for p in pts)
    rxz = max(math.hypot(p[0], p[2]) for p in pts)
    return tris, edges, rock, ridges, ry, rxz


# --------------------------------------------------------------------------
# rendering
# --------------------------------------------------------------------------

LIGHT = norm((-0.45, 0.75, -0.85))  # key light, upper-left, toward the viewer
FILL = norm((0.35, -0.55, -0.75))  # weak bounce, keeps shadowed rock solid
RIM = norm((0.70, 0.15, 0.45))  # cool rim from behind-right


class Renderer:
    def __init__(
        self,
        width,
        height,
        char_aspect=0.5,
        distance=6.5,
        fill=0.84,
        supersample=2,
        ramp=RAMPS["blocks"],
        gain=1.0,
        edge=1.0,
        style="faceted",
        matrix=True,
        matrix_width=1.0,
        gold=1.0,
        vein_seed=1.0,
    ):
        self.w, self.h = width, height
        self.ss = max(1, supersample)
        self.rw, self.rh = width * self.ss, height * self.ss
        self.char_aspect = char_aspect
        self.dist = distance
        self.ramp = ramp
        self.gain = gain
        self.edge = edge
        self.style = style
        self.vein_width = 0.17 * gold
        self.vein_seed = vein_seed
        (self.tris, self.edges, self.rock, self.ridges, ry, rxz) = build_model(
            matrix, matrix_width
        )
        # scale so the model fills `fill` of the tighter axis
        self.f = distance * 0.5 * fill * min(self.rh / ry, self.rw * char_aspect / rxz)

    def project(self, p):
        z = p[2]
        if z < 0.2:
            return None
        k = self.f / z
        return (
            self.rw * 0.5 + p[0] * k / self.char_aspect,
            self.rh * 0.5 - p[1] * k,
            z,
        )

    # ---- one frame -------------------------------------------------------
    def frame(self, t, spin=1.0, wobble=True):
        rw, rh = self.rw, self.rh
        n_px = rw * rh
        surf = [0.0] * n_px  # shade of the nearest visible facet
        zbuf = [1e30] * n_px  # depth of that facet
        occl = [1e30] * n_px  # depth of the nearest *opaque* facet (rock)
        rockl = [0.0] * n_px  # how much of a cell's light is rock
        goldl = [0.0] * n_px  # ...and how much of that is gold
        glow = [0.0] * n_px  # light refracted from every other facet
        core = [0.0] * n_px  # light arriving through the far side
        line = [0.0] * n_px  # traced arrises

        angle = t * spin
        tilt = math.radians(12) + (math.sin(angle) * 0.09 if wobble else 0.0)
        m = matmul(rot_x(tilt), rot_y(angle))
        d = self.dist
        opaque = self.style == "faceted"

        # ---- pass 1: the matrix. Opaque, matte, and it writes the occluder
        # buffer so crystals behind it don't bleed through.
        for a0, b0, c0, albedo, ao in self.rock:
            a = apply(m, a0)
            b = apply(m, b0)
            c = apply(m, c0)
            a = (a[0], a[1], a[2] + d)
            b = (b[0], b[1], b[2] + d)
            c = (c[0], c[1], c[2] + d)

            n = cross(sub(b, a), sub(c, a))
            if dot(n, n) < 1e-12:
                continue
            n = norm(n)
            mid = (
                (a[0] + b[0] + c[0]) / 3,
                (a[1] + b[1] + c[1]) / 3,
                (a[2] + b[2] + c[2]) / 3,
            )
            view = norm(mid)
            if dot(n, view) > 0:
                n = (-n[0], -n[1], -n[2])  # face the camera
                if dot(n, view) > 0:
                    continue

            lam = max(0.0, dot(n, LIGHT))
            rim = max(0.0, dot(n, RIM))
            fil = max(0.0, dot(n, FILL))
            base = (
                0.10 + 0.36 * lam**0.75 + 0.18 * lam**3 + 0.06 * rim + 0.13 * fil
            ) * albedo
            sh, shg = [], []
            for v, k in zip((a, b, c), ao):
                q = base * 0.82 * (0.58 + 0.62 * k)
                sh.append(q)
                # metal takes the light harder than the dull host rock, and
                # throws a tight highlight the rock never does
                half = norm(sub(LIGHT, norm(v)))
                shg.append(q * 2.05 + 0.30 * lam + 0.85 * max(0.0, dot(n, half)) ** 14)
            self._tri(
                a,
                b,
                c,
                tuple(sh),
                0.0,
                True,
                True,
                surf,
                zbuf,
                glow,
                core,
                occl,
                rockl,
                is_rock=True,
                goldl=goldl,
                sh_gold=tuple(shg),
                mverts=(a0, b0, c0),
            )

        # ---- pass 2: the crystals
        for tri in self.tris:
            a = apply(m, tri[0])
            b = apply(m, tri[1])
            c = apply(m, tri[2])
            a = (a[0], a[1], a[2] + d)
            b = (b[0], b[1], b[2] + d)
            c = (c[0], c[1], c[2] + d)

            n = cross(sub(b, a), sub(c, a))
            if dot(n, n) < 1e-12:
                continue
            n = norm(n)

            mid = (
                (a[0] + b[0] + c[0]) / 3,
                (a[1] + b[1] + c[1]) / 3,
                (a[2] + b[2] + c[2]) / 3,
            )
            view = norm(mid)
            facing = dot(n, view)
            front = facing < 0.0

            # Glass, shaded per vertex and interpolated: a big facet is not
            # uniformly bright, it grades as the viewing angle sweeps across
            # it. Flat-shading these reads as slabs of one character.
            fres = (1.0 - abs(facing)) ** 2.2
            sh = []
            for v in (a, b, c):
                vv = norm(v)
                f = (1.0 - abs(dot(n, vv))) ** 2.2
                q = 0.03 + 0.30 * f
                if front:
                    half = norm(sub(LIGHT, vv))
                    q += 1.30 * abs(dot(n, half)) ** 34  # sharp facet glint
                    q += 0.14 * abs(dot(n, RIM)) ** 8  # cool rim
                    q += 0.10 * abs(dot(n, LIGHT))
                else:
                    q *= 0.30  # far wall, muted
                sh.append(q)

            self._tri(
                a,
                b,
                c,
                tuple(sh),
                fres,
                front,
                opaque,
                surf,
                zbuf,
                glow,
                core,
                occl,
                rockl,
                goldl=goldl,
            )

        # traced arrises: what actually makes it read as faceted
        if self.edge > 0:
            for e in self.edges:
                p = apply(m, e[0])
                q = apply(m, e[1])
                self._line(
                    (p[0], p[1], p[2] + d),
                    (q[0], q[1], q[2] + d),
                    line,
                    zbuf,
                    occl,
                    opaque,
                )

        # ridges: where the matrix has fractured
        if self.edge > 0 and self.ridges:
            w = self.edge * 0.22
            for e in self.ridges:
                p = apply(m, e[0])
                q = apply(m, e[1])
                self._line(
                    (p[0], p[1], p[2] + d),
                    (q[0], q[1], q[2] + d),
                    line,
                    occl,
                    occl,
                    True,
                    near=w,
                    far=0.0,
                )

        return self.compose(surf, glow, core, line, rockl, goldl)

    # ---- triangle raster --------------------------------------------------
    def _tri(
        self,
        a,
        b,
        c,
        sh,
        fres,
        front,
        opaque,
        surf,
        zbuf,
        glow,
        core,
        occl,
        rockl,
        is_rock=False,
        goldl=None,
        sh_gold=None,
        mverts=None,
    ):
        pa, pb, pc = self.project(a), self.project(b), self.project(c)
        if not (pa and pb and pc):
            return
        area = (pb[0] - pa[0]) * (pc[1] - pa[1]) - (pb[1] - pa[1]) * (pc[0] - pa[0])
        if abs(area) < 1e-9:
            return
        rw, rh = self.rw, self.rh
        x0 = max(0, int(min(pa[0], pb[0], pc[0])))
        x1 = min(rw - 1, int(max(pa[0], pb[0], pc[0])) + 1)
        y0 = max(0, int(min(pa[1], pb[1], pc[1])))
        y1 = min(rh - 1, int(max(pa[1], pb[1], pc[1])) + 1)
        if x0 > x1 or y0 > y1:
            return
        inv = 1.0 / area
        za, zb, zc = pa[2], pb[2], pc[2]
        sa, sb, sc = sh
        if mverts is not None:
            ma, mb, mc = mverts
            qa, qb, qc = sh_gold
            vs, vw = self.vein_seed, self.vein_width
        for py in range(y0, y1 + 1):
            yc = py + 0.5
            row = py * rw
            for px in range(x0, x1 + 1):
                xc = px + 0.5
                w0 = (
                    (pb[0] - pa[0]) * (yc - pa[1]) - (pb[1] - pa[1]) * (xc - pa[0])
                ) * inv
                if w0 < 0:
                    continue
                w1 = (
                    (pc[0] - pb[0]) * (yc - pb[1]) - (pc[1] - pb[1]) * (xc - pb[0])
                ) * inv
                if w1 < 0:
                    continue
                w2 = (
                    (pa[0] - pc[0]) * (yc - pc[1]) - (pa[1] - pc[1]) * (xc - pc[0])
                ) * inv
                if w2 < 0:
                    continue
                i = row + px
                z = w1 * za + w2 * zb + w0 * zc
                shade = w1 * sa + w2 * sb + w0 * sc
                if is_rock:
                    if z < occl[i]:
                        occl[i] = z
                    if z < zbuf[i]:
                        g = 0.0
                        if mverts is not None and vw > 0:
                            # the vein field is evaluated in model space, so a
                            # seam stays put on the rock as it turns, and stays
                            # sharp however coarse the mesh underneath is
                            g = vein(
                                (
                                    w1 * ma[0] + w2 * mb[0] + w0 * mc[0],
                                    w1 * ma[1] + w2 * mb[1] + w0 * mc[1],
                                    w1 * ma[2] + w2 * mb[2] + w0 * mc[2],
                                ),
                                vs,
                                vw,
                            )
                            if g > 0:
                                shade += (w1 * qa + w2 * qb + w0 * qc - shade) * g
                        zbuf[i] = z
                        surf[i] = shade
                        rockl[i] = shade
                        goldl[i] = shade * g
                    continue
                if z > occl[i]:
                    continue  # buried in the rock
                if front and z < zbuf[i]:
                    zbuf[i] = z
                    surf[i] = shade
                    rockl[i] = 0.0
                    goldl[i] = 0.0
                    glow[i] += shade * 0.35
                else:
                    glow[i] += shade if not opaque else shade * 0.35
                    if not front:
                        core[i] += fres

    # ---- line raster ------------------------------------------------------
    def _line(self, p, q, line, zbuf, occl, opaque, near=None, far=None):
        pp, qq = self.project(p), self.project(q)
        if not (pp and qq):
            return
        dx, dy = qq[0] - pp[0], qq[1] - pp[1]
        steps = int(max(abs(dx), abs(dy))) + 1
        if steps > 6000:
            return
        rw, rh = self.rw, self.rh
        sx, sy, sz = dx / steps, dy / steps, (qq[2] - pp[2]) / steps
        x, y, z = pp[0], pp[1], pp[2]
        if near is None:
            near, far = self.edge * 0.80, self.edge * (0.20 if opaque else 0.26)
        for _ in range(steps + 1):
            ix, iy = int(x), int(y)
            if 0 <= ix < rw and 0 <= iy < rh:
                i = iy * rw + ix
                if z <= occl[i] + 0.02:
                    # an arris on the visible surface is crisp; one seen
                    # through the body of the crystal is a ghost
                    line[i] += near if z <= zbuf[i] + 0.08 else far
            x += sx
            y += sy
            z += sz

    # ---- resolve sub-pixels into characters -------------------------------
    def compose(self, surf, glow, core, line, rockl, goldl):
        ss, w, h, rw = self.ss, self.w, self.h, self.rw
        inv = 1.0 / (ss * ss)
        ramp = self.ramp
        top = len(ramp) - 1
        gain = self.gain
        body = 0.42 if self.style == "faceted" else 1.0
        out = []
        for cy in range(h):
            row = []
            for cx in range(w):
                l = c = rk = gd = 0.0
                for sy in range(ss):
                    base = (cy * ss + sy) * rw + cx * ss
                    for sx in range(ss):
                        i = base + sx
                        l += surf[i] + glow[i] * body + line[i]
                        c += core[i]
                        rk += rockl[i]
                        gd += goldl[i]
                if l <= 0.012 * (ss * ss):
                    row.append((" ", None))
                    continue
                rockness = rk / l if l > 0 else 0.0
                goldness = gd / l if l > 0 else 0.0
                l *= inv * gain
                v = 1.0 - math.exp(-l * 1.15)  # filmic rolloff
                idx = int(v * top + 0.5)
                # quantised, so neighbouring cells share one ANSI colour run
                qv = int(v * 26) / 26.0
                qc = int(min(1.0, c * inv * 0.6) * 6) / 6.0
                qr = int(min(1.0, rockness) * 5) / 5.0
                qg = int(min(1.0, goldness) * 6) / 6.0
                row.append((ramp[max(1, min(top, idx))], (qv, qc, qr, qg)))
            out.append(row)
        return out


# --------------------------------------------------------------------------
# colour: ice-blue crystal over a warm, dull matrix
# --------------------------------------------------------------------------

PALETTE = [
    (0.00, (20, 44, 74)),
    (0.28, (44, 96, 142)),
    (0.52, (98, 160, 198)),
    (0.74, (170, 216, 234)),
    (1.00, (246, 252, 255)),
]

ROCK = [
    (0.00, (26, 23, 22)),
    (0.35, (66, 57, 50)),
    (0.70, (112, 97, 82)),
    (1.00, (158, 142, 120)),
]

GOLD = [
    (0.00, (48, 30, 8)),
    (0.30, (132, 84, 16)),
    (0.60, (206, 152, 44)),
    (0.85, (244, 202, 92)),
    (1.00, (255, 240, 186)),
]


def _sample(table, v):
    for i in range(len(table) - 1):
        t0, c0 = table[i]
        t1, c1 = table[i + 1]
        if v <= t1:
            k = (v - t0) / (t1 - t0)
            return (
                c0[0] + (c1[0] - c0[0]) * k,
                c0[1] + (c1[1] - c0[1]) * k,
                c0[2] + (c1[2] - c0[2]) * k,
            )
    return table[-1][1]


def ramp_color(v, core, rock=0.0, gold=0.0):
    v = 0.0 if v < 0 else (1.0 if v > 1 else v)
    r, g, b = _sample(PALETTE, v)
    if rock > 0:  # blend toward the dull host rock
        rr, rg, rb = _sample(ROCK, v)
        r += (rr - r) * rock
        g += (rg - g) * rock
        b += (rb - b) * rock
    if gold > 0:  # ...and the seams running through it
        yr, yg, yb = _sample(GOLD, v)
        k = min(1.0, gold * 1.25)
        r += (yr - r) * k
        g += (yg - g) * k
        b += (yb - b) * k
    if core > 0 and rock < 0.5:  # inclusions: light through the body goes garnet
        k = min(0.5, core * 0.5) * (1.0 - rock)
        r += (208 - r) * k
        g -= g * k * 0.45
        b -= b * k * 0.22
    return int(r), int(g), int(b)


# --------------------------------------------------------------------------
# wordmark
# --------------------------------------------------------------------------

GLYPHS = {
    "o": [" ██ ", "█  █", "█  █", "█  █", " ██ "],
    "r": ["█ ██", "██  ", "█   ", "█   ", "█   "],
    "e": [" ██ ", "█  █", "████", "█   ", " ██ "],
    "a": [" ██ ", "   █", " ███", "█  █", " ███"],
    "b": ["█   ", "█   ", "███ ", "█  █", "███ "],
    "c": [" ██ ", "█   ", "█   ", "█   ", " ██ "],
    "d": ["   █", "   █", " ███", "█  █", " ███"],
    "i": ["█   ", "    ", "█   ", "█   ", "█   "],
    "l": ["██  ", " █  ", " █  ", " █  ", "███ "],
    "m": ["    ", "██  ", "█ ██", "█  █", "█  █"],
    "n": ["    ", "███ ", "█  █", "█  █", "█  █"],
    "s": [" ███", "█   ", " ██ ", "   █", "███ "],
    "t": [" █  ", "███ ", " █  ", " █  ", " ██ "],
    "u": ["    ", "█  █", "█  █", "█  █", " ███"],
    " ": ["    ", "    ", "    ", "    ", "    "],
}


def wordmark(text):
    rows = ["", "", "", "", ""]
    for ch in text.lower():
        g = GLYPHS.get(ch)
        if g:
            for i in range(5):
                rows[i] += g[i] + " "
    n = max(len(r) for r in rows) if rows[0] else 0
    return [r.ljust(n) for r in rows]  # block-aligned, so centring is stable


def frames(count=24, width=44, height=20, color=False, label="", **kw):
    """Bake `count` frames of one full revolution. Returns a list of frames,
    each a list of strings — drop them into your own CLI as a spinner."""
    r = Renderer(width, height, **kw)
    out = []
    for i in range(count):
        lines = render_text(r.frame(2 * math.pi * i / count), color)
        if label:
            lines += [""] + center(wordmark(label), width)
        out.append(lines)
    return out


# --------------------------------------------------------------------------
# terminal
# --------------------------------------------------------------------------

HIDE, SHOW, HOME, CLEAR, RESET = (
    "\x1b[?25l",
    "\x1b[?25h",
    "\x1b[H",
    "\x1b[2J",
    "\x1b[0m",
)


def render_text(cells, color):
    lines = []
    for row in cells:
        if not color:
            lines.append("".join(ch for ch, _ in row).rstrip())
            continue
        buf, last = [], None
        for ch, meta in row:
            if meta is None:
                if last is not None:
                    buf.append(RESET)
                    last = None
                buf.append(" ")
            else:
                rgb = ramp_color(*meta)
                if rgb != last:
                    buf.append("\x1b[38;2;%d;%d;%dm" % rgb)
                    last = rgb
                buf.append(ch)
        if last is not None:
            buf.append(RESET)
        lines.append("".join(buf))
    return lines


def visible_len(s):
    n, i = 0, 0
    while i < len(s):
        if s[i] == "\x1b":
            while i < len(s) and s[i] != "m":
                i += 1
        else:
            n += 1
        i += 1
    return n


def center(lines, width):
    return [" " * max(0, (width - visible_len(l)) // 2) + l for l in lines]


def main(argv=None):
    p = argparse.ArgumentParser(description="rotating ASCII crystal for `ore`")
    p.add_argument("--width", type=int, default=0, help="columns (0 = auto)")
    p.add_argument("--height", type=int, default=0, help="rows (0 = auto)")
    p.add_argument("--fps", type=float, default=30.0)
    p.add_argument("--speed", type=float, default=0.8, help="spin, radians/sec")
    p.add_argument("--ramp", default="blocks", choices=list(RAMPS))
    p.add_argument("--chars", default=None, help="custom ramp, dark to bright")
    p.add_argument("--ss", type=int, default=2, help="supersampling factor")
    p.add_argument("--gain", type=float, default=1.0, help="brightness")
    p.add_argument("--edge", type=float, default=1.0, help="facet-edge strength")
    p.add_argument(
        "--style",
        default="faceted",
        choices=["faceted", "glass"],
        help="faceted = solid silhouette, glass = fully translucent",
    )
    p.add_argument(
        "--char-aspect",
        type=float,
        default=0.5,
        help="cell width/height of your terminal font",
    )
    p.add_argument("--label", default="ore", help="wordmark ('' for none)")
    p.add_argument("--tagline", default="")
    p.add_argument(
        "--matrix-width",
        type=float,
        default=1.0,
        help="scale the host rock wider (1.3) or narrower (0.8)",
    )
    p.add_argument(
        "--gold", type=float, default=1.0, help="gold vein thickness, 0 for barren rock"
    )
    p.add_argument(
        "--vein-seed", type=float, default=1.0, help="reroll the vein pattern"
    )
    p.add_argument(
        "--no-matrix",
        action="store_true",
        help="floating crystal cluster, no host rock",
    )
    p.add_argument("--no-color", action="store_true")
    # ore: added so `--dump` can emit ANSI for baking coloured frames.
    p.add_argument(
        "--force-color",
        action="store_true",
        help="emit ANSI colour even when stdout is not a TTY",
    )
    p.add_argument("--no-wobble", action="store_true")
    p.add_argument("--once", action="store_true")
    p.add_argument(
        "--dump",
        type=int,
        default=0,
        metavar="N",
        help="print N evenly spaced frames and exit",
    )
    a = p.parse_args(argv)

    term = shutil.get_terminal_size((80, 30))
    extra = (7 if a.label else 0) + (2 if a.tagline else 0)
    width = a.width or max(24, min(term.columns - 2, 100))
    height = a.height or max(12, term.lines - 1 - extra)

    # ore: --dump normally forces plain text because it is piped, but the TUI
    # frames are baked from a pipe *and* want colour, hence --force-color.
    color = not a.no_color and (
        a.force_color
        or (not a.dump and sys.stdout.isatty() and os.environ.get("TERM") != "dumb")
    )

    r = Renderer(
        width,
        height,
        char_aspect=a.char_aspect,
        ramp=a.chars or RAMPS[a.ramp],
        supersample=a.ss,
        gain=a.gain,
        edge=a.edge,
        style=a.style,
        matrix=not a.no_matrix,
        matrix_width=a.matrix_width,
        gold=a.gold,
        vein_seed=a.vein_seed,
    )

    def compose(t):
        lines = render_text(r.frame(t, wobble=not a.no_wobble), color)
        width = r.w
        if a.label:
            mark = center(wordmark(a.label), width)
            if color:
                mark = ["\x1b[38;2;170;216;234m" + m + RESET for m in mark]
            lines += [""] + mark
        if a.tagline:
            tag = center([a.tagline], width)
            lines += [""] + (
                ["\x1b[38;2;92;122;148m" + tag[0] + RESET] if color else tag
            )
        return lines

    if a.dump:
        for i in range(a.dump):
            print("\n".join(compose(2 * math.pi * i / a.dump)))
            print("\n" + "-" * width + "\n")
        return 0

    if a.once:
        print("\n".join(compose(0.0)))
        return 0

    out = sys.stdout
    stop = {"now": False}
    signal.signal(signal.SIGINT, lambda *_: stop.__setitem__("now", True))
    out.write(HIDE + CLEAR)
    start = time.time()
    period = 1.0 / max(1e-3, a.fps)
    checked = 0.0
    try:
        while not stop["now"]:
            t0 = time.time()
            if (not a.width or not a.height) and t0 - checked > 0.5:
                checked = t0
                term = shutil.get_terminal_size((80, 30))
                nw = a.width or max(24, min(term.columns - 2, 100))
                nh = a.height or max(12, term.lines - 1 - extra)
                if (nw, nh) != (r.w, r.h):
                    r = Renderer(
                        nw,
                        nh,
                        char_aspect=a.char_aspect,
                        ramp=a.chars or RAMPS[a.ramp],
                        supersample=a.ss,
                        gain=a.gain,
                        edge=a.edge,
                        style=a.style,
                        matrix=not a.no_matrix,
                        matrix_width=a.matrix_width,
                        gold=a.gold,
                        vein_seed=a.vein_seed,
                    )
                    width = nw
                    out.write(CLEAR)
            lines = compose((t0 - start) * a.speed)
            out.write(HOME + "\n".join(l + "\x1b[K" for l in lines))
            out.flush()
            time.sleep(max(0.0, period - (time.time() - t0)))
    finally:
        out.write(SHOW + RESET + "\n")
        out.flush()
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except BrokenPipeError:  # e.g. piping --dump into head
        os._exit(0)
