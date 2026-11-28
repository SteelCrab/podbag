//! Every widget of podbag-ui in one window, with keyboard focus, text selection,
//! tooltips and wrapped text.
//!
//! Run: `cargo run -p podbag-ui --example gallery`
//!
//! Environment (for screenshots):
//! - `GALLERY_THEME=light|dark`: start with that theme instead of the system's.
//! - `GALLERY_DEMO=main|dialog|menu|copy`: play some input (a log selection, keyboard
//!   focus and a tooltip; an open dialog; an open menu and a toast; a copied word) through the synthetic input
//!   API, so the state shows without touching the keyboard or the mouse.
//! - `GALLERY_LOG=1`: timing and frame logs on stderr.
//! - `GALLERY_SHOT=1`: open the window above the others without taking the
//!   keyboard (window managers that hide inactive windows leave it on screen).

use std::time::Duration;

use podbag_ui::icons::{self, IconDef};
use podbag_ui::ui::{Theme, Ui, UiApp, UiView};
use podbag_ui::widgets::button::{self, IconKind, Kind};
use podbag_ui::widgets::checkbox::{self, checkbox};
use podbag_ui::widgets::dialog::{buttons_row, dialog};
use podbag_ui::widgets::layout::Row;
use podbag_ui::widgets::logs::{LogView, follow_now, log_view};
use podbag_ui::widgets::radio::{radio_group, segmented};
use podbag_ui::widgets::scroll::{ScrollState, scroll_view};
use podbag_ui::widgets::select::{Item, select};
use podbag_ui::widgets::selectable::{selectable_line, selectable_text};
use podbag_ui::widgets::slider::slider;
use podbag_ui::widgets::table::{Column, Width, table};
use podbag_ui::widgets::tabs::tabs;
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::widgets::toast::Toasts;
use podbag_ui::{
    App, Cx, Id, KeyPress, NamedKey, Point, Rect, Synthetic, TextInput, TextStyle, VirtualList,
    WindowOptions,
};

#[repr(u16)]
#[derive(Clone, Copy)]
enum Icon {
    Copy,
    Trash,
    Search,
    Play,
    Info,
    Sun,
    Moon,
}

fn install_icons() {
    let defs = vec![
        IconDef {
            stroke: "M9 9h10v11H9zM5 15V4h10",
            fill: "",
            width: 1.8,
        },
        IconDef {
            stroke: "M4 7h16M10 11v6M14 11v6M6 7l1 13h10l1-13M9 7V4h6v3",
            fill: "",
            width: 1.8,
        },
        IconDef {
            stroke: "M10.5 4a6.5 6.5 0 1 0 0 13a6.5 6.5 0 1 0 0-13M15.5 15.5 20 20",
            fill: "",
            width: 1.8,
        },
        IconDef {
            stroke: "",
            fill: "M8 5v14l11-7z",
            width: 0.,
        },
        IconDef {
            stroke: "M12 3a9 9 0 1 0 0 18a9 9 0 1 0 0-18M12 11v6M12 7.5v.5",
            fill: "",
            width: 1.8,
        },
        IconDef {
            stroke: "M12 8a4 4 0 1 0 0 8a4 4 0 1 0 0-8M12 2v2M12 20v2M2 12h2M20 12h2M5 5l1.5 1.5M17.5 17.5 19 19M5 19l1.5-1.5M17.5 6.5 19 5",
            fill: "",
            width: 1.8,
        },
        IconDef {
            stroke: "M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z",
            fill: "",
            width: 1.8,
        },
    ];
    icons::register(defs);
}

const ABOUT: &str = "podbag-ui paints the whole window every frame. Widgets are functions: \
they draw themselves and report clicks, keys and drags. This paragraph wraps at the width of \
its column; drag over it to select, double-click a word, triple-click a line. \
한국어 문장도 줄을 바꿉니다. 日本語の文も折り返します。中文句子也会换行。";

const JSON: &str = "{\n  \"Id\": \"4f1c9e2ab7d0\",\n  \"Name\": \"web\",\n  \"State\": { \"Status\": \"running\", \"Pid\": 4242 },\n  \"Config\": { \"Image\": \"nginx:1.27-alpine\", \"Cmd\": [\"nginx\", \"-g\", \"daemon off;\"] }\n}";

