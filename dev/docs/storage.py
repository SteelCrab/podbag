#!/usr/bin/env python3
"""docs/images/storage-*.svg: an animated story of two images, stored as layers and as files.

Image A and image B share most of their files. As layers, image B's top layer differs from
image A's by one file, so it is a new layer: the files it shares with image A are stored
again. As files, each file is stored once. The last scene shows the disk use measured in
docs/benchmarks-msa.md.

One picture per language, theme and shape: storage-light.svg and storage-dark.svg in English,
storage-<lang>-light.svg and storage-<lang>-dark.svg in the other languages, and a "-tall"
version of each for narrow screens. The motion is CSS; with "reduce motion" the picture
shows its last scene only.

usage: python3 dev/docs/storage.py
"""
import unicodedata
from html import escape
from pathlib import Path

LOOP = 20.0  # seconds
W, H = 800, 490
FONT = ("ui-sans-serif, -apple-system, 'Segoe UI', 'Apple SD Gothic Neo', 'Malgun Gothic', "
        "'Hiragino Sans', 'Yu Gothic', 'PingFang SC', 'Microsoft YaHei', 'Noto Sans CJK KR', "
        "Helvetica, Arial, sans-serif")
MONO = "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
THEMES = {
    "light": {"surface": "#fcfcfb", "disk": "#f2f1ed", "box": "#e6e4dc", "line": "#c3c2b7",
              "ink": "#0b0b0b", "ink2": "#52514e", "chip": "#ffffff", "chipline": "#c3c2b7",
              "accent": "#2a78d6", "on": "#ffffff", "again": "#c2410c", "on_again": "#ffffff",
              "once": "#15803d"},
    "dark": {"surface": "#1a1a19", "disk": "#232321", "box": "#34342f", "line": "#4a4a44",
             "ink": "#f0efec", "ink2": "#c3c2b7", "chip": "#1c1c1a", "chipline": "#5a5a53",
             "accent": "#2a78d6", "on": "#ffffff", "again": "#fb923c", "on_again": "#1a1a19",
             "once": "#4ade80"},
}

