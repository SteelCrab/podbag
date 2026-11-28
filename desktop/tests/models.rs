//! Models on the mock backend: opening the page starts nothing; Start, Pull, Run and
//! Unload; a run that is still going when the runner stops leaves no answer; a missing
//! Ollama is named and Start works again; quitting the app stops the runner.

use std::sync::Arc;
use std::time::{Duration, Instant};

use podbag_desktop::platform::Native;
use podbag_desktop::views::models::{self, NAME, PROMPT, PULL, RUN, START, STOP, unload_id};
use podbag_desktop::{Desktop, Msg, Options, Page, shell};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::{Calls, Gate, ModelOptions, Options as MockOptions};
use podbag_desktop_core::model_runner::{Action, Reply};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;
use podbag_ui::{Id, KeyPress, ModifiersState, NamedKey, Synthetic, View};

type H = Harness<UiView<Desktop>>;

/// The window on Models. `inline`: jobs run at once; else on threads (see `until`).
fn window(models: ModelOptions, calls: &Calls, inline: bool) -> H {
    podbag_desktop::icons::install();
    let opts = MockOptions {
        instant: true,
        models,
        calls: calls.clone(),
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(opts, || {});
    let options = if inline {
        Options::inline()
    } else {
        Options {
            inline: false,
            monitor: false,
            waker: Arc::new(|| {}),
        }
    };
    let desktop = Desktop::new(backend, events, options);
    let mut h = Harness::new(UiView::new(desktop), 1200., 900.);
    h.click_id(shell::nav_id(Page::Models));
    until(&mut h, |h| app(h).models.pending.is_none());
    h
}

fn app(h: &H) -> &Desktop {
    &h.view().app
}

/// Take in the jobs' results and paint until `done` (at most 5 s).
fn until(h: &mut H, done: impl Fn(&H) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        h.message(Msg::Wake);
        h.repaint();
        if done(h) {
            return;
        }
        assert!(Instant::now() < end, "timed out");
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Replace the text of the field `id`.
fn set(h: &mut H, id: Id, text: &str) {
    h.click_id(id);
    let m = if cfg!(target_os = "macos") {
        ModifiersState::SUPER
    } else {
        ModifiersState::CONTROL
    };
    h.press_key(KeyPress::char("a").with_mods(m));
    h.input(Synthetic::Text(text.into()));
}

/// Replace the text of the field `id`, then press Enter there.
fn submit(h: &mut H, id: Id, text: &str) {
    set(h, id, text);
    h.press_key(KeyPress::named(NamedKey::Enter));
}

#[test]
fn opening_models_starts_nothing() {
    let calls = Calls::default();
    let h = window(ModelOptions::default(), &calls, true);
    assert_eq!(app(&h).page, Page::Models);
    assert_eq!(
        calls.of("models"),
        ["status"],
        "opening Models only asks the state"
    );
    assert!(h.hitbox(START).is_some(), "Start runner shows");
    assert!(h.hitbox(STOP).is_none());
    assert!(h.shows("Runner stopped"));
    assert!(h.shows("Start local Ollama to download and run models"));
    assert!(h.hitbox(NAME).is_none(), "nothing to pull while it is off");
}

#[test]
fn start_pull_run_and_unload() {
    let calls = Calls::default();
    let mut h = window(ModelOptions::default(), &calls, true);
    h.click_id(START);
    assert!(h.hitbox(STOP).is_some(), "Stop runner replaces Start");
    assert!(h.shows("Runner active"));
    assert!(h.shows("Ollama 0.40.17"));
    assert!(h.shows("No models installed."));
    assert!(h.hitbox(RUN).is_none(), "nothing to run without a model");

    submit(&mut h, NAME, "smollm2:135m");
    assert!(h.shows("smollm2:135m"), "the downloaded model is listed");
    assert!(h.shows("Idle"));
    assert_eq!(
        app(&h).models.name.content(),
        "",
        "the field empties after a pull"
    );

    submit(&mut h, PROMPT, "Name a color.");
    assert_eq!(app(&h).models.response, "Blue.");
    assert!(h.shows("Blue."), "the local response shows");
    assert!(h.shows("Loaded"), "the model's state is read again");

    h.click_id(unload_id("smollm2:135m"));
    assert!(
        h.shows("Idle"),
        "Unload frees the model without removing it"
    );
    assert!(h.shows("smollm2:135m"));
    assert_eq!(
        calls.of("models"),
        ["status", "start", "pull", "run", "status", "unload"]
    );

    // A name that is not an Ollama registry model is refused.
    set(&mut h, NAME, "https://somewhere.test/m");
    h.click_id(PULL);
    assert!(h.shows("Use a local model name or tag from the Ollama registry."));
    // Refresh reads the state again.
    h.click_id(models::REFRESH);
    assert!(app(&h).models.error.is_none());
    assert_eq!(
        calls.of("models").last().map(String::as_str),
        Some("status")
    );
}

#[test]
fn a_run_cut_short_by_stop_leaves_no_answer() {
    let calls = Calls::default();
    let gate = Gate::default();
    let models = ModelOptions {
        gate: gate.clone(),
        ..ModelOptions::default()
    };
    let mut h = window(models, &calls, false);
    h.click_id(START);
    until(&mut h, |h| h.hitbox(STOP).is_some());
    submit(&mut h, NAME, "smollm2:135m");
    until(&mut h, |h| h.hitbox(RUN).is_some() && !app(h).models.busy());

    gate.close();
    submit(&mut h, PROMPT, "Generate another color.");
    until(&mut h, |h| app(h).models.pending == Some("run"));
    assert!(h.shows("Generating a response…"));
    // While it runs, Run is off; Stop is not.
    assert!(h.hitbox(RUN).is_none(), "Run is disabled");
    h.click_id(STOP);
    until(&mut h, |h| {
        h.hitbox(START).is_some() && !app(h).models.busy()
    });
    gate.open();
    // The run ends; its answer is dropped.
    std::thread::sleep(Duration::from_millis(100));
    until(&mut h, |_| true);
    assert!(app(&h).models.response.is_empty());
    assert!(!h.shows("Blue."), "a canceled generation shows nothing");
    assert_eq!(
        calls.of("models").iter().filter(|c| *c == "run").count(),
        1,
        "Run was off while a run went on"
    );

    h.click_id(START);
    until(&mut h, |h| {
        h.hitbox(STOP).is_some() && !app(h).models.busy()
    });
    assert!(h.shows("smollm2:135m"), "the models stay after a restart");
    assert!(!h.shows("Blue."), "no answer appears after the restart");
}

#[test]
fn a_missing_ollama_is_named_and_start_works_again() {
    let calls = Calls::default();
    let models = ModelOptions {
        missing: true,
        ..ModelOptions::default()
    };
    let mut h = window(models, &calls, true);
    h.click_id(START);
    assert!(h.shows("Ollama was not found. Install it and make it available on PATH."));
    assert!(app(&h).models.pending.is_none());
    h.click_id(START);
    assert_eq!(calls.of("models"), ["status", "start", "start"]);
}

#[test]
fn models_show_in_every_language() {
    let calls = Calls::default();
    let mut h = window(ModelOptions::default(), &calls, true);
    h.click_id(START);
    for lang in ["ko", "ja", "zh-CN", "es", "vi", "fil"] {
        h.view_mut().app.set_language(lang);
        h.repaint();
        let title = app(&h).t("nav.models");
        assert_ne!(title, "nav.models");
        assert!(h.shows(&title), "{lang}");
        assert!(h.shows(&app(&h).t("models.installed")), "{lang}");
        assert!(
            !h.texts().iter().any(|t| t.starts_with("models.")),
            "{lang}"
        );
    }
}

#[test]
fn quitting_stops_the_runner() {
    podbag_desktop::icons::install();
    let models = ModelOptions {
        running: true,
        ..ModelOptions::default()
    };
    let opts = MockOptions {
        instant: true,
        models,
        ..MockOptions::default()
    };
    let (backend, events) = Backend::mock(opts, || {});
    let api = backend.clone();
    let running = || match api.models(Action::Status) {
        Ok(Reply::Status(s)) => s.running,
        _ => false,
    };
    assert!(running());
    let desktop = Desktop::new(backend.clone(), events, Options::inline());
    let mut h = Harness::new(
        UiView::new(Native::new(desktop, Arc::new(|_| {}), false)),
        1200.,
        800.,
    );
    h.close_window();
    assert!(h.quit_requested());
    h.view_mut().exiting();
    assert!(!running(), "the runner stops with the app");
}