struct Row3 {
    name: &'static str,
    image: &'static str,
    state: &'static str,
}

const ROWS: [Row3; 6] = [
    Row3 {
        name: "web",
        image: "nginx:1.27-alpine",
        state: "running",
    },
    Row3 {
        name: "db",
        image: "postgres:17",
        state: "running",
    },
    Row3 {
        name: "cache",
        image: "redis:7.4",
        state: "exited",
    },
    Row3 {
        name: "worker-with-a-very-long-container-name",
        image: "ghcr.io/example/worker:2026.11",
        state: "created",
    },
    Row3 {
        name: "로그-수집기",
        image: "fluent/fluent-bit:3.2",
        state: "running",
    },
    Row3 {
        name: "proxy",
        image: "traefik:v3.2",
        state: "paused",
    },
];

struct Gallery {
    theme: usize,
    name: TextInput,
    search: TextInput,
    agree: bool,
    notify: bool,
    restart: usize,
    density: usize,
    engine: usize,
    cpus: f32,
    tab: usize,
    logs: LogView,
    json: LogView,
    table: VirtualList,
    controls: ScrollState,
    toasts: Toasts,
    dialog: Option<TextInput>,
}

impl Gallery {
    fn new(theme: usize) -> Self {
        let style = TextStyle::new(14.).line_height(20.);
        let ink = Theme::light().ink;
        let mut name = TextInput::new(Id::of("name"), "Container name", style, ink);
        name.set_content("web-한글-日本語");
        let mut logs = LogView::default();
        logs.push(sample_logs());
        let mut json = LogView::default();
        json.wrap = false;
        json.set_text(JSON);
        Self {
            theme,
            name,
            search: TextInput::new(Id::of("search"), "Search images", style, ink),
            agree: true,
            notify: false,
            restart: 1,
            density: 0,
            engine: 1,
            cpus: 4.,
            tab: 0,
            logs,
            json,
            table: VirtualList::new(36., 36.),
            controls: ScrollState::default(),
            toasts: Toasts::default(),
            dialog: None,
        }
    }

    fn heading(ui: &mut Ui, x: f32, y: f32, w: f32, text: &str) -> f32 {
        let style = ui.style().with_size(12.).weight(600);
        let muted = ui.theme.muted;
        ui.p.label(Rect::new(x, y, w, 18.), text, style, muted);
        26.
    }

