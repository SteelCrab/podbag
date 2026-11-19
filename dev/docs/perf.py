#!/usr/bin/env python3
"""docs/images/perf.svg: a 35-second animated comparison of podbag with Docker, Podman and
Kiri, from the numbers in docs/benchmarks.md and docs/benchmarks-msa.md. With --video, also
docs/media/perf.mp4 (needs Google Chrome, Node.js 22 and ffmpeg).

usage: python3 dev/docs/perf.py [--video]
"""
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
from motion import (DARK as C, FILL_BOX_CENTRE, FILL_BOX_LEFT, REPO, Motion, icon_chip,  # noqa: E402
                    icon_disk, icon_moon, icon_ram, icon_stopwatch, logo, render_video,
                    svg_doc, text)

W, H, T = 800, 450, 35.0
START = [("podbag (rootless)", 4.1, True), ("podbag (root)", 4.4, True), ("Kiri", 9.1, False),
         ("Podman", 46.4, False), ("Docker", 73.8, False)]
MEMORY = [("podbag (root)", 105, True), ("Kiri", 109, False), ("podbag (rootless)", 117, True),
          ("Podman", 203, False), ("Docker", 562, False)]
REST = [("podbag", 0, "0 MiB", True), ("Podman", 1, "0.6 MiB", False),
        ("Kiri", 1, "5.9 MiB", False), ("Docker", 4, "203 MiB", False)]
DISK = [("podbag (root)", 902, True), ("podbag (rootless)", 944, True), ("Podman", 1783, False),
        ("Kiri", 2960, False), ("Docker", 2970, False)]
CACHE = [("podbag (rootless)", 324, True), ("podbag (root)", 333, True), ("Kiri", 960, False),
         ("Podman", 975, False), ("Docker", 982, False)]


def fmt(value, decimals):
    return f"{value:,.{decimals}f}"


