//! The native image Security tab and the lifetime of a scan.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use podbag_desktop::views::{detail, image_analysis::SCAN};
use podbag_desktop::{Desktop, Msg, Options};
use podbag_desktop_core::image_analysis::Failure;
use podbag_desktop_core::mock::{Calls, Gate, ImageAnalysisOptions, Options as MockOptions};
use podbag_desktop_core::{Backend, Kind};
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;

type Window = Harness<UiView<Desktop>>;

fn window(analysis: ImageAnalysisOptions, calls: &Calls, options: Options) -> Window {
    podbag_desktop::icons::install();
    let (backend, events) = Backend::mock(
        MockOptions {
            instant: true,
            analysis,
            calls: calls.clone(),
            ..MockOptions::default()
        },
        || {},
    );
    let app = Desktop::new(backend, events, options);
    let mut h = Window::new(UiView::new(app), 1200., 900.);
    until(&mut h, |h| h.view().app.loaded);
    h.view_mut().app.command("detail images 0");
    h.repaint();
    h.click_id(detail::TABS.with(2usize));
    h
}

fn until(h: &mut Window, done: impl Fn(&Window) -> bool) {
    let end = Instant::now() + Duration::from_secs(5);
    loop {
        h.message(Msg::Wake);
        h.repaint();
        if done(h) {
            return;
        }
        assert!(Instant::now() < end, "timed out waiting for image analysis");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn scan_is_explicit_and_displays_findings_and_packages() {
    let calls = Calls::default();
    let mut h = window(ImageAnalysisOptions::default(), &calls, Options::inline());
    assert!(h.shows("Use local Trivy"));
    assert!(calls.of("analyze_image").is_empty());
    h.click_id(SCAN);
    until(&mut h, |h| h.shows("Vulnerabilities (1)"));
    assert!(h.shows("Installed packages (1)"));
    assert!(h.shows("High"));
    assert!(h.shows("DEMO-2026-0001"));
    assert!(h.shows("1.2.5-r11"));
    assert!(h.shows("musl"));
    assert_eq!(calls.of("analyze_image").len(), 1);
}

#[test]
fn empty_report_has_separate_messages() {
    let mut h = window(
        ImageAnalysisOptions {
            empty: true,
            ..ImageAnalysisOptions::default()
        },
        &Calls::default(),
        Options::inline(),
    );
    h.click_id(SCAN);
    until(&mut h, |h| {
        h.shows("Trivy reported no known vulnerabilities.")
    });
    assert!(h.shows("Trivy reported no installed packages."));
}

#[test]
fn every_scan_failure_explains_the_error_and_allows_retry() {
    for (failure, text) in [
        (Failure::Busy, "Another image scan is running."),
        (Failure::EngineChanged, "The engine changed."),
        (
            Failure::ImageUnavailable,
            "This image is no longer available",
        ),
        (Failure::ScannerMissing, "Trivy was not found."),
        (Failure::Export, "could not export this image."),
        (Failure::ExportStalled, "stopped sending this image."),
        (
            Failure::Workspace,
            "Could not create the files for this scan.",
        ),
        (Failure::Scanner, "Trivy could not scan this image."),
        (Failure::Report, "Trivy returned a report"),
    ] {
        let calls = Calls::default();
        let mut h = window(
            ImageAnalysisOptions {
                failure: Some(failure),
                ..ImageAnalysisOptions::default()
            },
            &calls,
            Options::inline(),
        );
        h.click_id(SCAN);
        until(&mut h, |h| h.shows(text));
        assert!(h.hitbox(SCAN).is_some(), "{failure:?}");
        h.click_id(SCAN);
        until(&mut h, |h| h.shows(text));
        assert_eq!(calls.of("analyze_image").len(), 2);
    }
}

#[test]
fn unavailable_image_still_shows_its_error_after_the_list_refreshes() {
    let mut h = window(
        ImageAnalysisOptions {
            failure: Some(Failure::ImageUnavailable),
            ..ImageAnalysisOptions::default()
        },
        &Calls::default(),
        Options::inline(),
    );
    h.click_id(SCAN);
    h.view_mut().app.lists.images.clear();
    until(&mut h, |h| {
        h.shows("This image is no longer available on the selected engine.")
    });
    assert!(h.hitbox(detail::BACK).is_some());
}

#[test]
fn closing_and_reopening_the_same_image_retires_an_in_flight_scan() {
    let calls = Calls::default();
    let gate = Gate::default();
    let wakeups = Arc::new(AtomicUsize::new(0));
    let wake = wakeups.clone();
    let mut h = window(
        ImageAnalysisOptions {
            gate: gate.clone(),
            ..ImageAnalysisOptions::default()
        },
        &calls,
        Options {
            inline: false,
            monitor: false,
            waker: Arc::new(move || {
                wake.fetch_add(1, Ordering::SeqCst);
            }),
        },
    );
    gate.close();
    h.click_id(SCAN);
    until(&mut h, |h| {
        !calls.of("analyze_image").is_empty() && h.shows("Scanning image")
    });
    assert!(h.hitbox(SCAN).is_none());
    let id = h.view().app.detail.as_ref().unwrap().id.clone();
    h.click_id(detail::BACK);
    h.view_mut().app.command("detail images 0 analysis");
    h.repaint();
    assert_eq!(h.view().app.detail.as_ref().unwrap().id, id);
    let last = wakeups.load(Ordering::SeqCst);
    gate.open();
    until(&mut h, |_| wakeups.load(Ordering::SeqCst) > last);
    assert!(h.hitbox(SCAN).is_some());
    assert!(!h.shows("DEMO-2026-0001"));

    let previous = h.view().app.session;
    h.view_mut().app.switch_engine("docker").unwrap();
    assert_eq!(
        h.view().app.api.analyze_image(previous, &id),
        Err(Failure::EngineChanged)
    );
    assert!(h.view().app.detail.is_none());
}

#[test]
fn other_resources_have_no_security_tab() {
    let mut h = window(
        ImageAnalysisOptions::default(),
        &Calls::default(),
        Options::inline(),
    );
    h.view_mut().app.command("detail containers 0");
    h.repaint();
    assert_eq!(h.view().app.detail.as_ref().unwrap().kind, Kind::Containers);
    assert!(!h.shows("Security"));
    assert!(h.hitbox(SCAN).is_none());
}