    fn controls(&mut self, ui: &mut Ui, r: Rect) {
        let mut state = std::mem::take(&mut self.controls);
        scroll_view(ui, Id::of("controls"), r, &mut state, |ui, area| {
            let (x, w) = (area.x, area.w - 14.);
            let mut y = area.y;
            y += Self::heading(ui, x, y, w, "BUTTONS");
            let mut row = Row::new(Rect::new(x, y, w, 32.), 8.);
            let bw = button::width(ui, "Start", Some(Icon::Play as u16), false);
            if button::button(
                ui,
                Id::of("start"),
                row.left(bw),
                "Start",
                Some(Icon::Play as u16),
                Kind::Primary,
                true,
            ) {
                self.toasts.push("Started web", false);
            }
            let bw = button::width(ui, "Restart", None, false);
            if button::button(
                ui,
                Id::of("restart"),
                row.left(bw),
                "Restart",
                None,
                Kind::Normal,
                true,
            ) {
                self.toasts.push("Restarted web", false);
            }
            let bw = button::width(ui, "Remove", None, false);
            if button::button(
                ui,
                Id::of("remove"),
                row.left(bw),
                "Remove",
                None,
                Kind::Danger,
                true,
            ) {
                self.dialog = Some(dialog_input());
            }
            let bw = button::width(ui, "Off", None, false);
            button::button(
                ui,
                Id::of("off"),
                row.left(bw),
                "Off",
                None,
                Kind::Normal,
                false,
            );
            y += 42.;
            let mut row = Row::new(Rect::new(x, y, w, 30.), 4.);
            for (key, icon, label, kind) in [
                ("copy", Icon::Copy, "Copy the ID", IconKind::Plain),
                ("info", Icon::Info, "Details", IconKind::Accent),
                (
                    "trash",
                    Icon::Trash,
                    "Remove the container",
                    IconKind::Danger,
                ),
            ] {
                if button::icon_button(
                    ui,
                    Id::of(key),
                    row.left(30.),
                    icon as u16,
                    label,
                    kind,
                    true,
                ) {
                    self.toasts.push(label, false);
                }
            }
            y += 46.;
            y += Self::heading(ui, x, y, w, "CHOICES");
            let lw = checkbox::width(ui, "Start at login");
            let mut agree = self.agree;
            checkbox(
                ui,
                Id::of("login"),
                Rect::new(x, y, lw, 26.),
                Some(&mut agree),
                "Start at login",
                true,
            );
            self.agree = agree;
            y += 28.;
            let lw = checkbox::width(ui, "Notify when a pull ends");
            let mut notify = self.notify;
            checkbox(
                ui,
                Id::of("notify"),
                Rect::new(x, y, lw, 26.),
                Some(&mut notify),
                "Notify when a pull ends",
                true,
            );
            self.notify = notify;
            y += 36.;
            let opts = ["No restart", "On failure", "Always"];
            if let Some(i) = radio_group(
                ui,
                Id::of("restart-policy"),
                Rect::new(x, y, w, 78.),
                26.,
                &opts,
                self.restart,
            ) {
                self.restart = i;
            }
            y += 88.;
            let segs = [(None, "Compact"), (None, "Normal"), (None, "Roomy")];
            if let Some(i) = segmented(
                ui,
                Id::of("density"),
                Rect::new(x, y, w.min(300.), 32.),
                &segs,
                self.density,
            ) {
                self.density = i;
            }
            y += 44.;
            let items = [
                Item::Group("Local"),
                Item::Option("Podbag engine"),
                Item::Option("Docker Desktop"),
                Item::Group("Remote"),
                Item::Option("ssh://build-server"),
            ];
            if let Some(i) = select(
                ui,
                Id::of("engine"),
                Rect::new(x, y, w.min(300.), 32.),
                &items,
                self.engine,
                true,
            ) {
                self.engine = i;
            }
            y += 46.;
            y += Self::heading(ui, x, y, w, &format!("CPUS: {}", self.cpus as u32));
            let mut cpus = self.cpus;
            if slider(
                ui,
                Id::of("cpus"),
                Rect::new(x - 8., y, w.min(316.), 24.),
                &mut cpus,
                1.,
                8.,
                1.,
            ) {
                self.cpus = cpus;
            }
            y += 36.;
            y += Self::heading(ui, x, y, w, "TEXT FIELDS");
            text_field(ui, &mut self.name, Rect::new(x, y, w, 34.), None, true);
            y += 42.;
            text_field(
                ui,
                &mut self.search,
                Rect::new(x, y, w, 34.),
                Some(Icon::Search as u16),
                true,
            );
            y += 50.;
            y - area.y
        });
        self.controls = state;
    }

    fn texts(&mut self, ui: &mut Ui, r: Rect) {
        let t = ui.theme;
        let (x, w) = (r.x, r.w);
        let mut y = r.y;
        let labels = vec![
            "Info".to_string(),
            "Inspect".to_string(),
            "Logs".to_string(),
        ];
        if let Some(i) = tabs(
            ui,
            Id::of("tabs"),
            Rect::new(x - 12., y, w, 36.),
            &labels,
            self.tab,
        ) {
            self.tab = i;
        }
        ui.p.fill(Rect::new(x - 12., y + 35., w + 12., 1.), t.line);
        y += 50.;
        let style = ui.style();
        y += selectable_text(
            ui,
            Id::of("about"),
            Point::new(x, y),
            w,
            ABOUT,
            style,
            t.ink2,
        ) + 16.;
        // A fields grid: labels left, selectable values right.
        let mono = TextStyle::new(12.5).mono().line_height(20.);
        let fields: [(&str, &str, bool); 4] = [
            ("ID", "4f1c9e2ab7d0c35e8a91f0b2d4c6e8fa", true),
            ("Image", "nginx:1.27-alpine", false),
            (
                "Command",
                "nginx -g 'daemon off;' --with-a-long-argument-list that wraps",
                true,
            ),
            (
                "A label too long for its column",
                "Hover the label for the whole text",
                false,
            ),
        ];
        for (i, (k, v, is_mono)) in fields.iter().enumerate() {
            ui.label(Rect::new(x, y, 110., 20.), k, style, t.ink2);
            let s = if *is_mono { mono } else { style };
            let h = selectable_text(
                ui,
                Id::of(("field", i)),
                Point::new(x + 126., y),
                w - 126.,
                v,
                s,
                t.ink,
            );
            y += h.max(20.) + 8.;
        }
        y += 8.;
        selectable_line(
            ui,
            Id::of("one-line"),
            Rect::new(x, y, w, 22.),
            "One line, clipped, still selectable to the end: sha256:9d3f0c1b2a4e6f8091a2b3c4d5e6f708192a3b4c",
            mono,
            t.muted,
        );
        y += 32.;
        let json_h = (r.bottom() - y).max(80.);
        log_view(
            ui,
            Id::of("json"),
            Rect::new(x, y, w, json_h),
            &mut self.json,
            "",
        );
    }