def build():
    m = Motion(T)
    o = []

    def bar_scene(t0, t1, icon, title, subtitle, rows, unit, decimals, callout):
        o.append(f'<g class="{m.window(t0, t1)}" opacity="0">')
        o.append(icon(56, 48, 52, C["accent"], cls=m.pop(t0 + 0.1, t1), style=FILL_BOX_CENTRE))
        o.append(text(126, 78, title, 26, C["ink"], 700, cls=m.rise(t0 + 0.2, t1)))
        o.append(text(126, 104, subtitle, 14, C["muted"], cls=m.rise(t0 + 0.35, t1)))
        top = max(v for _, v, _ in rows)
        for i, (label, value, mine) in enumerate(rows):
            y = 140 + i * 50
            g0 = t0 + 0.7 + i * 0.18
            g1 = g0 + 1.0
            o.append(text(250, y + 19, label, 15, C["ink"] if mine else C["muted"],
                          700 if mine else None, "end", cls=m.rise(t0 + 0.5 + i * 0.08, t1)))
            width = max(4.0, 420 * value / top)
            color = C["accent"] if mine else C["gray"]
            o.append(f'<rect class="{m.grow(g0, g1, t1)}" x="264" y="{y}" width="{width:.1f}" '
                     f'height="26" rx="5" fill="{color}" style="{FILL_BOX_LEFT}"/>')
            o.append(text(264 + width + 10, y + 19, f"{fmt(value, decimals)} {unit}", 15,
                          C["ink"], 700 if mine else None, cls=m.rise(g1 - 0.25, t1, dy=0)))
        o.append(text(400, 424, callout, 23, C["accent"], 800, "middle",
                      cls=m.pop(t0 + 2.6, t1), style=FILL_BOX_CENTRE))
        o.append("</g>")

    # The title.
    t0, t1 = 0.0, 3.6
    o.append(f'<g class="{m.window(0.05, t1)}" opacity="0">')
    o.append(logo(368, 52, 64, cls=m.drop(0.2, t1)))
    o.append(text(400, 178, "How podbag compares", 36, C["ink"], 800, "middle", cls=m.rise(0.6, t1)))
    o.append(text(400, 212, "with Docker, Podman and Kiri, on one Linux VM. Lower is better.",
                  16, C["muted"], None, "middle", cls=m.rise(0.9, t1)))
    icons = [icon_stopwatch, icon_chip, icon_moon, icon_disk, icon_ram]
    for i, icon in enumerate(icons):
        x = 400 - (len(icons) * 64 - 20) / 2 + i * 64
        o.append(icon(x, 252, 44, C["accent"], cls=m.pop(1.2 + i * 0.12, t1), style=FILL_BOX_CENTRE))
    o.append("</g>")

    hand = m.spin(4.2, 6.2, 2)
    bar_scene(3.6, 9.1, lambda x, y, s, c, cls, style: icon_stopwatch(x, y, s, c, hand, cls, style),
              "Start a container", "ms, median of 100 starts with no network", START, "ms", 1,
              "18× faster than Docker, 2× faster than Kiri")
    bar_scene(9.1, 14.6, icon_chip, "Memory for 100 containers",
              "MiB the system holds, one network", MEMORY, "MiB", 0,
              "About 5× less memory than Docker")

    # Nothing at rest.
    t0, t1 = 14.6, 19.6
    o.append(f'<g class="{m.window(t0, t1)}" opacity="0">')
    o.append(icon_moon(56, 48, 52, C["accent"], cls=m.pop(t0 + 0.1, t1), style=FILL_BOX_CENTRE))
    o.append(text(126, 78, "With no containers running", 26, C["ink"], 700, cls=m.rise(t0 + 0.2, t1)))
    o.append(text(126, 104, "processes and memory the tool keeps", 14, C["muted"],
                  cls=m.rise(t0 + 0.35, t1)))
    for i, (label, procs, mem, mine) in enumerate(REST):
        y = 150 + i * 58
        o.append(text(250, y + 20, label, 16, C["ink"] if mine else C["muted"],
                      700 if mine else None, "end", cls=m.rise(t0 + 0.5 + i * 0.1, t1)))
        if procs == 0:
            o.append(text(268, y + 20, "no processes", 16, C["once"], 700,
                          cls=m.rise(t0 + 0.9, t1)))
        for j in range(procs):
            cx = 280 + j * 30
            o.append(f'<g class="{m.pop(t0 + 0.9 + i * 0.15 + j * 0.1, t1)}" style="{FILL_BOX_CENTRE}">'
                     f'<circle class="beat" cx="{cx}" cy="{y + 14}" r="10" fill="{C["gray"]}"/></g>')
        o.append(text(560, y + 20, mem, 16, C["ink"], 700 if mine else None,
                      cls=m.rise(t0 + 1.3 + i * 0.1, t1, dy=0)))
    o.append(text(400, 424, "Nothing runs once the last container ends", 23, C["accent"], 800,
                  "middle", cls=m.pop(t0 + 2.4, t1), style=FILL_BOX_CENTRE))
    o.append("</g>")

    bar_scene(19.6, 25.1, icon_disk, "Disk for 8 Spring services in 2 releases",
              "MiB after their containers ran", DISK, "MiB", 0, "−70% disk: files, not layers")
    bar_scene(25.1, 30.6, icon_ram, "Memory to start 16 Spring services",
              "MiB of page cache read to start them", CACHE, "MiB", 0,
              "−67% memory: a shared file is cached once")

    # Summary: the last scene, which stays when motion is off.
    t0 = 30.6
    o.append(f'<g class="{m.window(t0, T)}">')
    o.append(text(400, 72, "podbag, measured", 30, C["ink"], 800, "middle", cls=m.rise(t0 + 0.1, T)))
    cards = [(icon_stopwatch, "4.1 ms", "to start a container", "Docker: 73.8 ms"),
             (icon_chip, "105 MiB", "for 100 containers", "Docker: 562 MiB"),
             (icon_disk, "902 MiB", "disk, 8 services × 2", "Docker: 2,970 MiB"),
             (icon_ram, "324 MiB", "page cache, 16 services", "Docker: 982 MiB")]
    for i, (icon, value, what, docker) in enumerate(cards):
        x = 40 + i * 185
        cls = m.rise(t0 + 0.3 + i * 0.15, T)
        o.append(f'<g class="{cls}"><rect x="{x}" y="104" width="165" height="208" rx="14" '
                 f'fill="{C["panel"]}" stroke="{C["line"]}"/>'
                 + icon(x + 60.5, 122, 44, C["accent"])
                 + text(x + 82.5, 214, value, 26, C["accent"], 800, "middle")
                 + text(x + 82.5, 242, what, 13.5, C["ink"], None, "middle")
                 + text(x + 82.5, 290, docker, 12.5, C["muted"], None, "middle") + "</g>")
    o.append(logo(318, 344, 40, cls=m.rise(t0 + 1.0, T)))
    o.append(text(368, 372, "podbag.dev", 24, C["ink"], 800, cls=m.rise(t0 + 1.1, T)))
    o.append("</g>")

    label = ("How podbag compares with Docker, Podman and Kiri on one Linux VM: a container "
             "starts in 4.1 ms (Docker 73.8 ms); 100 containers hold 105 MiB (Docker 562 MiB); "
             "nothing runs with no containers (Docker 203 MiB); 8 Spring services in 2 "
             "releases take 902 MiB of disk (Docker 2,970 MiB) and 324 MiB of page cache "
             "(Docker 982 MiB).")
    return svg_doc(W, H, m, o, label, "How podbag compares", C["bg"])


def main():
    svg = build()
    out = REPO / "docs/images/perf.svg"
    out.write_text(svg)
    print("wrote", out)
    if "--video" in sys.argv:
        render_video(svg, REPO / "docs/media/perf.mp4", W, H, T)


if __name__ == "__main__":
    main()
