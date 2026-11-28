//! Image compression on the mock backend: the question asked once, the switch in the
//! engine's settings, and compressed images in the Images view.

use podbag_desktop::icons::Icon;
use podbag_desktop::views::compression::{self, Note, SWITCH};
use podbag_desktop::views::resources;
use podbag_desktop::{Desktop, Msg, Options, Page, dialogs};
use podbag_desktop_core::compression::Mode;
use podbag_desktop_core::mock::{CompressionOptions, Options as MockOptions, Scenario};
use podbag_desktop_core::{Backend, Kind};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{KeyPress, NamedKey};

type H = Harness<UiView<Desktop>>;

fn window_with(opts: MockOptions) -> H {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        instant: true,
        ..opts
    };
    let (backend, events) = Backend::mock(opts, || {});
    let app = Desktop::new(backend, events, Options::inline());
    let mut h = Harness::new(UiView::new(app), 1200., 800.);
    h.repaint();
    h
}

/// The question shows at start (a new install, or an update to this version).
fn asked(scenario: Scenario, compression: CompressionOptions) -> H {
    window_with(MockOptions {
        scenario,
        compression_prompt: true,
        compression,
        ..MockOptions::default()
    })
}

fn settings_window(compression: CompressionOptions, lang: &str) -> H {
    let mut h = window_with(MockOptions {
        compression,
        language: Some(lang.into()),
        ..MockOptions::default()
    });
    open_engine_settings(&mut h);
    h
}

fn app(h: &H) -> &Desktop {
    &h.view().app
}

fn wake(h: &mut H) {
    h.message(Msg::Wake);
    h.repaint();
}

fn open_engine_settings(h: &mut H) {
    h.view_mut().app.open_section("engine");
    wake(h);
}

fn questioned(h: &H) -> bool {
    matches!(app(h).dialog, Some(dialogs::Dialog::Compression))
}

fn toasted(h: &H, part: &str) -> bool {
    app(h).toasts.texts().any(|(t, _)| t.contains(part))
}

/// The engine's setting, as the mock keeps it.
fn engine_on(h: &H) -> bool {
    app(h).api.compression(None).unwrap().is_on()
}

fn seen(h: &H) -> bool {
    app(h).api.settings().compression_prompt_seen
}

fn vm_running(h: &H) -> bool {
    app(h).api.engine(false).vm.as_deref() == Some("running")
}

fn click(h: &mut H, id: podbag_ui::Id) {
    h.click_id(id);
    wake(h);
}

/// The question as it opens: its title and icon, what it costs, and its two answers.
fn check_question(h: &H) {
    assert!(questioned(h), "the question shows");
    assert_eq!(h.focused(), Some(dialogs::CANCEL), "Not now has the focus");
    assert!(h.shows("Save disk space"));
    assert!(h.shows("A compressed image starts a little slower"));
    assert!(h.shows("Not now") && h.shows("Turn on"));
    let icons = &h.scene().layers;
    assert!(
        icons
            .iter()
            .flat_map(|l| &l.icons)
            .any(|i| i.icon == Icon::Compress.id()),
        "the compression icon is next to the title"
    );
}

#[test]
fn turn_on_sets_idle_and_the_switch_shows_it() {
    let mut h = asked(
        Scenario::Default,
        CompressionOptions {
            after: 129_600,
            ..CompressionOptions::default()
        },
    );
    check_question(&h);
    click(&mut h, dialogs::YES);
    assert!(!questioned(&h));
    assert!(engine_on(&h), "compression is set to idle");
    assert!(toasted(&h, "Image compression is on."));
    assert!(seen(&h), "it is not asked again");
    open_engine_settings(&mut h);
    let c = &app(&h).compression;
    assert!(
        c.enabled() && c.shown_on(),
        "the switch shows compression on"
    );
    assert!(h.shows("Compress idle images"));
    assert!(h.shows(
        "Compress images that no container used for 36 hours. They are uncompressed when a container needs them."
    ));
    click(&mut h, SWITCH);
    assert!(!engine_on(&h), "the switch turns it off");
    assert!(!app(&h).compression.shown_on() && app(&h).compression.enabled());
}

