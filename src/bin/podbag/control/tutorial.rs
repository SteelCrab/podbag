use super::{Result, json};
use crate::preferences::{Appearance, file};
use clap::Subcommand;
use podbag::dashboard::{
    lab::{Action, Lab, Snapshot},
    tutorial::{Installed, read_pack},
};
use serde::Deserialize;
use serde_json::json;
use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Install a local .podbag module
    Install { module: PathBuf },
    /// Show the installed module, instructions and completion state
    Status,
    /// Remove the installed module and its completion record
    Remove,
    /// Create a disposable volume, serve JSON over HTTP and verify the response
    Run {
        /// Name of the practice volume
        name: String,
        /// JSON input file, or - for stdin. Defaults to the module's starter JSON
        #[arg(short, long)]
        input: Option<PathBuf>,
    },
    /// Process newline-delimited JSON actions in a single lab until stdin closes
    ///
    /// Send {"action":"start"}, {"action":"create","value":"my-api"},
    /// {"action":"save","value":"{\"hello\":true}"}, {"action":"serve"},
    /// {"action":"request"} and {"action":"end"}. Send {"action":"status"} at any time.
    /// Omitting value on save uses the module's starter JSON.
    Session,
}

#[derive(Deserialize)]
struct Request {
    action: Action,
    value: Option<String>,
}

fn action(lab: &mut Lab, action: Action, value: Option<&str>) -> Result<Snapshot> {
    Ok(lab.run(action, value).map_err(|e| e.to_string())?)
}

fn status(record: Option<Installed>) -> Result {
    if let Some(record) = record {
        let appearance = Appearance::load();
        json(json!({
            "installed": true, "complete": record.complete, "language": appearance.lang(),
            "instructions": record.pack.words(appearance.lang())
        }))
    } else {
        json(json!({"installed": false, "complete": false}))
    }
}

fn installed(path: &Path) -> Result<Installed> {
    Installed::load(path)?
        .ok_or_else(|| "Install a module first with podbag tutorial install".into())
}

fn input(path: &Path) -> Result<String> {
    let mut value = String::new();
    let read = |source: &mut dyn Read, value: &mut String| {
        source.take(32 * 1024 + 1).read_to_string(value)
    };
    if path == Path::new("-") {
        read(&mut io::stdin().lock(), &mut value)?;
    } else {
        read(&mut std::fs::File::open(path)?, &mut value)?;
    }
    if value.len() > 32 * 1024 {
        return Err("Use 32 KiB or less.".into());
    }
    Ok(value)
}

fn session(path: &Path, lesson: Installed) -> Result {
    let mut lab = Lab::default();
    let handle = |lab: &mut Lab, line: &str| -> Result<Snapshot> {
        let req: Request = serde_json::from_str(line)?;
        let requested = matches!(req.action, Action::Request);
        let value = if matches!(req.action, Action::Save) {
            req.value.as_deref().or(Some(&lesson.pack.starter))
        } else {
            req.value.as_deref()
        };
        let snapshot = action(lab, req.action, value)?;
        if requested && snapshot.response.is_some() {
            Installed::record_completion(path, &lesson.pack)?;
        }
        Ok(snapshot)
    };
    for line in io::stdin().lock().lines() {
        let line = line?;
        match handle(&mut lab, &line) {
            Ok(state) => json(json!({"ok": true, "state": state}))?,
            Err(e) => json(json!({"ok": false, "error": e.to_string()}))?,
        }
        io::stdout().lock().flush()?;
    }
    Ok(())
}

pub fn run(args: Args) -> Result {
    let path = file("volume-api-tutorial.json")?;
    match args.cmd {
        Cmd::Install { module } => {
            let pack = read_pack(&module).map_err(|e| {
                if e == "tutorial.invalid" {
                    format!("Invalid tutorial module {}", module.display())
                } else {
                    e
                }
            })?;
            status(Some(Installed::install(&path, pack)?))
        }
        Cmd::Status => status(Installed::load(&path)?),
        Cmd::Remove => {
            Installed::remove(&path)?;
            status(None)
        }
        Cmd::Run {
            name,
            input: path_input,
        } => {
            let lesson = installed(&path)?;
            let mut lab = Lab::default();
            let data = path_input.as_deref().map(input).transpose()?;
            action(&mut lab, Action::Start, None)?;
            action(&mut lab, Action::Create, Some(&name))?;
            action(
                &mut lab,
                Action::Save,
                Some(data.as_deref().unwrap_or(&lesson.pack.starter)),
            )?;
            action(&mut lab, Action::Serve, None)?;
            let result = action(&mut lab, Action::Request, None)?;
            Installed::record_completion(&path, &lesson.pack)?;
            json(json!({"volume": result.volume, "response": result.response, "complete": true}))
        }
        Cmd::Session => session(&path, installed(&path)?),
    }
}
