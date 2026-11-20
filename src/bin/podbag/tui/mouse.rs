//! Clicks and the wheel. Where things are comes from the last drawn frame ([`Areas`]),
//! so clicks stay right after a resize.

use super::app::{App, Effect, Mode};
use podbag::client::Kind;
use ratatui::crossterm::event::{
    KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::layout::{Position, Rect};
use std::time::{Duration, Instant};

/// Two clicks on a row within this time open it.
const DOUBLE_CLICK: Duration = Duration::from_millis(500);
/// Lines of text one wheel step scrolls.
const WHEEL_LINES: usize = 3;

/// What was where in the last drawn frame.
#[derive(Debug, Clone, Default)]
pub struct Areas {
    pub tabs: Vec<(Rect, Kind)>,
    pub pages: Vec<(Rect, super::dashboard::Page)>,
    /// The engine's name in the header.
    pub engine: Option<Rect>,
    /// Where the `docker` command goes, in the header.
    pub docker: Option<Rect>,
    /// The list's rows, and the index of the first row shown.
    pub rows: Option<(Rect, usize)>,
    /// The key hints at the bottom, with the key each one stands for.
    pub keys: Vec<(Rect, KeyEvent)>,
    pub yes: Option<Rect>,
    pub no: Option<Rect>,
}

/// The key a hint at the bottom stands for; None for hints of several keys (`↑↓`).
pub fn hint_key(hint: &str) -> Option<KeyEvent> {
    let code = match hint {
        "⏎" | "Enter" => KeyCode::Enter,
        "Esc" | "any key" => KeyCode::Esc,
        "Tab" => KeyCode::Tab,
        "F2" => KeyCode::F(2),
        "F3" => KeyCode::F(3),
        _ => {
            let mut chars = hint.chars();
            match (chars.next(), chars.next()) {
                (Some(c), None) => KeyCode::Char(c),
                _ => return None,
            }
        }
    };
    Some(KeyEvent::new(code, KeyModifiers::NONE))
}

fn char_key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}

impl App {
    /// A mouse event; `at`: when it came (for double clicks).
    pub fn on_mouse(&mut self, m: MouseEvent, at: Instant) -> Effect {
        match m.kind {
            MouseEventKind::ScrollDown => self.wheel(true),
            MouseEventKind::ScrollUp => self.wheel(false),
            MouseEventKind::Down(MouseButton::Left) => {
                self.click(Position::new(m.column, m.row), at)
            }
            _ => Effect::None,
        }
    }

    fn wheel(&mut self, down: bool) -> Effect {
        match self.mode {
            Mode::Text { .. } => {
                let code = if down { KeyCode::Down } else { KeyCode::Up };
                for _ in 0..WHEEL_LINES {
                    self.on_key(KeyEvent::new(code, KeyModifiers::NONE));
                }
            }
            Mode::List | Mode::Search => self.move_by(if down { 1 } else { -1 }),
            _ => {}
        }
        Effect::None
    }

    fn click(&mut self, p: Position, at: Instant) -> Effect {
        let inside = |r: Option<Rect>| r.is_some_and(|r| r.contains(p));
        // A hint at the bottom acts like its key, whatever is shown.
        if let Some((_, key)) = self.areas.keys.iter().find(|(r, _)| r.contains(p)) {
            return self.on_key(*key);
        }
        match self.mode {
            Mode::Confirm { .. } if inside(self.areas.yes) => self.on_key(char_key('y')),
            Mode::Confirm { .. } if inside(self.areas.no) => self.on_key(char_key('n')),
            Mode::Help => {
                self.mode = Mode::List;
                Effect::None
            }
            Mode::List | Mode::Search => self.click_list(p, at),
            _ => Effect::None,
        }
    }

