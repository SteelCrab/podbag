//! Drawing the TUI: the engine and tabs at the top, the list with the selected object's
//! details, a message line and the keys at the bottom. Help, questions and prompts are
//! pop-ups; inspect and logs take the whole body. It fits 80×24. Where things are drawn
//! is kept in [`App::areas`], for the mouse.

use super::app::{App, Mode, Tone};
use super::mouse::{Areas, hint_key};
use super::pull::PullForm;
use podbag::client::{Kind, short_id};
use ratatui::Frame;
use ratatui::buffer::Buffer;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap};

const ACCENT: Color = Color::Blue;

pub fn draw(f: &mut Frame, app: &mut App) {
    let area = f.area();
    f.buffer_mut().set_style(area, app.appearance.style());
    app.areas = Areas::default();
    let [header, tabs, body, message, keys] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(f.area());
    draw_header(f, app, header);
    draw_tabs(f, app, tabs);
    let body = match training_parts(app) {
        Some(parts) => {
            // As tall as the wrapped hint needs, but the lists keep 3 rows.
            let height = parts.height(body.width).min(body.height.saturating_sub(3));
            let [body, panel] =
                Layout::vertical([Constraint::Min(3), Constraint::Length(height)]).areas(body);
            parts.draw(f, panel);
            body
        }
        None => body,
    };
    let body = if app.appearance.sidebar && body.width >= 56 {
        let width = app
            .appearance
            .sidebar_width
            .clamp(14, 36)
            .min(body.width / 3);
        let [nav, body] =
            Layout::horizontal([Constraint::Length(width), Constraint::Fill(1)]).areas(body);
        draw_sidebar(f, app, nav);
        body
    } else {
        body
    };
    match &app.mode {
        Mode::Text { .. } => draw_text(f, app, body),
        _ => draw_list(f, app, body),
    }
    draw_message(f, app, message);
    draw_keys(f, app, keys);
    match app.mode.clone() {
        Mode::Help => draw_help(f, app),
        Mode::Confirm { question, .. } => draw_confirm(f, app, question),
        Mode::Input { prompt, text, .. } => {
            popup(
                f,
                &prompt,
                vec![Line::from(vec![
                    Span::raw(text),
                    Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
                ])],
            );
        }
        Mode::Pull(form) => draw_pull(f, app, &form),
        _ => {}
    }
}

/// The training panel: the step before, the current step, what to press (wrapped, so
/// the key at its end and a long command show whole), and the next step.
struct TrainingParts<'a> {
    title: String,
    above: Vec<Line<'a>>,
    hint: Paragraph<'a>,
    below: Vec<Line<'a>>,
}

impl TrainingParts<'_> {
    /// The hint is indented by 2 inside the borders.
    const INDENT: u16 = 2;

    fn hint_rows(&self, inner_width: u16) -> u16 {
        let width = inner_width.saturating_sub(Self::INDENT).max(1);
        u16::try_from(self.hint.line_count(width)).unwrap_or(u16::MAX)
    }

    /// The rows the whole panel needs at WIDTH, borders included.
    fn height(&self, width: u16) -> u16 {
        let lines = self.above.len() + self.below.len();
        (2 + lines as u16).saturating_add(self.hint_rows(width.saturating_sub(2)))
    }

    /// Row by row from the top: in a short AREA the next step goes first, then the hint's end.
    fn draw(self, f: &mut Frame, area: Rect) {
        let block = Block::bordered().title(self.title.clone());
        let inner = block.inner(area);
        f.render_widget(block, area);
        let rows = self.hint_rows(inner.width);
        let mut y = inner.y;
        let mut row = |f: &mut Frame, height: u16, indent: u16, widget: Paragraph| {
            let rect = Rect::new(
                inner.x + indent,
                y,
                inner.width.saturating_sub(indent),
                height,
            );
            f.render_widget(widget, rect.intersection(inner));
            y = y.saturating_add(height);
        };
        for line in self.above {
            row(f, 1, 0, Paragraph::new(line));
        }
        row(f, rows, Self::INDENT, self.hint);
        for line in self.below {
            row(f, 1, 0, Paragraph::new(line));
        }
    }
}

