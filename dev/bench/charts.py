#!/usr/bin/env python3
"""Write the README's benchmark charts, docs/images/<name>-light.svg and -dark.svg.

The numbers are medians from docs/benchmarks.md (Linux host table) and
docs/benchmarks-msa.md; update them here when those tables change, then run
`python3 dev/bench/charts.py`.
"""
from html import escape
from pathlib import Path

CHARTS = {
    "start": (
        "Start a container",
        "ms, median of 100 starts with no network · lower is better",
        [("podbag (rootless)", 4.1), ("podbag (root)", 4.4), ("Kiri", 9.1),
         ("Podman", 46.4), ("Docker", 73.8)],
        "{:.1f} ms",
    ),
    "memory": (
        "Memory for 100 containers",
        "MiB the system holds, one network · lower is better",
        [("podbag (root)", 104.7), ("Kiri", 109.3), ("podbag (rootless)", 117.4),
         ("Podman", 203.3), ("Docker", 562.0)],
        "{:.0f} MiB",
    ),
    "disk": (
        "Disk for 8 Spring services in 2 releases",
        "MiB after their containers ran · lower is better",
        [("podbag (root)", 902), ("podbag (rootless)", 944), ("Podman", 1783),
         ("Kiri", 2960), ("Docker", 2970)],
        "{:,.0f} MiB",
    ),
    "cache": (
        "Page cache of 16 Spring services",
        "MiB read into memory to start them · lower is better",
        [("podbag (rootless)", 324), ("podbag (root)", 333), ("Kiri", 960),
         ("Podman", 975), ("Docker", 982)],
        "{:.0f} MiB",
    ),
}

# The reference palette: chart surface, ink, baseline, and categorical slot 1.
THEMES = {
    "light": {"surface": "#fcfcfb", "ink": "#0b0b0b", "ink2": "#52514e",
              "base": "#c3c2b7", "bar": "#2a78d6"},
    "dark": {"surface": "#1a1a19", "ink": "#f0efec", "ink2": "#c3c2b7",
             "base": "#383835", "bar": "#3987e5"},
}

W, LABELS, VALUES, ROW, BAR, TOP = 490, 136, 90, 30, 18, 70
FONT = "ui-sans-serif, -apple-system, 'Segoe UI', Helvetica, Arial, sans-serif"


def bar_path(x, y, w, h, r=4):
    """A bar from the baseline at x: square there, rounded at its data end."""
    if w <= r:
        return f"M{x},{y}h{w:.1f}v{h}h{-w:.1f}z"
    return (f"M{x},{y}h{w - r:.1f}a{r},{r} 0 0 1 {r},{r}v{h - 2 * r}"
            f"a{r},{r} 0 0 1 {-r},{r}h{-(w - r):.1f}z")


def chart(title, subtitle, rows, fmt, t):
    height = TOP + ROW * len(rows) + 14
    x0, span = LABELS, W - LABELS - VALUES
    top = max(v for _, v in rows)
    summary = "; ".join(f"{name} {fmt.format(v)}" for name, v in rows)
    out = [
        f'<svg xmlns="http://www.w3.org/2000/svg" width="{W}" height="{height}" '
        f'viewBox="0 0 {W} {height}" role="img" aria-label="{escape(title)}: {escape(summary)}" '
        f'font-family="{FONT}">',
        f"<title>{escape(title)}: {escape(summary)}</title>",
        f'<rect width="{W}" height="{height}" rx="10" fill="{t["surface"]}"/>',
        f'<text x="20" y="30" font-size="15" font-weight="600" fill="{t["ink"]}">'
        f"{escape(title)}</text>",
        f'<text x="20" y="49" font-size="12" fill="{t["ink2"]}">{escape(subtitle)}</text>',
    ]
    for i, (name, value) in enumerate(rows):
        y = TOP + i * ROW
        mid = y + BAR / 2 + 4
        w = max(value / top * span, 1.5)
        mine = name.startswith("podbag")
        out.append(
            f'<text x="{x0 - 10}" y="{mid:.1f}" font-size="12.5" text-anchor="end" '
            f'fill="{t["ink"] if mine else t["ink2"]}"'
            f'{" font-weight=\"600\"" if mine else ""}>{escape(name)}</text>'
        )
        out.append(f'<path d="{bar_path(x0, y, w, BAR)}" fill="{t["bar"]}"/>')
        out.append(
            f'<text x="{x0 + w + 6:.1f}" y="{mid:.1f}" font-size="12" fill="{t["ink"]}" '
            f'style="font-variant-numeric:tabular-nums">{escape(fmt.format(value))}</text>'
        )
    bottom = TOP + ROW * len(rows) - (ROW - BAR)
    out.append(
        f'<line x1="{x0}" y1="{TOP - 4}" x2="{x0}" y2="{bottom + 4}" '
        f'stroke="{t["base"]}" stroke-width="1"/>'
    )
    out.append("</svg>")
    return "\n".join(out) + "\n"


def main():
    out = Path(__file__).resolve().parents[2] / "docs" / "images"
    out.mkdir(parents=True, exist_ok=True)
    for name, (title, subtitle, rows, fmt) in CHARTS.items():
        for mode, theme in THEMES.items():
            (out / f"{name}-{mode}.svg").write_text(chart(title, subtitle, rows, fmt, theme))
    print(f"wrote {len(CHARTS) * len(THEMES)} charts to {out}")


if __name__ == "__main__":
    main()
