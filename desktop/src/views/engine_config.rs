//! Edit the local engine's config.toml with the same validation and stale-file check as the CLI.
use podbag_desktop_core::{ConfigDocument, ConfigEdit, EngineConfigRequest};
use podbag_ui::widgets::button::{self, Kind};
use podbag_ui::widgets::text_field::text_field;
use podbag_ui::{Id, Rect, TextInput, ui::Ui};

use crate::app::Desktop;
use crate::views::blocks::{input, spawn};

pub const OPEN: Id = Id(0x6366670001);
pub const TEXT: Id = Id(0x6366670002);
pub const CHECK: Id = Id(0x6366670003);
pub const SAVE: Id = Id(0x6366670004);
pub const CLOSE: Id = Id(0x6366670005);

pub struct Form {
    doc: Option<ConfigDocument>,
    editor: TextInput,
    busy: bool,
    status: Result<Option<&'static str>, String>,
}

impl Default for Form {
    fn default() -> Self {
        let mut editor = input(TEXT, "").multiline();
        editor.style = editor.style.mono();
        Self {
            doc: None,
            editor,
            busy: false,
            status: Ok(None),
        }
    }
}

impl Form {
    /// Keep the line endings of existing CRLF files when adding or pasting lines.
    fn contents(&self) -> String {
        if self
            .doc
            .as_ref()
            .and_then(|doc| doc.text.as_deref())
            .is_some_and(uses_crlf)
        {
            self.editor
                .content()
                .replace("\r\n", "\n")
                .replace('\n', "\r\n")
        } else {
            self.editor.content().into()
        }
    }
}

fn uses_crlf(text: &str) -> bool {
    text.contains("\r\n")
        && text
            .split_inclusive('\n')
            .all(|line| !line.ends_with('\n') || line.ends_with("\r\n"))
}

fn request(d: &mut Desktop, req: EngineConfigRequest) {
    let session = d.session;
    let saved = matches!(req, EngineConfigRequest::Save { .. });
    d.engine_config.busy = true;
    spawn(
        d,
        move |api| api.engine_config(session, req),
        move |d, result| {
            if d.session != session {
                return;
            }
            let form = &mut d.engine_config;
            form.busy = false;
            form.status = match result {
                Ok(Some(doc)) => {
                    if !saved {
                        let text = doc.text.as_deref().unwrap_or_default();
                        if uses_crlf(text) {
                            form.editor.set_content(&text.replace("\r\n", "\n"));
                        } else {
                            form.editor.set_content(text);
                        }
                        form.editor.edit.move_to(0);
                    }
                    form.doc = Some(doc);
                    Ok(saved.then_some("engineConfig.saved"))
                }
                Ok(None) => Ok(Some("engineConfig.valid")),
                Err(error) => Err(error),
            };
        },
    );
}

pub fn body(d: &mut Desktop, ui: &mut Ui, x: f32, y0: f32, w: f32) -> f32 {
    let tr = d.tr;
    let t = ui.theme;
    let mut y = y0;
    y += ui.paragraph(
        x,
        y,
        w,
        &tr.t("engineConfig.title"),
        ui.style().weight(600),
        t.ink,
    ) + 3.;
    y += ui.paragraph(x, y, w, &tr.t("engineConfig.localHint"), ui.style(), t.ink2) + 8.;
    let form = &mut d.engine_config;
    let mut action = None;
    if let Some(doc) = &form.doc {
        y += ui.paragraph(
            x,
            y,
            w,
            &doc.path.display().to_string(),
            ui.style().mono(),
            t.muted,
        ) + 6.;
        y += ui.paragraph(x, y, w, &tr.t("engineConfig.contents"), ui.style(), t.ink2) + 4.;
        if text_field(
            ui,
            &mut form.editor,
            Rect::new(x, y, w, 174.),
            None,
            !form.busy,
        ) {
            form.status = Ok(None);
        }
        y += 182.;
        let original = doc.text.as_deref().unwrap_or_default();
        let mut bx = x;
        for (id, key, kind, enabled) in [
            (
                SAVE,
                "action.save",
                Kind::Primary,
                form.contents() != original,
            ),
            (CHECK, "engineConfig.check", Kind::Normal, true),
            (CLOSE, "engineConfig.close", Kind::Normal, true),
        ] {
            let label = tr.t(key);
            let width = button::width(ui, &label, None, false);
            if button::button(
                ui,
                id,
                Rect::new(bx, y, width, 32.),
                &label,
                None,
                kind,
                enabled && !form.busy,
            ) {
                action = Some(id);
            }
            bx += width + 8.;
        }
    } else {
        let label = tr.t("engineConfig.edit");
        let width = button::width(ui, &label, None, false);
        if button::button(
            ui,
            OPEN,
            Rect::new(x, y, width, 32.),
            &label,
            None,
            Kind::Normal,
            !form.busy,
        ) {
            action = Some(OPEN);
        }
    }
    y += 38.;
    let message = if form.busy {
        Some((tr.t("engineConfig.loading"), t.ink2))
    } else {
        match &form.status {
            Ok(Some(key)) => Some((tr.t(key), t.ink2)),
            Err(message) => Some((message.clone(), t.red)),
            Ok(None) => None,
        }
    };
    if let Some((msg, color)) = message {
        y += ui.paragraph(x, y, w, &msg, ui.style(), color) + 5.;
    }
    match action {
        Some(OPEN) => request(d, EngineConfigRequest::Read),
        Some(CHECK) => {
            let text = form.contents();
            request(d, EngineConfigRequest::Check { text });
        }
        Some(SAVE) => {
            let edit = ConfigEdit {
                original: form.doc.as_ref().and_then(|doc| doc.text.clone()),
                text: form.contents(),
            };
            request(d, EngineConfigRequest::Save { edit });
        }
        Some(CLOSE) => *form = Form::default(),
        _ => {}
    }
    y - y0
}
