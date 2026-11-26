//! The icon sprite of desktop/ui/index.html, as path data for the icon registry of podbag-ui.

use podbag_ui::icons::{IconDef, register};

/// Icon ids: the order of `ICONS`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u16)]
#[allow(dead_code)] // the whole sprite of index.html, used or not
pub enum Icon {
    Tutorials,
    Containers,
    Images,
    Volumes,
    Networks,
    Kubernetes,
    KubePods,
    KubeServices,
    KubeDeployments,
    KubeClaims,
    Migration,
    Settings,
    Start,
    Stop,
    Restart,
    Pause,
    Remove,
    Logs,
    Shell,
    Inspect,
    Search,
    Refresh,
    Pull,
    Prune,
    Back,
    Copy,
    Rename,
    Sidebar,
    Forward,
    Switch,
    Undo,
    Vm,
    Update,
    Power,
    Globe,
    Appearance,
    Display,
    Sun,
    Moon,
    Warning,
    Close,
    Engine,
    Star,
    Check,
    Import,
}

impl Icon {
    pub fn id(self) -> u16 {
        self as u16
    }
}

fn circle(cx: f32, cy: f32, r: f32) -> String {
    ellipse(cx, cy, r, r)
}

fn ellipse(cx: f32, cy: f32, rx: f32, ry: f32) -> String {
    format!(
        "M{} {}a{rx} {ry} 0 1 0 {} 0a{rx} {ry} 0 1 0 {} 0",
        cx - rx,
        cy,
        2. * rx,
        -2. * rx
    )
}

fn rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> String {
    format!(
        "M{} {y}h{}a{r} {r} 0 0 1 {r} {r}v{}a{r} {r} 0 0 1 {} {r}h{}a{r} {r} 0 0 1 {} {}v{}a{r} {r} 0 0 1 {r} {}z",
        x + r,
        w - 2. * r,
        h - 2. * r,
        -r,
        -(w - 2. * r),
        -r,
        -r,
        -(h - 2. * r),
        -r
    )
}

/// Join the shapes of one icon. Each SVG element starts a new path, where a leading
/// relative move ("m16 16 4.5 4.5") is relative to the origin.
fn leak(parts: &[String]) -> &'static str {
    let joined: Vec<String> = parts
        .iter()
        .map(|p| match p.strip_prefix('m') {
            Some(rest) => format!("M0 0m{rest}"),
            None => p.clone(),
        })
        .collect();
    Box::leak(joined.join(" ").into_boxed_str())
}

fn s(d: &'static str) -> IconDef {
    IconDef {
        stroke: d,
        fill: "",
        width: 1.8,
    }
}

fn sp(parts: &[String]) -> IconDef {
    s(leak(parts))
}

