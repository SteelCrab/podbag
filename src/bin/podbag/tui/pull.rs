//! The pull form (`p` in Images): where to pull from (Tab cycles the locations), the
//! login there, a host for ECR, GAR and other registries, the image's name, and Docker
//! Hub search results when the engine can search.

use super::app::{App, Effect, Mode};
use podbag::client::{ApiError, Location, SearchResult};
use ratatui::crossterm::event::{KeyCode, KeyEvent};

/// The fewest characters a search needs.
const SEARCH_FROM: usize = 2;

/// One search result, as shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Found {
    pub name: String,
    /// Official, stars and the description, in one line.
    pub about: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PullForm {
    /// Which of [`Location::all`].
    pub location: usize,
    /// The host, for the locations that need one.
    pub host: String,
    /// The image's name, or what to search for.
    pub name: String,
    /// Typing goes to the host (else to the name).
    pub on_host: bool,
    pub found: Vec<Found>,
    /// The chosen search result.
    pub chosen: Option<usize>,
    /// Why the last search failed.
    pub error: Option<String>,
}

impl PullForm {
    /// The location, with its host when it needs one.
    pub fn place(&self) -> Location {
        let all = Location::all();
        let l = &all[self.location % all.len()];
        if l.needs_host() {
            l.with_host(&self.host)
        } else {
            l.clone()
        }
    }

    /// Another location: the next one (`step` 1) or the one before.
    fn go(&mut self, step: isize) {
        let n = Location::all().len() as isize;
        self.location = (self.location as isize + step).rem_euclid(n) as usize;
        self.host.clear();
        self.on_host = self.place().needs_host();
        self.found.clear();
        self.chosen = None;
        self.error = None;
    }
}

impl App {
    pub(super) fn pull_key(&mut self, key: KeyEvent, mut form: PullForm) -> Effect {
        let effect = match key.code {
            KeyCode::Esc => {
                self.mode = Mode::List;
                return Effect::None;
            }
            KeyCode::Tab => {
                form.go(1);
                Effect::Login(form.place())
            }
            KeyCode::BackTab => {
                form.go(-1);
                Effect::Login(form.place())
            }
            KeyCode::Enter if form.on_host => {
                if form.host.trim().is_empty() {
                    Effect::None
                } else {
                    form.on_host = false;
                    Effect::Login(form.place())
                }
            }
            KeyCode::Enter => {
                let place = form.place();
                let name = match form.chosen.and_then(|i| form.found.get(i)) {
                    Some(f) => f.name.clone(),
                    None => form.name.trim().to_string(),
                };
                if name.is_empty() || (place.needs_host() && place.host().is_empty()) {
                    Effect::None
                } else {
                    let image = place.qualify(&name);
                    self.mode = Mode::List;
                    self.busy = Some(format!("Pulling {image}"));
                    return Effect::Pull(image);
                }
            }
            KeyCode::Up => {
                match form.chosen {
                    Some(0) => form.chosen = None,
                    Some(i) => form.chosen = Some(i - 1),
                    None if form.place().needs_host() => form.on_host = true,
                    None => {}
                }
                Effect::None
            }
            KeyCode::Down => {
                if form.on_host {
                    form.on_host = false;
                } else if !form.found.is_empty() {
                    let last = form.found.len() - 1;
                    form.chosen = Some(form.chosen.map_or(0, |i| (i + 1).min(last)));
                }
                Effect::None
            }
            KeyCode::Backspace | KeyCode::Char(_) => {
                let text = if form.on_host {
                    &mut form.host
                } else {
                    &mut form.name
                };
                match key.code {
                    KeyCode::Char(c) => text.push(c),
                    _ => {
                        text.pop();
                    }
                }
                if form.on_host {
                    Effect::None
                } else {
                    form.chosen = None;
                    self.search_for(&mut form)
                }
            }
            _ => Effect::None,
        };
        self.mode = Mode::Pull(form);
        effect
    }

    /// A search for the name, when the engine can search and the place is Docker Hub.
    fn search_for(&self, form: &mut PullForm) -> Effect {
        let term = form.name.trim();
        if self.can_search && form.place() == Location::DockerHub && term.len() >= SEARCH_FROM {
            Effect::Search(term.to_string())
        } else {
            form.found.clear();
            form.error = None;
            Effect::None
        }
    }

