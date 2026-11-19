#!/usr/bin/env python3
"""docs/images/storage-{light,dark}.svg: two images stored as layers, and as files.

usage: python3 dev/docs/storage.py
"""
from html import escape
from pathlib import Path

FONT = "ui-sans-serif, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
THEMES = {
    "light": {"surface": "#fcfcfb", "box": "#f2f1ed", "line": "#c3c2b7", "ink": "#0b0b0b",
              "ink2": "#52514e", "arrow": "#898781", "accent": "#2a78d6", "on": "#ffffff",
              "chip": "#ffffff"},
    "dark": {"surface": "#1a1a19", "box": "#242422", "line": "#45453f", "ink": "#f0efec",
             "ink2": "#c3c2b7", "arrow": "#898781", "accent": "#3987e5", "on": "#ffffff",
             "chip": "#2e2e2b"},
}
W, H = 760, 470
CHAR = 7.0  # width of one character of the chips' 11 px monospace font


def chip_width(name):
    return len(name) * CHAR + 14


def svg(t):
    o = []

    def text(x, y, s, size=12, color="ink2", weight=None, anchor="start", font=FONT):
        w = f' font-weight="{weight}"' if weight else ""
        o.append(f'<text x="{x:.1f}" y="{y:.1f}" font-family="{font}" font-size="{size}" '
                 f'fill="{t[color]}"{w} text-anchor="{anchor}">{escape(s)}</text>')

    def rect(x, y, w, h, fill, stroke, dash=False, rx=7):
        d = ' stroke-dasharray="4 3"' if dash else ""
        o.append(f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{h:.1f}" rx="{rx}" '
                 f'fill="{fill}" stroke="{stroke}"{d}/>')

    def chips(x, y, names, mode="plain"):
        """A row of file chips; mode: plain, again (a stored copy), shared (kept once)."""
        for name in names:
            w = chip_width(name)
            if mode == "shared":
                rect(x, y, w, 22, t["accent"], t["accent"], rx=5)
                color = t["on"]
            elif mode == "again":
                rect(x, y, w, 22, t["chip"], t["accent"], rx=5)
                color = t["accent"]
            else:
                rect(x, y, w, 22, t["chip"], t["line"], rx=5)
                color = t["ink"]
            o.append(f'<text x="{x + w / 2:.1f}" y="{y + 15:.1f}" font-family="{MONO}" '
                     f'font-size="11" fill="{color}" text-anchor="middle">{escape(name)}</text>')
            x += w + 6

    def line(x1, y1, x2, y2):
        o.append(f'<line x1="{x1}" y1="{y1}" x2="{x2}" y2="{y2 - 7}" stroke="{t["arrow"]}" '
                 f'stroke-width="1.5"/>')
        # Arrowhead pointing along the line.
        import math
        a = math.atan2(y2 - y1, x2 - x1)
        p = [(x2, y2), (x2 - 9 * math.cos(a - 0.4), y2 - 9 * math.sin(a - 0.4)),
             (x2 - 9 * math.cos(a + 0.4), y2 - 9 * math.sin(a + 0.4))]
        o.append('<polygon points="' + " ".join(f"{px:.1f},{py:.1f}" for px, py in p)
                 + f'" fill="{t["arrow"]}"/>')

    o.append(f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
             f'viewBox="0 0 {W} {H}" role="img" aria-label="Layers against files: as layers, '
             f'the same jars rebuilt into a new layer are stored again; as files, the jars '
             f'that two images share are kept once and both images point to them" '
             f'font-family="{FONT}">')
    o.append("<title>Two images stored as layers, and as files</title>")
    o.append(f'<rect width="{W}" height="{H}" rx="10" fill="{t["surface"]}"/>')

    # Layers.
    x = 24
    text(x, 38, "Layers", 16, "ink", 700)
    text(x, 57, "Docker, Podman, Kiri, OrbStack, Colima")
    for i, (image, rows) in enumerate([
        ("image A (release n)", [("base", ["libc", "JRE 16"], "plain"),
                                 ("jars", ["spring-core.jar", "jackson.jar", "…"], "plain"),
                                 ("app", ["app A"], "plain")]),
        ("image B (release n+1)", [("base", ["libc'", "JRE 21"], "plain"),
                                   ("jars", ["spring-core.jar", "jackson.jar", "…"], "again"),
                                   ("app", ["app B"], "plain")]),
    ]):
        y = 86 + i * 136
        text(x, y, image, 12, "ink", 600)
        for j, (layer, names, mode) in enumerate(rows):
            ry = y + 8 + j * 36
            rect(x, ry, 344, 30, t["box"], t["line"])
            text(x + 12, ry + 19, layer, 11.5)
            chips(x + 60, ry + 4, names, mode)
    text(x, 352, "↑ the same jars again: stored, and cached, twice", 12, "accent", 600)
    text(x, 400, "Each layer is stored whole. Built again, the same", 12, "ink")
    text(x, 417, "jars are a new layer: stored, and cached, again.", 12, "ink")

    # Files.
    x = 392
    text(x, 38, "Files", 16, "ink", 700)
    text(x, 57, "podbag")
    for i, (image, sub) in enumerate([("image A", "release n"), ("image B", "release n+1")]):
        bx = x + i * 188
        rect(bx, 72, 156, 44, t["box"], t["line"])
        text(bx + 12, 90, image, 12.5, "ink", 600)
        text(bx + 12, 107, sub, 11)
    # The file store.
    rect(x, 150, 344, 226, "none", t["line"], dash=True)
    text(x + 334, 368, "file store: each file once", 11.5, "ink2", anchor="end")
    # Files that both images use, in the middle.
    rect(x + 44, 164, 256, 56, t["box"], t["accent"])
    text(x + 54, 181, "in both images: kept once", 11.5, "accent", 600)
    chips(x + 52, 190, ["spring-core.jar", "jackson.jar", "…"], "shared")
    # Files of one image only, below.
    for i, (label, names) in enumerate([("only in A", ["libc", "JRE 16", "app A"]),
                                        ("only in B", ["libc'", "JRE 21", "app B"])]):
        gx = x + 8 + i * 168
        rect(gx, 244, 160, 88, t["box"], t["line"])
        text(gx + 10, 261, label, 11.5)
        chips(gx + 8, 270, names[:2])
        chips(gx + 8, 298, names[2:])
    # Each image points to its files.
    line(x + 70, 116, x + 120, 164)
    line(x + 258, 116, x + 224, 164)
    line(x + 22, 116, x + 30, 244)
    line(x + 322, 116, x + 316, 244)
    text(x, 400, "Each file is stored once. Images point to the", 12, "ink")
    text(x, 417, "files they use; a shared file is cached once.", 12, "ink")

    text(W / 2, 452, "8 Spring services in 2 releases: 902 MiB on disk with podbag, "
         "2,970 MiB with Docker.", 12, "ink2", anchor="middle")
    o.append("</svg>")
    return "\n".join(o) + "\n"


out = Path(__file__).resolve().parents[2] / "docs" / "images"
for mode, theme in THEMES.items():
    (out / f"storage-{mode}.svg").write_text(svg(theme))
print("wrote", out / "storage-light.svg", out / "storage-dark.svg")