    fn logs_and_table(&mut self, ui: &mut Ui, r: Rect) {
        let t = ui.theme;
        let mut r = r;
        let bar = r.cut_top(30.);
        let mut row = Row::new(bar, 10.);
        let style = ui.style().weight(600);
        ui.p.label(row.left(60.), "Logs", style, t.ink);
        let lw = checkbox::width(ui, "Wrap");
        let mut wrap = self.logs.wrap;
        if checkbox(
            ui,
            Id::of("wrap"),
            row.right(lw),
            Some(&mut wrap),
            "Wrap",
            true,
        ) {
            self.logs.wrap = wrap;
        }
        let lw = checkbox::width(ui, "Follow");
        let mut follow = self.logs.follow;
        if checkbox(
            ui,
            Id::of("follow"),
            row.right(lw),
            Some(&mut follow),
            "Follow",
            true,
        ) {
            self.logs.follow = follow;
            if follow {
                follow_now(&mut self.logs);
            }
        }
        let copy = row.right_h(30., 30.);
        let has_selection = ui.selected_text().is_some();
        if button::icon_button(
            ui,
            Id::of("copy-selection"),
            copy,
            Icon::Copy as u16,
            "Copy the selection",
            IconKind::Plain,
            has_selection,
        ) && ui.copy_selection()
        {
            self.toasts.push("Copied", false);
        }
        r.cut_top(8.);
        let table_h = 36. * 7.;
        let logs = Rect::new(r.x, r.y, r.w, r.h - table_h - 16.);
        log_view(ui, Id::of("logs"), logs, &mut self.logs, "No logs");
        let area = Rect::new(r.x, logs.bottom() + 16., r.w, table_h);
        ui.p.bordered(area, 8., t.surface, 1., t.line);
        let cols = [
            Column {
                label: "Name",
                width: Width::Flex(1.2),
            },
            Column {
                label: "Image",
                width: Width::Flex(1.),
            },
            Column {
                label: "State",
                width: Width::Px(90.),
            },
        ];
        let mut list = std::mem::replace(&mut self.table, VirtualList::new(1., 1.));
        let small = ui.style().with_size(13.);
        let clicked = table(
            ui,
            Id::of("table"),
            area.inset(6., 0., 6., 4.),
            &cols,
            ROWS.len(),
            &mut list,
            |ui, i, c, cell| {
                let row = &ROWS[i];
                let text = [row.name, row.image, row.state][c];
                let color = if c == 0 { ui.theme.ink } else { ui.theme.ink2 };
                ui.label(cell, text, small, color);
            },
        );
        self.table = list;
        if let Some(i) = clicked {
            self.toasts.push(format!("Opened {}", ROWS[i].name), false);
        }
    }

