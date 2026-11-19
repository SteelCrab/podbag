"""Shared pieces of the animated pictures in docs/images: CSS keyframes on one loop, simple
icons, and the video renderer (frames from headless Chrome, then ffmpeg).

The animations are CSS, so they play where GitHub shows an SVG with <img>. With "reduce
motion", animations stop and each picture shows its last scene.
"""
import base64
import subprocess
import tempfile
from html import escape
from pathlib import Path

REPO = Path(__file__).resolve().parents[2]
SANS = "ui-sans-serif, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
DARK = {"bg": "#0f1013", "panel": "#1a1b20", "panel2": "#25262c", "line": "#34353c",
        "ink": "#f0efec", "muted": "#a3a29a", "accent": "#3987e5", "gray": "#6b6a64",
        "again": "#fb923c", "once": "#4ade80"}
EASE_OUT = "animation-timing-function:cubic-bezier(.2,.7,.2,1)"


class Motion:
    """Keyframe animations that all run on one loop of `loop` seconds."""

    def __init__(self, loop):
        self.loop = loop
        self.css = []
        self.count = 0

    def frames(self, stops):
        """A class for (second, declarations) stops; times may only go forward."""
        self.count += 1
        name = f"k{self.count}"
        out, last = [], -1.0
        for sec, decl in stops:
            pct = round(sec / self.loop * 100, 3)
            if pct <= last:
                pct = round(last + 0.001, 3)
            last = pct
            out.append(f"{pct:g}%{{{decl}}}")
        self.css.append(f"@keyframes {name}{{{''.join(out)}}}"
                        f".{name}{{animation:{name} {self.loop:g}s linear infinite both}}")
        return f"m {name}"

    def window(self, t0, t1, ramp=0.4):
        """Visible from t0 to t1."""
        return self.frames([(0, "opacity:0"), (t0, "opacity:0"), (t0 + ramp, "opacity:1"),
                            (t1 - ramp, "opacity:1"), (t1, "opacity:0"), (self.loop, "opacity:0")])

    def windows(self, *spans, ramp=0.3):
        """Visible in each (t0, t1) span."""
        stops = [(0, "opacity:0")]
        for t0, t1 in spans:
            stops += [(t0, "opacity:0"), (t0 + ramp, "opacity:1"), (t1 - ramp, "opacity:1"),
                      (t1, "opacity:0")]
        return self.frames(stops + [(self.loop, "opacity:0")])

    def rise(self, t0, t1, dy=14, ramp=0.5):
        """Slides up into place at t0; fades at t1."""
        up, home = f"transform:translateY({dy}px)", "transform:translateY(0px)"
        return self.frames([(0, f"opacity:0;{up}"), (t0, f"opacity:0;{up};{EASE_OUT}"),
                            (t0 + ramp, f"opacity:1;{home}"), (t1 - 0.3, f"opacity:1;{home}"),
                            (t1, f"opacity:0;{home}"), (self.loop, f"opacity:0;{home}")])

    def grow(self, t0, t1, end):
        """scaleX from 0 to 1 between t0 and t1; fades at end. Needs fill-box, origin left."""
        return self.frames([(0, "transform:scaleX(0);opacity:1"),
                            (t0, f"transform:scaleX(0);opacity:1;{EASE_OUT}"),
                            (t1, "transform:scaleX(1);opacity:1"),
                            (end - 0.3, "transform:scaleX(1);opacity:1"),
                            (end, "transform:scaleX(1);opacity:0"),
                            (self.loop, "transform:scaleX(1);opacity:0")])

    def pop(self, t0, end):
        """Scales in with a small overshoot. Needs fill-box, origin centre."""
        spring = "animation-timing-function:cubic-bezier(.2,.9,.3,1.4)"
        return self.frames([(0, "opacity:0;transform:scale(.6)"),
                            (t0, f"opacity:0;transform:scale(.6);{spring}"),
                            (t0 + 0.45, "opacity:1;transform:scale(1)"),
                            (end - 0.3, "opacity:1;transform:scale(1)"),
                            (end, "opacity:0;transform:scale(1)"),
                            (self.loop, "opacity:0;transform:scale(1)")])

    def drop(self, t0, end, dy=-70):
        """Falls into place with a small bounce."""
        return self.frames([(0, f"opacity:0;transform:translateY({dy}px)"),
                            (t0, f"opacity:0;transform:translateY({dy}px);"
                                 "animation-timing-function:cubic-bezier(.4,0,.7,1)"),
                            (t0 + 0.45, "opacity:1;transform:translateY(7px);"
                                        "animation-timing-function:ease-out"),
                            (t0 + 0.7, "opacity:1;transform:translateY(0px)"),
                            (end - 0.3, "opacity:1;transform:translateY(0px)"),
                            (end, "opacity:0;transform:translateY(0px)"),
                            (self.loop, "opacity:0;transform:translateY(0px)")])

    def move(self, dx, dy, t0, t1, vanish):
        """Appears at an offset, glides home, and fades out at vanish."""
        at, home = f"transform:translate({dx:.1f}px,{dy:.1f}px)", "transform:translate(0px,0px)"
        return self.frames([(0, f"opacity:0;{at}"), (t0, f"opacity:0;{at}"),
                            (t0 + 0.15, f"opacity:1;{at};animation-timing-function:"
                                        "cubic-bezier(.45,0,.25,1)"),
                            (t1, f"opacity:1;{home}"), (vanish, f"opacity:0;{home}"),
                            (self.loop, f"opacity:0;{home}")])

    def spin(self, t0, t1, turns=1.0):
        """Turns around its transform-origin between t0 and t1."""
        return self.frames([(0, "transform:rotate(0deg)"), (t0, "transform:rotate(0deg)"),
                            (t1, f"transform:rotate({360 * turns:g}deg)"),
                            (self.loop, f"transform:rotate({360 * turns:g}deg)")])

    def style(self):
        return ("<style>" + "".join(self.css) + ".beat{animation:beat 1.2s ease-in-out "
                "infinite}@keyframes beat{50%{opacity:.35}}"
                "@media (prefers-reduced-motion: reduce){.m,.beat{animation:none!important}}"
                "</style>")


