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
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState};

const ACCENT: Color = Color::Blue;

fn title(kind: Kind) -> &'static str {
    match kind {
        Kind::Containers => "Containers",
        Kind::Images => "Images",
        Kind::Volumes => "Volumes",
        Kind::Networks => "Networks",
    }
}

pub fn draw(f: &mut Frame, app: &mut App) {
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
        Span::styled("  Engine ", dim),
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
        let text = if app.loaded {
            format!(" {} {} {} ", n + 1, title(*k), app.count(*k))
        } else {
            format!(" {} {} ", n + 1, title(*k))
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

fn headers(kind: Kind) -> (Vec<&'static str>, Vec<Constraint>) {
    match kind {
        Kind::Containers => (
            vec!["NAME", "IMAGE", "STATUS", "PORTS"],
            vec![
                Constraint::Fill(2),
                Constraint::Fill(3),
                Constraint::Fill(3),
                Constraint::Fill(3),
            ],
        ),
        Kind::Images => (
            vec!["NAME", "ID", "SIZE", "CREATED"],
            vec![
                Constraint::Fill(4),
                Constraint::Length(12),
                Constraint::Length(9),
                Constraint::Fill(2),
            ],
        ),
        Kind::Volumes => (
            vec!["NAME", "DRIVER", "CREATED"],
            vec![
                Constraint::Fill(3),
                Constraint::Length(8),
                Constraint::Fill(2),
            ],
        ),
        Kind::Networks => (
            vec!["NAME", "DRIVER", "SUBNET", "ID"],
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
    if !app.loaded {
        return vec![Line::from("Loading…")];
    }
    if !app.search.is_empty() {
        return vec![Line::from(format!(
            "Nothing matches \"{}\". Press Esc to clear the search.",
            app.search
        ))];
    }
    if !matches!(app.engine.as_str(), "podbag" | "Docker") && app.tab != Kind::Images {
        return vec![Line::from(format!(
            "No {} on this engine.",
            app.tab.segment()
        ))];
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
            Line::from("No containers yet. Start one in another terminal:"),
            Line::from(""),
            Line::styled(
                format!("  {cli} run -d --name web -p 8080:80 nginx:alpine"),
                hint,
            ),
        ],
        Kind::Images => vec![Line::from("No images yet. Press p to pull one.")],
        Kind::Volumes => vec![
            Line::from("No volumes yet. Create one:"),
            Line::from(""),
            Line::styled(format!("  {cli} volume create data"), hint),
        ],
        Kind::Networks => vec![
            Line::from("No networks yet. Create one:"),
            Line::from(""),
            Line::styled(format!("  {cli} network create app"), hint),
        ],
    }
}

fn draw_list(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = app.rows(app.tab);
    let (list_area, details_area) = if area.height >= 14 && !rows.is_empty() {
        let [a, b] = Layout::vertical([Constraint::Min(5), Constraint::Length(6)]).areas(area);
        (a, Some(b))
    } else {
        (area, None)
    };
    if rows.is_empty() {
        let block = Block::new()
            .borders(Borders::TOP)
            .border_style(Style::new().fg(Color::DarkGray))
            .title(Span::styled(
                format!(" {} ", title(app.tab)),
                Style::new().add_modifier(Modifier::BOLD),
            ));
        f.render_widget(Paragraph::new(empty_text(app)).block(block), list_area);
        return;
    }
    let (names, widths) = headers(app.tab);
    let header = Row::new(names.into_iter().map(Cell::from)).style(
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
                field("ID", short_id(&c.id).to_string()),
                field("Image", c.image.clone()),
                field("Command", c.command.clone()),
                field(
                    "Created",
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
            let Some(i) = app.lists.images.iter().find(|i| i.id == id) else {
                return;
            };
            vec![
                field("ID", i.id.clone()),
                field("Tags", i.repo_tags.join(", ")),
                field("Size", super::app::size(i.size)),
                field("Created", super::app::ago(app.now, i.created)),
            ]
        }
        Kind::Volumes => {
            let Some(v) = app.lists.volumes.iter().find(|v| v.name == id) else {
                return;
            };
            vec![
                field("Name", v.name.clone()),
                field("Driver", v.driver.clone()),
                field("Path", v.mountpoint.clone()),
                field("Created", v.created_at.clone()),
            ]
        }
        Kind::Networks => {
            let Some(n) = app.lists.networks.iter().find(|n| n.id == id) else {
                return;
            };
            vec![
                field("ID", n.id.clone()),
                field("Driver", n.driver.clone()),
                field("Subnet", n.subnet().to_string()),
                field("Scope", n.scope.clone()),
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
            "No output yet.",
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
            format!("Update {v} available: podbag upgrade"),
            Style::new().fg(Color::Yellow),
        )
    } else {
        Line::styled(app.endpoint.clone(), Style::new().fg(Color::DarkGray))
    };
    f.render_widget(Paragraph::new(line), area);
}

fn draw_keys(f: &mut Frame, app: &mut App, area: Rect) {
    let mut spans = Vec::new();
    let hints = app.key_help();
    for (n, (k, what)) in hints.iter().enumerate() {
        if n > 0 {
            spans.push(Span::raw("  "));
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
        None => Span::styled("Checking the login…", dim),
        Some(None) => Span::styled("Not logged in", dim),
        Some(Some(user)) => Span::styled(
            format!("Logged in as {user}"),
            Style::new().fg(Color::Green),
        ),
    };
    let mut lines = vec![
        Line::from(vec![
            label("From"),
            Span::styled(
                place.label(),
                Style::new().fg(ACCENT).add_modifier(Modifier::BOLD),
            ),
            Span::styled("  (Tab: another)", dim),
        ]),
        Line::from(vec![label("Login"), login]),
    ];
    if place.needs_host() {
        lines.push(field("Host", &form.host, form.on_host));
    }
    lines.push(field("Image", &form.name, !form.on_host));
    if let Some(hint) = place.login_hint() {
        lines.push(Line::from(""));
        lines.push(Line::styled("To log in:", dim));
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
        lines.push(Line::styled("Type a name to search Docker Hub.", dim));
    }
    popup(f, "Pull image", lines);
}

/// A question, with Yes and No to click.
fn draw_confirm(f: &mut Frame, app: &mut App, question: String) {
    let button = Style::new()
        .fg(Color::White)
        .bg(ACCENT)
        .add_modifier(Modifier::BOLD);
    let buttons = vec![
        Span::raw("  "),
        Span::styled(" Yes ", button),
        Span::raw("   "),
        Span::styled(" No ", Style::new().add_modifier(Modifier::REVERSED)),
    ];
    let rect = popup(
        f,
        "Are you sure?",
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
    let head = |t: &str| Line::styled(t.to_string(), Style::new().add_modifier(Modifier::BOLD));
    let mut lines = vec![
        head("All lists"),
        key("1-4 ←→ Tab", "switch list"),
        key("↑↓ PgUp PgDn", "move"),
        key("⏎ i", "inspect"),
        key("/  Esc", "find, clear"),
        key("c", "prune what is not used"),
        key("F5", "refresh now"),
        key("E", "podbag or Docker (saved)"),
        key("m", "mouse on or off"),
        key("q", "quit"),
        head("Containers"),
        key("s x r", "start, stop, restart"),
        key("d", "remove"),
    ];
    lines.push(if app.shell {
        key("l  e", "logs, shell (sh)")
    } else {
        key("l", "logs")
    });
    if app.features.pause {
        lines.push(key("p", "pause or resume"));
    }
    if app.features.rename {
        lines.push(key("n", "rename"));
    }
    if app.docker.is_some() {
        lines.insert(9, key("D", "the docker command: podbag or back"));
    }
    lines.push(head("Images"));
    lines.push(key("p  d", "pull, remove"));
    popup(f, "Keys", lines);
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

#[cfg(test)]
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
            s.contains("Keys") && s.contains("start, stop, restart"),
            "{s}"
        );
        assert!(!s.contains("pause or resume"), "{s}");
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
