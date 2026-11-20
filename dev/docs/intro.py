#!/usr/bin/env python3
"""The README's intro: docs/images/intro.svg, a 30-second animation (CSS, looping) of how
podbag differs from other container engines. With --video, also docs/media/intro.mp4 (needs
Google Chrome, Node.js 22 and ffmpeg).

usage: python3 dev/docs/intro.py [--video]
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from motion import (DARK as C, FILL_BOX_CENTRE, FILL_BOX_LEFT, MONO, REPO, Motion,  # noqa: E402
                    icon_ram, icon_stopwatch, logo, render_video, svg_doc, text)

W, H, T = 800, 450, 30.0


def chip_width(name):
    return len(name) * 7.2 + 16


def build():
    m = Motion(T)
    o = []

    def chip(x, y, name, kind="plain", cls=None):
        w = chip_width(name)
        fill, stroke, ink = {"shared": (C["accent"], C["accent"], "#ffffff"),
                             "again": (C["again"], C["again"], "#1a1a19"),
                             "plain": (C["panel"], C["line"], C["ink"])}[kind]
        c = f' class="{cls}"' if cls else ""
        o.append(f'<g{c}><rect x="{x:.1f}" y="{y}" width="{w:.1f}" height="24" rx="6" fill="{fill}" '
                 f'stroke="{stroke}"/>' + text(x + w / 2, y + 16.5, name, 12, ink, None, "middle",
                                                font=MONO) + "</g>")
        return w

    def pill(x, y, name, note, cls, beat=False):
        dot = f'<circle{" class=\"beat\"" if beat else ""} cx="{x + 18}" cy="{y + 17}" r="5" fill="{C["accent"]}"/>'
        o.append(f'<g class="{cls}"><rect x="{x}" y="{y}" width="290" height="34" rx="9" '
                 f'fill="{C["panel2"]}"/>{dot}' + text(x + 32, y + 22, name, 14, C["ink"], None,
                                                        font=MONO)
                 + text(x + 278, y + 22, note, 12, C["muted"], None, "end") + "</g>")

    def bars(rows, t0, t1, top, unit, decimals):
        for i, (label, value, mine, g0, g1) in enumerate(rows):
            y = 150 + i * 62
            o.append(text(190, y + 21, label, 17, C["ink"] if mine else C["muted"],
                          700 if mine else None, "end", cls=m.rise(t0 + 0.4, t1)))
            width = max(5.0, 470 * value / top)
            o.append(f'<rect class="{m.grow(g0, g1, t1)}" x="204" y="{y}" width="{width:.1f}" '
                     f'height="30" rx="6" fill="{C["accent"] if mine else C["gray"]}" '
                     f'style="{FILL_BOX_LEFT}"/>')
            o.append(text(204 + width + 12, y + 21, f"{value:,.{decimals}f} {unit}", 17,
                          C["ink"], 700 if mine else None, cls=m.rise(g1 - 0.2, t1, dy=0)))

    # 1. Who we are.
    t1 = 3.8
    o.append(f'<g class="{m.window(0.05, t1)}" opacity="0">')
    o.append(logo(352, 40, 96, cls=m.drop(0.15, t1)))
    o.append(text(400, 206, "podbag", 68, C["ink"], 800, "middle", cls=m.rise(0.6, t1)))
    o.append(text(400, 248, "A container engine that works differently", 22, C["muted"], None,
                  "middle", cls=m.rise(1.0, t1)))
    words = ["no daemon", "files, not layers", "one static binary"]
    widths = [len(w) * 8.6 + 32 for w in words]
    x = 400 - (sum(widths) + 12 * (len(words) - 1)) / 2
    for i, (word, w) in enumerate(zip(words, widths)):
        o.append(f'<g class="{m.rise(1.5 + i * 0.2, t1)}"><rect x="{x:.1f}" y="284" width="{w:.1f}" '
                 f'height="34" rx="17" fill="{C["panel2"]}" stroke="{C["line"]}"/>'
                 + text(x + w / 2, 306, word, 15, C["ink"], 600, "middle") + "</g>")
        x += w + 12
    o.append("</g>")

    # 2. No daemon.
    t0, t1 = 3.8, 10.0
    o.append(f'<g class="{m.window(t0, t1)}" opacity="0">')
    o.append(text(400, 56, "Other engines keep daemons running", 26, C["ink"], 800, "middle",
                  cls=m.rise(4.0, 6.9)))
    o.append(text(400, 56, "podbag runs only while containers run", 26, C["ink"], 800, "middle",
                  cls=m.rise(6.9, 9.9)))
    for x, title, sub in [(60, "Docker Engine", "a client and two daemons"),
                          (410, "podbag", "one program, no daemon")]:
        o.append(f'<rect x="{x}" y="84" width="330" height="324" rx="16" fill="{C["panel"]}" '
                 f'stroke="{C["line"]}"/>')
        o.append(text(x + 20, 116, title, 18, C["ink"], 700))
        o.append(text(x + 20, 138, sub, 13, C["muted"]))
    pill(80, 156, "dockerd", "always on", m.rise(4.3, 9.9), beat=True)
    pill(80, 196, "containerd", "always on", m.rise(4.5, 9.9), beat=True)
    for i, name in enumerate(["web", "api", "db"]):
        pill(80, 236 + i * 40, "containerd-shim", name, m.rise(5.4 + i * 0.15, 8.2))
    o.append(f'<g class="{m.windows((4.0, 5.3), (8.9, 9.9))}"><rect x="430" y="156" width="290" '
             f'height="160" rx="10" fill="none" stroke="{C["line"]}" stroke-dasharray="6 5"/>'
             + text(575, 242, "nothing running", 16, C["muted"], None, "middle") + "</g>")
    pill(430, 156, "keeper", "about 2 MiB", m.rise(5.3, 8.9))
    for x in (80, 430):
        cx = x
        for i, name in enumerate(["web", "api", "db"]):
            cx += chip(cx, 362, name, cls=m.rise(5.4 + i * 0.15, 8.2)) + 8
    o.append(text(80, 392, "At rest: 4 processes, 203 MiB", 16, C["again"], 700,
                  cls=m.rise(8.4, 9.9)))
    o.append(text(430, 392, "At rest: nothing runs", 16, C["once"], 700, cls=m.rise(8.9, 9.9)))
    o.append("</g>")

    # 3. Files, not layers.
    t0, t1 = 10.0, 16.4
    o.append(f'<g class="{m.window(t0, t1)}" opacity="0">')
    o.append(text(400, 56, "Others store layers. podbag stores files.", 26, C["ink"], 800,
                  "middle", cls=m.rise(10.1, t1)))
    o.append(text(60, 100, "Layers: Docker, Podman, Kiri", 15, C["muted"], 600))
    o.append(text(420, 100, "Files: podbag", 15, C["muted"], 600))
    for y, label, names, kinds, start in [
            (114, "A", ["spring.jar", "jackson.jar", "orders.jar"], ["shared", "shared", "plain"], 10.5),
            (164, "B", ["spring.jar", "jackson.jar", "payments.jar"], ["again", "again", "plain"], 11.2)]:
        cls = m.rise(start, t1)
        o.append(f'<g class="{cls}"><rect x="60" y="{y}" width="330" height="40" rx="9" '
                 f'fill="{C["panel2"]}"/>' + text(78, y + 26, label, 14, C["muted"], 700) + "</g>")
        x = 100
        for name, kind in zip(names, kinds):
            x += chip(x, y + 8, name, kind, cls=cls) + 6
    o.append(text(60, 236, "the same jars, stored again", 15, C["again"], 700,
                  cls=m.rise(11.8, t1)))
    o.append(f'<rect x="420" y="114" width="320" height="90" rx="10" fill="none" '
             f'stroke="{C["line"]}" stroke-dasharray="6 5"/>')
    slots = {}
    x = 438
    for i, (name, kind) in enumerate([("spring.jar", "shared"), ("jackson.jar", "shared"),
                                      ("orders.jar", "plain")]):
        slots[name] = (x, 128)
        x += chip(x, 128, name, kind, cls=m.rise(10.6 + i * 0.12, t1)) + 8
    slots["payments.jar"] = (438, 164)
    chip(438, 164, "payments.jar", "plain", cls=m.rise(11.4, t1))
    # Image B's copies of the shared jars fly over from the left and merge with the ones here.
    source_x = {"spring.jar": 100, "jackson.jar": 100 + chip_width("spring.jar") + 6}
    for name in ("spring.jar", "jackson.jar"):
        sx, sy = slots[name]
        chip(sx, sy, name, "shared", cls=m.move(source_x[name] - sx, 172 - sy, 11.6, 12.3, 12.6))
        o.append(f'<rect class="{m.frames([(0, "opacity:0"), (12.3, "opacity:0"), (12.45, "opacity:1"), (13.0, "opacity:0"), (T, "opacity:0")])}" '
                 f'x="{sx - 3:.1f}" y="{sy - 3}" width="{chip_width(name) + 6:.1f}" height="30" rx="8" '
                 f'fill="none" stroke="{C["once"]}" stroke-width="2.5"/>')
    o.append(text(420, 236, "each file once", 15, C["once"], 700, cls=m.rise(12.6, t1)))
    o.append(text(400, 322, "−70% disk", 54, C["accent"], 800, "middle", cls=m.pop(13.2, t1),
                  style=FILL_BOX_CENTRE))
    o.append(text(400, 360, "8 Spring services in 2 releases: 902 MiB instead of 2,970 MiB", 16,
                  C["muted"], None, "middle", cls=m.rise(13.6, t1)))
    o.append("</g>")

    # 4. Stored once, cached once.
    t0, t1 = 16.4, 21.8
    o.append(f'<g class="{m.window(t0, t1)}" opacity="0">')
    o.append(icon_ram(56, 46, 52, C["accent"], cls=m.pop(16.5, t1), style=FILL_BOX_CENTRE))
    o.append(text(126, 76, "Files stored once are cached once", 26, C["ink"], 800,
                  cls=m.rise(16.6, t1)))
    o.append(text(126, 102, "Memory read to start 16 Spring services (page cache)", 15, C["muted"],
                  cls=m.rise(16.8, t1)))
    bars([("Docker", 982, False, 17.3, 18.4), ("podbag", 324, True, 17.5, 18.4)], t0, t1, 982,
         "MiB", 0)
    o.append(text(400, 330, "−67% memory", 54, C["accent"], 800, "middle", cls=m.pop(18.9, t1),
                  style=FILL_BOX_CENTRE))
    o.append(text(400, 368, "Containers that read the same file share one copy in memory.", 16,
                  C["muted"], None, "middle", cls=m.rise(19.3, t1)))
    o.append("</g>")

    # 5. Fast.
    t0, t1 = 21.8, 26.2
    o.append(f'<g class="{m.window(t0, t1)}" opacity="0">')
    o.append(icon_stopwatch(56, 46, 52, C["accent"], hand_cls=m.spin(22.2, 24.2, 2),
                            cls=m.pop(21.9, t1), style=FILL_BOX_CENTRE))
    o.append(text(126, 76, "And it starts fast", 26, C["ink"], 800, cls=m.rise(22.0, t1)))
    o.append(text(126, 102, "Start a container: median of 100 runs on Linux", 15, C["muted"],
                  cls=m.rise(22.2, t1)))
    bars([("Docker", 73.8, False, 22.6, 23.9), ("podbag", 4.1, True, 22.6, 22.75)], t0, t1, 73.8,
         "ms", 1)
    o.append(text(400, 330, "18× faster start", 54, C["accent"], 800, "middle",
                  cls=m.pop(24.1, t1), style=FILL_BOX_CENTRE))
    o.append(text(400, 368, "One static binary. No daemon to ask. No shim to start.", 16,
                  C["muted"], None, "middle", cls=m.rise(24.4, t1)))
    o.append("</g>")

    # 6. Get it: the last scene, which stays when motion is off.
    t0 = 26.2
    o.append(f'<g class="{m.window(t0, T)}">')
    o.append(logo(368, 40, 64, cls=m.drop(26.3, T)))
    o.append(text(400, 158, "podbag", 50, C["ink"], 800, "middle", cls=m.rise(26.6, T)))
    o.append(text(400, 192, "Linux · macOS · Windows · x86_64 and arm64", 17, C["muted"], None,
                  "middle", cls=m.rise(26.9, T)))
    cmd = "curl -fsSL https://podbag.dev/install.sh | sh"
    o.append(f'<g class="{m.rise(27.2, T)}"><rect x="70" y="222" width="660" height="48" rx="10" '
             f'fill="{C["panel2"]}" stroke="{C["line"]}"/>'
             + text(90, 251, "$ " + cmd, 11.6, C["ink"], None, font=MONO) + "</g>")
    o.append(text(400, 330, "podbag.dev", 30, C["accent"], 800, "middle", cls=m.rise(27.5, T)))
    o.append(text(400, 362, "Apache-2.0 · github.com/SteelCrab/podbag", 15, C["muted"], None,
                  "middle", cls=m.rise(27.8, T)))
    o.append("</g>")

    label = ("podbag intro. Other engines keep daemons running; podbag runs only while containers "
             "run, and nothing at rest. Others store layers; podbag stores files, each once: 902 "
             "MiB of disk instead of 2,970 MiB for 8 Spring services in 2 releases, and 324 MiB "
             "of page cache instead of 982 MiB. A container starts in 4.1 ms, 18 times faster "
             "than Docker's 73.8 ms. Linux, macOS and Windows; podbag.dev.")
    return svg_doc(W, H, m, o, label, "podbag: a container engine that works differently", C["bg"])


def main():
    svg = build()
    out = REPO / "docs/images/intro.svg"
    out.write_text(svg)
    print("wrote", out)
    if "--video" in sys.argv:
        render_video(svg, REPO / "docs/media/intro.mp4", W, H, T)


if __name__ == "__main__":
    main()