FILL_BOX_LEFT = "transform-box:fill-box;transform-origin:0 50%"
FILL_BOX_CENTRE = "transform-box:fill-box;transform-origin:50% 50%"


def text(x, y, value, size, color, weight=None, anchor="start", font=SANS, cls=None,
         style=None):
    w = f' font-weight="{weight}"' if weight else ""
    c = f' class="{cls}"' if cls else ""
    st = f' style="{style}"' if style else ""
    f = f' font-family="{font}"' if font != SANS else ""
    return (f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size:g}" fill="{color}"{w}{f}'
            f' text-anchor="{anchor}"{c}{st}>{escape(value)}</text>')


def logo(x, y, size, cls=None, style=None):
    data = base64.b64encode((REPO / "docs/images/logo-small.png").read_bytes()).decode()
    c = f' class="{cls}"' if cls else ""
    st = f' style="{style}"' if style else ""
    return (f'<image href="data:image/png;base64,{data}" x="{x:.1f}" y="{y:.1f}" '
            f'width="{size}" height="{size}"{c}{st}/>')


# Icons: line drawings in a size × size box at (x, y).

def _g(color, s, body, cls=None, style=None):
    c = f' class="{cls}"' if cls else ""
    st = f' style="{style}"' if style else ""
    return (f'<g fill="none" stroke="{color}" stroke-width="{max(2.0, s * 0.065):.1f}" '
            f'stroke-linecap="round" stroke-linejoin="round"{c}{st}>{body}</g>')


