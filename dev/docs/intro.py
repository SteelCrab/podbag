#!/usr/bin/env python3
"""The README's intro: docs/images/intro.svg, an animation that plays in the page
(SMIL, 26 s, looping). With --video, also docs/media/intro.mp4, from frames that
headless Chrome takes of the same SVG (needs Google Chrome and ffmpeg).

usage: python3 dev/docs/intro.py [--video]
"""
import subprocess
import sys
import tempfile
from html import escape
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
W, H, T = 800, 450, 26.0
SANS = "ui-sans-serif, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
BG, PANEL, INK, MUTED, ACCENT, GRAY = "#101114", "#1b1c20", "#f0efec", "#9a998f", "#3987e5", "#5b5a55"


def k(*pairs):
    """keyTimes and values from (seconds, value) pairs, for a T-long loop."""
    times = ";".join(f"{s / T:.4f}" for s, _ in pairs)
    values = ";".join(str(v) for _, v in pairs)
    return times, values


def fade(start, end, ramp=0.5):
    """Visible from start to end, with fades; hidden for the rest of the loop."""
    pairs = [(0, 0), (start, 0), (start + ramp, 1), (end - ramp, 1), (end, 0), (T, 0)]
    if start == 0:
        pairs = [(0, 0), (ramp, 1), (end - ramp, 1), (end, 0), (T, 0)]
    times, values = k(*pairs)
    return (f'<animate attributeName="opacity" dur="{T}s" repeatCount="indefinite" '
            f'keyTimes="{times}" values="{values}"/>')


def appear(at, end):
    times, values = k((0, 0), (at, 0), (at + 0.15, 1), (end, 1), (end + 0.01, 0), (T, 0))
    return (f'<animate attributeName="opacity" dur="{T}s" repeatCount="indefinite" '
            f'keyTimes="{times}" values="{values}"/>')


def grow(attr, start, end, full):
    times, values = k((0, 0), (start, 0), (end, full), (T, full))
    return (f'<animate attributeName="{attr}" dur="{T}s" repeatCount="indefinite" '
            f'keyTimes="{times}" values="{values}"/>')


def text(x, y, s, size, color=INK, weight=None, anchor="start", font=SANS, extra=""):
    w = f' font-weight="{weight}"' if weight else ""
    return (f'<text x="{x}" y="{y}" font-family="{font}" font-size="{size}" fill="{color}"'
            f'{w} text-anchor="{anchor}"{extra}>{escape(s)}</text>')


