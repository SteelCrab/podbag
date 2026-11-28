use podbag_desktop::views::{notifications, settings_search};
use podbag_desktop::{Desktop, Options, Page, dialogs, shell};
use podbag_desktop_core::Backend;
use podbag_desktop_core::mock::Options as MockOptions;
use podbag_ui::Synthetic;
use podbag_ui::testing::Harness;
use podbag_ui::ui::UiView;

fn window() -> Harness<UiView<Desktop>> {
    podbag_desktop::icons::install();
    let (backend, events) = Backend::mock(
        MockOptions {
            instant: true,
            ..Default::default()
        },
        || {},
    );
    Harness::new(
        UiView::new(Desktop::new(backend, events, Options::inline())),
        1200.,
        1150.,
    )
}

#[test]
fn suppresses_only_routine_popups_and_preserves_both_kinds_in_history() {
    let mut h = window();
    h.click_id(shell::nav_id(Page::Settings));
    h.click_id(settings_search::SEARCH);
    h.input(Synthetic::Text("routine task popups".into()));
    h.click_id(settings_search::result_id("notifications.status"));
    h.view_mut().app.toasts.push("Early status", false);
    h.click_id(notifications::STATUS);
    assert!(!h.view().app.api.settings().status_notifications);
    assert!(h.view().app.toasts.is_empty());
    h.view_mut().app.toasts.push("Finished task", false);
    h.view_mut().app.toasts.push("Failed task", true);
    assert_eq!(
        h.view().app.toasts.texts().collect::<Vec<_>>(),
        [("Failed task", true)]
    );
    h.click_id(notifications::OPEN);
    assert!(h.shows("Notification history"));
    assert!(h.shows("Finished task"));
    assert!(h.shows("Early status"));
    h.click_id(notifications::CLEAR);
    assert!(h.shows("No notifications yet."));
    h.click_id(dialogs::CANCEL);
    assert!(h.view().app.dialog.is_none());
}