def icon_stopwatch(x, y, s, color, hand_cls=None, cls=None, style=None):
    cx, cy, r = x + s / 2, y + s * 0.57, s * 0.36
    hand = (f'<line x1="{cx:.1f}" y1="{cy:.1f}" x2="{cx:.1f}" y2="{cy - r * 0.72:.1f}"'
            + (f' class="{hand_cls}"' if hand_cls else "")
            + f' style="transform-origin:{cx:.1f}px {cy:.1f}px"/>')
    body = (f'<circle cx="{cx:.1f}" cy="{cy:.1f}" r="{r:.1f}"/>'
            f'<line x1="{cx:.1f}" y1="{y + s * 0.1:.1f}" x2="{cx:.1f}" y2="{cy - r:.1f}"/>'
            f'<line x1="{cx - s * 0.1:.1f}" y1="{y + s * 0.1:.1f}" x2="{cx + s * 0.1:.1f}" '
            f'y2="{y + s * 0.1:.1f}"/>'
            f'<line x1="{cx + r * 0.78:.1f}" y1="{cy - r * 0.9:.1f}" x2="{cx + r * 0.98:.1f}" '
            f'y2="{cy - r * 1.1:.1f}"/>' + hand)
    return _g(color, s, body, cls, style)


def icon_chip(x, y, s, color, cls=None, style=None):
    a, w = s * 0.25, s * 0.5
    body = (f'<rect x="{x + a:.1f}" y="{y + a:.1f}" width="{w:.1f}" height="{w:.1f}" '
            f'rx="{s * 0.07:.1f}"/><rect x="{x + s * 0.38:.1f}" y="{y + s * 0.38:.1f}" '
            f'width="{s * 0.24:.1f}" height="{s * 0.24:.1f}" rx="{s * 0.03:.1f}"/>')
    for i in range(3):
        p = s * (0.34 + i * 0.16)
        body += (f'<line x1="{x + p:.1f}" y1="{y + s * 0.08:.1f}" x2="{x + p:.1f}" y2="{y + a:.1f}"/>'
                 f'<line x1="{x + p:.1f}" y1="{y + a + w:.1f}" x2="{x + p:.1f}" y2="{y + s * 0.92:.1f}"/>'
                 f'<line x1="{x + s * 0.08:.1f}" y1="{y + p:.1f}" x2="{x + a:.1f}" y2="{y + p:.1f}"/>'
                 f'<line x1="{x + a + w:.1f}" y1="{y + p:.1f}" x2="{x + s * 0.92:.1f}" y2="{y + p:.1f}"/>')
    return _g(color, s, body, cls, style)


def icon_disk(x, y, s, color, cls=None, style=None):
    cx, rx, ry = x + s / 2, s * 0.34, s * 0.11
    t, b = y + s * 0.2, y + s * 0.8
    m = (t + b) / 2
    body = (f'<ellipse cx="{cx:.1f}" cy="{t:.1f}" rx="{rx:.1f}" ry="{ry:.1f}"/>'
            f'<path d="M{cx - rx:.1f},{t:.1f} V{b:.1f} A{rx:.1f},{ry:.1f} 0 0 0 {cx + rx:.1f},{b:.1f} '
            f'V{t:.1f}"/><path d="M{cx - rx:.1f},{m:.1f} A{rx:.1f},{ry:.1f} 0 0 0 {cx + rx:.1f},{m:.1f}"/>')
    return _g(color, s, body, cls, style)


def icon_ram(x, y, s, color, cls=None, style=None):
    top, h = y + s * 0.3, s * 0.34
    body = (f'<rect x="{x + s * 0.06:.1f}" y="{top:.1f}" width="{s * 0.88:.1f}" height="{h:.1f}" '
            f'rx="{s * 0.05:.1f}"/>')
    for i in range(3):
        cx = x + s * (0.16 + i * 0.25)
        body += (f'<rect x="{cx:.1f}" y="{top + h * 0.25:.1f}" width="{s * 0.17:.1f}" '
                 f'height="{h * 0.5:.1f}" rx="{s * 0.02:.1f}"/>')
    for i in range(5):
        px = x + s * (0.16 + i * 0.17)
        body += f'<line x1="{px:.1f}" y1="{top + h:.1f}" x2="{px:.1f}" y2="{top + h + s * 0.12:.1f}"/>'
    return _g(color, s, body, cls, style)


