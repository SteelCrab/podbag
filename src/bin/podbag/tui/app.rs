//! The TUI's state and its keys. No input or output here: a key changes the state and
//! may ask the runtime for an [`Effect`]; the runtime's results come back as [`Msg`].

use podbag::client::{
    ContainerSummary, Features, ImageSummary, Kind, NetworkSummary, VolumeSummary, short_id,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// The objects, as last listed.
#[derive(Debug, Clone, Default)]
pub struct Lists {
    pub containers: Vec<ContainerSummary>,
    pub images: Vec<ImageSummary>,
    pub volumes: Vec<VolumeSummary>,
    pub networks: Vec<NetworkSummary>,
}

/// How a row looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Tone {
    Normal,
    Running,
    Stopped,
    Other,
}

/// One row of a list: what the user sees, and what an action needs.
#[derive(Debug, Clone)]
pub struct Row {
    pub id: String,
    pub name: String,
    pub cells: Vec<String>,
    pub tone: Tone,
    /// A running container; a network that cannot be removed.
    pub running: bool,
    pub builtin: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verb {
    Start,
    Stop,
    Restart,
    Pause,
    Unpause,
    Remove { force: bool },
}

impl Verb {
    /// The API action (`start`, …); removal has none.
    pub fn action(self) -> &'static str {
        match self {
            Verb::Start => "start",
            Verb::Stop => "stop",
            Verb::Restart => "restart",
            Verb::Pause => "pause",
            Verb::Unpause => "unpause",
            Verb::Remove { .. } => "remove",
        }
    }

    /// What is shown while it runs.
    pub fn doing(self) -> &'static str {
        match self {
            Verb::Start => "Starting",
            Verb::Stop => "Stopping",
            Verb::Restart => "Restarting",
            Verb::Pause => "Pausing",
            Verb::Unpause => "Resuming",
            Verb::Remove { .. } => "Removing",
        }
    }

    /// What is shown when it is done.
    pub fn done(self) -> &'static str {
        match self {
            Verb::Start => "Started",
            Verb::Stop => "Stopped",
            Verb::Restart => "Restarted",
            Verb::Pause => "Paused",
            Verb::Unpause => "Resumed",
            Verb::Remove { .. } => "Removed",
        }
    }
}

/// What the runtime must do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    None,
    Quit,
    Refresh,
    Act {
        kind: Kind,
        id: String,
        name: String,
        verb: Verb,
    },
    Inspect {
        kind: Kind,
        id: String,
        name: String,
        running: bool,
    },
    Logs {
        id: String,
        name: String,
    },
    CloseLogs,
    Shell {
        id: String,
        name: String,
    },
    Pull(String),
    Prune(Kind),
    Rename {
        id: String,
        to: String,
    },
}