fn training_parts(app: &App) -> Option<TrainingParts<'static>> {
    let panel = app.training.as_ref()?;
    let look = &app.appearance;
    let lang = look.lang();
    let mut above = Vec::new();
    if let Some(index) = panel.current.checked_sub(1) {
        above.push(Line::styled(
            format!("✓ {}", panel.name(index, lang)),
            Style::new().fg(Color::Green),
        ));
    }
    above.push(Line::styled(
        format!(
            "▶ {}/{} {}",
            panel.current + 1,
            panel.steps.len(),
            panel.name(panel.current, lang)
        ),
        Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
    ));
    let below = if panel.current + 1 < panel.steps.len() {
        vec![Line::styled(
            format!("○ {}", panel.name(panel.current + 1, lang)),
            Style::new().fg(Color::DarkGray),
        )]
    } else {
        Vec::new()
    };
    let hint = Paragraph::new(super::training::hint(look, panel, &app.lists))
        .style(Style::new().fg(Color::Yellow))
        .wrap(Wrap { trim: true });
    Some(TrainingParts {
        title: format!(" {} · {} ", look.text("training.title"), panel.title),
        above,
        hint,
        below,
    })
}

/// Spans in a row from `area`'s left: where each one is.
fn place(area: Rect, spans: &[Span]) -> Vec<Rect> {
    let mut x = area.x;
    spans
        .iter()
        .map(|s| {
            let w = (s.width() as u16).min((area.x + area.width).saturating_sub(x));
            let r = Rect::new(x, area.y, w, 1);
            x += w;
            r
        })
        .collect()
}