    fn paint_dialog(&mut self, ui: &mut Ui) {
        let Some(mut input) = self.dialog.take() else {
            return;
        };
        let t = ui.theme;
        let mut close = false;
        let mut done = false;
        let id = Id::of("remove-dialog");
        let escape = dialog(ui, id, 440., |ui, r| {
            let mut y = r.y;
            let h1 = ui.style().with_size(16.).weight(700);
            y += ui.paragraph(r.x, y, r.w, "Remove web?", h1, t.ink) + 10.;
            let text = "The container stops and its writable layer is deleted. Type its name to confirm. \
                        Tab moves through this dialog only; Escape closes it.";
            y += ui.paragraph(r.x, y, r.w, text, ui.style(), t.ink2) + 12.;
            text_field(ui, &mut input, Rect::new(r.x, y, r.w, 34.), None, true);
            y += 46.;
            let widths = [
                button::width(ui, "Remove", None, false),
                button::width(ui, "Cancel", None, false),
            ];
            let rects = buttons_row(r.x, y, r.w, &widths);
            let ok = input.content() == "web";
            if button::button(
                ui,
                id.with("yes"),
                rects[0],
                "Remove",
                None,
                Kind::Danger,
                ok,
            ) {
                done = true;
            }
            if button::button(
                ui,
                id.with("no"),
                rects[1],
                "Cancel",
                None,
                Kind::Normal,
                true,
            ) {
                close = true;
            }
            y + 32. - r.y
        });
        if done {
            self.toasts.push("Removed web", false);
        }
        if escape || close || done {
            ui.focus(Some(Id::of("remove")));
        } else {
            self.dialog = Some(input);
        }
    }
}

fn dialog_input() -> TextInput {
    TextInput::new(
        Id::of("confirm"),
        "web",
        TextStyle::new(14.).line_height(20.),
        Theme::light().ink,
    )
}

fn sample_logs() -> Vec<String> {
    let mut v = Vec::new();
    for i in 0..400 {
        v.push(match i % 7 {
            0 => format!("2026-11-26T09:{:02}:{:02}Z INFO  request GET /api/v1/containers status=200 in {}ms", i / 60 % 60, i % 60, 3 + i % 17),
            1 => format!("2026-11-26T09:{:02}:{:02}Z DEBUG cache hit for layer sha256:{:064x}", i / 60 % 60, i % 60, i * 7919),
            2 => "2026-11-26T09:00:00Z WARN  slow query: SELECT * FROM events WHERE created_at > now() - interval '1 day' ORDER BY created_at DESC LIMIT 500 took 1240ms, which is a long line that wraps when wrapping is on".to_string(),
            3 => format!("2026-11-26T09:{:02}:{:02}Z INFO  컨테이너 web 시작됨 (pid {})", i / 60 % 60, i % 60, 4000 + i),
            4 => format!("2026-11-26T09:{:02}:{:02}Z INFO  コンテナを起動しました / 容器已启动", i / 60 % 60, i % 60),
            5 => String::new(),
            _ => format!("2026-11-26T09:{:02}:{:02}Z ERROR upstream timed out (110: Connection timed out) while reading response header", i / 60 % 60, i % 60),
        });
    }
    v
}

impl UiApp for Gallery {
    type Message = String;

    fn ui(&mut self, ui: &mut Ui) {
        let t = ui.theme;
        let b = ui.bounds();
        ui.p.fill(b, t.page);
        let mut r = b;
        let top = r.cut_top(52.);
        ui.p.border_bottom(top, t.line);
        let mut row = Row::new(top.inset(20., 0., 16., 1.), 8.);
        let title = ui.style().with_size(17.).weight(700);
        ui.p.label(row.left(200.), "podbag-ui gallery", title, t.ink);
        let segs = [
            (Some(Icon::Info as u16), "System"),
            (Some(Icon::Sun as u16), "Light"),
            (Some(Icon::Moon as u16), "Dark"),
        ];
        if let Some(i) = segmented(
            ui,
            Id::of("theme"),
            row.right_h(300., 32.),
            &segs,
            self.theme,
        ) {
            self.theme = i;
        }
        let body = r.inset(20., 18., 20., 18.);
        let col = (body.w - 48.) / 3.;
        let c1 = Rect::new(body.x, body.y, col, body.h);
        let c2 = Rect::new(c1.right() + 24., body.y, col, body.h);
        let c3 = Rect::new(c2.right() + 24., body.y, col, body.h);
        self.controls(ui, c1);
        self.texts(ui, c2);
        self.logs_and_table(ui, c3);
        self.paint_dialog(ui);
        self.toasts.paint(ui);
    }

