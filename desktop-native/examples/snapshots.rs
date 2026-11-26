//! Pictures of every page on the mock backend, drawn by the GPU without a window (for
//! reviews, and for screens that cannot show a window, such as a locked one).
//!
//! `cargo run -p podbag-desktop-native --example snapshots -- <out dir> [filter]` writes
//! `native-<page>-<theme>.png`; with a filter, only the names that contain it. With
//! `PODBAG_SNAPSHOT_REAL=1`, Containers and Images on the real engine instead (its
//! settings are not saved).

use std::path::{Path, PathBuf};

use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::{Options as MockOptions, Scenario};
use podbag_desktop_native::{Desktop, Options, icons};
use podbag_ui::load_fonts;
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;

/// A picture: its name and the preview commands that lead to it.
struct Shot {
    name: &'static str,
    scenario: Scenario,
    commands: &'static [&'static str],
}

/// The pages and dialogs of Training, Migrate, updates, login, import and the notices
/// (`m2-views-<name>-<lang>-<theme>.png`).
const VIEW_SHOTS: &[(&str, &[&str], f32)] = &[
    ("migrate", &["nav migrate"], 1200.),
    ("migrate-kube", &["nav migrate", "migrate kube"], 1200.),
    ("migration", &["nav migrate", "migration 0"], 1200.),
    (
        "migration-done",
        &["nav migrate", "migration 0", "migration start"],
        1200.,
    ),
    (
        "training",
        &[
            "training install container-basics",
            "training install web-basics",
            "training install volume-basics",
            "training lesson",
            "training",
        ],
        1200.,
    ),
    (
        "training-offline",
        &[
            "training install container-basics",
            "training",
            "training refresh",
        ],
        1200.,
    ),
    (
        "training-module",
        &[
            "training install container-basics",
            "lab start container-basics",
            "lab advance",
            "training container-basics",
        ],
        1200.,
    ),
    (
        "training-course",
        &[
            "training install web-basics",
            "lab start web-basics",
            "lab advance",
            "training web-basics",
        ],
        1200.,
    ),
    (
        "tutorial",
        &[
            "training lesson",
            "labs end",
            "labs start",
            "labs create my-api",
            "labs save {\"message\":\"hello\"}",
            "labs serve",
            "labs request",
            "training volume-api",
        ],
        1200.,
    ),
    (
        "tutorial-start",
        &["training lesson", "labs end", "training volume-api"],
        1200.,
    ),
    (
        "lab",
        &[
            "training install container-basics",
            "lab start container-basics",
            "lab advance",
            "nav lab-containers",
        ],
        1200.,
    ),
    (
        "lab-volumes",
        &[
            "training install volume-basics",
            "lab start volume-basics",
            "lab advance",
            "nav lab-volumes",
        ],
        1200.,
    ),
    (
        "lab-empty",
        &[
            "training install container-basics",
            "labs end",
            "nav lab-containers",
        ],
        1200.,
    ),
    (
        "updates",
        &["check updates", "nav settings", "section updates"],
        1200.,
    ),
    (
        "update-admin",
        &[
            "check updates",
            "nav settings",
            "section updates",
            "upgrade",
        ],
        1200.,
    ),
    (
        "terminal-command",
        &["nav settings", "section terminalCommand"],
        1200.,
    ),
    ("login", &["nav images", "login quay"], 1200.),
    ("login-cloud", &["nav images", "login acr"], 1200.),
    ("import", &["nav images", "import"], 1200.),
    ("notice", &["nav containers", "notice cli"], 1200.),
    ("star", &["nav migrate", "star"], 1200.),
    ("narrow", &["nav containers"], 700.),
];

