#!/usr/bin/env python3
"""docs/images/architecture-{light,dark}.svg: Docker Engine and podbag on Linux.

usage: python3 dev/docs/architecture.py
"""
from html import escape
from pathlib import Path

FONT = "ui-sans-serif, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
THEMES = {
    "light": {"surface": "#fcfcfb", "box": "#f2f1ed", "line": "#c3c2b7", "ink": "#0b0b0b",
              "ink2": "#52514e", "arrow": "#898781", "accent": "#2a78d6", "on": "#ffffff"},
    "dark": {"surface": "#1a1a19", "box": "#242422", "line": "#45453f", "ink": "#f0efec",
             "ink2": "#c3c2b7", "arrow": "#898781", "accent": "#3987e5", "on": "#ffffff"},
}
W, H = 760, 486


def svg(t):
    o = []

    def text(x, y, s, size=12, color="ink2", weight=None, anchor="start"):
        w = f' font-weight="{weight}"' if weight else ""
        o.append(f'<text x="{x}" y="{y}" font-size="{size}" fill="{t[color]}"{w} '
                 f'text-anchor="{anchor}">{escape(s)}</text>')

    def box(x, y, w, h, name, note=None, pill=None, filled_pill=False):
        o.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="7" fill="{t["box"]}" '
                 f'stroke="{t["line"]}"/>')
        if note:
            text(x + 14, y + 19, name, 13.5, "ink", 600)
            text(x + 14, y + 36, note, 11.5)
        else:
            text(x + 14, y + h / 2 + 5, name, 13.5, "ink", 600)
        if pill:
            pw = 7 * len(pill) + 16
            px, py = x + w - pw - 10, y + h / 2 - 10
            fill = t["accent"] if filled_pill else "none"
            stroke = t["accent"]
            color = t["on"] if filled_pill else t["accent"]
            o.append(f'<rect x="{px}" y="{py}" width="{pw}" height="20" rx="10" fill="{fill}" '
                     f'stroke="{stroke}"/>')
            o.append(f'<text x="{px + pw / 2}" y="{py + 14}" font-size="11" fill="{color}" '
                     f'text-anchor="middle">{escape(pill)}</text>')

    def arrow(x, y1, y2, label=None, label_x=None):
        o.append(f'<line x1="{x}" y1="{y1}" x2="{x}" y2="{y2 - 7}" stroke="{t["arrow"]}" '
                 f'stroke-width="1.5"/>')
        o.append(f'<polygon points="{x - 5},{y2 - 8} {x + 5},{y2 - 8} {x},{y2}" '
                 f'fill="{t["arrow"]}"/>')
        if label:
            for i, line in enumerate(label.split("\n")):
                text(label_x or x + 12, (y1 + y2) / 2 + 4 + i * 15 - 7 * (label.count("\n")), line, 11.5)

    o.append(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
             f'viewBox="0 0 {W} {H}" role="img" aria-label="Docker Engine runs two daemons '
             f'and a shim per container and stores images as layers; podbag runs no daemon, '
             f'one keeper per user while containers run, and stores images as files" '
             f'font-family="{FONT}">')
    o.append("<title>Docker Engine and podbag on Linux: processes and image storage</title>")
    o.append(f'<rect width="{W}" height="{H}" rx="10" fill="{t["surface"]}"/>')

    # Docker Engine
    x = 30
    text(x, 38, "Docker Engine", 16, "ink", 700)
    text(x, 57, "a client and two daemons")
    box(x, 76, 320, 40, "docker CLI")
    arrow(x + 40, 116, 146, "API on /var/run/docker.sock")
    box(x, 146, 320, 44, "dockerd", pill="always running", filled_pill=True)
    arrow(x + 40, 190, 218, "gRPC")
    box(x, 218, 320, 44, "containerd", pill="always running", filled_pill=True)
    arrow(x + 40, 262, 290, "starts one per container")
    box(x, 290, 320, 44, "containerd-shim", pill="one per container")
    arrow(x + 40, 334, 362, "runc sets the container up, then exits")
    box(x, 362, 320, 40, "container")
    text(x, 432, "Images: layers. A changed layer is stored again, whole:", 12, "ink")
    text(x, 450, "a file in two layers is kept, and cached, twice.", 12, "ink")

    # podbag
    x = 410
    text(x, 38, "podbag", 16, "ink", 700)
    text(x, 57, "one program, no daemon")
    box(x, 76, 320, 44, "podbag", note="the command is the engine")
    arrow(x + 30, 120, 362, "creates each container itself:\nnamespaces, cgroups, seccomp",
          label_x=x + 44)
    box(x + 92, 262, 228, 46, "keeper", note="one per user, while containers run")
    arrow(x + 206, 308, 362, "logs, stop, wait", label_x=x + 218)
    box(x, 362, 320, 40, "container")
    text(x, 432, "Images: files. A file that many images share", 12, "ink")
    text(x, 450, "is kept once and cached once.", 12, "ink")

    text(W / 2, 476, "With no container, Docker's daemons keep running; podbag runs nothing.",
         11.5, "ink2", anchor="middle")
    o.append("</svg>")
    return "\n".join(o) + "\n"


out = Path(__file__).resolve().parents[2] / "docs" / "images"
for mode, theme in THEMES.items():
    (out / f"architecture-{mode}.svg").write_text(svg(theme))
print("wrote", out / "architecture-light.svg", out / "architecture-dark.svg")