/// What the runtime sends back.
#[derive(Debug)]
pub enum Msg {
    Lists(Result<Lists, String>),
    /// The events stream is open (true) or closed (false: polling).
    Live(bool),
    /// An event: the lists changed.
    Changed,
    Features(Features),
    /// An action ended: what to show, and whether it failed.
    Done(String, bool),
    /// Inspect's text.
    Text(String, Vec<String>),
    /// A log line, for the log view of this generation.
    LogLine(u64, String),
    LogEnd(u64, Option<String>),
    /// A line of a pull's progress.
    Progress(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    Pull,
    Rename { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Mode {
    List,
    /// Typing the search text.
    Search,
    Help,
    /// Inspect or logs: lines to scroll. `follow`: stay at the end (logs).
    Text {
        title: String,
        lines: Vec<String>,
        scroll: usize,
        logs: bool,
        follow: bool,
    },
    Confirm {
        question: String,
        then: Effect,
    },
    Input {
        prompt: String,
        text: String,
        purpose: Purpose,
    },
}

/// The most log lines kept.
const MAX_LOG_LINES: usize = 5000;

pub struct App {
    pub tab: Kind,
    pub lists: Lists,
    /// Whether the lists came at least once.
    pub loaded: bool,
    /// The selected row of each tab, in its shown (searched) rows.
    pub selected: [usize; 4],
    pub search: String,
    pub mode: Mode,
    /// The last message, and whether it is an error.
    pub message: Option<(String, bool)>,
    /// A long action that runs (a pull), with its last progress line.
    pub busy: Option<String>,
    pub features: Features,
    /// The events stream is open; otherwise the lists are polled.
    pub live: bool,
    /// The engine, as shown in the title.
    pub engine: String,
    /// Unix seconds, for "5 minutes ago".
    pub now: i64,
    /// The log view's generation: lines of an older one are dropped.
    pub log_generation: u64,
    /// The height of the text view, for page keys (set when drawn).
    pub page: usize,
}

fn tab_index(k: Kind) -> usize {
    Kind::ALL.iter().position(|x| *x == k).unwrap_or(0)
}

/// `Up 5 minutes` → seconds ago, as text: `5 minutes ago`.
pub fn ago(now: i64, then: i64) -> String {
    if then <= 0 {
        return String::new();
    }
    let secs = u64::try_from(now - then).unwrap_or(0);
    format!(
        "{} ago",
        podbag::api::docker::human_duration(secs).to_lowercase()
    )
}

/// RFC 3339 (`2026-11-01T10:00:00Z`) as Unix seconds.
fn epoch(rfc3339: &str) -> i64 {
    podbag::api::docker::epoch_of(rfc3339).map_or(0, |f| f as i64)
}

pub fn size(bytes: i64) -> String {
    podbag::image::human_size(u64::try_from(bytes).unwrap_or(0))
}

impl App {
    pub fn new(engine: String) -> App {
        App {
            tab: Kind::Containers,
            lists: Lists::default(),
            loaded: false,
            selected: [0; 4],
            search: String::new(),
            mode: Mode::List,
            message: None,
            busy: None,
            features: Features::default(),
            live: false,
            engine,
            now: 0,
            log_generation: 0,
            page: 10,
        }
    }

    /// The rows of a tab, searched.
    pub fn rows(&self, kind: Kind) -> Vec<Row> {
        let all: Vec<Row> = match kind {
            Kind::Containers => self
                .lists
                .containers
                .iter()
                .map(|c| Row {
                    id: c.id.clone(),
                    name: c.name().to_string(),
                    cells: vec![
                        c.name().to_string(),
                        c.image.clone(),
                        c.status.clone(),
                        c.ports_text(),
                    ],
                    tone: match c.state.as_str() {
                        "running" => Tone::Running,
                        "exited" | "dead" | "created" => Tone::Stopped,
                        _ => Tone::Other,
                    },
                    running: c.running(),
                    builtin: false,
                })
                .collect(),
            Kind::Images => self
                .lists
                .images
                .iter()
                .map(|i| Row {
                    id: i.id.clone(),
                    name: i.name(),
                    cells: vec![
                        i.name(),
                        short_id(&i.id).to_string(),
                        size(i.size),
                        ago(self.now, i.created),
                    ],
                    tone: Tone::Normal,
                    running: false,
                    builtin: false,
                })
                .collect(),
            Kind::Volumes => self
                .lists
                .volumes
                .iter()
                .map(|v| Row {
                    id: v.name.clone(),
                    name: v.name.clone(),
                    cells: vec![
                        v.name.clone(),
                        v.driver.clone(),
                        ago(self.now, epoch(&v.created_at)),
                    ],
                    tone: Tone::Normal,
                    running: false,
                    builtin: false,
                })
                .collect(),
            Kind::Networks => self
                .lists
                .networks
                .iter()
                .map(|n| Row {
                    id: n.id.clone(),
                    name: n.name.clone(),
                    cells: vec![
                        n.name.clone(),
                        n.driver.clone(),
                        n.subnet().to_string(),
                        short_id(&n.id).to_string(),
                    ],
                    tone: if n.builtin() {
                        Tone::Stopped
                    } else {
                        Tone::Normal
                    },
                    running: false,
                    builtin: n.builtin(),
                })
                .collect(),
        };
        let needle = self.search.to_lowercase();
        if needle.is_empty() {
            return all;
        }
        all.into_iter()
            .filter(|r| {
                r.id.starts_with(&needle)
                    || r.cells.iter().any(|c| c.to_lowercase().contains(&needle))
            })
            .collect()
    }

    pub fn count(&self, kind: Kind) -> usize {
        match kind {
            Kind::Containers => self.lists.containers.len(),
            Kind::Images => self.lists.images.len(),
            Kind::Volumes => self.lists.volumes.len(),
            Kind::Networks => self.lists.networks.len(),
        }
    }

    pub fn selected_index(&self) -> usize {
        let n = self.rows(self.tab).len();
        self.selected[tab_index(self.tab)].min(n.saturating_sub(1))
    }

    pub fn current(&self) -> Option<Row> {
        let rows = self.rows(self.tab);
        let i = self.selected_index();
        rows.into_iter().nth(i)
    }

    fn select(&mut self, i: usize) {
        self.selected[tab_index(self.tab)] = i;
    }

    fn move_by(&mut self, delta: isize) {
        let n = self.rows(self.tab).len();
        if n == 0 {
            return;
        }
        let i = self.selected_index() as isize + delta;
        self.select(i.clamp(0, n as isize - 1) as usize);
    }

    fn switch(&mut self, k: Kind) {
        self.tab = k;
        self.search.clear();
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some((text.into(), error));
    }

    /// New lists; the selection stays on the same object where it can.
    pub fn set_lists(&mut self, lists: Lists) {
        let before: Vec<Option<String>> = Kind::ALL
            .iter()
            .map(|k| {
                let rows = self.rows(*k);
                let i = self.selected[tab_index(*k)];
                rows.get(i).map(|r| r.id.clone())
            })
            .collect();
        self.lists = lists;
        self.loaded = true;
        for (n, k) in Kind::ALL.iter().enumerate() {
            if let Some(id) = &before[n]
                && let Some(i) = self.rows(*k).iter().position(|r| &r.id == id)
            {
                self.selected[n] = i;
            }
        }
    }

    pub fn on_msg(&mut self, msg: Msg) {
        match msg {
            Msg::Lists(Ok(l)) => self.set_lists(l),
            Msg::Lists(Err(e)) => self.say(e, true),
            Msg::Live(l) => self.live = l,
            Msg::Changed => {}
            Msg::Features(f) => self.features = f,
            Msg::Done(text, error) => {
                self.busy = None;
                self.say(text, error);
            }
            Msg::Text(title, lines) => {
                self.mode = Mode::Text {
                    title,
                    lines,
                    scroll: 0,
                    logs: false,
                    follow: false,
                }
            }
            Msg::LogLine(generation, line) => {
                if generation != self.log_generation {
                    return;
                }
                if let Mode::Text {
                    lines, logs: true, ..
                } = &mut self.mode
                {
                    lines.push(line);
                    if lines.len() > MAX_LOG_LINES {
                        lines.drain(..lines.len() - MAX_LOG_LINES);
                    }
                }
            }
            Msg::LogEnd(generation, error) => {
                if generation == self.log_generation
                    && let Some(e) = error
                {
                    self.say(e, true);
                }
            }
            Msg::Progress(line) => self.busy = Some(line),
        }
    }

    /// Open the log view (a new generation); the runtime starts the stream.
    pub fn open_logs(&mut self, name: &str) -> u64 {
        self.log_generation += 1;
        self.mode = Mode::Text {
            title: format!("Logs: {name}"),
            lines: Vec::new(),
            scroll: 0,
            logs: true,
            follow: true,
        };
        self.log_generation
    }

    pub fn on_key(&mut self, key: KeyEvent) -> Effect {
        if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('c') {
            return Effect::Quit;
        }
        match self.mode.clone() {
            Mode::List => self.list_key(key),
            Mode::Search => {
                match key.code {
                    KeyCode::Enter => self.mode = Mode::List,
                    KeyCode::Esc => {
                        self.search.clear();
                        self.mode = Mode::List;
                    }
                    KeyCode::Backspace => {
                        self.search.pop();
                    }
                    KeyCode::Up => self.move_by(-1),
                    KeyCode::Down => self.move_by(1),
                    KeyCode::Char(c) => {
                        self.search.push(c);
                        self.select(0);
                    }
                    _ => {}
                }
                Effect::None
            }
            Mode::Help => {
                self.mode = Mode::List;
                Effect::None
            }
            Mode::Text { .. } => self.text_key(key),
            Mode::Confirm { then, .. } => match key.code {
                KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
                    self.mode = Mode::List;
                    then
                }
                KeyCode::Char('n' | 'N' | 'q') | KeyCode::Esc => {
                    self.mode = Mode::List;
                    Effect::None
                }
                _ => Effect::None,
            },
            Mode::Input {
                prompt,
                mut text,
                purpose,
            } => match key.code {
                KeyCode::Esc => {
                    self.mode = Mode::List;
                    Effect::None
                }
                KeyCode::Enter => {
                    self.mode = Mode::List;
                    let text = text.trim().to_string();
                    if text.is_empty() {
                        return Effect::None;
                    }
                    match purpose {
                        Purpose::Pull => {
                            self.busy = Some(format!("Pulling {text}"));
                            Effect::Pull(text)
                        }
                        Purpose::Rename { id } => Effect::Rename { id, to: text },
                    }
                }
                code => {
                    match code {
                        KeyCode::Backspace => {
                            text.pop();
                        }
                        KeyCode::Char(c) => text.push(c),
                        _ => {}
                    }
                    self.mode = Mode::Input {
                        prompt,
                        text,
                        purpose,
                    };
                    Effect::None
                }
            },
        }
    }

    fn text_key(&mut self, key: KeyEvent) -> Effect {
        let page = self.page.max(1);
        let Mode::Text {
            lines,
            scroll,
            logs,
            follow,
            ..
        } = &mut self.mode
        else {
            return Effect::None;
        };
        let last = lines.len().saturating_sub(page);
        // While following, the view is at the end.
        if *follow {
            *scroll = last;
        }
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                let was_logs = *logs;
                self.mode = Mode::List;
                return if was_logs {
                    Effect::CloseLogs
                } else {
                    Effect::None
                };
            }
            KeyCode::Up | KeyCode::Char('k') => {
                *follow = false;
                *scroll = scroll.saturating_sub(1);
            }
            KeyCode::Down | KeyCode::Char('j') => *scroll = (*scroll + 1).min(last),
            KeyCode::PageUp => {
                *follow = false;
                *scroll = scroll.saturating_sub(page);
            }
            KeyCode::PageDown | KeyCode::Char(' ') => *scroll = (*scroll + page).min(last),
            KeyCode::Home | KeyCode::Char('g') => {
                *follow = false;
                *scroll = 0;
            }
            KeyCode::End | KeyCode::Char('G') => {
                *scroll = last;
                *follow = *logs;
            }
            KeyCode::Char('f') if *logs => {
                *follow = !*follow;
                *scroll = last;
            }
            _ => {}
        }
        if *logs && *scroll >= last {
            *follow = true;
        }
        Effect::None
    }