    /// A search's answer: shown when it is for the name as it is now.
    pub(super) fn found(&mut self, term: &str, result: Result<Vec<SearchResult>, ApiError>) {
        if let Err(e) = &result
            && e.unsupported()
        {
            self.can_search = false;
        }
        let Mode::Pull(form) = &mut self.mode else {
            return;
        };
        if form.name.trim() != term {
            return;
        }
        form.chosen = None;
        form.found.clear();
        form.error = None;
        match result {
            Ok(list) => {
                form.found = list
                    .into_iter()
                    .map(|r| Found {
                        about: format!(
                            "{}★{} {}",
                            if r.is_official { "official " } else { "" },
                            r.star_count,
                            r.description
                        ),
                        name: r.name,
                    })
                    .collect();
            }
            Err(e) if e.unsupported() => {}
            Err(e) => form.error = Some(format!("Cannot search: {e}")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::app::tests::{code, key, sample};
    use super::super::app::{App, Effect, Mode, Msg};
    use super::super::view;
    use podbag::client::{ApiError, Location, SearchResult};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::crossterm::event::KeyCode;

    fn screen(app: &mut App) -> String {
        let mut t = Terminal::new(TestBackend::new(100, 30)).unwrap();
        t.draw(|f| view::draw(f, app)).unwrap();
        view::buffer_text(t.backend().buffer())
    }

    fn open() -> App {
        let mut app = sample();
        app.on_key(key('2'));
        assert_eq!(app.on_key(key('p')), Effect::Login(Location::DockerHub));
        app
    }

    fn type_text(app: &mut App, text: &str) -> Vec<Effect> {
        text.chars().map(|c| app.on_key(key(c))).collect()
    }

    fn found(name: &str) -> SearchResult {
        SearchResult {
            name: name.into(),
            description: format!("The {name} image"),
            star_count: 10,
            is_official: true,
        }
    }

    #[test]
    fn a_short_name_pulls_from_docker_hub() {
        let mut app = open();
        let s = screen(&mut app);
        assert!(s.contains("Pull image") && s.contains("Docker Hub"), "{s}");
        type_text(&mut app, "redis:7");
        assert_eq!(
            app.on_key(code(KeyCode::Enter)),
            Effect::Pull("redis:7".into())
        );
        assert_eq!(app.mode, Mode::List);
        assert!(app.busy.is_some());
    }

    #[test]
    fn tab_goes_to_the_next_location_and_qualifies_the_name() {
        let mut app = open();
        assert_eq!(
            app.on_key(code(KeyCode::Tab)),
            Effect::Login(Location::GitHub)
        );
        assert!(screen(&mut app).contains("GitHub"));
        type_text(&mut app, "me/app:1");
        assert_eq!(
            app.on_key(code(KeyCode::Enter)),
            Effect::Pull("ghcr.io/me/app:1".into())
        );
        // Shift+Tab goes back; all the way round comes to Docker Hub again.
        let mut app = open();
        assert_eq!(
            app.on_key(code(KeyCode::BackTab)),
            Effect::Login(Location::Custom(String::new()))
        );
        assert_eq!(
            app.on_key(code(KeyCode::Tab)),
            Effect::Login(Location::DockerHub)
        );
        for _ in 0..Location::all().len() - 1 {
            app.on_key(code(KeyCode::Tab));
        }
        assert_eq!(
            app.on_key(code(KeyCode::Tab)),
            Effect::Login(Location::DockerHub)
        );
    }

    #[test]
    fn ecr_asks_for_its_host_and_shows_how_to_log_in() {
        let mut app = open();
        app.on_key(code(KeyCode::Tab));
        app.on_key(code(KeyCode::Tab));
        let s = screen(&mut app);
        assert!(s.contains("Amazon ECR") && s.contains("Host"), "{s}");
        assert!(s.contains("aws ecr get-login-password"), "{s}");
        // No host yet: no pull.
        assert_eq!(app.on_key(code(KeyCode::Enter)), Effect::None);
        let host = "123456789012.dkr.ecr.eu-west-1.amazonaws.com";
        type_text(&mut app, host);
        // Enter on the host: its login, then the name.
        assert_eq!(
            app.on_key(code(KeyCode::Enter)),
            Effect::Login(Location::Ecr(host.into()))
        );
        type_text(&mut app, "app:1");
        assert_eq!(
            app.on_key(code(KeyCode::Enter)),
            Effect::Pull(format!("{host}/app:1"))
        );
    }

    #[test]
    fn the_login_state_of_the_location_shows() {
        let mut app = open();
        assert!(screen(&mut app).contains("Checking the login"));
        app.on_msg(Msg::Login(Location::DockerHub, Some("alice".into())));
        assert!(screen(&mut app).contains("Logged in as alice"));
        app.on_key(code(KeyCode::Tab));
        app.on_msg(Msg::Login(Location::GitHub, None));
        assert!(screen(&mut app).contains("Not logged in"));
    }

    #[test]
    fn search_results_can_be_chosen() {
        let mut app = open();
        let effects = type_text(&mut app, "ngi");
        assert_eq!(effects.last(), Some(&Effect::Search("ngi".into())));
        // An older answer is dropped.
        app.on_msg(Msg::Found("ng".into(), Ok(vec![found("old")])));
        assert!(!screen(&mut app).contains("old"));
        app.on_msg(Msg::Found(
            "ngi".into(),
            Ok(vec![found("nginx"), found("nginx/unit")]),
        ));
        let s = screen(&mut app);
        assert!(
            s.contains("nginx/unit") && s.contains("The nginx image"),
            "{s}"
        );
        app.on_key(code(KeyCode::Down));
        app.on_key(code(KeyCode::Down));
        assert_eq!(
            app.on_key(code(KeyCode::Enter)),
            Effect::Pull("nginx/unit".into())
        );
    }

    #[test]
    fn no_search_where_the_engine_has_none() {
        let mut app = open();
        type_text(&mut app, "ngi");
        let unsupported = ApiError {
            status: 501,
            message: "search is not supported by podbag".into(),
        };
        app.on_msg(Msg::Found("ngi".into(), Err(unsupported)));
        assert!(!app.can_search);
        assert!(!screen(&mut app).contains("not supported"));
        assert_eq!(app.on_key(key('n')), Effect::None);
        // Only Docker Hub is searched.
        let mut app = open();
        app.on_key(code(KeyCode::Tab));
        assert!(
            type_text(&mut app, "me/app")
                .iter()
                .all(|e| *e == Effect::None)
        );
    }

    #[test]
    fn esc_cancels() {
        let mut app = open();
        type_text(&mut app, "x");
        assert_eq!(app.on_key(code(KeyCode::Esc)), Effect::None);
        assert_eq!(app.mode, Mode::List);
    }
}
