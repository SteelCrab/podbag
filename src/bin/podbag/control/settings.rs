use super::{Result, json};
use crate::preferences::{self, Appearance};
use clap::{Subcommand, ValueEnum};
use podbag::client::{Location, podbag_program, registries};
use serde_json::{Value, json};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Startup {
    Status,
    Enable,
    Disable,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the current terminal settings as JSON
    Show,
    /// Read or set the language (system, en, ko, ja, zh-CN, fil, es, vi)
    Language { value: Option<String> },
    /// Read or set the theme (system, light, dark)
    Theme { value: Option<String> },
    /// Read or change the sidebar layout
    Sidebar {
        #[arg(long)]
        visible: Option<bool>,
        /// Width in columns, from 14 to 36
        #[arg(long)]
        width: Option<u16>,
    },
    /// Manage the Desktop app's login entry
    Startup {
        #[arg(value_enum, default_value = "status")]
        action: Startup,
    },
    /// List registry presets, accounts and login commands, or configure a preset
    Registry {
        #[command(subcommand)]
        cmd: Registry,
    },
}

#[derive(Subcommand)]
enum Registry {
    /// List registry presets and their configured hosts
    List,
    /// Show the preset's host, login account and cloud login command
    Show { preset: String },
    /// Read or save the host for a configurable preset
    Host {
        preset: String,
        host: Option<String>,
    },
}

fn place(preset: &str, host: Option<&str>, saved: &Value) -> Result<Location> {
    preferences::registry_place(preset, host, saved).ok_or_else(|| {
        format!("Unknown registry preset {preset}; see podbag settings registry list").into()
    })
}

pub(super) fn qualify_image(image: &str, preset: Option<&str>) -> Result<String> {
    if let Some(preset) = preset {
        let location = place(preset, None, &preferences::registry_hosts())?;
        if location.needs_host() && location.host().is_empty() {
            return Err(format!(
                "Configure the host with podbag settings registry host {preset} HOST"
            )
            .into());
        }
        Ok(location.qualify(image))
    } else {
        Ok(image.to_owned())
    }
}

fn registry(cmd: Registry) -> Result {
    let saved = preferences::registry_hosts();
    let logins = registries::config_dir()
        .map(|dir| registries::logins(&dir))
        .unwrap_or_default();
    let describe = |loc: &Location| {
        json!({
            "id": loc.id(), "name": loc.label(), "host": loc.host(),
            "host_example": loc.host_example(), "needs_host": loc.needs_host(),
            "account": loc.account(&logins), "login_command": loc.login_hint(),
        })
    };
    match cmd {
        Registry::List => json(
            Location::all()
                .iter()
                .map(|loc| describe(&loc.with_host(saved[loc.id()].as_str().unwrap_or(""))))
                .collect::<Vec<_>>(),
        ),
        Registry::Show { preset } => json(describe(&place(&preset, None, &saved)?)),
        Registry::Host { preset, host } => {
            let loc = place(&preset, host.as_deref(), &saved)?;
            if host.is_some() {
                preferences::save_registry_host(&loc)?;
            }
            json(json!({"preset": preset, "host": loc.host()}))
        }
    }
}

pub fn run(args: Args) -> Result {
    match args.cmd {
        Cmd::Show => json(Appearance::load()),
        Cmd::Language { value } => {
            let look = value
                .as_deref()
                .map(Appearance::set_language)
                .transpose()?
                .unwrap_or_else(Appearance::load);
            json(json!({"language": look.language}))
        }
        Cmd::Theme { value } => {
            let look = value
                .as_deref()
                .map(Appearance::set_theme)
                .transpose()?
                .unwrap_or_else(Appearance::load);
            json(json!({"theme": look.theme}))
        }
        Cmd::Sidebar { visible, width } => {
            let look = if visible.is_none() && width.is_none() {
                Appearance::load()
            } else {
                Appearance::set_sidebar(visible, width)?
            };
            json(json!({"visible": look.sidebar, "width": look.sidebar_width}))
        }
        Cmd::Startup { action } => {
            let enabled = match action {
                Startup::Status => None,
                Startup::Enable => Some(true),
                Startup::Disable => Some(false),
            };
            json(json!({"enabled": preferences::startup(&podbag_program(), enabled)?}))
        }
        Cmd::Registry { cmd } => registry(cmd),
    }
}
