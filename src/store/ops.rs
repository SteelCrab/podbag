use serde::{Deserialize, Serialize};

/// A change to the image store, run inside the `openpod __store` helper.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StoreOp {
    Pull { name: String, progress: bool },
    RemoveImage { name: String },
    Prune,
    RemoveContainer { id: String },
}

/// Entry point of `openpod __store '<json>'`. Returns the process exit code.
#[cfg(unix)]
pub fn helper_main(json: &str) -> i32 {
    let mut status = crate::helper::StatusChannel::take();
    let op: StoreOp = match serde_json::from_str(json) {
        Ok(op) => op,
        Err(e) => {
            return status.fail(crate::Error::new(
                crate::ErrorKind::Internal,
                "cannot read a store job",
                e.to_string(),
                "report this bug",
            ));
        }
    };
    match run_as_store_owner(op) {
        Ok(output) => {
            status.report(&crate::helper::StatusMessage::Done { output });
            0
        }
        Err(e) => status.fail(e),
    }
}

/// Entry point of `openpod __store '<json>'`: no store helper on this system.
#[cfg(not(unix))]
pub fn helper_main(_json: &str) -> i32 {
    eprintln!("openpod: {}", crate::Error::unsupported_os());
    1
}

#[cfg(target_os = "linux")]
fn run_as_store_owner(op: StoreOp) -> crate::Result<serde_json::Value> {
    // SAFETY: geteuid has no preconditions.
    let rootless = unsafe { libc::geteuid() } != 0;
    // Decide the directories out here: inside the user namespace the user ID is 0,
    // which would select the root-mode directories.
    let dirs = crate::paths::dirs()?;
    crate::linux::userns::run_in_userns(move || linux_ops::run(op, &dirs, rootless))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn run_as_store_owner(_op: StoreOp) -> crate::Result<serde_json::Value> {
    Err(crate::Error::unsupported_os())
}

#[cfg(target_os = "linux")]
mod linux_ops {
    use super::StoreOp;
    use crate::image::Image;
    use crate::image::registry::Registry;
    use crate::paths::Dirs;
    use crate::store::{Progress, Store, images_in_use, remove_tree};
    use crate::{Result, config, id};
    use serde_json::Value;

    /// Runs as the store owner: root, or root of the user namespace when rootless.
    pub(super) fn run(op: StoreOp, dirs: &Dirs, rootless: bool) -> Result<Value> {
        match op {
            StoreOp::Pull { name, progress } => {
                let store = Store::open(dirs, !rootless)?;
                let limit = config::load(dirs)?.store_limit;
                let in_use = || images_in_use(dirs);
                let mut show = |p: Progress| {
                    if progress {
                        print_progress(&p);
                    }
                };
                let (full, record) =
                    store.pull(&name, &mut Registry::new(), limit, &in_use, &mut show)?;
                to_json(&Image {
                    name: full,
                    digest: record.digest,
                    size: record.size,
                    config: record.config,
                })
            }
            StoreOp::RemoveImage { name } => to_json(
                &Store::open(dirs, !rootless)?.remove_image(&name, &|| images_in_use(dirs))?,
            ),
            StoreOp::Prune => {
                to_json(&Store::open(dirs, !rootless)?.prune(&|| images_in_use(dirs))?)
            }
            StoreOp::RemoveContainer { id } => {
                id::check(&id)?;
                let dir = dirs.container_dir(&id);
                remove_tree(&dir).map_err(|e| {
                    crate::Error::io(
                        format!("cannot remove container directory {}", dir.display()),
                        &e,
                        "remove it by hand",
                    )
                })?;
                Ok(Value::Null)
            }
        }
    }

    fn to_json<T: serde::Serialize>(v: &T) -> Result<Value> {
        serde_json::to_value(v).map_err(|e| {
            crate::Error::new(
                crate::ErrorKind::Internal,
                "cannot encode a store result",
                e.to_string(),
                "report this bug",
            )
        })
    }

    fn print_progress(p: &Progress) {
        let short = |d: &str| {
            d.strip_prefix("sha256:")
                .unwrap_or(d)
                .chars()
                .take(12)
                .collect::<String>()
        };
        match p {
            Progress::Resolved { name, layers, .. } => {
                eprintln!("Pulling {name} ({layers} layers)")
            }
            Progress::LayerCached { index, digest } => {
                eprintln!("  layer {} {}: already exists", index + 1, short(digest))
            }
            Progress::LayerDone {
                index,
                digest,
                bytes,
            } => {
                eprintln!(
                    "  layer {} {}: done ({})",
                    index + 1,
                    short(digest),
                    crate::image::human_size(*bytes)
                )
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_op_wire_format() {
        let op = StoreOp::Pull {
            name: "alpine".into(),
            progress: false,
        };
        assert_eq!(
            serde_json::to_string(&op).unwrap(),
            r#"{"op":"pull","name":"alpine","progress":false}"#
        );
        let back: StoreOp = serde_json::from_str(r#"{"op":"remove_container","id":"x"}"#).unwrap();
        assert_eq!(back, StoreOp::RemoveContainer { id: "x".into() });
    }
}
