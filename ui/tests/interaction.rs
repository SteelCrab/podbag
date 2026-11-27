//! Widget behavior through the headless harness: focus, keyboard activation,
//! popups, dialogs, selection and copy, tooltips, IME.

use std::time::Duration;

use podbag_ui::testing::Harness;
use podbag_ui::ui::{Theme, Ui, UiApp, UiView};
use podbag_ui::widgets::button::{self, IconKind, Kind};
use podbag_ui::widgets::checkbox::checkbox;
use podbag_ui::widgets::dialog::dialog;
use podbag_ui::widgets::logs::{LogView, log_view};
use podbag_ui::widgets::select::{Item, select};
use podbag_ui::widgets::selectable::selectable_text;
use podbag_ui::widgets::tabs::tabs;
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::{
    Cx, Id, KeyPress, ModifiersState, NamedKey, Point, Rect, Synthetic, TextInput, TextStyle,
    load_fonts,
};

#[derive(Default)]
struct Form {
    clicks: Vec<&'static str>,
    /// Enter on the checkbox (a form would send itself).
    entered: usize,
    check: bool,
    choice: usize,
    tab: usize,
    dialog: bool,
    dialog_escape: bool,
    input: Option<TextInput>,
    block: &'static str,
    logs: LogView,
    show_logs: bool,
}

const A: Id = Id(1);
const B: Id = Id(2);
const CHECK: Id = Id(3);
const MENU: Id = Id(4);
const TABS: Id = Id(5);
const FIELD: Id = Id(6);
const IN_DIALOG: Id = Id(7);
const BLOCK: Id = Id(8);
const LOGS: Id = Id(9);

impl UiApp for Form {
    type Message = ();

    fn ui(&mut self, ui: &mut Ui) {
        if button::button(
            ui,
            A,
            Rect::new(10., 10., 80., 30.),
            "A",
            None,
            Kind::Normal,
            true,
        ) {
            self.clicks.push("a");
        }
        if button::icon_button(
            ui,
            B,
            Rect::new(100., 10., 30., 30.),
            0,
            "Bee",
            IconKind::Plain,
            true,
        ) {
            self.clicks.push("b");
        }
        if ui.entered(CHECK) {
            self.entered += 1;
        }
        let mut c = self.check;
        checkbox(
            ui,
            CHECK,
            Rect::new(10., 50., 100., 24.),
            Some(&mut c),
            "Check",
            true,
        );
        self.check = c;
        let items = [
            Item::Option("one"),
            Item::Group("g"),
            Item::Option("two"),
            Item::Option("three"),
        ];
        if let Some(i) = select(
            ui,
            MENU,
            Rect::new(10., 80., 150., 30.),
            &items,
            self.choice,
            true,
        ) {
            self.choice = i;
        }
        let labels = vec!["x".to_string(), "y".to_string(), "z".to_string()];
        if let Some(i) = tabs(ui, TABS, Rect::new(10., 120., 300., 30.), &labels, self.tab) {
            self.tab = i;
        }
        let input = self.input.get_or_insert_with(|| {
            TextInput::new(FIELD, "", TextStyle::new(14.), Theme::light().ink)
        });
        text_field(ui, input, Rect::new(10., 160., 200., 30.), None, true);
        let style = TextStyle::new(14.).line_height(20.);
        let ink = ui.theme.ink;
        if !self.block.is_empty() {
            selectable_text(
                ui,
                BLOCK,
                Point::new(10., 200.),
                300.,
                self.block,
                style,
                ink,
            );
        }
        if self.show_logs {
            log_view(
                ui,
                LOGS,
                Rect::new(10., 300., 400., 200.),
                &mut self.logs,
                "",
            );
        }
        if self.dialog {
            let escape = dialog(ui, Id(100), 300., |ui, r| {
                if button::button(
                    ui,
                    IN_DIALOG,
                    Rect::new(r.x, r.y, 80., 30.),
                    "In",
                    None,
                    Kind::Normal,
                    true,
                ) {
                    self.clicks.push("in");
                }
                40.
            });
            if escape {
                self.dialog = false;
                self.dialog_escape = true;
            }
        }
    }

    fn message(&mut self, _: (), _: &mut Cx) {}

    fn theme(&self, _: bool) -> Theme {
        Theme::light()
    }
}

fn harness(form: Form) -> Harness<UiView<Form>> {
    Harness::new(UiView::new(form), 800., 600.)
}

fn key(k: NamedKey) -> KeyPress {
    KeyPress::named(k)
}

#[test]
fn tab_moves_in_paint_order_and_wraps() {
    let mut h = harness(Form::default());
    let order = [A, B, CHECK, MENU, TABS, FIELD];
    for id in order {
        h.tab();
        assert_eq!(h.focused(), Some(id));
    }
    h.tab();
    assert_eq!(h.focused(), Some(A));
    h.shift_tab();
    assert_eq!(h.focused(), Some(FIELD));
    h.shift_tab();
    assert_eq!(h.focused(), Some(TABS));
}

