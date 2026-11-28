//! The window's lifecycle (close, reopen), the mouse side buttons and images, through
//! the headless harness.

use podbag_ui::testing::Harness;
use podbag_ui::ui::{Theme, Ui, UiApp, UiView};
use podbag_ui::{Cx, Image, ImageData, NavButton, Rect, rgb};

#[derive(Default)]
struct App {
    /// Hide on close instead of quitting.
    hide_on_close: bool,
    nav: Vec<NavButton>,
    image: Option<Image>,
}

impl UiApp for App {
    type Message = ();

    fn ui(&mut self, ui: &mut Ui) {
        self.nav.extend(ui.take_nav());
        if let Some(image) = self.image {
            ui.p.image(image, Rect::new(10., 10., 32., 32.));
            ui.p.image_tinted(image, Rect::new(50., 10., 16., 16.), rgb(0x112233));
        }
    }

    fn message(&mut self, _: (), _: &mut Cx) {}

    fn theme(&self, _: bool) -> Theme {
        Theme::light()
    }

    fn close_requested(&mut self, cx: &mut Cx) {
        if self.hide_on_close {
            cx.hide_window();
        } else {
            cx.quit();
        }
    }
}

#[test]
fn closing_quits_by_default() {
    let mut h = Harness::new(UiView::new(App::default()), 400., 300.);
    h.close_window();
    assert!(h.quit_requested());
}

#[test]
fn an_app_can_hide_on_close_and_show_on_reopen() {
    let app = App {
        hide_on_close: true,
        ..App::default()
    };
    let mut h = Harness::new(UiView::new(app), 400., 300.);
    assert!(h.window_visible());
    h.close_window();
    assert!(!h.quit_requested());
    assert!(!h.window_visible());
    h.reopen();
    assert!(h.window_visible());
}

#[test]
fn side_buttons_reach_the_app_once() {
    let mut h = Harness::new(UiView::new(App::default()), 400., 300.);
    h.nav(NavButton::Back);
    h.nav(NavButton::Forward);
    h.repaint();
    assert_eq!(h.view().app.nav, [NavButton::Back, NavButton::Forward]);
}

#[test]
fn images_go_into_the_scene() {
    let red = ImageData::new(2, 2, [255, 0, 0, 255].repeat(4)).unwrap();
    let app = App {
        image: Some(Image::register(red)),
        ..App::default()
    };
    let h = Harness::new(UiView::new(app), 400., 300.);
    assert_eq!(h.scene().image_count(), 2);
}

/// Draws through the GPU where there is one (skipped without).
#[test]
fn images_draw_in_color_and_tinted() {
    let red = ImageData::new(2, 2, [255, 0, 0, 255].repeat(4)).unwrap();
    let app = App {
        image: Some(Image::register(red)),
        ..App::default()
    };
    let mut h = Harness::new(UiView::new(app), 100., 60.);
    let Some((w, _, px)) = h.capture(2.) else {
        return;
    };
    let at = |x: u32, y: u32| {
        let i = ((y * w + x) * 4) as usize;
        [px[i], px[i + 1], px[i + 2]]
    };
    // The colored image at (10, 10, 32, 32), the tinted one at (50, 10, 16, 16).
    assert_eq!(at(52, 52), [255, 0, 0]);
    assert_eq!(at(116, 36), [0x11, 0x22, 0x33]);
    // The page around them.
    let page = Theme::light().page.to_u8();
    assert_eq!(at(4, 4), [page[0], page[1], page[2]]);
}
