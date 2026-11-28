//! The Desktop's in-session notification history and routine popup preference.
use super::blocks::{Buttons, heading};
use crate::{
    app::{Desktop, now_secs},
    dialogs::CANCEL,
    fmt,
};
use podbag_ui::ui::Ui;
use podbag_ui::widgets::{
    button::Kind,
    checkbox,
    dialog::dialog,
    scroll::{ScrollState, scroll_view},
    toast::Toasts,
};
use podbag_ui::{Id, Rect};
use std::collections::VecDeque;

pub const OPEN: Id = Id(0x6e6f740001);
pub const STATUS: Id = Id(0x6e6f740002);
pub const CLEAR: Id = Id(0x6e6f740003);

struct Entry {
    text: String,
    error: bool,
    time: i64,
}

pub struct Notifications {
    pub popups: Toasts,
    history: VecDeque<Entry>,
    routine: bool,
}

impl Notifications {
    pub fn new(routine: bool) -> Self {
        Self {
            popups: Toasts::default(),
            history: VecDeque::new(),
            routine,
        }
    }

    pub fn push(&mut self, text: impl Into<String>, error: bool) {
        let text = text.into();
        if error || self.routine {
            self.popups.push(text.clone(), error);
        }
        if self.history.len() == 50 {
            self.history.pop_back();
        }
        self.history.push_front(Entry {
            text,
            error,
            time: now_secs(),
        });
    }

    pub fn is_empty(&self) -> bool {
        self.popups.is_empty()
    }
    pub fn texts(&self) -> impl Iterator<Item = (&str, bool)> {
        self.popups.texts()
    }
}

pub fn settings(d: &mut Desktop, ui: &mut Ui, x: f32, y: f32, w: f32) -> f32 {
    let mut enabled = d.settings.status_notifications;
    if checkbox::checkbox(
        ui,
        STATUS,
        Rect::new(x, y, w, 32.),
        Some(&mut enabled),
        &d.t("notifications.status"),
        true,
    ) {
        let mut settings = d.settings.clone();
        settings.status_notifications = enabled;
        match d.api.set_settings(settings.clone()) {
            Ok(()) => {
                d.settings = settings;
                d.toasts.routine = enabled;
                if !enabled {
                    d.toasts.popups.dismiss_routine();
                }
            }
            Err(err) => d.toasts.push(err, true),
        }
    }
    36. + ui.paragraph(
        x,
        y + 36.,
        w,
        &d.t("notifications.hint"),
        ui.style(),
        ui.theme.ink2,
    )
}

pub fn history(d: &mut Desktop, ui: &mut Ui, scroll: &mut ScrollState) -> bool {
    let tr = d.tr;
    let mut close = false;
    let escape = dialog(ui, OPEN.with("history"), 510., |ui, r| {
        let t = ui.theme;
        let mut y = r.y;
        y += heading(ui, r.x, y, r.w, &tr.t("notifications.history"), 18.) + 12.;
        if d.toasts.history.is_empty() {
            y += ui.paragraph(
                r.x,
                y,
                r.w,
                &tr.t("notifications.empty"),
                ui.style(),
                t.ink2,
            ) + 12.;
        } else {
            let now = now_secs();
            let max = (ui.bounds().h - 195.).clamp(68., 360.);
            let height = scroll.content_height().clamp(68., max);
            scroll_view(
                ui,
                OPEN.with("entries"),
                Rect::new(r.x, y, r.w, height),
                scroll,
                |ui, area| {
                    let mut top = area.y;
                    for entry in &d.toasts.history {
                        let time = fmt::ago(entry.time, now, tr.lang);
                        let label = if entry.error {
                            format!("{} · {time}", tr.t("notifications.error"))
                        } else {
                            time
                        };
                        top += ui.paragraph(
                            area.x,
                            top,
                            area.w,
                            &label,
                            ui.style().with_size(12.),
                            if entry.error { t.red } else { t.muted },
                        ) + 2.;
                        top +=
                            ui.paragraph(area.x, top, area.w, &entry.text, ui.style(), t.ink) + 8.;
                    }
                    top - area.y
                },
            );
            y += height + 12.;
        }
        let mut buttons = Buttons::new(r.x, y, r.w);
        close = buttons.add(
            ui,
            CANCEL,
            &tr.t("notifications.close"),
            None,
            Kind::Normal,
            true,
        );
        if buttons.add(
            ui,
            CLEAR,
            &tr.t("notifications.clear"),
            None,
            Kind::Normal,
            !d.toasts.history.is_empty(),
        ) {
            d.toasts.history.clear();
            *scroll = ScrollState::default();
        }
        y + buttons.height(y) - r.y
    });
    close || escape
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn history_keeps_the_last_fifty_messages_newest_first() {
        let mut messages = Notifications::new(false);
        for i in 0..55 {
            messages.push(i.to_string(), false);
        }
        assert!(messages.is_empty());
        let actual: Vec<_> = messages.history.iter().map(|entry| &entry.text).collect();
        let expected: Vec<_> = (5..55).rev().map(|i| i.to_string()).collect();
        assert_eq!(actual, expected.iter().collect::<Vec<_>>());
    }
}