#[test]
fn not_now_keeps_it_off_and_says_where_to_turn_it_on() {
    let mut h = asked(Scenario::Default, CompressionOptions::default());
    check_question(&h);
    click(&mut h, dialogs::CANCEL);
    assert!(!questioned(&h));
    assert!(!engine_on(&h), "no change");
    assert!(seen(&h), "the answer is kept");
    assert!(toasted(&h, "You can turn it on in Settings at any time."));
    open_engine_settings(&mut h);
    assert!(app(&h).compression.enabled() && !app(&h).compression.shown_on());
    // Escape is Not now too.
    let mut h = asked(Scenario::Default, CompressionOptions::default());
    h.press_key(KeyPress::named(NamedKey::Escape));
    wake(&mut h);
    assert!(!questioned(&h) && seen(&h) && !engine_on(&h));
}

#[test]
fn a_switch_that_fails_goes_back_and_shows_the_error() {
    let mut h = settings_window(
        CompressionOptions {
            fail: Some("the VM is not running".into()),
            ..CompressionOptions::default()
        },
        "en",
    );
    assert!(app(&h).compression.enabled());
    // The switch is disabled while the change runs; its result comes on the next frame.
    compression::write(&mut h.view_mut().app, true, false);
    assert!(!app(&h).compression.enabled(), "disabled while it works");
    assert!(
        app(&h).compression.shown_on(),
        "it shows the value asked for"
    );
    wake(&mut h);
    assert!(app(&h).compression.enabled());
    assert!(!app(&h).compression.shown_on(), "back to off");
    assert!(toasted(&h, "the VM is not running"));
}

#[test]
fn a_failed_turn_on_asks_again_at_the_next_launch() {
    let mut h = asked(
        Scenario::Default,
        CompressionOptions {
            fail: Some("disk is full".into()),
            ..CompressionOptions::default()
        },
    );
    click(&mut h, dialogs::YES);
    assert!(!questioned(&h));
    assert!(toasted(
        &h,
        "Could not turn on image compression: disk is full"
    ));
    assert!(!seen(&h));
}

#[test]
fn compression_already_on_is_not_asked_about() {
    let h = asked(
        Scenario::Default,
        CompressionOptions {
            on: true,
            ..CompressionOptions::default()
        },
    );
    assert!(!questioned(&h));
    assert!(seen(&h), "on counts as an answer");
}

#[test]
fn a_stopped_vm_is_not_started_to_ask_and_the_switch_waits_for_it() {
    let mut h = asked(Scenario::VmStopped, CompressionOptions::default());
    check_question(&h);
    assert!(!vm_running(&h), "the setting was not read in the VM");
    // Turn on runs in the VM, which starts it.
    click(&mut h, dialogs::YES);
    assert!(engine_on(&h) && vm_running(&h));
    assert!(seen(&h));
    // Settings with the VM off: the switch waits, with the default age.
    let mut h = window_with(MockOptions {
        scenario: Scenario::VmStopped,
        compression: CompressionOptions {
            on: true,
            after: 86_400,
            ..CompressionOptions::default()
        },
        ..MockOptions::default()
    });
    open_engine_settings(&mut h);
    assert_eq!(app(&h).compression.note, Note::VmOff);
    assert!(!app(&h).compression.enabled(), "it waits for the VM");
    assert!(h.shows("The VM is off"));
    assert!(
        h.shows("for 7 days."),
        "the default age while it is unknown"
    );
    assert!(!vm_running(&h));
    h.view_mut().app.run_vm_action("start");
    wake(&mut h);
    wake(&mut h);
    let c = &app(&h).compression;
    assert!(c.enabled() && c.shown_on(), "read once the VM runs");
    assert!(!h.shows("The VM is off"));
    assert!(h.shows("for 1 day."), "the engine's age");
}

