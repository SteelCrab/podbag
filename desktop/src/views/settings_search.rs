//! Search in Settings: the settings the page shows now, by their names, their
//! section's name and other words for them, in the language shown. A closed section is
//! searched too. A result opens its section and focuses the setting.

use podbag_ui::focus::FocusKind;
use podbag_ui::ui::Ui;
use podbag_ui::widgets::button::{self, IconKind};
use podbag_ui::winit::keyboard::NamedKey;
use podbag_ui::{CursorIcon, Id, Rect, TextInput};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

use crate::app::Desktop;
use crate::icons::Icon;
use crate::views::blocks::input;
use crate::views::{compression, notices, settings, updates};
use podbag_ui::widgets::text_field::plain_field;

pub const SEARCH: Id = Id(0x7365_6172_6368);
pub const CLEAR: Id = Id(0x7365_6172_6369);

/// The button of the result `key`.
pub fn result_id(key: &str) -> Id {
    Id::of(("setting-result", key))
}

pub struct SettingsSearch {
    pub input: TextInput,
}

impl Default for SettingsSearch {
    fn default() -> Self {
        SettingsSearch {
            input: input(SEARCH, ""),
        }
    }
}

impl SettingsSearch {
    pub fn searching(&self) -> bool {
        !fold(self.input.content()).is_empty()
    }
}

/// A setting to find: its name's key, the keys of other words for it, the section it
/// is in (whose header it is, when `id` is that header) and what gets the focus.
struct Entry {
    key: &'static str,
    keywords: &'static [&'static str],
    section: Option<&'static str>,
    /// None: the control is disabled; the section's header gets the focus.
    id: Option<Id>,
}

/// A match: the setting's key, its name and its section's name ("" for none).
#[derive(Clone, Debug, PartialEq)]
pub struct Hit {
    pub key: &'static str,
    pub text: String,
    pub context: String,
}

/// Lowercase, without accents and in compatibility forms (as NFKD folds them).
pub fn fold(text: &str) -> String {
    text.nfkd()
        .filter(|c| !is_combining_mark(*c))
        .collect::<String>()
        .to_lowercase()
        .trim()
        .to_string()
}

fn entry(
    key: &'static str,
    keywords: &'static [&'static str],
    section: Option<&'static str>,
    id: Option<Id>,
) -> Entry {
    Entry {
        key,
        keywords,
        section,
        id,
    }
}

/// The settings the page shows now, in its order.
fn entries(d: &Desktop) -> Vec<Entry> {
    let section = |key: &'static str, terms: &'static [&'static str]| {
        entry(
            section_key(key),
            terms,
            Some(key),
            Some(settings::section_id(key)),
        )
    };
    let mut out = vec![
        entry(
            "settings.launchAtLogin",
            &["settings.startup", "settings.search.terms.startup"],
            None,
            Some(settings::LAUNCH),
        ),
        entry(
            "settings.language",
            &["settings.search.terms.language"],
            None,
            Some(settings::LANGUAGE),
        ),
        entry(
            "settings.theme",
            &[
                "settings.light",
                "settings.dark",
                "settings.search.terms.theme",
            ],
            None,
            Some(settings::THEME),
        ),
        section("engine", &["settings.search.terms.engine"]),
    ];
    if d.connection.local {
        let id = d.compression.enabled().then_some(compression::SWITCH);
        out.push(entry(
            "compression.label",
            &["settings.search.terms.compression"],
            Some("engine"),
            id,
        ));
    }
    if let Some(vm) = d.vm.as_ref().filter(|_| d.has_vm()) {
        out.push(section("vm", &["settings.search.terms.vm"]));
        if vm.resources_editable {
            let f = &d.vm_form;
            out.push(entry(
                "settings.cpus",
                &["settings.search.terms.cpus"],
                Some("vm"),
                Some(f.cpus.id),
            ));
            out.push(entry(
                "settings.memory",
                &["settings.search.terms.memory"],
                Some("vm"),
                Some(f.memory.id),
            ));
            out.push(entry(
                "settings.idle",
                &["settings.search.terms.idle"],
                Some("vm"),
                Some(f.idle.id),
            ));
            if vm.shared_paths.is_some() {
                out.push(entry(
                    "settings.sharedFolders",
                    &[],
                    Some("vm"),
                    Some(f.share.id),
                ));
            }
        }
        if vm.platform == "macos" {
            out.push(section("syncShares", &["settings.search.terms.syncShares"]));
        }
    }
    out.push(section("updates", &["settings.search.terms.updates"]));
    let (checking, upgrading) = (d.update.checking, d.update.upgrading);
    let newer = d.updates.as_ref().is_some_and(|u| u.newer);
    out.push(entry(
        "update.check",
        &[],
        Some("updates"),
        (!checking && !upgrading).then_some(updates::CHECK),
    ));
    out.push(entry(
        "update.install",
        &[],
        Some("updates"),
        (newer && !checking && !upgrading).then_some(updates::INSTALL),
    ));
    if let Some(c) = &d.notices.cli {
        out.push(section(
            "terminalCommand",
            &["settings.search.terms.terminalCommand"],
        ));
        if c.state == "missing" && c.problem.is_none() {
            let id = (!d.notices.settings.busy).then_some(notices::SETTINGS_INSTALL);
            out.push(entry("cli.install", &[], Some("terminalCommand"), id));
        }
    }
    out.push(section("about", &["settings.search.terms.about"]));
    out
}