# The words in the picture. File names stay as they are.
TEXT = {
    "en": {
        "c1": "Two images share most of their files",
        "c2": "Layers: one file differs, so the whole layer is stored again",
        "c3": "Files: podbag stores each file once",
        "c4": "Measured with 8 Spring services in 2 releases",
        "imga": "image A", "imgb": "image B", "legend": "in both images",
        "layers": "Layers", "files": "Files",
        "kept": "✓ same layer: kept once",
        "cl": "8 files stored", "cl2": " · 2 of them twice",
        "cf": "6 files stored", "cf2": " · each once",
        "disk": "Disk", "group": ",",
        "title": "Two images stored as layers, and as files",
        "aria": "Animation. Two images share most of their files. As layers, a layer is kept "
                "once only when it is the same; image B's top layer differs by one file, so "
                "spring.jar and jackson.jar are stored again: 8 files. podbag stores each file "
                "once: 6 files. Measured with 8 Spring services in 2 releases: 2,970 MiB on "
                "disk with layers (Docker), 902 MiB with podbag.",
    },
    "ko": {
        "c1": "두 이미지는 파일 대부분이 같습니다",
        "c2": "레이어 방식: 파일 하나만 달라도 레이어 전체를 다시 저장합니다",
        "c3": "파일 방식: podbag은 파일마다 한 번만 저장합니다",
        "c4": "실측: Spring 서비스 8개, 릴리스 2개",
        "imga": "이미지 A", "imgb": "이미지 B", "legend": "두 이미지에 모두 있는 파일",
        "layers": "레이어", "files": "파일",
        "kept": "✓ 같은 레이어: 한 번만 저장",
        "cl": "파일 8개 저장", "cl2": " · 2개는 두 번",
        "cf": "파일 6개 저장", "cf2": " · 모두 한 번",
        "disk": "디스크", "group": ",",
        "title": "두 이미지를 레이어로, 그리고 파일로 저장할 때",
        "aria": "애니메이션. 두 이미지는 파일 대부분이 같습니다. 레이어 방식은 레이어가 똑같을 "
                "때만 한 번 저장합니다. 이미지 B의 위쪽 레이어는 파일 하나가 달라서 spring.jar와 "
                "jackson.jar를 다시 저장합니다: 파일 8개. podbag은 파일마다 한 번만 저장합니다: "
                "파일 6개. Spring 서비스 8개, 릴리스 2개로 잰 디스크: 레이어(Docker) 2,970 MiB, "
                "podbag 902 MiB.",
    },
    "ja": {
        "c1": "2つのイメージは、ほとんどのファイルが同じです",
        "c2": "レイヤー方式: ファイルが1つ違うだけで、レイヤー全体をもう一度保存します",
        "c3": "ファイル方式: podbag はファイルを1回だけ保存します",
        "c4": "実測: Spring サービス8個、2リリース",
        "imga": "イメージ A", "imgb": "イメージ B", "legend": "両方のイメージにあるファイル",
        "layers": "レイヤー", "files": "ファイル",
        "kept": "✓ 同じレイヤー: 1回だけ保存",
        "cl": "8ファイルを保存", "cl2": " · うち2つは2回",
        "cf": "6ファイルを保存", "cf2": " · すべて1回",
        "disk": "ディスク", "group": ",",
        "title": "2つのイメージをレイヤーとファイルで保存する",
        "aria": "アニメーション。2つのイメージは、ほとんどのファイルが同じです。レイヤー方式では、"
                "同じレイヤーだけが1回で済みます。イメージ B の上のレイヤーはファイルが1つ違うので、"
                "spring.jar と jackson.jar をもう一度保存します: 8ファイル。podbag はファイルを"
                "1回だけ保存します: 6ファイル。Spring サービス8個・2リリースの実測ディスク: "
                "レイヤー (Docker) 2,970 MiB、podbag 902 MiB。",
    },
    "zh-CN": {
        "c1": "两个镜像的大部分文件相同",
        "c2": "按层存储：只要一个文件不同，整层就要再存一次",
        "c3": "按文件存储：podbag 每个文件只存一次",
        "c4": "实测：8 个 Spring 服务，2 个版本",
        "imga": "镜像 A", "imgb": "镜像 B", "legend": "两个镜像都有的文件",
        "layers": "按层", "files": "按文件",
        "kept": "✓ 相同的层：只存一次",
        "cl": "存了 8 个文件", "cl2": " · 其中 2 个存了两次",
        "cf": "存了 6 个文件", "cf2": " · 每个只存一次",
        "disk": "磁盘", "group": ",",
        "title": "两个镜像按层存储与按文件存储",
        "aria": "动画。两个镜像的大部分文件相同。按层存储时，只有完全相同的层才只存一次；镜像 B 的"
                "上层有一个文件不同，所以 spring.jar 和 jackson.jar 又存了一次：共 8 个文件。"
                "podbag 每个文件只存一次：共 6 个文件。8 个 Spring 服务、2 个版本的实测磁盘用量："
                "按层（Docker）2,970 MiB，podbag 902 MiB。",
    },
    "fil": {
        "c1": "Halos pareho ang mga file ng dalawang image",
        "c2": "Mga layer: kapag may isang file na iba, iniimbak ulit ang buong layer",
        "c3": "Mga file: iniimbak ng podbag ang bawat file nang isang beses",
        "c4": "Sinukat sa 8 Spring service sa 2 release",
        "imga": "image A", "imgb": "image B", "legend": "nasa dalawang image",
        "layers": "Mga layer", "files": "Mga file",
        "kept": "✓ parehong layer: isang beses lang",
        "cl": "8 file ang nakaimbak", "cl2": " · 2 rito ay dalawang beses",
        "cf": "6 file ang nakaimbak", "cf2": " · tig-iisang beses",
        "disk": "Disk", "group": ",",
        "title": "Dalawang image na iniimbak bilang mga layer, at bilang mga file",
        "aria": "Animation. Halos pareho ang mga file ng dalawang image. Sa mga layer, isang "
                "beses lang iniimbak ang layer kapag pareho ito; iba ang isang file sa itaas na "
                "layer ng image B, kaya iniimbak ulit ang spring.jar at jackson.jar: 8 file. "
                "Iniimbak ng podbag ang bawat file nang isang beses: 6 file. Sinukat sa 8 Spring "
                "service sa 2 release: 2,970 MiB sa disk gamit ang mga layer (Docker), 902 MiB "
                "gamit ang podbag.",
    },
    "es": {
        "c1": "Dos imágenes comparten la mayoría de sus archivos",
        "c2": "Capas: si un solo archivo es distinto, toda la capa se guarda otra vez",
        "c3": "Archivos: podbag guarda cada archivo una sola vez",
        "c4": "Medido con 8 servicios Spring en 2 versiones",
        "imga": "imagen A", "imgb": "imagen B", "legend": "en las dos imágenes",
        "layers": "Capas", "files": "Archivos",
        "kept": "✓ misma capa: se guarda una vez",
        "cl": "8 archivos guardados", "cl2": " · 2 de ellos dos veces",
        "cf": "6 archivos guardados", "cf2": " · cada uno una vez",
        "disk": "Disco", "group": ".",
        "title": "Dos imágenes guardadas por capas y por archivos",
        "aria": "Animación. Dos imágenes comparten la mayoría de sus archivos. Con capas, una "
                "capa se guarda una vez solo si es igual; la capa superior de la imagen B tiene "
                "un archivo distinto, así que spring.jar y jackson.jar se guardan otra vez: 8 "
                "archivos. podbag guarda cada archivo una vez: 6 archivos. Medido con 8 servicios "
                "Spring en 2 versiones: 2.970 MiB de disco con capas (Docker), 902 MiB con podbag.",
    },
    "vi": {
        "c1": "Hai image có hầu hết các tệp giống nhau",
        "c2": "Lớp: chỉ cần một tệp khác, cả lớp được lưu lại lần nữa",
        "c3": "Tệp: podbag lưu mỗi tệp một lần",
        "c4": "Đo với 8 dịch vụ Spring trong 2 bản phát hành",
        "imga": "image A", "imgb": "image B", "legend": "có trong cả hai image",
        "layers": "Lớp", "files": "Tệp",
        "kept": "✓ cùng lớp: chỉ lưu một lần",
        "cl": "Lưu 8 tệp", "cl2": " · 2 tệp lưu hai lần",
        "cf": "Lưu 6 tệp", "cf2": " · mỗi tệp một lần",
        "disk": "Đĩa", "group": ".",
        "title": "Hai image được lưu theo lớp và theo tệp",
        "aria": "Hoạt ảnh. Hai image có hầu hết các tệp giống nhau. Khi lưu theo lớp, một lớp "
                "chỉ được lưu một lần khi nó giống hệt; lớp trên của image B khác một tệp, nên "
                "spring.jar và jackson.jar được lưu lại: 8 tệp. podbag lưu mỗi tệp một lần: 6 "
                "tệp. Đo với 8 dịch vụ Spring trong 2 bản phát hành: 2.970 MiB đĩa khi lưu theo "
                "lớp (Docker), 902 MiB với podbag.",
    },
}