def svg():
    o = [f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{H}" '
         f'viewBox="0 0 {W} {H}" role="img" aria-label="podbag: a container engine with no '
         f'daemon, in one static binary, that stores images file by file">',
         "<title>podbag intro</title>",
         f'<rect width="{W}" height="{H}" rx="14" fill="{BG}"/>']

    # 1. The name.
    o.append("<g opacity=\"0\">" + fade(0, 4.5))
    o.append(text(W / 2, 200, "podbag", 72, weight=800, anchor="middle"))
    o.append(text(W / 2, 248, "A container engine with no daemon,", 22, MUTED, anchor="middle"))
    o.append(text(W / 2, 278, "in one static binary.", 22, MUTED, anchor="middle"))
    o.append("</g>")

    # 2. A terminal: run a container, and nothing is left running.
    o.append("<g opacity=\"0\">" + fade(4.5, 12))
    o.append(f'<rect x="60" y="50" width="680" height="280" rx="10" fill="{PANEL}"/>')
    for i, c in enumerate(["#ec6a5e", "#f4bf4f", "#61c554"]):
        o.append(f'<circle cx="{84 + i * 20}" cy="72" r="6" fill="{c}"/>')
    lines = [
        (5.0, 6.4, 120, "$ podbag run --rm alpine echo hello", INK),
        (6.7, None, 152, "hello", INK),
        (7.4, 8.6, 196, "$ sleep 15; pgrep -l podbag", INK),
        (8.9, None, 228, "# nothing: no daemon stays running", MUTED),
    ]
    for n, (start, typed_end, y, s, color) in enumerate(lines):
        if typed_end:
            width = len(s) * 11.5 + 12
            o.append(f'<clipPath id="type{n}"><rect x="84" y="{y - 22}" height="32" width="0">'
                     + grow("width", start, typed_end, width) + "</rect></clipPath>")
            o.append(text(88, y, s, 19, color, font=MONO, extra=f' clip-path="url(#type{n})"'))
        else:
            o.append("<g opacity=\"0\">" + appear(start, 11.99)
                     + text(88, y, s, 19, color, font=MONO) + "</g>")
    o.append("<g opacity=\"0\">" + appear(9.3, 11.99))
    o.append(text(W / 2, 382, "A container starts in about 4 ms on Linux.", 22, weight=600, anchor="middle"))
    o.append(text(W / 2, 414, "Seconds after the last one ends, nothing runs.", 18, MUTED, anchor="middle"))
    o.append("</g></g>")

    # 3. Storage: files, each kept once.
    o.append("<g opacity=\"0\">" + fade(12, 18.5))
    o.append(text(W / 2, 74, "Images are stored file by file", 30, weight=700, anchor="middle"))
    o.append(text(W / 2, 106, "8 Spring services in 2 releases", 17, MUTED, anchor="middle"))
    rows = [
        (160, "Disk", [("podbag", 902, ACCENT), ("Docker", 2970, GRAY)], "MiB"),
        (290, "Page cache to start them", [("podbag", 324, ACCENT), ("Docker", 982, GRAY)], "MiB"),
    ]
    for y, label, bars, unit in rows:
        o.append(text(110, y, label, 18, weight=600))
        top = max(v for _, v, _ in bars)
        for i, (name, value, color) in enumerate(bars):
            by = y + 16 + i * 38
            full = value / top * 470
            o.append(text(190, by + 20, name, 16, MUTED, anchor="end"))
            o.append(f'<rect x="204" y="{by + 4}" height="22" rx="4" fill="{color}" width="0">'
                     + grow("width", 12.7, 13.9, round(full, 1)) + "</rect>")
            o.append("<g opacity=\"0\">" + appear(14.0, 18.49)
                     + text(214 + full, by + 21, f"{value:,} {unit}", 16) + "</g>")
    o.append("</g>")

    # 4. Where it runs.
    o.append("<g opacity=\"0\">" + fade(18.5, 23))
    o.append(text(W / 2, 170, "Linux · macOS · Windows", 40, weight=700, anchor="middle"))
    o.append(text(W / 2, 214, "x86_64 and arm64 · Windows through WSL 2 (experimental)", 18,
                  MUTED, anchor="middle"))
    o.append(text(W / 2, 290, "Works with the docker CLI, Compose and buildx", 22,
                  anchor="middle"))
    o.append("</g>")

    # 5. Where to get it.
    o.append("<g opacity=\"0\">" + fade(23, T))
    o.append(text(W / 2, 200, "github.com/SteelCrab/podbag", 34, weight=700, anchor="middle"))
    o.append(text(W / 2, 246, "Apache-2.0 · v0.1.0", 20, MUTED, anchor="middle"))
    o.append("</g>")

    o.append("</svg>")
    return "\n".join(o) + "\n"


def video(svg_text, out):
    chrome = "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"
    fps = 12
    with tempfile.TemporaryDirectory() as d:
        page = Path(d) / "intro.html"
        page.write_text(
            "<!doctype html><html><body style=\"margin:0;background:#101114\">"
            + svg_text
            + "<script>const s=document.querySelector('svg');s.pauseAnimations();"
              "s.setCurrentTime(parseFloat(location.hash.slice(1)));</script></body></html>")
        frames = int(T * fps)
        for i in range(frames):
            subprocess.run([chrome, "--headless=new", "--disable-gpu", "--hide-scrollbars",
                            f"--window-size={W},{H}", "--force-device-scale-factor=1.6",
                            f"--screenshot={d}/f{i:04d}.png", f"file://{page}#{i / fps:.3f}"],
                           check=True, capture_output=True)
        out.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-framerate", str(fps), "-i",
                        f"{d}/f%04d.png", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf",
                        "22", "-movflags", "+faststart", str(out)], check=True)


def main():
    text_svg = svg()
    (REPO / "docs/images/intro.svg").write_text(text_svg)
    print("wrote docs/images/intro.svg")
    if "--video" in sys.argv:
        video(text_svg, REPO / "docs/media/intro.mp4")
        print("wrote docs/media/intro.mp4")


if __name__ == "__main__":
    main()
