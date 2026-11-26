//! The app's bitmaps: the logo, the registry icons of the pull panel and the Docker
//! import (desktop/ui/registry-icons.svg, rendered to PNG by
//! `assets/make-registry-icons.sh`), and the window and tray icons.

use std::collections::HashMap;
use std::sync::OnceLock;

use podbag_ui::ui::Ui;
use podbag_ui::{Color, Image, ImageData, Rect};

/// The logo of the empty and offline pages (192 x 192).
const LOGO: &[u8] = include_bytes!("../assets/logo.png");
/// The window icon.
const WINDOW_ICON: &[u8] = include_bytes!("../assets/icon-128.png");
/// The tray icon: a template image (black and alpha) for the macOS menu bar, the app
/// icon elsewhere.
pub const TRAY_TEMPLATE: &[u8] = include_bytes!("../assets/tray-template.png");
pub const TRAY_ICON: &[u8] = include_bytes!("../assets/icon-64.png");

macro_rules! registry {
    ($($id:literal),* $(,)?) => {
        &[$(($id, include_bytes!(concat!("../assets/registry/", $id, ".png")))),*]
    };
}

/// The registry icons, by location id.
const REGISTRY: &[(&str, &[u8])] = registry![
    "acr",
    "alibaba",
    "chainguard",
    "cloudsmith",
    "custom",
    "digitalocean",
    "dockerhub",
    "ecr",
    "gar",
    "github",
    "gitlab",
    "harbor",
    "huawei",
    "ibm",
    "jfrog",
    "kubernetes",
    "mcr",
    "nexus",
    "ngc",
    "ocir",
    "ovh",
    "quay",
    "redhat",
    "scaleway",
    "suse",
    "tencent",
];

/// Locations that show another one's mark (`<use>` in the sprite).
const SAME_AS: &[(&str, &str)] = &[
    ("ecr-public", "ecr"),
    ("dhi", "dockerhub"),
    ("gcr", "gar"),
    ("oracle", "ocir"),
    ("gitlab-self", "gitlab"),
];

/// Marks in the text color (`currentColor` in the sprite): drawn as a tinted mask.
const MONOCHROME: &[&str] = &["chainguard", "cloudsmith", "custom", "github", "nexus"];

struct Images {
    logo: Image,
    registry: HashMap<&'static str, Image>,
}

fn decode(bytes: &[u8]) -> Image {
    Image::png(bytes).expect("bundled PNG")
}

fn images() -> &'static Images {
    static IMAGES: OnceLock<Images> = OnceLock::new();
    IMAGES.get_or_init(|| Images {
        logo: decode(LOGO),
        registry: REGISTRY
            .iter()
            .map(|(id, png)| (*id, decode(png)))
            .collect(),
    })
}

/// Decode the bitmaps now rather than on first use.
pub fn install() {
    images();
}

pub fn logo() -> Image {
    images().logo
}

/// The window's icon.
pub fn window_icon() -> ImageData {
    ImageData::from_png(WINDOW_ICON).expect("bundled PNG")
}

/// A location's mark, and whether it takes the text color.
pub fn registry_icon(id: &str) -> Option<(Image, bool)> {
    let id = SAME_AS
        .iter()
        .find(|(alias, _)| *alias == id)
        .map_or(id, |(_, real)| real);
    let image = *images().registry.get(id)?;
    Some((image, MONOCHROME.contains(&id)))
}

/// Draw a location's mark into `rect` (square); `ink` colors the monochrome ones.
/// Unknown ids draw nothing.
pub fn draw_registry_icon(ui: &mut Ui, id: &str, rect: Rect, ink: Color) {
    match registry_icon(id) {
        Some((image, true)) => ui.p.image_tinted(image, rect, ink),
        Some((image, false)) => ui.p.image(image, rect),
        None => {}
    }
}

/// Draw the logo into `rect`.
pub fn draw_logo(ui: &mut Ui, rect: Rect) {
    ui.p.image(logo(), rect);
}

#[cfg(test)]
mod tests {
    use super::*;
    use podbag_desktop_core::images;

    #[test]
    fn every_pull_location_has_an_icon() {
        for location in images::locations(&[], &[]) {
            assert!(
                registry_icon(location.id).is_some(),
                "missing icon for {}",
                location.id
            );
        }
    }

    #[test]
    fn bundled_images_decode() {
        assert_eq!(logo().size(), (192, 192));
        assert_eq!(window_icon().width, 128);
        for (id, _) in REGISTRY {
            assert_eq!(registry_icon(id).unwrap().0.size(), (64, 64), "{id}");
        }
        assert!(ImageData::from_png(TRAY_TEMPLATE).is_ok());
        assert!(ImageData::from_png(TRAY_ICON).is_ok());
    }

    #[test]
    fn the_sprite_and_the_pngs_name_the_same_marks() {
        let sprite = include_str!("../../desktop/ui/registry-icons.svg");
        let mut ids: Vec<&str> = REGISTRY.iter().map(|(id, _)| *id).collect();
        ids.extend(SAME_AS.iter().map(|(alias, _)| *alias));
        ids.sort_unstable();
        let mut in_sprite: Vec<&str> = sprite
            .split("<symbol id=\"")
            .skip(1)
            .filter_map(|s| s.split('"').next())
            .collect();
        in_sprite.sort_unstable();
        assert_eq!(ids, in_sprite);
    }
}