LAYERS_TAG = "Docker, Podman, Kiri"
BASE = ["linux", "java"]
A_TOP = ["spring.jar", "jackson.jar", "orders.jar"]
B_TOP = ["spring.jar", "jackson.jar", "payments.jar"]
SHARED = {"linux", "java", "spring.jar", "jackson.jar"}
STORE = [["linux", "java", "spring.jar"], ["jackson.jar", "orders.jar", "payments.jar"]]
DISK_LAYERS, DISK_FILES = 2970, 902  # MiB, docs/benchmarks-msa.md

CH, CG, BP, BH = 22, 6, 8, 34  # chip height, gap between chips, box padding, box height


def text_width(s, size, mono=False):
    """A rough width, enough to keep text inside its box."""
    em = 0.0
    for ch in s:
        if unicodedata.east_asian_width(ch) in "WF":
            em += 1.0
        elif mono:
            em += 0.6
        elif ch in "il.,:;'|!·":
            em += 0.28
        elif ch == " ":
            em += 0.3
        elif ch in "mwMW✓":
            em += 0.86
        elif ch.isupper() or ch.isdigit():
            em += 0.64
        else:
            em += 0.54
    return em * size


def fit(s, size, width, mono=False):
    while size > 9 and text_width(s, size, mono) > width:
        size -= 0.5
    return size