#[test]
fn focus_ring_shows_for_keyboard_focus_only() {
    let mut h = harness(Form::default());
    let rings = |h: &Harness<UiView<Form>>| {
        h.scene().layers[0]
            .overlay
            .iter()
            .filter(|q| q.border == 2.)
            .count()
    };
    h.click_id(A);
    assert_eq!(h.focused(), Some(A));
    assert_eq!(rings(&h), 0);
    h.tab();
    assert_eq!(h.focused(), Some(B));
    assert_eq!(rings(&h), 1);
    // A click hides it again.
    h.click_id(CHECK);
    assert_eq!(rings(&h), 0);
}

#[test]
fn enter_and_space_activate_the_focused_widget() {
    let mut h = harness(Form::default());
    h.tab();
    h.press_key(key(NamedKey::Enter));
    h.tab();
    h.press_key(key(NamedKey::Space));
    assert_eq!(h.view().app.clicks, ["a", "b"]);
    h.tab();
    h.press_key(key(NamedKey::Space));
    assert!(h.view().app.check);
    // Key repeats do not click again.
    let mut held = key(NamedKey::Enter);
    held.repeat = true;
    h.shift_tab();
    h.press_key(held);
    assert_eq!(h.view().app.clicks, ["a", "b"]);
}

#[test]
fn entered_takes_only_enter_on_the_focused_widget() {
    let mut h = harness(Form::default());
    h.click_id(CHECK);
    assert!(h.view().app.check, "a click toggles");
    assert_eq!(h.view().app.entered, 0);
    h.press_key(key(NamedKey::Space));
    assert!(!h.view().app.check, "Space toggles");
    h.press_key(key(NamedKey::Enter));
    assert_eq!(h.view().app.entered, 1, "Enter is taken");
    assert!(!h.view().app.check, "and does not toggle");
}

#[test]
fn select_menu_works_with_the_keyboard() {
    let mut h = harness(Form::default());
    for _ in 0..4 {
        h.tab();
    }
    assert_eq!(h.focused(), Some(MENU));
    h.press_key(key(NamedKey::Enter));
    assert!(h.hitbox(MENU.with("menu")).is_some(), "menu open");
    // Down skips the group heading; Enter chooses.
    h.press_key(key(NamedKey::ArrowDown));
    h.press_key(key(NamedKey::Enter));
    assert_eq!(h.view().app.choice, 2);
    assert!(h.hitbox(MENU.with("menu")).is_none(), "menu closed");
    assert_eq!(h.focused(), Some(MENU));
    // Arrow opens, Escape closes without choosing.
    h.press_key(key(NamedKey::ArrowDown));
    assert!(h.hitbox(MENU.with("menu")).is_some());
    h.press_key(key(NamedKey::ArrowDown));
    h.press_key(key(NamedKey::Escape));
    assert!(h.hitbox(MENU.with("menu")).is_none());
    assert_eq!(h.view().app.choice, 2);
}

#[test]
fn arrows_move_between_tabs() {
    let mut h = harness(Form::default());
    for _ in 0..5 {
        h.tab();
    }
    assert_eq!(h.focused(), Some(TABS));
    h.press_key(key(NamedKey::ArrowRight));
    h.press_key(key(NamedKey::ArrowRight));
    assert_eq!(h.view().app.tab, 2);
    h.press_key(key(NamedKey::ArrowRight));
    assert_eq!(h.view().app.tab, 0);
    h.press_key(key(NamedKey::End));
    assert_eq!(h.view().app.tab, 2);
    // Clicking a tab focuses the group.
    h.click_id(A);
    h.click_id(TABS.with(1usize));
    assert_eq!((h.view().app.tab, h.focused()), (1, Some(TABS)));
}

#[test]
fn dialogs_trap_tab_and_close_on_escape() {
    let mut h = harness(Form::default());
    h.tab();
    h.view_mut().app.dialog = true;
    h.repaint();
    // Focus is on A, under the dialog: Enter does not click it.
    h.press_key(key(NamedKey::Enter));
    assert!(h.view().app.clicks.is_empty());
    h.tab();
    assert_eq!(h.focused(), Some(IN_DIALOG));
    h.tab();
    assert_eq!(h.focused(), Some(IN_DIALOG));
    h.press_key(key(NamedKey::Enter));
    assert_eq!(h.view().app.clicks, ["in"]);
    h.press_key(key(NamedKey::Escape));
    assert!(h.view().app.dialog_escape);
    assert!(!h.view().app.dialog);
}

#[test]
fn ime_is_on_only_in_a_text_field() {
    let mut h = harness(Form::default());
    assert!(!h.ime_enabled());
    h.click_id(FIELD);
    assert!(h.ime_enabled());
    h.input(Synthetic::Ime(podbag_ui::winit::event::Ime::Preedit(
        "한".into(),
        Some((3, 3)),
    )));
    h.input(Synthetic::Text("한".into()));
    h.input(Synthetic::Text("글".into()));
    assert_eq!(h.view().app.input.as_ref().unwrap().content(), "한글");
    h.tab();
    assert!(!h.ime_enabled());
}