/// podbag, the engine (click or E to switch), the VM's state, where the `docker`
/// command goes (click or D to switch), and whether events come.
fn draw_header(f: &mut Frame, app: &mut App, area: Rect) {
    let [left, right] =
        Layout::horizontal([Constraint::Min(0), Constraint::Length(10)]).areas(area);
    let dim = Style::new().fg(Color::DarkGray);
    let mut spans = vec![
        Span::styled(
            " podbag ",
            Style::new()
                .fg(Color::White)
                .bg(ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(
            format!("  {} ", app.appearance.text("settings.engine")),
            dim,
        ),
        Span::styled(
            app.engine.clone(),
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ),
        Span::styled(" (E)", dim),
    ];
    if let Some(vm) = app.vm {
        let color = if vm == "running" {
            Color::Green
        } else {
            Color::DarkGray
        };
        spans.push(Span::styled("  VM ", dim));
        spans.push(Span::styled(vm, Style::new().fg(color)));
    }
    let docker_at = spans.len() + 1;
    if let Some(d) = &app.docker {
        spans.push(Span::styled("  docker → ", dim));
        spans.push(Span::styled(d.name.clone(), Style::new().fg(ACCENT)));
    }
    let places = place(left, &spans);
    app.areas.engine = places.get(2).copied();
    app.areas.docker = app
        .docker
        .as_ref()
        .and_then(|_| places.get(docker_at).copied());
    f.render_widget(Paragraph::new(Line::from(spans)), left);
    let live = if app.live {
        Span::styled("● live ", Style::new().fg(Color::Green))
    } else {
        Span::styled("○ polling ", Style::new().fg(Color::DarkGray))
    };
    f.render_widget(
        Paragraph::new(Line::from(live)).alignment(Alignment::Right),
        right,
    );
}

fn draw_tabs(f: &mut Frame, app: &mut App, area: Rect) {
    let mut spans = Vec::new();
    for (n, k) in Kind::ALL.iter().enumerate() {
        let label = app.appearance.text(&format!("nav.{}", k.segment()));
        let text = if app.loaded {
            format!(" {} {} {} ", n + 1, label, app.count(*k))
        } else {
            format!(" {} {} ", n + 1, label)
        };
        let style = if *k == app.tab {
            Style::new()
                .fg(ACCENT)
                .add_modifier(Modifier::BOLD | Modifier::UNDERLINED)
        } else {
            Style::new()
        };
        spans.push(Span::raw(" "));
        spans.push(Span::styled(text, style));
    }
    let places = place(area, &spans);
    app.areas.tabs = Kind::ALL
        .iter()
        .enumerate()
        .map(|(n, k)| (places[2 * n + 1], *k))
        .collect();
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn draw_sidebar(f: &mut Frame, app: &mut App, area: Rect) {
    let block = Block::new().borders(Borders::RIGHT);
    let inner = block.inner(area);
    f.render_widget(block, area);
    for (index, kind) in Kind::ALL.into_iter().enumerate() {
        let row = Rect::new(inner.x, inner.y + index as u16, inner.width, 1).intersection(inner);
        let style = if kind == app.tab {
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let name = app.appearance.text(&format!("nav.{}", kind.segment()));
        f.render_widget(
            Paragraph::new(format!("{} {name}", index + 1)).style(style),
            row,
        );
        app.areas.tabs.push((row, kind));
    }
    for (index, page) in super::dashboard::Page::ALL.into_iter().enumerate() {
        let row =
            Rect::new(inner.x, inner.y + 5 + index as u16, inner.width, 1).intersection(inner);
        f.render_widget(
            Paragraph::new(format!(
                "F{} {}",
                index + 6,
                app.appearance.text(page.label())
            )),
            row,
        );
        app.areas.pages.push((row, page));
    }
}

fn headers(kind: Kind) -> (Vec<&'static str>, Vec<Constraint>) {
    match kind {
        Kind::Containers => (
            vec!["col.name", "col.image", "detail.status", "col.ports"],
            vec![
                Constraint::Fill(2),
                Constraint::Fill(3),
                Constraint::Fill(3),
                Constraint::Fill(3),
            ],
        ),
        Kind::Images => (
            vec!["col.name", "col.id", "col.size", "col.created"],
            vec![
                Constraint::Fill(4),
                Constraint::Length(12),
                Constraint::Length(9),
                Constraint::Fill(2),
            ],
        ),
        Kind::Volumes => (
            vec!["col.name", "col.driver", "col.created"],
            vec![
                Constraint::Fill(3),
                Constraint::Length(8),
                Constraint::Fill(2),
            ],
        ),
        Kind::Networks => (
            vec!["col.name", "col.driver", "col.subnet", "col.id"],
            vec![
                Constraint::Fill(2),
                Constraint::Length(8),
                Constraint::Fill(2),
                Constraint::Length(12),
            ],
        ),
    }
}

/// What an empty list says.
fn empty_text(app: &App) -> Vec<Line<'static>> {
    if let Some(error) = &app.lists.errors[super::app::tab_index(app.tab)] {
        return vec![
            Line::styled(error.clone(), Style::new().fg(Color::Red)),
            Line::from(format!("F5 · {}", app.appearance.text("action.retry"))),
        ];
    }
    if !app.loaded {
        return vec![Line::from(app.appearance.text("engine.checking"))];
    }
    if !app.search.is_empty() {
        return vec![Line::from(
            app.appearance
                .text("empty.search")
                .replace("{q}", &app.search),
        )];
    }
    let hint = Style::new().fg(ACCENT);
    // The command of the engine shown.
    let cli = if app.engine == "podbag" {
        "podbag"
    } else {
        "docker"
    };
    match app.tab {
        Kind::Containers => vec![
            Line::from(app.appearance.text("empty.containers.title")),
            Line::from(""),
            Line::styled(
                format!("  {cli} run -d --name web -p 8080:80 nginx:alpine"),
                hint,
            ),
        ],
        Kind::Images => vec![Line::from(app.appearance.text("empty.images.title"))],
        Kind::Volumes => vec![
            Line::from(app.appearance.text("empty.volumes.title")),
            Line::from(""),
            Line::styled(format!("  {cli} volume create data"), hint),
        ],
        Kind::Networks => vec![
            Line::from(app.appearance.text("empty.networks.title")),
            Line::from(""),
            Line::styled(format!("  {cli} network create app"), hint),
        ],
    }
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.rows(app.tab);
    let (list_area, details_area) = if area.height >= 14 && !rows.is_empty() {
        let [a, b] = Layout::vertical([Constraint::Min(5), Constraint::Length(7)]).areas(area);
        (a, Some(b))
    } else {
        (area, None)
    };
    if rows.is_empty() {
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(
                format!(
                    " {} ",
                    app.appearance.text(&format!("nav.{}", app.tab.segment()))
                ),
                Style::new().add_modifier(Modifier::BOLD),
            ));
        f.render_widget(Paragraph::new(empty_text(app)).block(block), list_area);
        return;
    }
    let (names, widths) = headers(app.tab);
    let header = Row::new(
        names
            .into_iter()
            .map(|key| Cell::from(app.appearance.text(key).to_uppercase())),
    )
    .style(
        Style::new()
            .add_modifier(Modifier::BOLD)
            .fg(Color::DarkGray),
    );
    let body = rows.iter().map(|r| {
        let style = match r.tone {
            Tone::Running => Style::new().fg(Color::Green),
            Tone::Stopped => Style::new().fg(Color::DarkGray),
            Tone::Other => Style::new().fg(Color::Yellow),
            Tone::Normal => Style::new(),
        };
        let mut cells: Vec<Cell> = r.cells.iter().map(|c| Cell::from(c.clone())).collect();
        // The state colour on the status only; the name stays plain.
        if app.tab == Kind::Containers {
            cells[2] = Cell::from(r.cells[2].clone()).style(style);
            Row::new(cells)
        } else {
            Row::new(cells).style(style)
        }
    });
    let table = Table::new(body, widths)
        .header(header)
        .column_spacing(1)
        .row_highlight_style(Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD));
    let mut state = TableState::default().with_selected(Some(app.selected_index()));
    f.render_stateful_widget(table, list_area, &mut state);
    // Under the header row.
    let rows_area = Rect {
        y: list_area.y + 1,
        height: list_area.height.saturating_sub(1),
        ..list_area
    };
    app.areas.rows = Some((rows_area, state.offset()));
    if let (Some(area), Some(row)) = (details_area, app.current()) {
        draw_details(f, app, &row.id, &row.name, area);
    }
}

/// The selected object's summary, under the list.
fn draw_details(f: &mut Frame, app: &App, id: &str, name: &str, area: Rect) {
    let field = |k: &str, v: String| {
        let k = app.appearance.text(k);
        Line::from(vec![
            Span::styled(format!("{k:<9}"), Style::new().fg(Color::DarkGray)),
            Span::raw(v),
        ])
    };
    let lines = match app.tab {
        Kind::Containers => {
            let Some(c) = app.lists.containers.iter().find(|c| c.id == id) else {
                return;
            };
            vec![
                field("col.id", short_id(&c.id).to_string()),
                field("col.image", c.image.clone()),
                field("detail.command", c.command.clone()),
                field(
                    "col.created",
                    format!(
                        "{}   Ports: {}",
                        super::app::ago(app.now, c.created),
                        if c.ports.is_empty() {
                            "none".to_string()
                        } else {
                            c.ports_text()
                        }
                    ),
                ),
            ]
        }
        Kind::Images => {
            // A training-mode row's ID is one of the image's tags.
            let tagged = |i: &&podbag::client::ImageSummary| i.repo_tags.iter().any(|t| t == id);
            let Some(i) = app.lists.images.iter().find(|i| i.id == id || tagged(i)) else {
                return;
            };
            vec![
                field("col.id", i.id.clone()),
                field("detail.tags", i.repo_tags.join(", ")),
                field(
                    "detail.used",
                    app.lists
                        .containers
                        .iter()
                        .filter(|c| c.image == i.id || i.repo_tags.contains(&c.image))
                        .map(|c| c.name())
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
                field("col.size", super::app::size(i.size)),
                field("col.created", super::app::ago(app.now, i.created)),
            ]
        }
        Kind::Volumes => {
            let Some(v) = app.lists.volumes.iter().find(|v| v.name == id) else {
                return;
            };
            vec![
                field("col.name", v.name.clone()),
                field("col.driver", v.driver.clone()),
                field("detail.path", v.mountpoint.clone()),
                field("col.created", v.created_at.clone()),
            ]
        }
        Kind::Networks => {
            let Some(n) = app.lists.networks.iter().find(|n| n.id == id) else {
                return;
            };
            vec![
                field("col.id", n.id.clone()),
                field("col.driver", n.driver.clone()),
                field("col.subnet", n.subnet().to_string()),
                field(
                    "detail.gateway",
                    n.ipam
                        .config
                        .first()
                        .map(|c| c.gateway.clone())
                        .unwrap_or_default(),
                ),
                field("detail.scope", n.scope.clone()),
            ]
        }
    };
    let block = Block::new()
        .borders(Borders::TOP)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {name} "),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Paragraph::new(lines).block(block), area);
}

fn draw_text(f: &mut Frame, app: &mut App, area: Rect) {
    let Mode::Text {
        title,
        lines,
        scroll,
        logs,
        follow,
    } = &app.mode
    else {
        return;
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    let inner = block.inner(area);
    let page = usize::from(inner.height).max(1);
    let last = lines.len().saturating_sub(page);
    let start = if *follow { last } else { (*scroll).min(last) };
    let shown: Vec<Line> = if lines.is_empty() && *logs {
        vec![Line::styled(
            app.appearance.text("detail.noLogs"),
            Style::new().fg(Color::DarkGray),
        )]
    } else {
        lines
            .iter()
            .skip(start)
            .take(page)
            .map(|l| Line::from(l.clone()))
            .collect()
    };
    f.render_widget(Paragraph::new(shown).block(block), area);
    app.page = page;
}

fn draw_message(f: &mut Frame, app: &App, area: Rect) {
    let line = if let Mode::Search = app.mode {
        Line::from(vec![
            Span::styled("/", Style::new().fg(ACCENT)),
            Span::raw(app.search.clone()),
            Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED)),
        ])
    } else if let Some(busy) = &app.busy {
        Line::styled(format!("… {busy}"), Style::new().fg(Color::Yellow))
    } else if let Some((text, error)) = &app.message {
        let color = if *error { Color::Red } else { Color::Green };
        Line::styled(text.clone(), Style::new().fg(color))
    } else if !app.search.is_empty() {
        Line::from(vec![
            Span::styled("Search: ", Style::new().fg(Color::DarkGray)),
            Span::raw(app.search.clone()),
        ])
    } else if let Some(v) = &app.update {
        Line::styled(
            format!(
                "{} · a",
                app.appearance
                    .text("update.available")
                    .replace("{version}", v)
            ),
            Style::new().fg(Color::Yellow),
        )
    } else {
        Line::styled(app.endpoint.clone(), Style::new().fg(Color::DarkGray))
    };
    f.render_widget(Paragraph::new(line), area);
}

/// The hints that fit WIDTH columns. `? help` and `q quit` stay at the end: the middle
/// hints go first, from the end of the middle, then the first hint.
fn fit_hints(mut hints: Vec<(&'static str, String)>, width: usize) -> Vec<(&'static str, String)> {
    let wide = |hints: &[(&str, String)]| {
        let words: usize = hints
            .iter()
            .map(|(k, what)| Span::raw(*k).width() + 1 + Span::raw(what.as_str()).width())
            .sum();
        words + hints.len().saturating_sub(1)
    };
    let tail = hints
        .iter()
        .rev()
        .take_while(|(k, _)| matches!(*k, "?" | "q"))
        .count();
    // The last hint before the tail: the end of the middle, and at last the first hint.
    while wide(&hints) > width && hints.len() > tail {
        hints.remove(hints.len() - tail - 1);
    }
    hints
}

fn draw_keys(f: &mut Frame, app: &mut App, area: Rect) {
    let mut spans = Vec::new();
    let hints = fit_hints(
        app.key_help()
            .into_iter()
            .map(|(k, what)| (k, app.appearance.hint(what)))
            .collect(),
        usize::from(area.width),
    );
    for (n, (k, what)) in hints.iter().enumerate() {
        if n > 0 {
            spans.push(Span::raw(" "));
        }
        spans.push(Span::styled(
            *k,
            Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
        ));
        spans.push(Span::raw(format!(" {what}")));
    }
    let places = place(area, &spans);
    // Each hint is a gap (but the first), its key and its words.
    let mut at = 0;
    for (n, (k, _)) in hints.iter().enumerate() {
        if n > 0 {
            at += 1;
        }
        let (key, words) = (places[at], places[at + 1]);
        at += 2;
        if let Some(event) = hint_key(k) {
            app.areas
                .keys
                .push((key.union(words).intersection(area), event));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// The pull form: where from, the login there, the host and name, search results.
fn draw_pull(f: &mut Frame, app: &App, form: &PullForm) {
    let dim = Style::new().fg(Color::DarkGray);
    let cursor = Span::styled(" ", Style::new().add_modifier(Modifier::REVERSED));
    let local = |english: &str, key: &str| {
        if app.appearance.lang() == "en" {
            english.to_owned()
        } else {
            app.appearance.text(key)
        }
    };
    let label = |t: &str| Span::styled(format!("{t:<7}"), dim);
    let field = |name: &str, text: &str, typing: bool| {
        let mut spans = vec![label(name), Span::raw(text.to_string())];
        if typing {
            spans.push(cursor.clone());
        }
        Line::from(spans)
    };
    let place = form.place();
    let login = match app.logins.get(&place) {
        None => Span::styled(local("Checking the login…", "engine.checking"), dim),
        Some(None) => Span::styled(local("Not logged in", "terminal.noAccount"), dim),
        Some(Some(user)) => Span::styled(
            local("Logged in as {user}", "terminal.account").replace("{user}", user),
            Style::new().fg(Color::Green),
        ),
    };
    let mut lines = vec![
        Line::from(vec![
            label(&local("From", "pull.location")),
            Span::styled(
                place.label(),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  (Tab / F2)", dim),
        ]),
        Line::from(vec![label(&local("Login", "pull.user")), login]),
    ];
    if place.needs_host() {
        lines.push(field(
            &local("Host", "settings.socket"),
            &form.host,
            form.on_host,
        ));
    }
    lines.push(field(
        &local("Image", "col.image"),
        &form.name,
        !form.on_host,
    ));
    if let Some(hint) = place.login_hint() {
        lines.push(Line::from(""));
        lines.push(Line::styled(app.appearance.text("pull.login"), dim));
        // `aws ecr … | docker login …` on two lines.
        for part in hint.split(" | ") {
            lines.push(Line::styled(format!("  {part}"), dim));
        }
    }
    if let Some(e) = &form.error {
        lines.push(Line::from(""));
        lines.push(Line::styled(e.clone(), Style::new().fg(Color::Red)));
    } else if !form.found.is_empty() {
        lines.push(Line::from(""));
        for (i, r) in form.found.iter().enumerate() {
            let chosen = form.chosen == Some(i);
            let style = if chosen {
                Style::new().add_modifier(Modifier::REVERSED | Modifier::BOLD)
            } else {
                Style::new()
            };
            let about: String = r.about.chars().take(50).collect();
            lines.push(Line::from(vec![
                Span::styled(format!("{:<28}", r.name), style),
                Span::styled(format!(" {about}"), dim),
            ]));
        }
    } else if app.can_search && place == podbag::client::Location::DockerHub {
        lines.push(Line::from(""));
        lines.push(Line::styled(
            local("Type a name to search Docker Hub.", "pull.hint"),
            dim,
        ));
    }
    popup(f, &app.appearance.text("action.pull"), lines);
}

/// A question, with Yes and No to click.
fn draw_confirm(f: &mut Frame, app: &mut App, question: String) {
    let button = Style::new()
        .fg(Color::White)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD);
    let buttons = vec![
        Span::raw("  "),
        Span::styled(format!(" {} ", app.appearance.text("terminal.yes")), button),
        Span::raw("   "),
        Span::styled(
            format!(" {} ", app.appearance.text("terminal.no")),
            Style::new().add_modifier(Modifier::REVERSED),
        ),
    ];
    let rect = popup(
        f,
        &app.appearance.text("confirm.title"),
        vec![
            Line::from(question),
            Line::from(""),
            Line::from(buttons.clone()),
        ],
    );
    // Inside the border, on the third line.
    let line = Rect::new(rect.x + 1, rect.y + 3, rect.width.saturating_sub(2), 1);
    if line.y < rect.y + rect.height.saturating_sub(1) {
        let places = place(line, &buttons);
        app.areas.yes = Some(places[1]);
        app.areas.no = Some(places[3]);
    }
}

fn draw_help(f: &mut Frame, app: &App) {
    let key = |k: &str, what: &str| {
        Line::from(vec![
            Span::styled(
                format!(" {k:<13}"),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::raw(what.to_string()),
        ])
    };
    let name = |key: &str| app.appearance.text(key);
    let mut lines = vec![
        key(
            "1–4, Tab",
            &Kind::ALL
                .map(|k| name(&format!("nav.{}", k.segment())))
                .join(" / "),
        ),
        key(
            "Alt+← / →",
            &format!("{} / {}", name("action.back"), name("action.forward")),
        ),
        key("/  Esc", &name("search.placeholder")),
        key("⏎ i", &name("action.inspect")),
        key("a / F2", &name("col.actions")),
        key("F5", &name("action.refresh")),
        key(
            "F6–F9",
            &super::dashboard::Page::ALL
                .map(|p| name(p.label()))
                .join(" / "),
        ),
        key(
            "b  [ ]",
            &format!("{} / {}", name("sidebar.show"), name("sidebar.resize")),
        ),
        key(
            "E",
            &format!("{} · podbag / Docker", name("settings.engine")),
        ),
        key(
            "s x r",
            &["action.start", "action.stop", "action.restart"]
                .map(name)
                .join(" / "),
        ),
        key("l", &name("action.logs")),
        key(
            "p  d  c",
            &["action.pull", "action.remove", "action.prune"]
                .map(name)
                .join(" / "),
        ),
        key("v", &name("volume.create")),
    ];
    if app.shell {
        lines.push(key("e", &name("action.shell")));
    }
    if app.features.pause {
        lines.push(key(
            "p",
            &["action.pause", "action.resume"].map(name).join(" / "),
        ));
    }
    if app.features.rename {
        lines.push(key("n", &name("action.rename")));
    }
    if app.docker.is_some() {
        lines.push(key("D", &name("docker.command")));
    }
    lines.push(key("m", &name("terminal.mouse")));
    lines.push(key("q", &name("tray.quit")));
    popup(f, &name("terminal.keys"), lines);
}

/// A box in the middle of the screen; where it is.
fn popup(f: &mut Frame, title: &str, lines: Vec<Line>) -> Rect {
    let area = f.area();
    let width = lines
        .iter()
        .map(Line::width)
        .max()
        .unwrap_or(0)
        .max(title.chars().count() + 4)
        .max(30) as u16
        + 4;
    let width = width.min(area.width.saturating_sub(2));
    let height = (lines.len() as u16 + 2).min(area.height.saturating_sub(2));
    let rect = Rect {
        x: area.x + (area.width.saturating_sub(width)) / 2,
        y: area.y + (area.height.saturating_sub(height)) / 2,
        width,
        height,
    };
    let block = Block::bordered()
        .border_style(Style::new().fg(ACCENT))
        .title(Span::styled(
            format!(" {title} "),
            Style::new().add_modifier(Modifier::BOLD),
        ));
    f.render_widget(Clear, rect);
    f.render_widget(Paragraph::new(lines).block(block), rect);
    rect
}

/// A drawn screen as text, one line per row.
pub fn buffer_text(buf: &Buffer) -> String {
    let a = buf.area;
    (a.y..a.y + a.height)
        .map(|y| {
            (a.x..a.x + a.width)
                .map(|x| buf[(x, y)].symbol())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::app::tests::{code, key, sample};
    use super::super::app::{Effect, Msg};
    use super::*;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| draw(f, app)).unwrap();
        buffer_text(t.backend().buffer())
    }

    #[test]
    fn hints_that_do_not_fit_go_from_the_middle() {
        let hints = |v: &[(&'static str, &str)]| -> Vec<(&'static str, String)> {
            v.iter().map(|(k, w)| (*k, w.to_string())).collect()
        };
        let (a, t, s, x) = (
            ("a", "actions"),
            (":", "training"),
            ("s", "start"),
            ("x", "stop"),
        );
        let (help, quit) = (("?", "help"), ("q", "quit"));
        let all = hints(&[a, t, s, x, help, quit]);
        // 49 columns in all.
        assert_eq!(fit_hints(all.clone(), 49), all);
        assert_eq!(fit_hints(all.clone(), 48), hints(&[a, t, s, help, quit]));
        assert_eq!(fit_hints(all.clone(), 41), hints(&[a, t, help, quit]));
        assert_eq!(fit_hints(all.clone(), 33), hints(&[a, help, quit]));
        // Help and quit always stay.
        assert_eq!(fit_hints(all.clone(), 22), hints(&[help, quit]));
        assert_eq!(fit_hints(all.clone(), 5), hints(&[help, quit]));
        // Wide letters count twice.
        let wide = hints(&[("a", "작업"), help, quit]);
        assert_eq!(fit_hints(wide.clone(), 20), wide);
        assert_eq!(fit_hints(wide.clone(), 19), hints(&[help, quit]));
        // Without help and quit (search): the end of the line goes first.
        let search = hints(&[("Enter", "done"), ("Esc", "clear"), ("↑↓", "move")]);
        assert_eq!(fit_hints(search.clone(), 28), search);
        assert_eq!(
            fit_hints(search.clone(), 27),
            hints(&[("Enter", "done"), ("Esc", "clear")])
        );
    }

    #[test]
    fn containers_at_80_by_24() {
        let mut app = sample();
        let s = screen(&mut app, 80, 24);
        for want in [
            "podbag",
            "1 Containers 2",
            "2 Images 1",
            "4 Networks 2",
            "NAME",
            "web",
            "nginx:alpine",
            "Up 2 minutes",
            "8080->80/tcp",
            "Exited (0) 1 hour ago",
            "a1b2c3d4e5f6",
            "polling",
        ] {
            assert!(s.contains(want), "{want:?} missing in\n{s}");
        }
        // The whole key line fits.
        let keys = s.lines().last().unwrap();
        assert!(
            keys.contains("s start") && keys.ends_with("q quit"),
            "{keys}"
        );
        assert_eq!(s.lines().count(), 24);
    }

    #[test]
    fn every_tab_draws() {
        let mut app = sample();
        app.on_key(key('2'));
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("alpine:latest") && s.contains("8MB"), "{s}");
        assert!(s.contains("p pull"), "{s}");
        app.on_key(key('3'));
        assert!(screen(&mut app, 80, 24).contains("pgdata"));
        app.on_key(key('4'));
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("bridge") && s.contains("app"), "{s}");
    }

    #[test]
    fn empty_lists_say_what_to_do() {
        let mut app = sample();
        app.lists.containers.clear();
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("No containers yet"), "{s}");
        assert!(s.contains("podbag run -d"), "{s}");
        // With Docker, the docker command.
        app.engine = "Docker".into();
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("  docker run -d"), "{s}");
        app.engine = "podbag".into();
        app.search = "zzz".into();
        app.lists = sample().lists;
        assert!(screen(&mut app, 80, 24).contains("Nothing matches"));
    }

    #[test]
    fn questions_help_and_prompts() {
        let mut app = sample();
        app.on_key(key('d'));
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("Stop and remove container web?"), "{s}");
        assert!(s.contains("y yes"), "{s}");
        app.on_key(code(KeyCode::Esc));
        app.on_key(key('?'));
        let s = screen(&mut app, 80, 24);
        assert!(
            s.contains("Keys") && s.contains("Start / Stop / Restart"),
            "{s}"
        );
        assert!(!s.contains("Pause / Resume"), "{s}");
        app.on_key(key('x'));
        app.on_key(key('2'));
        app.on_key(key('p'));
        assert!(screen(&mut app, 80, 24).contains("Pull image"));
    }

    #[test]
    fn logs_and_inspect_take_the_body() {
        let mut app = sample();
        let generation = app.open_logs("web");
        assert!(screen(&mut app, 80, 24).contains("No output yet"));
        for n in 0..100 {
            app.on_msg(Msg::LogLine(generation, format!("request {n}")));
        }
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("Logs: web") && s.contains("request 99"), "{s}");
        assert!(!s.contains("request 50"), "{s}");
        assert_eq!(app.on_key(code(KeyCode::Esc)), Effect::CloseLogs);
        app.on_msg(Msg::Text(
            "Inspect: web".into(),
            vec!["{".into(), "  \"Id\": \"a1b2\"".into(), "}".into()],
        ));
        let s = screen(&mut app, 80, 24);
        assert!(
            s.contains("Inspect: web") && s.contains("\"Id\": \"a1b2\""),
            "{s}"
        );
    }

    #[test]
    fn small_terminals_still_draw() {
        let mut app = sample();
        let s = screen(&mut app, 40, 10);
        assert!(s.contains("web"), "{s}");
    }
}