def chip_w(name):
    return text_width(name, 12, mono=True) + 16


def row_w(names, gap=CG):
    return sum(chip_w(n) for n in names) + gap * (len(names) - 1)


BW = max(row_w(A_TOP), row_w(B_TOP), row_w(BASE)) + 2 * BP


class Layout:
    """Where everything sits. WIDE puts the images and the disks side by side; TALL stacks
    them for narrow screens."""

    def __init__(self, **kw):
        self.__dict__.update(kw)


WIDE = Layout(
    w=800, h=490, suffix="",
    caption=(400, 34, 17, 760),               # x, baseline, size, width
    towers=[(400 - 15 - BW, 72), (415, 72)],  # x, label baseline; boxes 10 and 50 below
    legend=(400, 173),                        # centre, top of the square
    layers=(14, 224, 380, 190), files=(406, 224, 380, 190),  # x, y, w, h
    slots=(238, 280, 322),                    # A's top layer, B's top layer, the base layer
    box_x=70, rows=(262, 306),
    counters=((22, 398), (414, 398)),
    meters=((22, 444), (414, 444)), bar=360,
)
TALL = Layout(
    w=420, h=810, suffix="-tall",
    caption=(210, 26, 15, 396),
    towers=[((420 - BW) / 2, 82), ((420 - BW) / 2, 186)],
    legend=(210, 278),
    layers=(10, 330, 400, 184), files=(10, 558, 400, 140),
    slots=(344, 386, 428),
    box_x=66, rows=(584, 628),
    counters=((18, 498), (18, 682)),
    meters=((18, 730), (18, 774)), bar=380,
)


def number(n, group):
    return f"{n:,}".replace(",", group)


def chips_at(x, y, names, gap=CG):
    """(name, x, y) of each chip in a row."""
    out = []
    for n in names:
        out.append((n, x, y))
        x += chip_w(n) + gap
    return out


def store_slots(L):
    x0, _, w, _ = L.files
    slots = {}
    for row, y in zip(STORE, L.rows):
        for name, cx, cy in chips_at(x0 + w / 2 - row_w(row, 10) / 2, y, row, 10):
            slots[name] = (cx, cy)
    return slots


def tower_chips(L, i):
    """Where each file sits in image A (i = 0) or B (i = 1): {name: (x, y)}."""
    x, label = L.towers[i]
    pos = {n: (cx, cy) for n, cx, cy in chips_at(x + BP, label + 16, A_TOP if i == 0 else B_TOP)}
    pos.update({n: (cx, cy) for n, cx, cy in chips_at(x + BP, label + 56, BASE)})
    return pos


def lines_of(s, size, width):
    """Split a caption into lines that fit the width."""
    if text_width(s, size) <= width:
        return [s]
    cjk = any(unicodedata.east_asian_width(c) in "WF" for c in s)
    words, sep = (list(s), "") if cjk else (s.split(" "), " ")
    lines, cur = [], ""
    for word in words:
        trial = cur + (sep if cur else "") + word
        if cur and text_width(trial, size) > width:
            lines.append(cur)
            cur = word.lstrip()
        else:
            cur = trial
    return lines + [cur]


