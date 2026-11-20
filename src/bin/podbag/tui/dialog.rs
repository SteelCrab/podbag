//! Terminal lists, editable fields and progress screens for dashboard actions.
use super::preferences::Appearance;
use podbag::client::ApiError;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::{
    DefaultTerminal,
    layout::{Constraint, Layout},
    style::{Modifier, Style},
    widgets::{Block, Borders, List, ListState, Paragraph, Wrap},
};
use std::{
    collections::BTreeSet,
    io::{self, Write},
    sync::mpsc,
    time::Duration,
};

pub enum Error {
    Back,
    Failed(String),
    Io(io::Error),
}
pub type Result<T> = std::result::Result<T, Error>;
impl From<String> for Error {
    fn from(s: String) -> Self {
        Self::Failed(s)
    }
}
impl From<io::Error> for Error {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}
impl From<ApiError> for Error {
    fn from(e: ApiError) -> Self {
        Self::Failed(e.message)
    }
}

pub struct Screen<'a> {
    pub terminal: &'a mut DefaultTerminal,
    pub look: &'a mut Appearance,
}

fn input_event() -> Result<Event> {
    loop {
        let e = event::read()?;
        if let Event::Key(k) = &e {
            if k.kind == KeyEventKind::Release {
                continue;
            }
            if k.code == KeyCode::Esc
                || (k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL))
            {
                return Err(Error::Back);
            }
        }
        return Ok(e);
    }
}