    fn list_key(&mut self, key: KeyEvent) -> Effect {
        match key.code {
            KeyCode::Char('q') => return Effect::Quit,
            KeyCode::Esc => {
                self.search.clear();
                self.message = None;
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Tab | KeyCode::Right => {
                self.switch(Kind::ALL[(tab_index(self.tab) + 1) % 4]);
            }
            KeyCode::BackTab | KeyCode::Left => {
                self.switch(Kind::ALL[(tab_index(self.tab) + 3) % 4]);
            }
            KeyCode::Char(c @ '1'..='4') => {
                self.switch(Kind::ALL[usize::from(c as u8 - b'1')]);
            }
            KeyCode::Up | KeyCode::Char('k') => self.move_by(-1),
            KeyCode::Down | KeyCode::Char('j') => self.move_by(1),
            KeyCode::PageUp => self.move_by(-10),
            KeyCode::PageDown => self.move_by(10),
            KeyCode::Home | KeyCode::Char('g') => self.select(0),
            KeyCode::End | KeyCode::Char('G') => self.move_by(isize::MAX / 2),
            KeyCode::F(5) => return Effect::Refresh,
            KeyCode::Char('c') => return self.confirm_prune(),
            KeyCode::Char('p') if self.tab == Kind::Images => {
                self.mode = Mode::Input {
                    prompt: "Pull image".into(),
                    text: String::new(),
                    purpose: Purpose::Pull,
                };
            }
            _ => return self.row_key(key),
        }
        Effect::None
    }

    /// Keys that act on the selected row.
    fn row_key(&mut self, key: KeyEvent) -> Effect {
        let Some(row) = self.current() else {
            return Effect::None;
        };
        let kind = self.tab;
        let act = |verb| Effect::Act {
            kind,
            id: row.id.clone(),
            name: row.name.clone(),
            verb,
        };
        let containers = kind == Kind::Containers;
        match key.code {
            KeyCode::Enter | KeyCode::Char('i') => Effect::Inspect {
                kind,
                id: row.id.clone(),
                name: row.name.clone(),
                running: row.running,
            },
            KeyCode::Char('d') | KeyCode::Delete => {
                if row.builtin {
                    self.say(
                        format!("{} is built in: it cannot be removed", row.name),
                        true,
                    );
                    return Effect::None;
                }
                let what = match kind {
                    Kind::Containers => "container",
                    Kind::Images => "image",
                    Kind::Volumes => "volume",
                    Kind::Networks => "network",
                };
                let question = if row.running {
                    format!("Stop and remove container {}?", row.name)
                } else if kind == Kind::Volumes {
                    format!("Remove volume {} and its data?", row.name)
                } else {
                    format!("Remove {what} {}?", row.name)
                };
                self.mode = Mode::Confirm {
                    question,
                    then: act(Verb::Remove { force: row.running }),
                };
                Effect::None
            }
            KeyCode::Char('s') if containers => act(Verb::Start),
            KeyCode::Char('x') if containers => act(Verb::Stop),
            KeyCode::Char('r') if containers => act(Verb::Restart),
            KeyCode::Char('p') if containers && self.features.pause => {
                let paused = self
                    .lists
                    .containers
                    .iter()
                    .any(|c| c.id == row.id && c.state == "paused");
                act(if paused { Verb::Unpause } else { Verb::Pause })
            }
            KeyCode::Char('n') if containers && self.features.rename => {
                self.mode = Mode::Input {
                    prompt: format!("New name for {}", row.name),
                    text: row.name.clone(),
                    purpose: Purpose::Rename { id: row.id.clone() },
                };
                Effect::None
            }
            KeyCode::Char('l') if containers => Effect::Logs {
                id: row.id.clone(),
                name: row.name.clone(),
            },
            KeyCode::Char('e') if containers => {
                if row.running {
                    Effect::Shell {
                        id: row.id.clone(),
                        name: row.name.clone(),
                    }
                } else {
                    self.say(
                        format!("{} is not running: start it first (s)", row.name),
                        true,
                    );
                    Effect::None
                }
            }
            _ => Effect::None,
        }
    }

    fn confirm_prune(&mut self) -> Effect {
        let question = match self.tab {
            Kind::Containers => "Remove all stopped containers?",
            Kind::Images => "Remove all images that no container uses?",
            Kind::Volumes => "Remove all volumes that no container uses, with their data?",
            Kind::Networks => "Remove all networks that no container uses?",
        };
        self.mode = Mode::Confirm {
            question: question.into(),
            then: Effect::Prune(self.tab),
        };
        Effect::None
    }

    /// The keys shown at the bottom, for the current tab and mode: (key, what it does).
    pub fn key_help(&self) -> Vec<(&'static str, &'static str)> {
        match &self.mode {
            Mode::Search => vec![("Enter", "done"), ("Esc", "clear"), ("↑↓", "move")],
            Mode::Text { logs: true, .. } => vec![
                ("↑↓", "scroll"),
                ("PgUp/PgDn", "page"),
                ("f", "follow"),
                ("Esc", "back"),
            ],
            Mode::Text { .. } => vec![("↑↓", "scroll"), ("PgUp/PgDn", "page"), ("Esc", "back")],
            Mode::Confirm { .. } => vec![("y", "yes"), ("n", "no")],
            Mode::Input { .. } => vec![("Enter", "ok"), ("Esc", "cancel")],
            Mode::Help => vec![("any key", "back")],
            Mode::List => {
                let mut keys = match self.tab {
                    Kind::Containers => vec![
                        ("s", "start"),
                        ("x", "stop"),
                        ("r", "restart"),
                        ("d", "remove"),
                        ("l", "logs"),
                        ("e", "shell"),
                    ],
                    Kind::Images => vec![("p", "pull"), ("d", "remove"), ("c", "prune")],
                    Kind::Volumes | Kind::Networks => vec![("d", "remove"), ("c", "prune")],
                };
                if self.tab != Kind::Containers {
                    keys.push(("⏎", "inspect"));
                }
                keys.extend([("/", "find"), ("?", "help"), ("q", "quit")]);
                keys
            }
        }
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use ratatui::crossterm::event::KeyEvent;

    pub fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    pub fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    pub fn sample() -> App {
        let mut app = App::new("unix:///run/user/1000/podbag/docker.sock".into());
        app.now = 1_700_003_600;
        let containers: Vec<ContainerSummary> =
            serde_json::from_value(podbag::client::fake::containers_json()).unwrap();
        app.set_lists(Lists {
            containers,
            images: vec![ImageSummary {
                id: "sha256:2222222222222222".into(),
                repo_tags: vec!["alpine:latest".into()],
                size: 8_000_000,
                created: 1_700_000_000,
                containers: 1,
            }],
            volumes: vec![VolumeSummary {
                name: "pgdata".into(),
                driver: "local".into(),
                ..Default::default()
            }],
            networks: serde_json::from_value(serde_json::json!([
                {"Name": "bridge", "Id": "b0", "Driver": "bridge"},
                {"Name": "app", "Id": "a1", "Driver": "bridge"}
            ]))
            .unwrap(),
        });
        app
    }

    #[test]
    fn actions_on_the_selected_container() {
        let mut app = sample();
        assert_eq!(app.current().unwrap().name, "web");
        let stop = app.on_key(key('x'));
        assert!(matches!(stop, Effect::Act { verb: Verb::Stop, ref name, .. } if name == "web"));
        app.on_key(code(KeyCode::Down));
        assert_eq!(app.current().unwrap().name, "job");
        assert!(matches!(
            app.on_key(key('s')),
            Effect::Act {
                verb: Verb::Start,
                ..
            }
        ));
        // No shell in a stopped container.
        assert_eq!(app.on_key(key('e')), Effect::None);
        assert!(app.message.as_ref().unwrap().1);
    }

    #[test]
    fn remove_asks_first() {
        let mut app = sample();
        assert_eq!(app.on_key(key('d')), Effect::None);
        let Mode::Confirm { question, .. } = &app.mode else {
            panic!("no question")
        };
        assert_eq!(question, "Stop and remove container web?");
        // No: nothing happens.
        assert_eq!(app.on_key(key('n')), Effect::None);
        assert_eq!(app.mode, Mode::List);
        app.on_key(key('d'));
        assert!(matches!(
            app.on_key(key('y')),
            Effect::Act {
                verb: Verb::Remove { force: true },
                ..
            }
        ));
    }

    #[test]
    fn built_in_networks_stay() {
        let mut app = sample();
        app.on_key(key('4'));
        assert_eq!(app.current().unwrap().name, "bridge");
        assert_eq!(app.on_key(key('d')), Effect::None);
        assert_eq!(app.mode, Mode::List);
        app.on_key(code(KeyCode::Down));
        app.on_key(key('d'));
        assert!(matches!(app.mode, Mode::Confirm { .. }));
    }

    #[test]
    fn search_narrows_the_list() {
        let mut app = sample();
        app.on_key(key('/'));
        for c in "job".chars() {
            app.on_key(key(c));
        }
        app.on_key(code(KeyCode::Enter));
        let rows = app.rows(Kind::Containers);
        assert_eq!(rows.len(), 1);
        assert_eq!(app.current().unwrap().name, "job");
        app.on_key(code(KeyCode::Esc));
        assert_eq!(app.rows(Kind::Containers).len(), 2);
    }

    #[test]
    fn pull_asks_for_a_name() {
        let mut app = sample();
        app.on_key(key('2'));
        app.on_key(key('p'));
        for c in "redis:7".chars() {
            app.on_key(key(c));
        }
        assert_eq!(
            app.on_key(code(KeyCode::Enter)),
            Effect::Pull("redis:7".into())
        );
        assert!(app.busy.is_some());
        app.on_msg(Msg::Done("Pulled redis:7".into(), false));
        assert!(app.busy.is_none());
    }

    #[test]
    fn missing_features_have_no_keys() {
        let mut app = sample();
        assert_eq!(app.on_key(key('p')), Effect::None);
        assert_eq!(app.on_key(key('n')), Effect::None);
        app.features.pause = true;
        assert!(matches!(
            app.on_key(key('p')),
            Effect::Act {
                verb: Verb::Pause,
                ..
            }
        ));
    }

    #[test]
    fn selection_follows_the_object() {
        let mut app = sample();
        app.on_key(code(KeyCode::Down));
        let mut lists = app.lists.clone();
        lists.containers.reverse();
        app.set_lists(lists);
        assert_eq!(app.current().unwrap().name, "job");
        assert_eq!(app.selected_index(), 0);
    }

    #[test]
    fn logs_follow_until_scrolled_up() {
        let mut app = sample();
        app.page = 2;
        let generation = app.open_logs("web");
        for n in 0..5 {
            app.on_msg(Msg::LogLine(generation, format!("line {n}")));
        }
        // A stale stream's lines are dropped.
        app.on_msg(Msg::LogLine(generation - 1, "old".into()));
        let Mode::Text { lines, .. } = &app.mode else {
            panic!()
        };
        assert_eq!(lines.len(), 5);
        app.on_key(code(KeyCode::Up));
        let Mode::Text { follow, scroll, .. } = &app.mode else {
            panic!()
        };
        assert!(!follow);
        assert_eq!(*scroll, 2);
        assert_eq!(app.on_key(code(KeyCode::Esc)), Effect::CloseLogs);
    }
}