def svg(lang, t, L):
    s = TEXT[lang]
    o, css = [], []
    count = [0]

    def anim(frames):
        """A keyframe animation over the whole loop; frames are (second, declarations)."""
        count[0] += 1
        name = f"k{count[0]}"
        stops, last = [], -1.0
        for sec, decl in frames:
            pct = round(sec / LOOP * 100, 3)
            if pct <= last:
                pct = round(last + 0.001, 3)
            last = pct
            stops.append(f"{pct:g}%{{{decl}}}")
        css.append(f"@keyframes {name}{{{''.join(stops)}}}"
                   f".{name}{{animation:{name} {LOOP:g}s linear infinite both}}")
        return f"m {name}"

    def show(t0, end=19.2):
        return anim([(0, "opacity:0"), (t0, "opacity:0"), (t0 + 0.3, "opacity:1"),
                     (end, "opacity:1"), (end + 0.4, "opacity:0"), (LOOP, "opacity:0")])

    def window(t0, t1):
        return anim([(0, "opacity:0"), (t0, "opacity:0"), (t0 + 0.3, "opacity:1"),
                     (t1 - 0.3, "opacity:1"), (t1, "opacity:0"), (LOOP, "opacity:0")])

    def flash(t0):
        return anim([(0, "opacity:0"), (t0, "opacity:0"), (t0 + 0.2, "opacity:1"),
                     (t0 + 0.8, "opacity:0"), (LOOP, "opacity:0")])

    def move(dx, dy, t0, t1, vanish=None):
        """Appear at an offset from the final place, then glide there."""
        at = f"transform:translate({dx:.1f}px,{dy:.1f}px)"
        home = "transform:translate(0px,0px)"
        f = [(0, f"opacity:0;{at}"), (t0, f"opacity:0;{at}"),
             (t0 + 0.15, f"opacity:1;{at};animation-timing-function:cubic-bezier(.45,0,.25,1)"),
             (t1, f"opacity:1;{home}")]
        if vanish is None:
            f += [(19.2, f"opacity:1;{home}"), (19.6, f"opacity:0;{home}"),
                  (LOOP, f"opacity:0;{home}")]
        else:
            f += [(vanish, f"opacity:0;{home}"), (LOOP, f"opacity:0;{home}")]
        return anim(f)

    def text(x, y, value, size, color="ink2", weight=None, anchor="start", cls=None,
             hidden=False):
        w = f' font-weight="{weight}"' if weight else ""
        c = f' class="{cls}"' if cls else ""
        h = ' opacity="0"' if hidden else ""
        o.append(f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size:g}" fill="{t[color]}"{w}'
                 f' text-anchor="{anchor}"{c}{h}>{escape(value)}</text>')

    def caption(value, cls, hidden):
        x, y, size, width = L.caption
        lines = lines_of(value, size, width)
        while len(lines) > 2 and size > 11:
            size -= 0.5
            lines = lines_of(value, size, width)
        spans = "".join(f'<tspan x="{x:.1f}" dy="{0 if i == 0 else size * 1.25:.1f}">'
                        f'{escape(line)}</tspan>' for i, line in enumerate(lines))
        h = ' opacity="0"' if hidden else ""
        o.append(f'<text x="{x:.1f}" y="{y:.1f}" font-size="{size:g}" fill="{t["ink"]}" '
                 f'font-weight="700" text-anchor="middle" class="{cls}"{h}>{spans}</text>')

    def chip(x, y, name, kind):
        w = chip_w(name)
        if kind == "shared":
            fill, stroke, color = t["accent"], t["accent"], t["on"]
        elif kind == "again":
            fill, stroke, color = t["again"], t["again"], t["on_again"]
        else:
            fill, stroke, color = t["chip"], t["chipline"], t["ink"]
        o.append(f'<rect x="{x:.1f}" y="{y:.1f}" width="{w:.1f}" height="{CH}" rx="5" '
                 f'fill="{fill}" stroke="{stroke}"/>')
        o.append(f'<text x="{x + w / 2:.1f}" y="{y + 15:.1f}" font-family="{MONO}" '
                 f'font-size="12" fill="{color}" text-anchor="middle">{escape(name)}</text>')

    def kind(name):
        return "shared" if name in SHARED else "plain"

    def box(x, y, names):
        o.append(f'<rect x="{x:.1f}" y="{y}" width="{BW:.1f}" height="{BH}" rx="7" '
                 f'fill="{t["box"]}" stroke="{t["line"]}"/>')
        for n, cx, cy in chips_at(x + BP, y + 6, names):
            chip(cx, cy, n, kind(n))

    def group(cls, hidden=False):
        h = ' opacity="0"' if hidden else ""
        o.append(f'<g class="{cls}"{h}>')

    def ring(x, y, w, h, color, cls):
        o.append(f'<rect class="{cls}" opacity="0" x="{x - 3:.1f}" y="{y - 3:.1f}" '
                 f'width="{w + 6:.1f}" height="{h + 6:.1f}" rx="8" fill="none" '
                 f'stroke="{t[color]}" stroke-width="2.5"/>')

    lx0 = L.layers[0]

    def tag(y, label, cls, hidden=False):
        h = ' opacity="0"' if hidden else ""
        o.append(f'<g class="{cls}"{h}><rect x="{lx0 + 8}" y="{y + 7}" width="44" height="20" '
                 f'rx="10" fill="{t["surface"]}" stroke="{t["line"]}"/>'
                 f'<text x="{lx0 + 30}" y="{y + 21}" font-size="11" font-weight="700" '
                 f'fill="{t["ink2"]}" text-anchor="middle">{label}</text></g>')

    o.append(f'<svg xmlns="http://www.w3.org/2000/svg" width="{L.w}" height="{L.h}" '
             f'viewBox="0 0 {L.w} {L.h}" lang="{lang}" xml:lang="{lang}" role="img" '
             f'aria-label="{escape(s["aria"])}" font-family="{FONT}">')
    o.append(f"<title>{escape(s['title'])}</title>")
    style_at = len(o)
    o.append(f'<rect width="{L.w}" height="{L.h}" rx="12" fill="{t["surface"]}"/>')

    # The words of each scene, one at a time at the top.
    for key, cls, hidden in [("c1", window(0.2, 3.0), True), ("c2", window(3.0, 8.6), True),
                             ("c3", window(8.6, 13.6), True), ("c4", show(13.6), False)]:
        caption(s[key], cls, hidden)

    # The two images.
    for i, label in enumerate([s["imga"], s["imgb"]]):
        x, ly = L.towers[i]
        text(x + BW / 2, ly, label, fit(label, 13, BW), "ink", 700, "middle")
        box(x, ly + 10, A_TOP if i == 0 else B_TOP)
        box(x, ly + 50, BASE)
    rings = flash(0.9)
    for i in (0, 1):
        for n, (cx, cy) in tower_chips(L, i).items():
            if n in SHARED:
                ring(cx, cy, chip_w(n), CH, "accent", rings)
    cx, top = L.legend
    size = fit(s["legend"], 12.5, L.w - 40)
    lw = 13 + 7 + text_width(s["legend"], size)
    o.append(f'<rect x="{cx - lw / 2:.1f}" y="{top}" width="13" height="13" rx="3" '
             f'fill="{t["accent"]}"/>')
    text(cx - lw / 2 + 20, top + 11, s["legend"], size)

    # The two disks.
    for (x, y, w, h), title, sub in [(L.layers, s["layers"], LAYERS_TAG),
                                     (L.files, s["files"], "podbag")]:
        o.append(f'<text x="{x + 8}" y="{y - 12}" font-size="16" font-weight="700" '
                 f'fill="{t["ink"]}">{escape(title)}<tspan dx="8" font-size="12.5" '
                 f'font-weight="400" fill="{t["ink2"]}">{escape(sub)}</tspan></text>')
        o.append(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="10" '
                 f'fill="{t["disk"]}" stroke="{t["line"]}"/>')

    # Layers: A's two layers, then B's base (the same layer: kept once), then B's top layer
    # (a new layer, so the shared jars are stored again).
    s1, s2, s3 = L.slots
    bx = L.box_x
    (ax, al), (bxx, bl) = L.towers
    tag(s1, "A", show(4.0))
    group(move(ax - bx, al + 10 - s1, 3.2, 4.0))
    box(bx, s1, A_TOP)
    o.append("</g>")
    group(move(ax - bx, al + 50 - s3, 4.2, 5.0))
    box(bx, s3, BASE)
    o.append("</g>")
    tag(s3, "A", window(5.0, 6.5), hidden=True)
    group(move(bxx - bx, bl + 50 - s3, 5.4, 6.2, vanish=6.5), hidden=True)
    box(bx, s3, BASE)
    o.append("</g>")
    ring(bx, s3, BW, BH, "once", flash(6.2))
    tag(s3, "A+B", show(6.5))
    kx = bx + BP + row_w(BASE) + 12
    size = fit(s["kept"], 12, bx + BW - 8 - kx)
    text(kx, s3 + 22, s["kept"], size, "once", 700, cls=show(6.5))
    group(move(bxx - bx, bl + 10 - s2, 6.9, 7.7))
    box(bx, s2, B_TOP)
    o.append("</g>")
    tag(s2, "B", show(7.7))
    group(show(7.9))
    for n, cx, cy in chips_at(bx + BP, s2 + 6, B_TOP):
        if n in SHARED:
            chip(cx, cy, n, "again")
    o.append("</g>")

    def counter(at, main, extra, color, cls, width):
        x, y = at
        size = fit(main + extra, 13, width)
        o.append(f'<text class="{cls}" x="{x}" y="{y}" font-size="{size:g}">'
                 f'<tspan font-weight="700" fill="{t["ink"]}">{escape(main)}</tspan>'
                 f'<tspan fill="{t[color]}" font-weight="600">{escape(extra)}</tspan></text>')

    counter(L.counters[0], s["cl"], s["cl2"], "again", show(8.3), L.layers[2] - 16)

    # Files: A's files go into the store; B's shared files find their copy there and merge;
    # only B's own file is new.
    slots = store_slots(L)
    a_pos, b_pos = tower_chips(L, 0), tower_chips(L, 1)
    for i, n in enumerate(BASE + A_TOP):
        sx, sy = a_pos[n]
        x, y = slots[n]
        t0 = 8.9 + i * 0.16
        group(move(sx - x, sy - y, t0, t0 + 0.8))
        chip(x, y, n, kind(n))
        o.append("</g>")
    for i, n in enumerate(BASE + B_TOP):
        sx, sy = b_pos[n]
        x, y = slots[n]
        t0 = 10.7 + i * 0.16
        if n in SHARED:
            group(move(sx - x, sy - y, t0, t0 + 0.8, vanish=t0 + 1.15), hidden=True)
            chip(x, y, n, kind(n))
            o.append("</g>")
            ring(x, y, chip_w(n), CH, "once", flash(t0 + 0.8))
        else:
            group(move(sx - x, sy - y, t0, t0 + 0.8))
            chip(x, y, n, kind(n))
            o.append("</g>")
    counter(L.counters[1], s["cf"], s["cf2"], "once", show(12.5), L.files[2] - 16)

    # What it comes to, measured.
    grow = anim([(0, "transform:scaleX(0)"),
                 (14.0, "transform:scaleX(0);animation-timing-function:cubic-bezier(.2,.7,.2,1)"),
                 (15.0, "transform:scaleX(1)"), (19.2, "transform:scaleX(1);opacity:1"),
                 (19.6, "transform:scaleX(1);opacity:0"), (LOOP, "transform:scaleX(1);opacity:0")])
    for (x, y), mib in zip(L.meters, (DISK_LAYERS, DISK_FILES)):
        label = f"{s['disk']} {number(mib, s['group'])} MiB"
        text(x, y, label, 14, "ink", 700, cls=show(13.8))
        o.append(f'<rect class="{grow}" x="{x}" y="{y + 8}" width="{L.bar * mib / DISK_LAYERS:.1f}" '
                 f'height="14" rx="4" fill="{t["accent"]}" '
                 f'style="transform-box:fill-box;transform-origin:0 50%"/>')

    o.append("</svg>")
    css.append("@media (prefers-reduced-motion: reduce){.m{animation:none!important}}")
    o.insert(style_at, "<style>" + "".join(css) + "</style>")
    return "\n".join(o) + "\n"


def main():
    out = Path(__file__).resolve().parents[2] / "docs" / "images"
    n = 0
    for lang in TEXT:
        for L in (WIDE, TALL):
            for mode, theme in THEMES.items():
                stem = "storage" if lang == "en" else f"storage-{lang}"
                (out / f"{stem}{L.suffix}-{mode}.svg").write_text(svg(lang, theme, L))
                n += 1
    print(f"wrote {n} pictures in {out}")


if __name__ == "__main__":
    main()
