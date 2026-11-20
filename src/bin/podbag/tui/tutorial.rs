//! Install a lesson as data and run its exercises in the shared disposable lab.
use super::dialog::{Result, Screen};
use crate::preferences::file;
use podbag::dashboard::{
    lab::{Action, Lab},
    tutorial::{Installed, read_pack},
};
use std::path::Path;

pub fn run(ui: &mut Screen, lab: &mut Lab) -> Result<()> {
    let path = file("volume-api-tutorial.json")?;
    let mut lesson = match Installed::load(&path)? {
        Some(record) => record,
        None => {
            let lang = ui.look.lang();
            let guide = if lang == "en" {
                "https://podbag.dev/blog/".into()
            } else {
                format!("https://podbag.dev/{lang}/blog/")
            };
            while ui.pick(
                &ui.t("tutorial.chooseHelp"),
                &[ui.t("tutorial.choose"), guide.clone()],
            )? == 1
            {
                ui.message(&ui.t("nav.tutorials"), &guide)?;
            }
            let name = ui.input(&ui.t("tutorial.choose"), "", false, false)?;
            let pack = read_pack(Path::new(&name)).map_err(|e| ui.t(&e))?;
            ui.confirm(&pack.words(ui.look.lang()).title, "tutorial.install")?;
            Installed::install(&path, pack)?
        }
    };
    let lang = ui.look.lang().to_owned();
    let w = lesson.pack.words(&lang);
    let mut state = lab.run(Action::Status, None).map_err(|e| e.to_string())?;
    loop {
        if !state.active {
            let status = ui.t(if lesson.complete {
                "tutorial.complete"
            } else {
                "tutorial.installed"
            });
            let title = format!("{} · {status}", w.title);
            let index = ui.pick(
                &title,
                &[
                    ui.t("lab.start"),
                    w.goal.clone(),
                    ui.t("tutorial.uninstall"),
                ],
            )?;
            match index {
                1 => {
                    ui.message(&w.title, &w.goal)?;
                    continue;
                }
                2 => {
                    ui.confirm(&ui.t("tutorial.removeNote"), "tutorial.uninstall")?;
                    Installed::remove(&path)?;
                    return Ok(());
                }
                _ => {
                    state = lab.run(Action::Start, None).map_err(|e| e.to_string())?;
                    continue;
                }
            }
        }
        let (instruction, primary, action) = if state.volume.is_none() {
            (&w.create, "lab.create", Action::Create)
        } else if state.source.is_none() {
            (&w.write, "lab.save", Action::Save)
        } else if state.url.is_none() {
            (&w.serve, "lab.serve", Action::Serve)
        } else {
            (&w.request, "lab.send", Action::Request)
        };
        let mut entries = vec![ui.t(primary)];
        if state.source.is_some() {
            entries.push(ui.t("lab.edit"))
        }
        if let Some(reply) = &state.response {
            entries.push(format!(
                "HTTP {} · {}",
                reply.status,
                ui.t("lab.responseStep")
            ))
        }
        if let Some(url) = &state.url {
            entries.push(format!("{} · {url}", ui.t("action.copy")));
        }
        entries.push(ui.t("lab.end"));
        let i = ui.pick(instruction, &entries)?;
        let (act, value) = if i == entries.len() - 1 {
            ui.confirm(&ui.t("lab.endNote"), "lab.end")?;
            (Action::End, None)
        } else if let Some(url) = &state.url
            && i + 2 == entries.len()
        {
            ui.message(&ui.t("action.copy"), url)?;
            continue;
        } else if i == 0 {
            let v = match action {
                Action::Create => Some(ui.input(&ui.t("lab.name"), "", false, false)?),
                Action::Save => {
                    Some(ui.input("response.json", &lesson.pack.starter, false, true)?)
                }
                _ => None,
            };
            (action, v)
        } else if i == 1 {
            (
                Action::Save,
                Some(ui.input(
                    "response.json",
                    state.source.as_deref().unwrap_or(""),
                    false,
                    true,
                )?),
            )
        } else if let Some(reply) = &state.response {
            ui.message(
                state.url.as_deref().unwrap_or("HTTP"),
                &format!("HTTP {}\n{}", reply.status, reply.body),
            )?;
            continue;
        } else {
            continue;
        };
        let requested = matches!(act, Action::Request);
        state = lab.run(act, value.as_deref()).map_err(|e| e.to_string())?;
        if requested && let Some(reply) = &state.response {
            lesson.complete = true;
            Installed::record_completion(&path, &lesson.pack)?;
            ui.message(
                state.url.as_deref().unwrap_or("HTTP"),
                &format!("HTTP {}\n{}", reply.status, reply.body),
            )?;
        }
    }
}