/// Register the icons with own-ui (once, at start).
pub fn install() {
    let p = |d: &str| d.to_string();
    let icons = vec![
        s(
            "M12 6.5C9 4.5 5.5 4.5 2.5 5v13c3-.5 6.5-.5 9.5 1.5 3-2 6.5-2 9.5-1.5V5c-3-.5-6.5-.5-9.5 1.5Zm0 0v13",
        ),
        s("M3 7.5 12 3l9 4.5v9L12 21l-9-4.5zM3 7.5 12 12l9-4.5M12 12v9"),
        s("m12 3 9 4.5-9 4.5-9-4.5zm-9 9 9 4.5 9-4.5m-18 4.5 9 4.5 9-4.5"),
        sp(&[
            ellipse(12., 5.5, 8., 2.5),
            p("M4 5.5v13c0 1.4 3.6 2.5 8 2.5s8-1.1 8-2.5v-13M4 12c0 1.4 3.6 2.5 8 2.5s8-1.1 8-2.5"),
        ]),
        sp(&[
            rect(9., 3., 6., 5., 1.),
            rect(3., 16., 6., 5., 1.),
            rect(15., 16., 6., 5., 1.),
            p("M12 8v4M6 16v-2h12v2"),
        ]),
        sp(&[
            p("m12 2 8.2 4 2 8.8-5.7 7H7.5l-5.7-7 2-8.8z"),
            circle(12., 12., 3.),
            p("M12 5.5V9m5.6-.2-3 1.7m3 5-3-1.8M12 18.5V15m-5.6.5 3-1.8m-3-4.9 3 1.7"),
        ]),
        s("m12 2 8.5 5v10L12 22l-8.5-5V7zm0 5 4.5 2.5v5L12 17l-4.5-2.5v-5z"),
        sp(&[
            circle(12., 5., 2.5),
            circle(5., 18.5, 2.5),
            circle(19., 18.5, 2.5),
            p("M12 7.5V12M5 16v-4h14v4"),
        ]),
        sp(&[p("M4 15V4h12M8 18V8h12"), rect(12., 12., 10., 9., 1.5)]),
        sp(&[
            ellipse(11., 5.5, 7.5, 3.),
            p("M3.5 5.5v12c0 1.7 3.4 3 7.5 3M18.5 5.5v5M3.5 11.5c0 1.7 3.4 3 7.5 3m2 2.5 3 3 5-6"),
        ]),
        s("M3 7h15m-4-4 4 4-4 4M21 17H6m4-4-4 4 4 4"),
        sp(&[
            circle(12., 12., 3.),
            p(
                "M19.4 15a1.7 1.7 0 0 0 .3 1.8l.1.1a2 2 0 1 1-2.8 2.8l-.1-.1a1.7 1.7 0 0 0-1.8-.3 1.7 1.7 0 0 0-1 1.5V21a2 2 0 1 1-4 0v-.1a1.7 1.7 0 0 0-1.1-1.5 1.7 1.7 0 0 0-1.8.3l-.1.1a2 2 0 1 1-2.8-2.8l.1-.1a1.7 1.7 0 0 0 .3-1.8 1.7 1.7 0 0 0-1.5-1H3a2 2 0 1 1 0-4h.1a1.7 1.7 0 0 0 1.5-1.1 1.7 1.7 0 0 0-.3-1.8l-.1-.1a2 2 0 1 1 2.8-2.8l.1.1a1.7 1.7 0 0 0 1.8.3H9a1.7 1.7 0 0 0 1-1.5V3a2 2 0 1 1 4 0v.1a1.7 1.7 0 0 0 1 1.5 1.7 1.7 0 0 0 1.8-.3l.1-.1a2 2 0 1 1 2.8 2.8l-.1.1a1.7 1.7 0 0 0-.3 1.8V9a1.7 1.7 0 0 0 1.5 1H21a2 2 0 1 1 0 4h-.1a1.7 1.7 0 0 0-1.5 1z",
            ),
        ]),
        s("M7 4.5v15l12-7.5z"),
        sp(&[rect(6., 6., 12., 12., 1.5)]),
        s("M20 12a8 8 0 1 1-2.3-5.7M20 4v5h-5"),
        s("M8 5v14M16 5v14"),
        s("M4 7h16M9 7V4.5h6V7M6 7l1 13h10l1-13M10 11v6M14 11v6"),
        s("M5 4h10l4 4v12H5zM8 11h8M8 14.5h8M8 18h5"),
        sp(&[rect(3., 4., 18., 16., 2.), p("m7 9 3 3-3 3M12.5 15H17")]),
        sp(&[circle(12., 12., 9.), p("M12 11v6M12 7.5v.5")]),
        sp(&[circle(11., 11., 6.5), p("m16 16 4.5 4.5")]),
        s(
            "M4 12a8 8 0 0 1 13.7-5.7L20 8.5M20 4v4.5h-4.5M20 12a8 8 0 0 1-13.7 5.7L4 15.5M4 20v-4.5h4.5",
        ),
        s("M12 4v11M7 10.5l5 5 5-5M5 20h14"),
        s("M14 4 9.5 12.5M6 12.5h8.5l1.5 7.5H4.5zM8 16.5v3.5M12 16.5v3.5"),
        s("M15 5 8 12l7 7"),
        sp(&[
            rect(8., 8., 12., 12., 2.),
            p("M16 8V5a1 1 0 0 0-1-1H5a1 1 0 0 0-1 1v10a1 1 0 0 0 1 1h3"),
        ]),
        s("M4 20h4L19 9l-4-4L4 16zm9.5-13.5 4 4"),
        sp(&[
            rect(3., 4.5, 18., 15., 2.),
            p("M9 4.5v15M5.5 8.5h1.5M5.5 11.5h1.5"),
        ]),
        s("m9 5 7 7-7 7"),
        s("M4 8h15m-4-4 4 4-4 4M20 16H5m4-4-4 4 4 4"),
        s("m9 4-5 5 5 5M5 9h9a6 6 0 0 1 0 12"),
        sp(&[
            rect(6., 6., 12., 12., 2.),
            p("M10 10h4v4h-4zM9 3v3M15 3v3M9 18v3M15 18v3M3 9h3M3 15h3M18 9h3M18 15h3"),
        ]),
        sp(&[circle(12., 12., 9.), p("M12 16.5v-9M8 11l4-4 4 4")]),
        s("M12 3v8m-5-6a8 8 0 1 0 10 0"),
        sp(&[
            circle(12., 12., 9.),
            ellipse(12., 12., 4., 9.),
            p("M3 12h18"),
        ]),
        IconDef {
            stroke: leak(&[circle(12., 12., 9.)]),
            fill: "M12 3a9 9 0 0 1 0 18z",
            width: 1.8,
        },
        sp(&[rect(3., 4., 18., 13., 2.), p("M12 17v3m-4 0h8")]),
        sp(&[
            circle(12., 12., 4.),
            p("M12 2v2m0 16v2M2 12h2m16 0h2M5 5l1.5 1.5m11 11L19 19M5 19l1.5-1.5m11-11L19 5"),
        ]),
        s("M20.5 14a8.8 8.8 0 0 1-10.5-10.5A9 9 0 1 0 20.5 14z"),
        s("M12 4 2.5 20h19zM12 10v4.5M12 17.5v.5"),
        s("m6 6 12 12M18 6 6 18"),
        sp(&[
            p("M4 8h16M4 16h16"),
            circle(9., 8., 2.2),
            circle(15., 16., 2.2),
        ]),
        s("m12 3.5 2.6 5.3 5.9.9-4.3 4.1 1 5.8-5.2-2.7-5.2 2.7 1-5.8L3.5 9.7l5.9-.9z"),
        s("m5 12.5 4.5 4.5L19 7.5"),
        s("M4 14v5h16v-5M12 4v10M8 10l4 4 4-4"),
    ];
    register(icons);
}
