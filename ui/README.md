# podbag-ui

The UI layer of Podbag Desktop. A small immediate-mode UI framework on
[winit](https://github.com/rust-windowing/winit) (windows and input),
[wgpu](https://github.com/gfx-rs/wgpu) (drawing) and
[glyphon](https://github.com/grovesNL/glyphon) / cosmic-text (text shaping).
It runs on macOS, Windows and Linux (Wayland and X11).

Try it: `cargo run -p podbag-ui --example gallery`.

## Frame loop

An app implements `ui::UiApp`. Its `ui(&mut self, ui: &mut Ui)` paints the whole
window, every frame, from the app's state:

```rust
fn ui(&mut self, ui: &mut Ui) {
    let r = ui.bounds();
    if button::button(ui, Id::of("start"), Rect::new(20., 20., 90., 32.), "Start",
                      None, Kind::Primary, true) {
        self.start();
    }
    text_field(ui, &mut self.search, Rect::new(20., 64., r.w - 40., 34.), None, true);
}
```

Widgets are functions. They draw into the frame and return what happened to them
(clicked, changed, chosen). State that must last between frames is the app's
(`TextInput`, `LogView`, `ScrollState`, `VirtualList`) or lives in `ui.mem`.

Frames are painted only when needed: after input, when the hovered widget changes,
on resize or a scale change, for a timer (`Ui::wake_at`, `Ui::repaint_at`), or when
a message comes from another thread (`Proxy::send`, then `UiApp::message`). A frame
that used input paints once more, so the change shows.

`App::new()` starts loading fonts on a thread; `App::run(options, || UiView::new(app))`
opens the window and runs the event loop.

## Ids

`Id::of(key)` hashes any key (`Id::of(("row", 3))`); `id.with(key)` makes a child id.
A widget uses its id for its hitbox, its focus and its state in `ui.mem`. Ids must be
stable from one frame to the next and unique among what is on screen.

## Layers and hit testing

Everything painted goes into one of five layers (`layer::BASE`, `POPUP`, `MODAL`,
`MODAL_POPUP`, `TOP`). Higher layers draw on top and win hit tests; in a layer,
what was painted later is on top. Input is matched against the hitboxes of the last
frame: `Ui::clicked`, `pressed`, `released`, `is_active` (dragging) and `hovered`
ask about an id. Scroll regions take wheel and trackpad input the same way.

## Focus and keyboard

Widgets register as focusable while they paint (`Ui::focusable`), so Tab and
Shift+Tab follow the paint order. A click focuses what it hits. The framework uses
some keys before the app sees them:

- Tab, Shift+Tab: move the focus. A focus ring shows when the focus came from the
  keyboard. While a dialog (the modal layer) has focusable widgets, Tab stays in it.
- Enter, Space: click the focused button, checkbox, select or radio button.
- Escape: closes the open menu. `dialog()` returns true on an Escape no widget used.
- Arrow keys: tabs, radio groups, segmented controls and sliders move their choice;
  an open select menu moves its highlight.

Other keys go to the focused text field, then to the app (`Ui::keys`,
`Ui::key_pressed`, `Ui::shortcut`). The IME is on only while a text field has the
focus; the field tells the IME where its caret is.

## Text

`TextSystem` shapes and caches text (`Paint::layout`, `layout_wrapped`). Fonts are
the platform UI font, a monospace font and CJK fallbacks (Hangul, Japanese, Chinese),
read from known files; without them it scans the system fonts. Han characters follow
the UI language (`TextSystem::set_language`).

- `Ui::label` draws one line with an ellipsis, and a tooltip with the whole text when
  it was cut. `Ui::paragraph` wraps (between words, inside a word only when it does
  not fit) and returns the height.
- `TextLayout` answers hit tests and caret positions on any line, and gives the
  rects of a selection.
- Read-only text can be selected and copied: `selectable_text` (a wrapped block),
  `selectable_line`, and `LogView` (many lines, selection across lines, wrap on or
  off). Drag, double-click for a word, triple-click for a line, Shift+click to
  extend, Cmd/Ctrl+A in the focused block, Cmd/Ctrl+C to copy. There is one
  selection per window; `Ui::selected_text` and `Ui::copy_selection` give it to the
  app.
- `TextInput` is a single-line editor with selection, word moves, clipboard and IME
  composition.

## Tooltips

`Ui::tooltip(id, anchor, text)` shows `text` after the pointer rests on `anchor` (or
the keyboard focus is on `id`) for 600 ms. Icon buttons take a label and use it.

## Lists

`VirtualList` paints only the rows in view. Rows are the same height, or each its
own (`VirtualList::variable`): those start at an estimate and are measured as they
come into view, which is how wrapped log lines get their real height.

## Tests

`cargo test -p podbag-ui` runs the unit tests and `tests/interaction.rs`. The
`testing::Harness` runs a view without a window or a GPU and feeds it `Synthetic`
input (clicks, drags, keys, IME, modifiers) through the same dispatch as a real
window. `Proxy::synthetic` feeds the same input to a real window, e.g. for demos.

## Modules

| Module | What |
| --- | --- |
| `app` | Window, event loop, GPU frame |
| `host` | Input dispatch, frames, hover (no window) |
| `ui` | `Ui`, `UiApp`, `Theme`, keyboard handling |
| `paint`, `scene` | Paint context, display list, layers, hit testing |
| `gpu`, `quad.wgsl`, `icons` | Rendering: rounded quads, text, vector icons |
| `text` | Fonts, shaping, layout queries, the editing model |
| `selection`, `focus` | Selection and focus order (no GPU) |
| `virtual_list` | Visible-range math for long lists |
| `widgets` | Buttons, checkbox, radio, select, slider, tabs, text field, dialog, toast, table, scroll view, log view, selectable text |
| `testing` | The headless harness |