const SHOTS: &[Shot] = &[
    Shot {
        name: "containers",
        scenario: Scenario::Default,
        commands: &["nav containers"],
    },
    Shot {
        name: "images",
        scenario: Scenario::Default,
        commands: &["nav images"],
    },
    Shot {
        name: "volumes",
        scenario: Scenario::Default,
        commands: &["nav volumes"],
    },
    Shot {
        name: "networks",
        scenario: Scenario::Default,
        commands: &["nav networks"],
    },
    Shot {
        name: "kubernetes",
        scenario: Scenario::Default,
        commands: &["kube pods"],
    },
    Shot {
        name: "kubernetes-services",
        scenario: Scenario::Default,
        commands: &["kube services"],
    },
    Shot {
        name: "settings",
        scenario: Scenario::Default,
        commands: &["nav settings", "section engine"],
    },
    Shot {
        name: "settings-vm",
        scenario: Scenario::Default,
        commands: &["nav settings", "section vm"],
    },
    Shot {
        name: "training",
        scenario: Scenario::Default,
        commands: &["nav training"],
    },
    Shot {
        name: "migrate",
        scenario: Scenario::Default,
        commands: &["nav migrate"],
    },
    Shot {
        name: "detail-info",
        scenario: Scenario::Default,
        commands: &["detail containers 0 info"],
    },
    Shot {
        name: "detail-logs",
        scenario: Scenario::Default,
        commands: &["detail containers 0 logs"],
    },
    Shot {
        name: "detail-inspect",
        scenario: Scenario::Default,
        commands: &["detail containers 0 inspect"],
    },
    Shot {
        name: "image-detail",
        scenario: Scenario::Default,
        commands: &["detail images 0 info"],
    },
    Shot {
        name: "pull",
        scenario: Scenario::Default,
        commands: &["nav images", "pull nginx"],
    },
    Shot {
        name: "confirm",
        scenario: Scenario::Default,
        commands: &["remove containers 0"],
    },
    Shot {
        name: "rename",
        scenario: Scenario::Default,
        commands: &["rename 0"],
    },
    Shot {
        name: "empty",
        scenario: Scenario::Empty,
        commands: &["nav containers"],
    },
    Shot {
        name: "offline",
        scenario: Scenario::Offline,
        commands: &["nav containers"],
    },
    Shot {
        name: "vm-stopped",
        scenario: Scenario::VmStopped,
        commands: &["nav containers"],
    },
    Shot {
        name: "docker",
        scenario: Scenario::Docker,
        commands: &["nav containers"],
    },
];

