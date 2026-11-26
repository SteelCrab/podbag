//! Parts the Training, Migrate and notice views share: background jobs that end in a
//! change of the window, a card that fits its content, headings and text, a command box
//! with a copy button, a block of preformatted text and a row of buttons.

use podbag_desktop_core::Api;
use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind, Kind as ButtonKind};
use podbag_ui::widgets::selectable::selectable_text;
use podbag_ui::{Color, CursorIcon, Id, Point, Rect, TextInput, TextStyle};

use crate::app::Desktop;
use crate::icons::Icon;
use crate::jobs::Done;

/// Run `work` on the API away from the frame, then `then` on the window with its result.
pub fn spawn<R: Send + 'static>(
    d: &Desktop,
    work: impl FnOnce(&dyn Api) -> R + Send + 'static,
    then: impl FnOnce(&mut Desktop, R) + Send + 'static,
) {
    d.jobs.run(move |api| {
        let result = work(api);
        Done::View(Box::new(move |d| then(d, result)))
    });
}

/// A one-line input in the app's text style.
pub fn input(id: Id, placeholder: &str) -> TextInput {
    TextInput::new(
        id,
        placeholder,
        TextStyle::new(14.).line_height(20.),
        Color::TRANSPARENT,
    )
}

/// A card at `x, y`, `w` wide, with 22 px padding. `body(ui, x, y, w)` paints inside and
/// returns its height; the card fits it from the next frame. Returns the card's height.
pub fn card(
    ui: &mut Ui,
    id: Id,
    x: f32,
    y: f32,
    w: f32,
    body: impl FnOnce(&mut Ui, f32, f32, f32) -> f32,
) -> f32 {
    let t = ui.theme;
    const PAD: f32 = 22.;
    let h = ui.mem.heights.get(&id).copied().unwrap_or(120.);
    ui.p.bordered(Rect::new(x, y, w, h), 10., t.surface, 1., t.line);
    let used = body(ui, x + PAD, y + PAD, w - 2. * PAD) + 2. * PAD;
    if (used - h).abs() > 0.5 {
        ui.mem.heights.insert(id, used);
        ui.repaint();
    }
    used
}

/// Bold text of `size`; returns its height.
pub fn heading(ui: &mut Ui, x: f32, y: f32, w: f32, text: &str, size: f32) -> f32 {
    let style = ui.style().with_size(size).weight(700);
    let ink = ui.theme.ink;
    ui.paragraph(x, y, w, text, style, ink)
}

/// Wrapped body text; returns its height.
pub fn text(ui: &mut Ui, x: f32, y: f32, w: f32, text: &str, color: Color) -> f32 {
    let style = ui.style();
    ui.paragraph(x, y, w, text, style, color)
}

/// Smaller wrapped text (hints, status lines); returns its height.
pub fn small(ui: &mut Ui, x: f32, y: f32, w: f32, text: &str, color: Color) -> f32 {
    let style = ui.style().with_size(12.5);
    ui.paragraph(x, y, w, text, style, color)
}

/// A command in monospace that wraps, with a copy button (app.js `codeLine`). Returns
/// its height.
pub fn code_box(d: &mut Desktop, ui: &mut Ui, id: Id, x: f32, y: f32, w: f32, text: &str) -> f32 {
    let t = ui.theme;
    let style = TextStyle::new(13.).mono().line_height(18.);
    let text_w = (w - 14. - 42.).max(40.);
    let layout = ui.p.layout_wrapped(text, style, text_w);
    let h = (layout.height + 24.).max(42.);
    let r = Rect::new(x, y, w, h);
    ui.p.bordered(r, 8., t.sunken, 1., t.line);
    selectable_text(
        ui,
        id.with("text"),
        Point::new(r.x + 14., r.y + (h - layout.height) / 2.),
        text_w,
        text,
        style,
        t.ink,
    );
    let label = d.t("action.copy");
    let b = Rect::new(r.right() - 36., r.y + (h - 30.) / 2., 30., 30.);
    if button::icon_button(ui, id, b, Icon::Copy.id(), &label, IconKind::Plain, true) {
        d.copy(text, ui);
    }
    h
}

/// Preformatted text in a sunken box (lab output, upgrade output). Returns its height.
pub fn pre(ui: &mut Ui, id: Id, x: f32, y: f32, w: f32, text: &str) -> f32 {
    let t = ui.theme;
    let style = TextStyle::new(12.5).mono().line_height(18.);
    let layout = ui.p.layout_wrapped(text, style, w - 24.);
    let h = layout.height + 20.;
    ui.p.bordered(Rect::new(x, y, w, h), 8., t.sunken, 1., t.line);
    selectable_text(
        ui,
        id,
        Point::new(x + 12., y + 10.),
        w - 24.,
        text,
        style,
        t.ink,
    );
    h
}

/// Buttons placed from the left, wrapping to a new line when the row is full.
pub struct Buttons {
    x0: f32,
    x: f32,
    y: f32,
    right: f32,
    used: bool,
}

impl Buttons {
    pub fn new(x: f32, y: f32, w: f32) -> Buttons {
        Buttons {
            x0: x,
            x,
            y,
            right: x + w,
            used: false,
        }
    }

    /// A button; true when clicked.
    pub fn add(
        &mut self,
        ui: &mut Ui,
        id: Id,
        label: &str,
        icon: Option<Icon>,
        kind: ButtonKind,
        enabled: bool,
    ) -> bool {
        let icon = icon.map(Icon::id);
        let w = button::width(ui, label, icon, false);
        if self.used && self.x + w > self.right {
            self.x = self.x0;
            self.y += 40.;
        }
        let r = Rect::new(self.x, self.y, w, 32.);
        self.x += w + 8.;
        self.used = true;
        button::button(ui, id, r, label, icon, kind, enabled)
    }

    /// The height of the rows used.
    pub fn height(&self, y0: f32) -> f32 {
        if self.used { self.y + 32. - y0 } else { 0. }
    }
}

/// A heading that is a link (`h2 > button.link`). Returns (clicked, height).
pub fn link(ui: &mut Ui, id: Id, x: f32, y: f32, w: f32, text: &str, size: f32) -> (bool, f32) {
    let t = ui.theme;
    let style = ui.style().with_size(size).weight(700);
    let layout = ui.p.layout_wrapped(text, style, w);
    let r = Rect::new(x, y, layout.width.min(w), layout.height);
    let color = if ui.hovered(id) { t.accent_text } else { t.ink };
    ui.p.draw_text(&layout, x, y, color);
    ui.p.hitbox(r, id, CursorIcon::Pointer);
    ui.focusable(id, FocusKind::Activate, r.expand(3.), 5.);
    (ui.clicked(id), layout.height)
}

/// A registry's logo (registry-icons.svg in the web page).
pub fn registry_mark(ui: &mut Ui, r: Rect, registry: &str) {
    let ink = ui.theme.ink;
    crate::images::draw_registry_icon(ui, registry, r, ink);
}
