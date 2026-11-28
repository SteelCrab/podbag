//! The UI layer of Podbag Desktop: a small immediate-mode UI framework on winit
//! (windows and input), wgpu (drawing) and glyphon / cosmic-text (text).
//!
//! An app implements `ui::UiApp`: its `ui()` paints the whole window every frame
//! with the `widgets`, and widgets report clicks, keys and drags as they paint.
//! `App::run` opens the window and runs the event loop. See README.md.

mod app;
pub mod focus;
mod geom;
mod gpu;
mod host;
pub mod icons;
pub mod image;
mod image_gpu;
mod input;
#[cfg(target_os = "macos")]
mod macos;
mod paint;
mod scene;
pub mod selection;
pub mod testing;
mod text;
mod text_input;
pub mod ui;
mod virtual_list;
pub mod widgets;

pub use app::{App, Proxy, WindowOptions};
pub use focus::FocusKind;
pub use geom::{Color, Point, Rect, rgb, rgba};
pub use host::{Cx, Event, NavButton, Synthetic, View};
pub use image::{Image, ImageData};
pub use input::{Clipboard, KeyPress, ScrollDelta, shortcut_letter, shortcut_modifier};
pub use paint::Paint;
pub use scene::{Id, Scene, layer};
pub use text::{
    EditBuffer, Fonts, LINE_HEIGHT, TextLayout, TextStyle, TextSystem, load_fonts, split_lines,
};
pub use text_input::TextInput;
pub use virtual_list::VirtualList;
pub use winit;
pub use winit::keyboard::{ModifiersState, NamedKey};
pub use winit::window::CursorIcon;