fn main() {
    let mut args = std::env::args().skip(1);
    let out = PathBuf::from(args.next().unwrap_or_else(|| ".".into()));
    let filter = args.next().unwrap_or_default();
    let scale: f32 = std::env::var("PODBAG_SNAPSHOT_SCALE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(2.);
    icons::install();
    if std::env::var_os("PODBAG_SNAPSHOT_REAL").is_some() {
        return real(&out, scale);
    }
    if filter.starts_with("m2-views") || filter.starts_with("views") {
        return views(&out, &filter, scale);
    }
    let mut h: Option<Harness<UiView<Desktop>>> = None;
    let mut scenario = None;
    for (lang, theme) in [
        ("en", "light"),
        ("en", "dark"),
        ("ko", "light"),
        ("ko", "dark"),
    ] {
        for shot in SHOTS {
            let name = match lang {
                "en" => format!("native-{}-{theme}", shot.name),
                _ => format!("native-{}-{lang}-{theme}", shot.name),
            };
            if !name.contains(&filter) {
                continue;
            }
            // A new window for each scenario, and for each picture of a dialog.
            if scenario != Some(shot.scenario) || h.is_none() {
                h = Some(window(shot.scenario));
                scenario = Some(shot.scenario);
            }
            let harness = h.as_mut().expect("window");
            for c in [format!("theme {theme}"), format!("lang {lang}")] {
                harness.view_mut().app.command(&c);
            }
            for c in shot.commands {
                harness.view_mut().app.command(c);
            }
            harness.repaint();
            harness.repaint();
            match harness.capture(scale) {
                Some((w, hgt, rgba)) => {
                    let path = out.join(format!("{name}.png"));
                    write_png(&path, w, hgt, &rgba).expect("write png");
                    println!("{}", path.display());
                }
                None => {
                    eprintln!("no GPU");
                    return;
                }
            }
            // Dialogs and panels close before the next picture.
            harness.view_mut().app.command("nav containers");
            harness.view_mut().app.dialog = None;
            harness.view_mut().app.pull = None;
        }
    }
}

/// The M2 views, each in a new window so that no state carries over.
fn views(out: &Path, filter: &str, scale: f32) {
    let filter = filter.strip_prefix("views").unwrap_or(filter);
    for (lang, theme) in [
        ("en", "light"),
        ("en", "dark"),
        ("ko", "light"),
        ("ko", "dark"),
    ] {
        for (shot, commands, width) in VIEW_SHOTS {
            let name = format!("m2-views-{shot}-{lang}-{theme}");
            if !name.contains(filter) {
                continue;
            }
            // A `-offline` picture: the engine and podbag.dev do not answer.
            let scenario = if shot.ends_with("-offline") {
                Scenario::Offline
            } else {
                Scenario::Default
            };
            let opts = MockOptions {
                instant: true,
                all_features: true,
                scenario,
                ..MockOptions::default()
            };
            let (backend, events) = Backend::mock(opts, || {});
            let app = Desktop::new(backend, events, Options::inline());
            let mut h = Harness::with_fonts(UiView::new(app), *width, 800., load_fonts());
            for c in [format!("theme {theme}"), format!("lang {lang}")] {
                h.view_mut().app.command(&c);
            }
            for c in *commands {
                h.view_mut().app.command(c);
                h.repaint();
            }
            for _ in 0..3 {
                h.repaint();
            }
            match h.capture(scale) {
                Some((w, hgt, rgba)) => {
                    let path = out.join(format!("{name}.png"));
                    write_png(&path, w, hgt, &rgba).expect("write png");
                    println!("{}", path.display());
                }
                None => {
                    eprintln!("no GPU");
                    return;
                }
            }
        }
    }
}

fn real(out: &Path, scale: f32) {
    let (backend, events) = Backend::real(env!("CARGO_PKG_VERSION"), None, || {});
    let app = Desktop::new(backend, events, Options::inline());
    let mut h = Harness::with_fonts(UiView::new(app), 1200., 800., load_fonts());
    for page in ["containers", "images"] {
        for c in ["theme light", "lang en", &format!("nav {page}")] {
            h.view_mut().app.command(c);
        }
        h.repaint();
        h.repaint();
        let Some((w, hgt, rgba)) = h.capture(scale) else {
            eprintln!("no GPU");
            return;
        };
        let path = out.join(format!("native-real-{page}.png"));
        write_png(&path, w, hgt, &rgba).expect("write png");
        println!("{}", path.display());
    }
}

fn window(scenario: Scenario) -> Harness<UiView<Desktop>> {
    let opts = MockOptions {
        scenario,
        instant: true,
        all_features: true,
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(opts, || {});
    let app = Desktop::new(backend, events, Options::inline());
    Harness::with_fonts(UiView::new(app), 1200., 800., load_fonts())
}

/// A PNG with stored (uncompressed) zlib blocks: no dependencies.
fn write_png(path: &Path, w: u32, h: u32, rgba: &[u8]) -> std::io::Result<()> {
    fn crc32(data: &[u8]) -> u32 {
        let mut crc = 0xffff_ffffu32;
        for b in data {
            crc ^= *b as u32;
            for _ in 0..8 {
                crc = if crc & 1 != 0 {
                    (crc >> 1) ^ 0xedb8_8320
                } else {
                    crc >> 1
                };
            }
        }
        !crc
    }
    fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        out.extend_from_slice(&body);
        out.extend_from_slice(&crc32(&body).to_be_bytes());
    }
    // The rows, each with filter byte 0, and alpha made opaque.
    let mut raw = Vec::with_capacity((w * 4 + 1) as usize * h as usize);
    for row in rgba.chunks((w * 4) as usize) {
        raw.push(0);
        for px in row.chunks(4) {
            raw.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    let mut z = vec![0x78, 0x01];
    let blocks: Vec<&[u8]> = raw.chunks(65_535).collect();
    for (i, block) in blocks.iter().enumerate() {
        z.push(u8::from(i + 1 == blocks.len()));
        let len = block.len() as u16;
        z.extend_from_slice(&len.to_le_bytes());
        z.extend_from_slice(&(!len).to_le_bytes());
        z.extend_from_slice(block);
    }
    let (mut a, mut b) = (1u32, 0u32);
    for byte in &raw {
        a = (a + *byte as u32) % 65_521;
        b = (b + a) % 65_521;
    }
    z.extend_from_slice(&((b << 16) | a).to_be_bytes());
    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&w.to_be_bytes());
    ihdr.extend_from_slice(&h.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut png, b"IHDR", &ihdr);
    chunk(&mut png, b"IDAT", &z);
    chunk(&mut png, b"IEND", &[]);
    std::fs::write(path, png)
}