/// The name's key of a section.
fn section_key(key: &'static str) -> &'static str {
    match key {
        "engine" => "settings.engine",
        "vm" => "settings.vm",
        "syncShares" => "settings.syncShares",
        "updates" => "settings.updates",
        "terminalCommand" => "settings.terminalCommand",
        _ => "settings.about",
    }
}

/// What `query` finds: every word in the name, the section's name or the other words;
/// the names that match best first.
pub fn hits(d: &Desktop, query: &str) -> Vec<Hit> {
    let query = fold(query);
    if query.is_empty() {
        return Vec::new();
    }
    let words: Vec<&str> = query.split_whitespace().collect();
    let mut found: Vec<(u8, Hit)> = Vec::new();
    for e in entries(d) {
        let text = d.t(e.key);
        let context = match e.section {
            Some(s) if section_key(s) != e.key => d.t(section_key(s)),
            _ => String::new(),
        };
        let mut haystack = vec![text.clone(), context.clone()];
        haystack.extend(e.keywords.iter().map(|k| d.t(k)));
        let haystack = fold(&haystack.join(" "));
        if !words.iter().all(|w| haystack.contains(w)) {
            continue;
        }
        let name = fold(&text);
        let rank = if name == query {
            0
        } else if name.starts_with(&query) {
            1
        } else if name.contains(&query) {
            2
        } else {
            3
        };
        found.push((
            rank,
            Hit {
                key: e.key,
                text,
                context,
            },
        ));
    }
    found.sort_by_key(|(rank, _)| *rank);
    found.into_iter().map(|(_, hit)| hit).collect()
}

/// Empty the search; the field keeps the focus when `focus`.
pub fn clear(d: &mut Desktop, focus: bool) {
    d.settings_search.input.set_content("");
    if focus {
        d.focus_next = Some(SEARCH);
    }
}

/// Show the setting `key`: the search empties, its section opens and it gets the
/// focus.
pub fn reveal(d: &mut Desktop, key: &str) {
    let Some(e) = entries(d).into_iter().find(|e| e.key == key) else {
        return;
    };
    clear(d, false);
    if let Some(section) = e.section {
        d.open_sections.insert(section);
    }
    d.focus_next = e.id.or(e.section.map(settings::section_id));
}

/// The search box and, while it has text, the results instead of the settings.
/// Returns the height.
pub fn paint(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let t = ui.theme;
    let mut y = y0;
    let label = d.t("settings.search.label");
    d.settings_search.input.set_placeholder(&label);
    // The box: an icon, the field and a clear button while it has text.
    let focused = ui.has_focus(SEARCH);
    let r = Rect::new(x, y, w, 38.);
    let border = if focused { t.accent } else { t.line };
    ui.p.bordered(r, 8., t.surface, if focused { 1.5 } else { 1. }, border);
    ui.p.icon(
        Icon::Search.id(),
        Rect::new(r.x + 12., r.y + 11., 16., 16.),
        t.muted,
    );
    let has_text = !d.settings_search.input.content().is_empty();
    let field = Rect::new(
        r.x + 36.,
        r.y,
        r.w - 36. - if has_text { 40. } else { 12. },
        r.h,
    );
    // Keys before the field: arrows move to the results, Escape clears.
    let down = has_text && ui.key_for(SEARCH, NamedKey::ArrowDown);
    let escape = has_text && ui.key_for(SEARCH, NamedKey::Escape);
    let enter = has_text && ui.key_for(SEARCH, NamedKey::Enter);
    plain_field(ui, &mut d.settings_search.input, field, None, true);
    let hits = hits(d, d.settings_search.input.content());
    if has_text {
        let clear_label = d.t("settings.search.clear");
        if button::icon_button(
            ui,
            CLEAR,
            Rect::new(r.right() - 33., r.y + 5., 28., 28.),
            Icon::Close.id(),
            &clear_label,
            IconKind::Plain,
            true,
        ) {
            clear(d, true);
            ui.repaint();
        }
    }
    y += 38.;
    if escape || down || enter {
        ui.repaint();
    }
    if escape {
        clear(d, true);
        return y - y0;
    }
    if down && let Some(first) = hits.first() {
        d.focus_next = Some(result_id(first.key));
    }
    if enter && let Some(first) = hits.first() {
        reveal(d, first.key);
        return y - y0;
    }
    if !d.settings_search.searching() {
        return y - y0;
    }
    // The results, in a card.
    y += 8.;
    let card_y = y;
    let id = Id::of("settings-search-results");
    let h = ui.mem.heights.get(&id).copied().unwrap_or(50.);
    ui.p.bordered(Rect::new(x, card_y, w, h), 10., t.surface, 1., t.line);
    let mut cy = card_y + 4.;
    if hits.is_empty() {
        let none = d.t("settings.search.noResults");
        cy += 12. + ui.paragraph(x + 16., cy + 12., w - 32., &none, ui.style(), t.ink2) + 12.;
    }
    let mut chosen = None;
    let mut focus = None;
    for (i, hit) in hits.iter().enumerate() {
        if i > 0 {
            ui.p.fill(Rect::new(x + 4., cy, w - 8., 1.), t.line);
        }
        let rid = result_id(hit.key);
        let small = ui.style().with_size(12.5);
        let text_w = w - 8. - 24. - 26.;
        let tl = ui.p.layout_wrapped(&hit.text, ui.style(), text_w);
        let cl =
            (!hit.context.is_empty()).then(|| ui.p.layout_wrapped(&hit.context, small, text_w));
        let inner = tl.height + cl.as_ref().map_or(0., |c| c.height + 2.);
        let rh = (inner + 16.).max(42.);
        let br = Rect::new(x + 4., cy, w - 8., rh);
        let lit = ui.hovered(rid) || (ui.has_focus(rid) && ui.focus_visible());
        if lit {
            ui.p.rounded(br, 6., t.hover);
        }
        let ty = br.y + (rh - inner) / 2.;
        ui.p.draw_text(&tl, br.x + 12., ty, t.ink);
        if let Some(cl) = &cl {
            ui.p.draw_text(cl, br.x + 12., ty + tl.height + 2., t.muted);
        }
        ui.p.icon(
            Icon::Forward.id(),
            Rect::new(br.right() - 26., br.y + (rh - 14.) / 2., 14., 14.),
            t.muted,
        );
        ui.p.hitbox(br, rid, CursorIcon::Pointer);
        ui.focusable(rid, FocusKind::Activate, br, 6.);
        if ui.key_for(rid, NamedKey::Escape) {
            clear(d, true);
            ui.repaint();
        } else if ui.key_for(rid, NamedKey::ArrowDown) {
            focus = Some(result_id(hits[(i + 1).min(hits.len() - 1)].key));
        } else if ui.key_for(rid, NamedKey::ArrowUp) {
            focus = Some(if i == 0 {
                SEARCH
            } else {
                result_id(hits[i - 1].key)
            });
        }
        if ui.clicked(rid) {
            chosen = Some(hit.key);
        }
        cy += rh;
    }
    let used = cy + 4. - card_y;
    if (used - h).abs() > 0.5 {
        ui.mem.heights.insert(id, used);
        ui.repaint();
    }
    if let Some(f) = focus {
        d.focus_next = Some(f);
        ui.repaint();
    }
    if let Some(key) = chosen {
        reveal(d, key);
        ui.repaint();
    }
    y + used - y0
}