impl Screen<'_> {
    pub fn t(&self, key: &str) -> String {
        self.look.text(key)
    }
    pub fn menu(&mut self, title: &str, keys: &[&str]) -> Result<usize> {
        let rows = keys.iter().map(|k| self.t(k)).collect::<Vec<_>>();
        self.pick(&self.t(title), &rows)
    }
    pub fn pick(&mut self, title: &str, rows: &[String]) -> Result<usize> {
        Ok(self.select(title, rows, false)?[0])
    }
    pub fn select(&mut self, title: &str, rows: &[String], multi: bool) -> Result<Vec<usize>> {
        let mut cursor = 0usize;
        let mut query = String::new();
        let mut chosen = BTreeSet::new();
        let mut state = ListState::default();
        let style = self.look.style();
        loop {
            let q = query.to_lowercase();
            let visible: Vec<_> = rows
                .iter()
                .enumerate()
                .filter(|(_, s)| {
                    let text = s.to_lowercase();
                    q.split_whitespace().all(|word| text.contains(word))
                })
                .collect();
            cursor = cursor.min(visible.len().saturating_sub(1));
            state.select((!visible.is_empty()).then_some(cursor));
            let hints = if multi {
                "↑↓  Space ✓   Enter ↵   Esc ←"
            } else {
                "↑↓   Enter ↵   Esc ←"
            };
            let items: Vec<_> = visible
                .iter()
                .map(|(i, s)| {
                    if !multi {
                        s.to_string()
                    } else {
                        format!("{} {s}", if chosen.contains(i) { "[×]" } else { "[ ]" })
                    }
                })
                .collect();
            self.terminal.draw(|f| {
                let [body, foot] =
                    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
                let heading = Paragraph::new(title)
                    .wrap(Wrap { trim: false })
                    .style(style);
                let height = heading
                    .line_count(body.width.saturating_sub(2).max(1))
                    .min(u16::MAX as usize) as u16;
                let (block, body) = if height > 1 {
                    let [top, list] = Layout::vertical([
                        Constraint::Length(height.min(body.height / 2).max(1)),
                        Constraint::Fill(1),
                    ])
                    .areas(body);
                    f.render_widget(heading, top);
                    (Block::bordered().style(style), list)
                } else {
                    (Block::bordered().style(style).title(title), body)
                };
                if items.is_empty() {
                    f.render_widget(
                        Paragraph::new(self.look.text("empty.search").replace("{q}", &query))
                            .block(block),
                        body,
                    )
                } else {
                    f.render_stateful_widget(
                        List::new(items)
                            .block(block)
                            .highlight_symbol("› ")
                            .highlight_style(Style::new().add_modifier(Modifier::REVERSED)),
                        body,
                        &mut state,
                    )
                }
                f.render_widget(
                    Paragraph::new(format!("{hints}   / {query}")).style(style),
                    foot,
                );
            })?;
            match input_event()? {
                Event::Key(KeyEvent { code, .. }) => match code {
                    KeyCode::Up => cursor = cursor.saturating_sub(1),
                    KeyCode::Down => cursor = cursor.saturating_add(1),
                    KeyCode::PageUp => cursor = cursor.saturating_sub(10),
                    KeyCode::PageDown => cursor = cursor.saturating_add(10),
                    KeyCode::Home => cursor = 0,
                    KeyCode::End => cursor = usize::MAX,
                    KeyCode::Backspace => {
                        query.pop();
                        cursor = 0
                    }
                    KeyCode::Char(' ') if multi => {
                        if let Some((i, _)) = visible.get(cursor)
                            && !chosen.remove(i)
                        {
                            chosen.insert(*i);
                        }
                    }
                    KeyCode::Enter if multi && !chosen.is_empty() => {
                        return Ok(chosen.into_iter().collect());
                    }
                    KeyCode::Enter => {
                        if let Some((i, _)) = visible.get(cursor) {
                            return Ok(vec![*i]);
                        }
                    }
                    KeyCode::Char(c) => {
                        query.push(c);
                        cursor = 0
                    }
                    _ => {}
                },
                Event::Paste(s) => {
                    query.push_str(&s);
                    cursor = 0
                }
                _ => {}
            }
        }
    }
    pub fn confirm(&mut self, title: &str, action: &str) -> Result<()> {
        let choices = [self.t("action.cancel"), self.t(action)];
        if self.pick(title, &choices)? == 1 {
            Ok(())
        } else {
            Err(Error::Back)
        }
    }
    pub fn input(
        &mut self,
        title: &str,
        initial: &str,
        secret: bool,
        multiline: bool,
    ) -> Result<String> {
        let mut value: Vec<char> = initial.chars().collect();
        let mut cursor = value.len();
        let style = self.look.style();
        loop {
            let mut display: String = value
                .iter()
                .enumerate()
                .map(|(i, c)| {
                    let ch = if secret { '•' } else { *c };
                    if i == cursor {
                        format!("▏{ch}")
                    } else {
                        ch.to_string()
                    }
                })
                .collect();
            if cursor == value.len() {
                display.push('▏')
            }
            let hint = if multiline {
                "Ctrl+S ↵   Esc ←"
            } else {
                "Enter ↵   Esc ←"
            };
            self.terminal.draw(|f| {
                let [body, foot] =
                    Layout::vertical([Constraint::Min(1), Constraint::Length(1)]).areas(f.area());
                let block = Block::default()
                    .borders(Borders::ALL)
                    .style(style)
                    .title(title);
                let inner = block.inner(body);
                let before: String = value[..cursor].iter().collect();
                let height = Paragraph::new(before)
                    .wrap(Wrap { trim: false })
                    .line_count(inner.width.max(1));
                f.render_widget(
                    Paragraph::new(display)
                        .block(block)
                        .wrap(Wrap { trim: false })
                        .scroll((
                            height
                                .saturating_sub(inner.height as usize)
                                .min(u16::MAX as usize) as u16,
                            0,
                        )),
                    body,
                );
                f.render_widget(Paragraph::new(hint).style(style), foot);
            })?;
            match input_event()? {
                Event::Key(k) => match k.code {
                    KeyCode::Enter if !multiline => return Ok(value.iter().collect()),
                    KeyCode::Char('s')
                        if multiline && k.modifiers.contains(KeyModifiers::CONTROL) =>
                    {
                        return Ok(value.iter().collect());
                    }
                    KeyCode::Enter => {
                        value.insert(cursor, '\n');
                        cursor += 1
                    }
                    KeyCode::Backspace if cursor > 0 => {
                        cursor -= 1;
                        value.remove(cursor);
                    }
                    KeyCode::Delete if cursor < value.len() => {
                        value.remove(cursor);
                    }
                    KeyCode::Left => cursor = cursor.saturating_sub(1),
                    KeyCode::Right => cursor = (cursor + 1).min(value.len()),
                    KeyCode::Home => cursor = 0,
                    KeyCode::End => cursor = value.len(),
                    KeyCode::Char('u') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                        value.clear();
                        cursor = 0
                    }
                    KeyCode::Char(c)
                        if !k
                            .modifiers
                            .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                    {
                        value.insert(cursor, c);
                        cursor += 1
                    }
                    _ => {}
                },
                Event::Paste(s) => {
                    for c in s
                        .chars()
                        .filter(|c| !c.is_control() || (multiline && *c == '\n'))
                    {
                        value.insert(cursor, c);
                        cursor += 1
                    }
                }
                _ => {}
            }
        }
    }
    pub fn message(&mut self, title: &str, body: &str) -> Result<()> {
        self.message_with_copy(title, body, body)
    }

    pub fn message_with_copy(&mut self, title: &str, body: &str, copy: &str) -> Result<()> {
        let mut scroll = 0u16;
        let hint = format!(" ↑↓   y {}   Enter ↵   Esc ← ", self.t("action.copy"));
        loop {
            self.terminal.draw(|f| {
                let block = Block::default()
                    .borders(Borders::ALL)
                    .title(title)
                    .title_bottom(hint.as_str())
                    .style(self.look.style());
                let rect = block.inner(f.area());
                let p = Paragraph::new(body).wrap(Wrap { trim: false });
                scroll = scroll.min(
                    p.line_count(rect.width.max(1))
                        .saturating_sub(rect.height as usize)
                        .min(u16::MAX as usize) as u16,
                );
                f.render_widget(p.block(block).scroll((scroll, 0)), f.area());
            })?;
            if let Event::Key(k) = input_event()? {
                match k.code {
                    KeyCode::Char('y') => {
                        let mut out = io::stdout().lock();
                        write!(
                            out,
                            "\x1b]52;c;{}\x07",
                            podbag::base64::encode(copy.as_bytes())
                        )?;
                        out.flush()?;
                    }
                    KeyCode::Enter | KeyCode::Char('q') => return Ok(()),
                    KeyCode::Down => scroll = scroll.saturating_add(1),
                    KeyCode::Up => scroll = scroll.saturating_sub(1),
                    KeyCode::PageDown => scroll = scroll.saturating_add(10),
                    KeyCode::PageUp => scroll = scroll.saturating_sub(10),
                    _ => {}
                }
            }
        }
    }
    pub fn work<T: Send>(
        &mut self,
        title: &str,
        task: impl FnOnce(&dyn Fn(&str)) -> std::result::Result<T, String> + Send,
    ) -> Result<T> {
        std::thread::scope(|scope| {
            let (tx, rx) = mpsc::channel();
            let job = scope.spawn(move || {
                task(&|s| {
                    let _ = tx.send(s.to_owned());
                })
            });
            let mut phase = String::new();
            loop {
                self.terminal.draw(|f| {
                    f.render_widget(
                        Paragraph::new(phase.as_str())
                            .wrap(Wrap { trim: false })
                            .block(Block::bordered().title(title).style(self.look.style())),
                        f.area(),
                    )
                })?;
                match rx.recv_timeout(Duration::from_millis(100)) {
                    Ok(s) => phase = self.t(&s),
                    Err(mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(mpsc::RecvTimeoutError::Timeout) => {}
                }
            }
            job.join()
                .map_err(|_| Error::Failed("Dashboard operation failed".into()))?
                .map_err(Error::from)
        })
    }
}