#[test]
fn an_unknown_vm_state_is_not_woken_and_the_question_comes() {
    let mut h = asked(
        Scenario::VmStopped,
        CompressionOptions {
            vm_error: true,
            ..CompressionOptions::default()
        },
    );
    check_question(&h);
    assert!(!vm_running(&h));
    click(&mut h, dialogs::CANCEL);
    assert!(seen(&h));
}

#[test]
fn a_reply_without_a_mode_asks_nothing_and_saves_nothing() {
    let h = asked(
        Scenario::Default,
        CompressionOptions {
            broken: true,
            ..CompressionOptions::default()
        },
    );
    assert!(!questioned(&h));
    assert!(!seen(&h), "read again at the next launch");
}

#[test]
fn an_environment_setting_that_keeps_it_off_is_named() {
    let mut h = asked(
        Scenario::Default,
        CompressionOptions {
            kept: true,
            ..CompressionOptions::default()
        },
    );
    click(&mut h, dialogs::YES);
    assert!(!questioned(&h));
    assert!(toasted(
        &h,
        "Could not turn on image compression: An environment setting keeps image compression off."
    ));
    assert!(!toasted(&h, "Image compression is on."));
    assert!(!seen(&h));
}

#[test]
fn another_dialog_puts_the_question_off_and_the_star_prompt_comes_first() {
    let mut h = window_with(MockOptions {
        compression_prompt: true,
        first_launch: true,
        ..MockOptions::default()
    });
    assert!(matches!(app(&h).dialog, Some(dialogs::Dialog::Star)));
    // Asked while another dialog is open: not this launch.
    compression::prompt(&mut h.view_mut().app);
    wake(&mut h);
    assert!(matches!(app(&h).dialog, Some(dialogs::Dialog::Star)));
    assert!(!seen(&h));
    // After the star prompt, the question.
    click(&mut h, dialogs::CANCEL);
    assert!(questioned(&h));
}

#[test]
fn another_engine_has_no_question_and_no_switch() {
    let mut h = asked(Scenario::Docker, CompressionOptions::default());
    assert!(!questioned(&h));
    open_engine_settings(&mut h);
    assert!(h.hitbox(SWITCH).is_none());
    assert!(!h.shows("Compress idle images"));
}

#[test]
fn an_age_of_zero_compresses_at_once() {
    let h = settings_window(
        CompressionOptions {
            on: true,
            after: 0,
            ..CompressionOptions::default()
        },
        "en",
    );
    assert!(h.shows(
        "Compress images as soon as no container uses them. They are uncompressed when a container needs them."
    ));
}

#[test]
fn an_environment_setting_that_keeps_it_on_is_named() {
    let mut h = settings_window(
        CompressionOptions {
            on: true,
            kept: true,
            ..CompressionOptions::default()
        },
        "en",
    );
    assert!(app(&h).compression.shown_on());
    click(&mut h, SWITCH);
    assert!(toasted(
        &h,
        "Could not turn off image compression: An environment setting keeps image compression on."
    ));
    assert!(app(&h).compression.shown_on() && app(&h).compression.enabled());
}

#[test]
fn a_change_whose_reply_has_no_mode_fails() {
    let mut h = settings_window(
        CompressionOptions {
            broken: true,
            ..CompressionOptions::default()
        },
        "en",
    );
    assert!(app(&h).compression.enabled());
    click(&mut h, SWITCH);
    assert!(toasted(&h, "Something went wrong"));
    assert!(
        !toasted(&h, "environment setting"),
        "no environment setting is blamed"
    );
    assert!(!app(&h).compression.shown_on() && app(&h).compression.enabled());
}