def icon_moon(x, y, s, color, cls=None, style=None):
    cx, cy, r = x + s * 0.44, y + s * 0.56, s * 0.32
    body = (f'<path d="M{cx + r * 0.35:.1f},{cy - r:.1f} A{r:.1f},{r:.1f} 0 1 0 {cx + r:.1f},'
            f'{cy + r * 0.45:.1f} A{r * 0.8:.1f},{r * 0.8:.1f} 0 0 1 {cx + r * 0.35:.1f},{cy - r:.1f} Z"/>'
            f'<path d="M{x + s * 0.66:.1f},{y + s * 0.12:.1f} h{s * 0.16:.1f} l-{s * 0.16:.1f},'
            f'{s * 0.16:.1f} h{s * 0.16:.1f}"/>')
    return _g(color, s, body, cls, style)


def icon_layers(x, y, s, color, cls=None, style=None):
    body = ""
    for i in range(3):
        yy = y + s * (0.3 + i * 0.17)
        body += (f'<path d="M{x + s * 0.5:.1f},{yy - s * 0.12:.1f} L{x + s * 0.9:.1f},{yy:.1f} '
                 f'L{x + s * 0.5:.1f},{yy + s * 0.12:.1f} L{x + s * 0.1:.1f},{yy:.1f} Z"/>')
    return _g(color, s, body, cls, style)


def icon_file(x, y, s, color, cls=None, style=None):
    body = (f'<path d="M{x + s * 0.26:.1f},{y + s * 0.1:.1f} H{x + s * 0.6:.1f} L{x + s * 0.76:.1f},'
            f'{y + s * 0.26:.1f} V{y + s * 0.9:.1f} H{x + s * 0.26:.1f} Z"/>'
            f'<path d="M{x + s * 0.6:.1f},{y + s * 0.1:.1f} V{y + s * 0.26:.1f} H{x + s * 0.76:.1f}"/>')
    return _g(color, s, body, cls, style)


def svg_doc(w, h, motion, body, label, title, bg):
    return (f'<svg xmlns="http://www.w3.org/2000/svg" width="{w}" height="{h}" viewBox="0 0 {w} {h}" '
            f'role="img" aria-label="{escape(label)}" font-family="{SANS}">\n'
            f'<title>{escape(title)}</title>\n{motion.style()}\n'
            f'<rect width="{w}" height="{h}" rx="14" fill="{bg}"/>\n' + "\n".join(body) + "\n</svg>\n")


def render_video(svg_text, out, w, h, seconds, fps=24, scale=1.6):
    """Frames of the SVG at each moment, from one headless Chrome, then an H.264 MP4."""
    with tempfile.TemporaryDirectory() as d:
        page = Path(d) / "page.html"
        page.write_text('<!doctype html><html><body style="margin:0;background:#000">'
                        + svg_text + "</body></html>")
        subprocess.run(["node", str(REPO / "dev/docs/frames.mjs"), page.as_uri(), d, str(w),
                        str(h), str(scale), str(fps), str(seconds)], check=True)
        out.parent.mkdir(parents=True, exist_ok=True)
        subprocess.run(["ffmpeg", "-y", "-loglevel", "error", "-framerate", str(fps), "-i",
                        f"{d}/f%05d.png", "-c:v", "libx264", "-pix_fmt", "yuv420p", "-crf", "22",
                        "-preset", "slow", "-movflags", "+faststart", str(out)], check=True)
    print("wrote", out)
