//! `commit`: a container's changes as a new image on top of its own.

use super::steps::{self, Action};
use crate::image::ImageConfig;
use crate::{Error, ErrorKind, Result};
#[cfg(target_os = "linux")]
use {
    super::Built,
    crate::ContainerHandle,
    crate::store::ops::StoreOp,
    crate::store::{Committed, Store},
};

/// What `commit` makes.
#[derive(Debug, Clone, Default)]
pub struct CommitOptions {
    /// The new image's name (`repo[:tag]`); None leaves it without one.
    pub name: Option<String>,
    /// Dockerfile instructions for the config (`--change`): CMD, ENTRYPOINT, ENV, EXPOSE,
    /// LABEL, ONBUILD, USER, VOLUME, WORKDIR, STOPSIGNAL.
    pub changes: Vec<String>,
}

/// The `--change` instructions Docker allows.
const CHANGES: [&str; 11] = [
    "CMD",
    "ENTRYPOINT",
    "ENV",
    "EXPOSE",
    "HEALTHCHECK",
    "LABEL",
    "ONBUILD",
    "USER",
    "VOLUME",
    "WORKDIR",
    "STOPSIGNAL",
];

/// Apply `--change` instructions to a config.
pub fn apply_changes(config: &mut ImageConfig, changes: &[String]) -> Result<()> {
    let mut stage = steps::Stage::default();
    let mut args = steps::Args::default();
    for change in changes {
        let keyword = change
            .split_whitespace()
            .next()
            .unwrap_or("")
            .to_ascii_uppercase();
        if !CHANGES.contains(&keyword.as_str()) {
            return Err(Error::new(
                ErrorKind::InvalidInput,
                "cannot change the image configuration",
                format!("{keyword} is not a configuration instruction"),
                format!("use one of {}", CHANGES.join(", ")),
            ));
        }
        let ins = super::dockerfile::parse_line(change, '\\').map_err(|why| {
            Error::new(
                ErrorKind::InvalidInput,
                "cannot change the image configuration",
                format!("{change}: {why}"),
                "fix the change",
            )
        })?;
        match steps::apply(&ins, config, &mut stage, &mut args, '\\')? {
            Action::Config | Action::Workdir(_) => {}
            _ => unreachable!("only config instructions are allowed"),
        }
    }
    Ok(())
}

/// The config of an image made from a container: its image's, with the container's
/// command, environment, user, working directory and labels.
pub fn config_of(image: &ImageConfig, spec: &crate::spec::ContainerSpec) -> ImageConfig {
    let mut config = image.clone();
    config.env = spec.env.clone();
    config.user = spec.user.clone();
    config.workdir = spec.workdir.clone();
    match spec.command.strip_prefix(image.entrypoint.as_slice()) {
        Some(rest) if !image.entrypoint.is_empty() => config.cmd = rest.to_vec(),
        _ if image.entrypoint.is_empty() => config.cmd = spec.command.clone(),
        _ => {
            config.entrypoint = Vec::new();
            config.cmd = spec.command.clone();
        }
    }
    config
        .labels
        .extend(spec.labels.iter().map(|(k, v)| (k.clone(), v.clone())));
    config
}

/// Make an image of the container's changes.
#[cfg(target_os = "linux")]
pub fn commit(handle: &ContainerHandle, opts: &CommitOptions) -> Result<Built> {
    commit_with_metadata(handle, opts, "", "")
}

/// Commit with metadata from the CLI or Engine API.
#[cfg(target_os = "linux")]
pub fn commit_with_metadata(
    handle: &ContainerHandle,
    opts: &CommitOptions,
    author: &str,
    message: &str,
) -> Result<Built> {
    let dirs = crate::paths::dirs()?;
    let spec = crate::state::read_config(&dirs, handle.id())?;
    let Some(image) = &spec.image else {
        return Err(Error::new(
            ErrorKind::Unsupported,
            format!("cannot commit container {}", spec.name),
            "it runs on a plain folder (--rootfs), not on an image",
            "copy its files with `podbag cp`",
        ));
    };
    let store = Store::open_existing(&dirs).ok_or_else(|| crate::image::not_pulled(image))?;
    let (_, record) = store.find(image)?;
    let mut config = config_of(&record.config, &spec);
    apply_changes(&mut config, &opts.changes)?;
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut layer = handle.layer()?;
    let committed: Committed = from_value(crate::helper::run_store_op_with_input(
        &StoreOp::CommitLayer {
            base: record.diff_ids.clone(),
            config: config.clone(),
            created,
        },
        &mut layer,
    )?)?;
    let mut history = record.history_with_layers();
    history.push(crate::image::manifest::HistoryEntry {
        created: Some(crate::shim::logbook::rfc3339(created, 0)),
        created_by: spec.command.join(" "),
        author: author.to_string(),
        comment: message.to_string(),
        ..Default::default()
    });
    let mut diff_ids = record.diff_ids;
    diff_ids.push(committed.diff_id);
    let id: String = from_value(crate::helper::run_store_op(
        &StoreOp::CreateImage {
            config,
            diff_ids,
            created,
            artifact: true,
            history,
        },
        false,
    )?)?;
    let names: Vec<String> = match &opts.name {
        Some(n) => from_value(crate::helper::run_store_op(
            &StoreOp::NameImage {
                digest: id.clone(),
                names: vec![n.clone()],
            },
            false,
        )?)?,
        None => Vec::new(),
    };
    Ok(Built { id, names })
}

#[cfg(target_os = "linux")]
fn from_value<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Result<T> {
    serde_json::from_value(v).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "the store helper sent an unreadable result",
            e.to_string(),
            "report this bug",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_commit_keeps_what_the_container_ran_with() {
        let image = ImageConfig {
            entrypoint: vec!["/entry".into()],
            cmd: vec!["default".into()],
            volumes: vec!["/data".into()],
            ..ImageConfig::default()
        };
        let mut spec = crate::spec::spec_fixture();
        spec.command = vec!["/entry".into(), "given".into()];
        spec.env = vec![("A".into(), "1".into())];
        spec.user = Some("app".into());
        spec.workdir = Some("/srv".into());
        spec.labels.insert("role".into(), "web".into());
        let c = config_of(&image, &spec);
        assert_eq!(c.entrypoint, ["/entry"]);
        assert_eq!(c.cmd, ["given"]);
        assert_eq!(c.env, [("A".to_string(), "1".to_string())]);
        assert_eq!(c.user.as_deref(), Some("app"));
        assert_eq!(c.workdir.as_deref(), Some("/srv"));
        assert_eq!(c.labels["role"], "web");
        assert_eq!(c.volumes, ["/data"]);
        // A command that replaced the entry point.
        spec.command = vec!["sh".into()];
        let c = config_of(&image, &spec);
        assert!(c.entrypoint.is_empty());
        assert_eq!(c.cmd, ["sh"]);
    }

    #[test]
    fn changes_set_the_config_and_refuse_others() {
        let mut c = ImageConfig::default();
        apply_changes(
            &mut c,
            &[
                "CMD [\"run\"]".into(),
                "ENV A=1".into(),
                "EXPOSE 8080".into(),
                "WORKDIR /srv".into(),
            ],
        )
        .unwrap();
        assert_eq!(c.cmd, ["run"]);
        assert_eq!(c.env, [("A".into(), "1".into())]);
        assert_eq!(c.exposed_ports, ["8080/tcp"]);
        assert_eq!(c.workdir.as_deref(), Some("/srv"));
        let e = apply_changes(&mut c, &["RUN rm -rf /".into()]).unwrap_err();
        assert!(e.why.contains("RUN"), "{e}");
    }
}