    fn click_list(&mut self, p: Position, at: Instant) -> Effect {
        if let Some((_, page)) = self.areas.pages.iter().find(|(r, _)| r.contains(p)) {
            return Effect::Page(*page);
        }
        if let Some((_, kind)) = self.areas.tabs.iter().find(|(r, _)| r.contains(p)) {
            let kind = *kind;
            self.mode = Mode::List;
            self.switch(kind);
            return Effect::None;
        }
        if self.areas.engine.is_some_and(|r| r.contains(p)) {
            self.mode = Mode::List;
            return Effect::SwitchEngine;
        }
        if self.areas.docker.is_some_and(|r| r.contains(p)) {
            self.mode = Mode::List;
            return self.on_key(char_key('D'));
        }
        let Some((rows, first)) = self.areas.rows else {
            return Effect::None;
        };
        if !rows.contains(p) {
            return Effect::None;
        }
        let i = first + usize::from(p.y - rows.y);
        if i >= self.rows(self.tab).len() {
            return Effect::None;
        }
        self.select(i);
        let double = self.last_click.is_some_and(|(then, kind, row)| {
            kind == self.tab && row == i && at.saturating_duration_since(then) <= DOUBLE_CLICK
        });
        if double {
            self.last_click = None;
            self.mode = Mode::List;
            return self.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE));
        }
        self.last_click = Some((at, self.tab, i));
        Effect::None
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::super::app::tests::{code, key, sample};
    use super::super::app::{App, Effect, Mode, Verb};
    use super::super::view;
    use podbag::client::Kind;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::{
        KeyCode, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
    };
    use std::time::{Duration, Instant};

    fn screen(app: &mut App, w: u16, h: u16) -> String {
        let mut t = Terminal::new(TestBackend::new(w, h)).unwrap();
        t.draw(|f| view::draw(f, app)).unwrap();
        view::buffer_text(t.backend().buffer())
    }

    /// Where `text` is on the screen: column and row of its first character.
    fn find(screen: &str, text: &str) -> (u16, u16) {
        for (y, line) in screen.lines().enumerate() {
            if let Some(i) = line.find(text) {
                let x = line[..i].chars().count();
                return (x as u16, y as u16);
            }
        }
        panic!("{text:?} not on the screen:\n{screen}")
    }

    fn mouse(kind: MouseEventKind, (column, row): (u16, u16)) -> MouseEvent {
        MouseEvent {
            kind,
            column,
            row,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn click(app: &mut App, at: (u16, u16), when: Instant) -> Effect {
        app.on_mouse(mouse(MouseEventKind::Down(MouseButton::Left), at), when)
    }

    #[test]
    fn a_click_on_a_tab_switches_the_list() {
        let mut app = sample();
        let s = screen(&mut app, 80, 24);
        assert_eq!(
            click(&mut app, find(&s, "2 Images"), Instant::now()),
            Effect::None
        );
        assert_eq!(app.tab, Kind::Images);
        let s = screen(&mut app, 80, 24);
        click(&mut app, find(&s, "4 Networks"), Instant::now());
        assert_eq!(app.tab, Kind::Networks);
    }

    #[test]
    fn a_click_selects_a_row_and_a_double_click_opens_it() {
        let mut app = sample();
        let s = screen(&mut app, 80, 24);
        let job = find(&s, "job");
        let t = Instant::now();
        assert_eq!(click(&mut app, job, t), Effect::None);
        assert_eq!(app.current().unwrap().name, "job");
        // Again soon: its details, as Enter.
        let open = click(&mut app, job, t + Duration::from_millis(200));
        assert!(
            matches!(open, Effect::Inspect { ref name, .. } if name == "job"),
            "{open:?}"
        );
        // Two slow clicks are two clicks.
        let mut app = sample();
        screen(&mut app, 80, 24);
        click(&mut app, job, t);
        assert_eq!(
            click(&mut app, job, t + Duration::from_secs(2)),
            Effect::None
        );
        // A click under the last row selects nothing.
        let s = screen(&mut app, 80, 24);
        let (x, y) = find(&s, "job");
        click(&mut app, find(&s, "web"), t + Duration::from_secs(5));
        click(&mut app, (x, y + 1), t + Duration::from_secs(9));
        assert_eq!(app.current().unwrap().name, "web");
    }

    #[test]
    fn the_wheel_moves_the_list_and_scrolls_text() {
        let mut app = sample();
        screen(&mut app, 80, 24);
        let down =
            |app: &mut App| app.on_mouse(mouse(MouseEventKind::ScrollDown, (5, 5)), Instant::now());
        down(&mut app);
        assert_eq!(app.current().unwrap().name, "job");
        app.on_mouse(mouse(MouseEventKind::ScrollUp, (5, 5)), Instant::now());
        assert_eq!(app.current().unwrap().name, "web");
        // Logs and details scroll.
        let generation = app.open_logs("web");
        for n in 0..100 {
            app.on_msg(super::super::app::Msg::LogLine(
                generation,
                format!("line {n}"),
            ));
        }
        screen(&mut app, 80, 24);
        app.on_mouse(mouse(MouseEventKind::ScrollUp, (5, 5)), Instant::now());
        let Mode::Text { follow, scroll, .. } = &app.mode else {
            panic!()
        };
        assert!(!follow);
        let up = *scroll;
        down(&mut app);
        let Mode::Text { scroll, .. } = &app.mode else {
            panic!()
        };
        assert!(*scroll > up);
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("line 99"), "{s}");
    }

    #[test]
    fn hints_act_like_their_keys() {
        let mut app = sample();
        let s = screen(&mut app, 80, 24);
        let start = click(&mut app, find(&s, "s start"), Instant::now());
        assert!(
            matches!(
                start,
                Effect::Act {
                    verb: Verb::Start,
                    ..
                }
            ),
            "{start:?}"
        );
        // The word after the key works too.
        let (x, y) = find(&s, "q quit");
        assert_eq!(click(&mut app, (x + 3, y), Instant::now()), Effect::Quit);
        // The help pop-up goes on a click.
        app.on_key(key('?'));
        let s = screen(&mut app, 80, 24);
        click(&mut app, find(&s, "Keys"), Instant::now());
        assert_eq!(app.mode, Mode::List);
    }

    #[test]
    fn yes_and_no_in_a_question() {
        let mut app = sample();
        app.on_key(key('d'));
        let s = screen(&mut app, 80, 24);
        assert_eq!(
            click(&mut app, find(&s, "No"), Instant::now()),
            Effect::None
        );
        assert_eq!(app.mode, Mode::List);
        app.on_key(key('c'));
        let s = screen(&mut app, 80, 24);
        assert_eq!(
            click(&mut app, find(&s, "Yes"), Instant::now()),
            Effect::Prune(Kind::Containers)
        );
    }

    #[test]
    fn the_engine_name_switches_the_engine() {
        let mut app = sample();
        let s = screen(&mut app, 80, 24);
        let (x, y) = find(&s, "Engine");
        // The label itself, after "Engine ".
        assert_eq!(
            click(&mut app, (x + 8, y), Instant::now()),
            Effect::SwitchEngine
        );
        assert_eq!(app.on_key(key('E')), Effect::SwitchEngine);
    }

    #[test]
    fn the_header_shows_where_docker_goes() {
        let mut app = sample();
        assert!(!screen(&mut app, 80, 24).contains("docker →"));
        app.docker = Some(podbag::client::DockerTarget {
            context: Some("desktop-linux".into()),
            host: "unix:///x/docker.sock".into(),
            name: "Docker Desktop".into(),
            is_podbag: false,
            from_env: false,
        });
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("docker → Docker Desktop"), "{s}");
        assert_eq!(
            click(&mut app, find(&s, "Docker Desktop"), Instant::now()),
            Effect::None
        );
        assert!(matches!(app.mode, Mode::Confirm { .. }));
    }

    #[test]
    fn hit_areas_follow_a_resize() {
        let mut app = sample();
        app.on_key(key('d'));
        let small = screen(&mut app, 80, 24);
        let yes_small = find(&small, "Yes");
        let big = screen(&mut app, 120, 40);
        let yes_big = find(&big, "Yes");
        assert_ne!(yes_small, yes_big);
        // The old place is nothing now.
        assert_eq!(click(&mut app, yes_small, Instant::now()), Effect::None);
        assert!(matches!(app.mode, Mode::Confirm { .. }));
        assert!(matches!(
            click(&mut app, yes_big, Instant::now()),
            Effect::Act {
                verb: Verb::Remove { .. },
                ..
            }
        ));
        // The key line moves down with the bottom.
        let s = screen(&mut app, 100, 30);
        let (x, y) = find(&s, "q quit");
        assert_eq!(y, 29);
        assert_eq!(click(&mut app, (x, 23), Instant::now()), Effect::None);
        assert_eq!(click(&mut app, (x, y), Instant::now()), Effect::Quit);
    }

    #[test]
    fn m_turns_the_mouse_off_and_on() {
        let mut app = sample();
        assert!(app.mouse);
        assert_eq!(app.on_key(key('m')), Effect::Mouse(false));
        assert!(!app.mouse);
        assert_eq!(app.on_key(key('m')), Effect::Mouse(true));
        app.on_key(key('?'));
        let s = screen(&mut app, 80, 24);
        assert!(s.contains("mouse on or off"), "{s}");
        app.on_key(code(KeyCode::Esc));
    }
}
