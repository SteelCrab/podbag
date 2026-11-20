//! The TUI's state and its keys. No input or output here: a key changes the state and
//! may ask the runtime for an [`Effect`]; the runtime's results come back as [`Msg`].

use super::mouse::Areas;
use super::pull::PullForm;
use podbag::client::{
    ApiError, ContainerSummary, DockerTarget, Features, ImageSummary, Kind, Location,
    NetworkSummary, SearchResult, VolumeSummary, short_id,
};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::HashMap;
use std::time::Instant;

/// The objects, as last listed.
#[derive(Debug, Clone, Default)]
pub struct Lists {
    pub containers: Vec<ContainerSummary>,
    pub images: Vec<ImageSummary>,
    pub volumes: Vec<VolumeSummary>,
    pub networks: Vec<NetworkSummary>,
    pub errors: [Option<String>; 4],
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
    Dashboard,
    Page(super::dashboard::Page),
    SaveLayout,
    ChooseRegistry,
    RegistryLogin(Location),
    CreateVolume,
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
    /// Go from podbag to Docker, or back (and save the choice).
    SwitchEngine,
    /// Take the mouse (true) or leave it to the terminal.
    Mouse(bool),
    /// Who is logged in at a pull location.
    Login(Location),
    /// Search Docker Hub (once the typing stops).
    Search(String),
    /// Point the `docker` command at podbag (true), or back to the context before.
    DockerCli {
        to_podbag: bool,
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
    /// A newer podbag, as `x.y.z`.
    Update(Option<String>),
    /// The account logged in at a location (None: no login).
    Login(Location, Option<String>),
    /// What a search for this text found.
    Found(String, Result<Vec<SearchResult>, ApiError>),
    /// Where the `docker` command goes now, and the context `D` goes back to.
    DockerTarget(DockerTarget, Option<String>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Purpose {
    PullTag { repository: String },
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
    Pull(PullForm),
}

/// The most log lines kept.
const MAX_LOG_LINES: usize = 5000;

pub struct App {
    pub appearance: super::preferences::Appearance,
    pub tab: Kind,
    pub lists: Lists,
    /// Whether the lists came at least once.
    pub loaded: bool,
    /// The selected row of each tab, in its shown (searched) rows.
    pub selected: [usize; 4],
    pub search: String,
    searches: [String; 4],
    history: Vec<Kind>,
    history_index: usize,
    pub mode: Mode,
    /// The last message, and whether it is an error.
    pub message: Option<(String, bool)>,
    /// A long action that runs (a pull), with its last progress line.
    pub busy: Option<String>,
    pub features: Features,
    /// The events stream is open; otherwise the lists are polled.
    pub live: bool,
    /// The engine, as shown in the header: `podbag`, `Docker` or a socket's path.
    pub engine: String,
    /// Where its API is, as shown when there is no message.
    pub endpoint: String,
    /// The macOS VM's state (podbag only): running, standby, starting, stopped.
    pub vm: Option<&'static str>,
    /// A newer podbag, as `x.y.z`.
    pub update: Option<String>,
    /// `e` can open a shell (Docker needs the docker CLI).
    pub shell: bool,
    /// The TUI takes the mouse.
    pub mouse: bool,
    /// Where things were in the last drawn frame.
    pub areas: Areas,
    /// The last click on a row: when, in which list, which row.
    pub last_click: Option<(Instant, Kind, usize)>,
    /// Where the `docker` command goes (None: no docker CLI, so no `D`).
    pub docker: Option<DockerTarget>,
    /// The context `D` goes back to from podbag.
    pub docker_previous: Option<String>,
    /// The engine can search Docker Hub (false once it says it cannot).
    pub can_search: bool,
    /// Who is logged in at each pull location, as far as known.
    pub logins: HashMap<Location, Option<String>>,
    /// Unix seconds, for "5 minutes ago".
    pub now: i64,
    /// The log view's generation: lines of an older one are dropped.
    pub log_generation: u64,
    /// The height of the text view, for page keys (set when drawn).
    pub page: usize,
}

pub(super) fn tab_index(k: Kind) -> usize {
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
        podbag::client::time::human_duration(secs).to_lowercase()
    )
}

/// RFC 3339 (`2026-11-01T10:00:00Z`) as Unix seconds.
fn epoch(rfc3339: &str) -> i64 {
    podbag::client::time::epoch_of(rfc3339).map_or(0, |f| f as i64)
}

pub fn size(bytes: i64) -> String {
    podbag::image::human_size(u64::try_from(bytes).unwrap_or(0))
}

impl App {
    pub fn new(engine: String, endpoint: String) -> App {
        App {
            appearance: super::preferences::Appearance::default(),
            tab: Kind::Containers,
            lists: Lists::default(),
            loaded: false,
            selected: [0; 4],
            search: String::new(),
            searches: Default::default(),
            history: vec![Kind::Containers],
            history_index: 0,
            mode: Mode::List,
            message: None,
            busy: None,
            features: Features::default(),
            live: false,
            engine,
            endpoint,
            vm: None,
            update: None,
            shell: true,
            mouse: true,
            areas: Areas::default(),
            last_click: None,
            docker: None,
            docker_previous: None,
            can_search: true,
            logins: HashMap::new(),
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
        let query = if self.tab == kind {
            &self.search
        } else {
            &self.searches[tab_index(kind)]
        };
        let needle = query.trim().to_lowercase();
        if needle.is_empty() {
            return all;
        }
        all.into_iter()
            .enumerate()
            .filter(|(index, r)| {
                (kind == Kind::Images
                    && self.lists.images[*index]
                        .repo_tags
                        .iter()
                        .any(|tag| tag.to_lowercase().contains(&needle)))
                    || (kind == Kind::Containers
                        && self.lists.containers[*index]
                            .state
                            .to_lowercase()
                            .contains(&needle))
                    || r.id.to_lowercase().contains(&needle)
                    || r.cells.iter().any(|c| c.to_lowercase().contains(&needle))
            })
            .map(|(_, row)| row)
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

    pub(super) fn select(&mut self, i: usize) {
        self.selected[tab_index(self.tab)] = i;
    }

    pub(super) fn move_by(&mut self, delta: isize) {
        let n = self.rows(self.tab).len();
        if n == 0 {
            return;
        }
        let i = self.selected_index() as isize + delta;
        self.select(i.clamp(0, n as isize - 1) as usize);
    }

    pub(super) fn switch(&mut self, k: Kind) {
        if self.tab == k {
            return;
        }
        self.history.truncate(self.history_index + 1);
        self.history.push(k);
        self.history_index += 1;
        self.show_tab(k);
    }

    fn show_tab(&mut self, k: Kind) {
        self.searches[tab_index(self.tab)] = std::mem::take(&mut self.search);
        self.tab = k;
        self.search = std::mem::take(&mut self.searches[tab_index(k)]);
    }

    fn say(&mut self, text: impl Into<String>, error: bool) {
        self.message = Some((text.into(), error));
    }

    /// Another engine: nothing of the old one stays.
    pub fn switched(&mut self, engine: String, endpoint: String, shell: bool) {
        self.message = Some((format!("Engine: {engine}"), false));
        self.engine = engine;
        self.endpoint = endpoint;
        self.shell = shell;
        self.lists = Lists::default();
        self.loaded = false;
        self.selected = [0; 4];
        self.searches = Default::default();
        self.history = vec![self.tab];
        self.history_index = 0;
        self.search.clear();
        self.mode = Mode::List;
        self.busy = None;
        self.features = Features::default();
        self.live = false;
        self.vm = None;
        self.last_click = None;
        self.can_search = true;
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
            Msg::Update(v) => self.update = v,
            Msg::Login(location, account) => {
                self.logins.insert(location, account);
            }
            Msg::Found(term, found) => self.found(&term, found),
            Msg::DockerTarget(target, previous) => {
                self.docker = Some(target);
                self.docker_previous = previous;
            }
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

    pub fn paste(&mut self, input: &str) -> Effect {
        if !matches!(self.mode, Mode::Search | Mode::Pull(_) | Mode::Input { .. }) {
            return Effect::None;
        }
        let mut effect = Effect::None;
        for c in input.chars().filter(|c| !c.is_control()) {
            effect = self.on_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        effect
    }

    pub(super) fn open_pull(&mut self) -> Effect {
        let form = PullForm::default();
        let login = Effect::Login(form.place());
        self.mode = Mode::Pull(form);
        login
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
            Mode::Pull(form) => self.pull_key(key, form),
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
                    if let Purpose::PullTag { repository } = purpose {
                        let tag = if text.is_empty() { "latest" } else { &text };
                        let image = format!("{repository}:{tag}");
                        self.busy = Some(format!("Pulling {image}"));
                        return Effect::Pull(image);
                    }
                    if text.is_empty() {
                        return Effect::None;
                    }
                    match purpose {
                        Purpose::Rename { id } => Effect::Rename { id, to: text },
                        Purpose::PullTag { .. } => unreachable!(),
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
            KeyCode::Char('P' | 'p')
                if key
                    .modifiers
                    .contains(KeyModifiers::CONTROL | KeyModifiers::SHIFT) =>
            {
                return self.open_pull();
            }
            KeyCode::Char('q') => return Effect::Quit,
            KeyCode::Esc => {
                self.search.clear();
                self.message = None;
            }
            KeyCode::Char('b') => {
                self.appearance.sidebar = !self.appearance.sidebar;
                return Effect::SaveLayout;
            }
            KeyCode::Char(c @ ('[' | ']')) if self.appearance.sidebar => {
                let delta = if c == '[' { -2 } else { 2 };
                self.appearance.sidebar_width = self
                    .appearance
                    .sidebar_width
                    .saturating_add_signed(delta)
                    .clamp(14, 36);
                return Effect::SaveLayout;
            }
            KeyCode::F(n @ 6..=9) => {
                return Effect::Page(super::dashboard::Page::ALL[usize::from(n - 6)]);
            }
            KeyCode::Char('?') => self.mode = Mode::Help,
            KeyCode::Char('/') => self.mode = Mode::Search,
            KeyCode::Left | KeyCode::Right if key.modifiers.contains(KeyModifiers::ALT) => {
                let step = if key.code == KeyCode::Left { -1 } else { 1 };
                self.history_index = self
                    .history_index
                    .saturating_add_signed(step)
                    .min(self.history.len() - 1);
                self.show_tab(self.history[self.history_index]);
            }
            KeyCode::Char('f' | 'k') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.mode = Mode::Search
            }
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
            KeyCode::F(2) | KeyCode::Char('a') => return Effect::Dashboard,
            KeyCode::Char('v') if self.tab == Kind::Volumes => return Effect::CreateVolume,
            KeyCode::Char('E') => return Effect::SwitchEngine,
            KeyCode::Char('D') => self.ask_docker_cli(),
            KeyCode::Char('m') => {
                self.mouse = !self.mouse;
                let text = if self.mouse {
                    "The mouse is on"
                } else {
                    "The mouse is off: the terminal selects text"
                };
                self.say(text, false);
                return Effect::Mouse(self.mouse);
            }
            KeyCode::Char('c') => return self.confirm_prune(),
            KeyCode::Char('p') if self.tab == Kind::Images => {
                return self.open_pull();
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
                let force = containers
                    && self
                        .lists
                        .containers
                        .iter()
                        .any(|c| c.id == row.id && (c.running() || c.state == "paused"));
                let question = if self.appearance.lang() != "en" {
                    self.appearance
                        .text(match kind {
                            Kind::Containers if force => "confirm.removeRunning",
                            Kind::Containers => "confirm.removeContainer",
                            Kind::Images => "confirm.removeImage",
                            Kind::Volumes => "confirm.removeVolume",
                            Kind::Networks => "confirm.removeNetwork",
                        })
                        .replace("{name}", &row.name)
                } else if force {
                    format!("Stop and remove container {}?", row.name)
                } else if kind == Kind::Volumes {
                    format!("Remove volume {} and its data?", row.name)
                } else {
                    format!("Remove {what} {}?", row.name)
                };
                self.mode = Mode::Confirm {
                    question,
                    then: act(Verb::Remove { force }),
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
                if !self.shell {
                    self.say("No shell: the docker CLI is not installed", true);
                    Effect::None
                } else if row.running {
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

    /// `D`: point the `docker` command at podbag, or back; a question first.
    fn ask_docker_cli(&mut self) {
        let Some(d) = &self.docker else {
            return;
        };
        let (mut question, to_podbag) = if d.is_podbag {
            let back = self.docker_previous.as_deref().unwrap_or("default");
            (
                format!("Point the docker command back to the context {back}?"),
                false,
            )
        } else {
            (
                format!(
                    "Point the docker command at podbag? It goes to {} now.",
                    d.name
                ),
                true,
            )
        };
        if d.from_env {
            question.push_str(" DOCKER_HOST is set: shells that set it keep using it.");
        }
        self.mode = Mode::Confirm {
            question,
            then: Effect::DockerCli { to_podbag },
        };
    }

    fn confirm_prune(&mut self) -> Effect {
        let question = self.appearance.text(match self.tab {
            Kind::Containers => "confirm.pruneContainers",
            Kind::Images => "confirm.pruneImages",
            Kind::Volumes => "confirm.pruneVolumes",
            Kind::Networks => "confirm.pruneNetworks",
        });
        self.mode = Mode::Confirm {
            question,
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
            Mode::Pull(_) => vec![
                ("F2", "registries"),
                ("F3", "login"),
                ("Tab", "from"),
                ("↑↓", "choose"),
                ("Enter", "pull"),
                ("Esc", "cancel"),
            ],
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
                if !self.shell {
                    keys.retain(|(k, _)| *k != "e");
                }
                if self.tab != Kind::Containers {
                    keys.push(("⏎", "inspect"));
                }
                keys.insert(0, ("a", "actions"));
                if self.tab == Kind::Volumes {
                    keys.push(("v", "create"));
                }
                keys.extend([("/", "find"), ("?", "help"), ("q", "quit")]);
                keys
            }
        }
    }
}

#[cfg(all(test, unix))]
pub mod tests {
    use super::*;
    use podbag::client::DockerTarget;
    use ratatui::crossterm::event::KeyEvent;

    pub fn key(c: char) -> KeyEvent {
        KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
    }

    pub fn code(c: KeyCode) -> KeyEvent {
        KeyEvent::new(c, KeyModifiers::NONE)
    }

    pub fn sample() -> App {
        let mut app = App::new(
            "podbag".into(),
            "unix:///run/user/1000/podbag/docker.sock".into(),
        );
        app.appearance.language = "en".into();
        app.now = 1_700_003_600;
        let containers: Vec<ContainerSummary> =
            serde_json::from_value(podbag::client::fake::containers_json()).unwrap();
        app.set_lists(Lists {
            errors: Default::default(),
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
    fn dashboard_and_volume_creation_work_with_empty_lists() {
        let mut app = App::new("podbag".into(), String::new());
        assert_eq!(app.on_key(key('a')), Effect::Dashboard);
        app.on_key(key('3'));
        assert_eq!(app.on_key(key('v')), Effect::CreateVolume);
        assert_eq!(app.on_key(code(KeyCode::F(2))), Effect::Dashboard);
    }
    #[test]
    fn removing_a_paused_container_requests_force_after_confirmation() {
        let mut app = sample();
        app.lists.containers[0].state = "paused".into();
        assert_eq!(app.on_key(key('d')), Effect::None);
        assert!(matches!(
            app.on_key(key('y')),
            Effect::Act {
                verb: Verb::Remove { force: true },
                ..
            }
        ));
    }
    #[test]
    fn image_search_includes_secondary_tags() {
        let mut app = sample();
        app.lists.images[0].repo_tags.push("demo:stable".into());
        app.on_key(key('2'));
        for c in "/stable".chars() {
            app.on_key(key(c));
        }
        assert_eq!(app.rows(Kind::Images).len(), 1);
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

    fn docker_at(name: &str, is_podbag: bool, from_env: bool) -> DockerTarget {
        DockerTarget {
            context: (!from_env).then(|| "desktop-linux".to_string()),
            host: "unix:///x/docker.sock".into(),
            name: name.into(),
            is_podbag,
            from_env,
        }
    }

    #[test]
    fn d_points_the_docker_command_at_podbag_after_a_question() {
        let mut app = sample();
        // No docker CLI: no D.
        assert_eq!(app.on_key(key('D')), Effect::None);
        assert_eq!(app.mode, Mode::List);
        app.docker = Some(docker_at("Docker Desktop", false, false));
        app.on_key(key('D'));
        let Mode::Confirm { question, .. } = &app.mode else {
            panic!("no question")
        };
        assert!(
            question.contains("podbag") && question.contains("Docker Desktop"),
            "{question}"
        );
        assert_eq!(app.on_key(key('y')), Effect::DockerCli { to_podbag: true });
        // At podbag: back to the context before.
        app.docker = Some(docker_at("podbag", true, false));
        app.docker_previous = Some("desktop-linux".into());
        app.on_key(key('D'));
        let Mode::Confirm { question, .. } = &app.mode else {
            panic!("no question")
        };
        assert!(question.contains("desktop-linux"), "{question}");
        assert_eq!(app.on_key(key('y')), Effect::DockerCli { to_podbag: false });
        // DOCKER_HOST is set: contexts do not reach shells that have it.
        app.docker = Some(docker_at("Docker Desktop", false, true));
        app.on_key(key('D'));
        let Mode::Confirm { question, .. } = &app.mode else {
            panic!("no question")
        };
        assert!(question.contains("DOCKER_HOST"), "{question}");
        assert_eq!(app.on_key(key('n')), Effect::None);
    }
}