#[test]
fn text_field_keeps_tab_and_enter_for_the_app() {
    let mut h = harness(Form::default());
    h.click_id(FIELD);
    h.input(Synthetic::Text("ab".into()));
    h.press_key(KeyPress::char("c"));
    h.press_key(key(NamedKey::Enter));
    assert_eq!(h.view().app.input.as_ref().unwrap().content(), "abc");
    h.tab();
    assert_eq!(h.focused(), Some(A));
}

#[test]
fn tooltips_wait_then_show() {
    let mut h = harness(Form::default());
    let r = h.hitbox(B).unwrap();
    h.input(Synthetic::Move(Point::new(r.x + 5., r.y + 5.)));
    let tip_layer = |h: &Harness<UiView<Form>>| h.scene().layers[podbag_ui::layer::TOP].quads.len();
    assert_eq!(tip_layer(&h), 0);
    assert!(h.wake_at().is_some(), "a timer for the tooltip");
    std::thread::sleep(Duration::from_millis(650));
    h.timer();
    assert!(tip_layer(&h) > 0, "tooltip shown");
    // Leaving hides it.
    h.input(Synthetic::Move(Point::new(500., 500.)));
    assert_eq!(tip_layer(&h), 0);
}

fn with_fonts(form: Form) -> Option<Harness<UiView<Form>>> {
    let fonts = load_fonts();
    if fonts.faces == 0 {
        eprintln!("no fonts: skipped");
        return None;
    }
    Some(Harness::with_fonts(UiView::new(form), 800., 600., fonts))
}

#[test]
fn drag_selects_text_and_copy_puts_it_on_the_clipboard() {
    let Some(mut h) = with_fonts(Form {
        block: "hello selectable world",
        ..Default::default()
    }) else {
        return;
    };
    let r = h.hitbox(BLOCK).unwrap();
    h.drag(
        Point::new(r.x + 1., r.y + 10.),
        Point::new(r.right() - 1., r.y + 10.),
    );
    assert_eq!(h.focused(), Some(BLOCK));
    h.press_key(KeyPress::shortcut('c'));
    assert_eq!(
        h.clipboard().read().as_deref(),
        Some("hello selectable world")
    );
    // Double click: a word.
    let x = r.x + 70.; // in "selectable" at 14 px
    h.click(Point::new(x, r.y + 10.));
    h.click(Point::new(x, r.y + 10.));
    h.press_key(KeyPress::shortcut('c'));
    assert_eq!(h.clipboard().read().as_deref(), Some("selectable"));
    // A click elsewhere clears the selection; Cmd+C then copies nothing new.
    h.click(Point::new(700., 500.));
    h.clipboard().write("before".into());
    h.press_key(KeyPress::shortcut('c'));
    assert_eq!(h.clipboard().read().as_deref(), Some("before"));
}

#[test]
fn logs_select_across_lines_and_select_all() {
    let mut form = Form {
        show_logs: true,
        ..Default::default()
    };
    form.logs.push((0..100).map(|i| format!("line {i}")));
    form.logs.follow = false;
    let Some(mut h) = with_fonts(form) else {
        return;
    };
    let r = h.hitbox(LOGS).unwrap();
    // From the start of line 1 to the middle of line 3 (rows are 19 px).
    h.drag(
        Point::new(r.x + 0.5, r.y + 19. + 5.),
        Point::new(r.x + 200., r.y + 3. * 19. + 5.),
    );
    h.press_key(KeyPress::shortcut('c'));
    assert_eq!(
        h.clipboard().read().as_deref(),
        Some("line 1\nline 2\nline 3")
    );
    h.press_key(KeyPress::shortcut('a'));
    h.press_key(KeyPress::shortcut('c'));
    let all = h.clipboard().read().unwrap();
    assert!(all.starts_with("line 0\nline 1\n") && all.ends_with("line 99"));
    // The selected line is dropped from the front (over 6000 lines): the selection
    // goes with it, and Cmd+C copies nothing.
    h.drag(
        Point::new(r.x + 0.5, r.y + 5.),
        Point::new(r.x + 200., r.y + 5.),
    );
    h.view_mut()
        .app
        .logs
        .push((100..6001).map(|i| format!("line {i}")));
    h.repaint();
    h.clipboard().write("before".into());
    h.press_key(KeyPress::shortcut('c'));
    assert_eq!(h.clipboard().read().as_deref(), Some("before"));
}

#[test]
fn shift_click_extends_the_selection() {
    let Some(mut h) = with_fonts(Form {
        block: "alpha beta gamma",
        ..Default::default()
    }) else {
        return;
    };
    let r = h.hitbox(BLOCK).unwrap();
    h.click(Point::new(r.x + 0.5, r.y + 10.));
    h.input(Synthetic::Modifiers(ModifiersState::SHIFT));
    h.click(Point::new(r.right() - 0.5, r.y + 10.));
    h.input(Synthetic::Modifiers(ModifiersState::empty()));
    h.press_key(KeyPress::shortcut('c'));
    assert_eq!(h.clipboard().read().as_deref(), Some("alpha beta gamma"));
}