    fn message(&mut self, line: String, cx: &mut Cx) {
        self.logs.push([line]);
        cx.notify();
    }

    fn tick(&mut self, _cx: &mut Cx) -> bool {
        self.toasts.expire()
    }

    fn theme(&self, system_dark: bool) -> Theme {
        let dark = match self.theme {
            1 => false,
            2 => true,
            _ => system_dark,
        };
        if dark { Theme::dark() } else { Theme::light() }
    }
}

/// Input for the screenshots: everything goes through `Proxy::synthetic`.
fn demo(proxy: podbag_ui::Proxy<String>, scene: &str) {
    let send = |s: Synthetic| {
        proxy.synthetic(s);
        std::thread::sleep(Duration::from_millis(60));
    };
    let key = |k: NamedKey| send(Synthetic::Key(KeyPress::named(k)));
    let tabs = |n: usize| (0..n).for_each(|_| key(NamedKey::Tab));
    std::thread::sleep(Duration::from_millis(800));
    match scene {
        "dialog" => {
            // Tab to "Remove" (after the theme switch, Start and Restart), Enter opens
            // the dialog; Tab goes into it and stays there.
            tabs(4);
            key(NamedKey::Enter);
            tabs(1);
            send(Synthetic::Text("we".into()));
            tabs(1);
        }
        "menu" => {
            // Enter on "Start" shows a toast; then Tab to the engine select, open it
            // with an arrow key and move down.
            tabs(2);
            key(NamedKey::Enter);
            tabs(10);
            key(NamedKey::ArrowDown);
            key(NamedKey::ArrowDown);
        }
        "copy" => {
            // Double-click a word of the paragraph and copy it to the system clipboard.
            let at = Point::new((1360. - 40. - 48.) / 3. + 44. + 30., 52. + 18. + 50. + 10.);
            send(Synthetic::Click(at));
            send(Synthetic::Click(at));
            send(Synthetic::Key(KeyPress::shortcut('c')));
        }
        _ => {
            // Select across log lines: rows are 19 px, from the second line to the fifth.
            let col = (1360. - 40. - 48.) / 3.;
            let x = 20. + 2. * (col + 24.) + 14.;
            let top = 52. + 18. + 30. + 8. + 12.;
            send(Synthetic::Press(Point::new(x + 150., top + 19. + 8.)));
            send(Synthetic::Move(Point::new(x + 200., top + 19. * 3. + 8.)));
            send(Synthetic::Release(Point::new(
                x + 240.,
                top + 19. * 4. + 8.,
            )));
            // Back to the "Follow" checkbox with the keyboard: its focus ring shows.
            let back = KeyPress::named(NamedKey::Tab).with_mods(podbag_ui::ModifiersState::SHIFT);
            send(Synthetic::Key(back.clone()));
            send(Synthetic::Key(back));
            // Rest on an icon button until its tooltip shows.
            send(Synthetic::Move(Point::new(
                20. + 15. + 34.,
                52. + 18. + 26. + 42. + 15.,
            )));
        }
    }
}

fn main() {
    let app = App::<String>::new();
    install_icons();
    let theme = match std::env::var("GALLERY_THEME").as_deref() {
        Ok("light") => 1,
        Ok("dark") => 2,
        _ => 0,
    };
    let shot = std::env::var_os("GALLERY_SHOT").is_some();
    if let Ok(scene) = std::env::var("GALLERY_DEMO") {
        let proxy = app.proxy();
        std::thread::spawn(move || demo(proxy, &scene));
    } else {
        // New log lines now and then, to show following.
        let proxy = app.proxy();
        std::thread::spawn(move || {
            for i in 0.. {
                std::thread::sleep(Duration::from_secs(2));
                if !proxy.send(format!("2026-11-26T10:00:{:02}Z INFO  tick {i}", i % 60)) {
                    break;
                }
            }
        });
    }
    let opts = WindowOptions {
        title: "podbag-ui gallery".into(),
        width: 1360.,
        height: 860.,
        min_size: Some((900., 600.)),
        activate: !shot,
        always_on_top: shot,
        debug: std::env::var_os("GALLERY_LOG").is_some(),
        ..Default::default()
    };
    app.run(opts, move || UiView::new(Gallery::new(theme)));
}