#[test]
fn settings_opened_again_during_a_change_read_after_it() {
    let mut h = settings_window(CompressionOptions::default(), "en");
    assert!(app(&h).compression.enabled());
    // A change, then Images and Settings again before its result came in.
    let d = &mut h.view_mut().app;
    compression::write(d, true, false);
    d.go(Page::Images);
    d.open_section("engine");
    assert!(
        !app(&h).compression.enabled(),
        "the read waits for the change"
    );
    wake(&mut h);
    let c = &app(&h).compression;
    assert!(c.enabled() && c.shown_on(), "the result of the change");
    assert!(h.shows("The Images view shows compressed images and how much smaller they got."));
    assert_eq!(
        app(&h).api.compression(None).unwrap().mode,
        Some(Mode::Idle)
    );
}

// ---- the Images view ----

/// The clip width of the text drawn for `text` (its cell).
fn cell_width(h: &H, text: &str) -> f32 {
    h.scene()
        .layers
        .iter()
        .flat_map(|l| &l.texts)
        .find(|t| t.layout.text() == text)
        .map(|t| t.clip.w)
        .unwrap_or_else(|| panic!("no text {text}"))
}

fn compress_icons(h: &H) -> usize {
    h.scene()
        .layers
        .iter()
        .flat_map(|l| &l.icons)
        .filter(|i| i.icon == Icon::Compress.id())
        .count()
}

fn images_window(lang: &str) -> H {
    let mut h = window_with(MockOptions {
        language: Some(lang.into()),
        ..MockOptions::default()
    });
    h.view_mut().app.go(Page::Images);
    h.repaint();
    h
}

fn image_row(h: &H, name: &str) -> podbag_ui::Id {
    let i = app(h)
        .lists
        .images
        .iter()
        .position(|i| i.name() == name)
        .unwrap();
    resources::table_id(Kind::Images).with(("row", i))
}

#[test]
fn a_compressed_image_shows_its_size_on_disk_and_how_much_smaller_it_got() {
    let mut h = images_window("en");
    assert!(h.shows("3.9 MB"), "the size on disk first");
    assert!(h.shows("From 8.5 MB, 54% smaller"));
    assert_eq!(
        compress_icons(&h),
        1,
        "only the compressed image has the icon"
    );
    assert!(h.shows("52.4 MB"), "a plain image keeps one line");
    let wide = cell_width(&h, "52.4 MB");
    assert!(
        wide > 150.,
        "the Size column is wider for the second line ({wide})"
    );
    // Without a compressed row, the column keeps its width.
    h.view_mut().app.search[1].set_content("nginx");
    h.repaint();
    let narrow = cell_width(&h, "52.4 MB");
    assert!(narrow < 110., "{narrow}");
    assert_eq!(compress_icons(&h), 0);
    h.view_mut().app.search[1].set_content("");
    h.repaint();
    // The details: both sizes and the ratio.
    h.click_id(image_row(&h, "alpine:3.22"));
    assert!(h.shows("Compression"));
    assert!(h.shows("Compressed: 3.9 MB (8.5 MB before)"));
    assert!(h.shows("54% smaller"));
    h.view_mut().app.go(Page::Images);
    h.repaint();
    h.click_id(image_row(&h, "nginx:alpine"));
    assert!(h.shows("Tags"));
    assert!(
        !h.shows("Compression"),
        "no Compression row for a plain image"
    );
}

#[test]
fn the_compressed_size_reads_in_korean() {
    let h = images_window("ko");
    assert!(h.shows("8.5MB에서 54% 줄어듦"));
}

#[test]
fn the_settings_say_where_to_see_it_while_it_is_on() {
    let note = "The Images view shows compressed images and how much smaller they got.";
    let mut h = settings_window(CompressionOptions::default(), "en");
    assert!(!h.shows(note), "hidden while off");
    click(&mut h, SWITCH);
    assert!(h.shows(note));
    click(&mut h, SWITCH);
    assert!(!h.shows(note), "hidden again once off");
    let h = settings_window(
        CompressionOptions {
            on: true,
            ..CompressionOptions::default()
        },
        "ko",
    );
    assert!(h.shows("이미지 화면에서 압축된 이미지와 줄어든 비율을 볼 수 있습니다."));
}
