use serde::{Deserialize, Serialize};

/// A change to the image store, run inside the `podbag __store` helper.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum StoreOp {
    Pull {
        name: String,
        progress: bool,
        /// One JSON [`crate::image::PullEvent`] per line on stderr.
        #[serde(default)]
        events: bool,
        /// A login (JSON [`crate::image::auth::AuthConfig`]) comes on stdin: never on the
        /// command line, where other users could read it.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        auth: bool,
    },
    /// Send the image `name` to its registry.
    Push {
        name: String,
        progress: bool,
        /// One JSON [`crate::image::PushEvent`] per line on stderr.
        #[serde(default)]
        events: bool,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        auth: bool,
    },
    /// Give the image `source` the name `name` too.
    Tag {
        source: String,
        name: String,
    },
    RemoveImage {
        name: String,
    },
    Prune,
    /// Remove the images without a name that no container uses.
    PruneDangling,
    RemoveContainer {
        id: String,
    },
    RemoveVolume {
        name: String,
    },
    /// Read an image's files for `check-image` (as the owner, who can read them all).
    CheckImage {
        name: String,
    },
    /// Store the images of the archive on stdin (`docker load`).
    Load,
    /// Write the images `names` as an archive on stdout (`docker save`).
    Save {
        names: Vec<String>,
    },
}

/// Entry point of `podbag __store '<json>'`. Returns the process exit code.
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
    let auth = match &op {
        StoreOp::Pull { auth: true, .. } | StoreOp::Push { auth: true, .. } => {
            match read_auth(&mut std::io::stdin().lock()) {
                Ok(a) => a,
                Err(e) => return status.fail(e),
            }
        }
        _ => None,
    };
    match run_as_store_owner(op, auth) {
        Ok(output) => {
            status.report(&crate::helper::StatusMessage::Done { output });
            0
        }
        Err(e) => status.fail(e),
    }
}

/// Entry point of `podbag __store '<json>'`: no store helper on this system.
#[cfg(not(unix))]
pub fn helper_main(_json: &str) -> i32 {
    eprintln!("podbag: {}", crate::Error::unsupported_os());
    1
}

/// The login a job got on stdin: JSON, or nothing.
#[cfg(unix)]
fn read_auth(
    input: &mut dyn std::io::Read,
) -> crate::Result<Option<crate::image::auth::AuthConfig>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    input
        .take(64 * 1024)
        .read_to_end(&mut bytes)
        .map_err(|e| crate::Error::io("cannot read the login of a store job", &e, "try again"))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(None);
    }
    serde_json::from_slice(&bytes).map(Some).map_err(|e| {
        crate::Error::new(
            crate::ErrorKind::Internal,
            "cannot read the login of a store job",
            e.to_string(),
            "report this bug",
        )
    })
}

#[cfg(target_os = "linux")]
fn run_as_store_owner(
    op: StoreOp,
    auth: Option<crate::image::auth::AuthConfig>,
) -> crate::Result<serde_json::Value> {
    // SAFETY: geteuid has no preconditions.
    let rootless = unsafe { libc::geteuid() } != 0;
    // Decide the directories out here: inside the user namespace the user ID is 0,
    // which would select the root-mode directories.
    let dirs = crate::paths::dirs()?;
    crate::linux::userns::run_in_userns(move || linux_ops::run(op, &dirs, rootless, auth))
}

#[cfg(all(unix, not(target_os = "linux")))]
fn run_as_store_owner(
    _op: StoreOp,
    _auth: Option<crate::image::auth::AuthConfig>,
) -> crate::Result<serde_json::Value> {
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
    pub(super) fn run(
        op: StoreOp,
        dirs: &Dirs,
        rootless: bool,
        auth: Option<crate::image::auth::AuthConfig>,
    ) -> Result<Value> {
        match op {
            StoreOp::Push {
                name,
                progress,
                events,
                ..
            } => {
                Store::open_existing(dirs).ok_or_else(|| crate::image::not_pulled(&name))?;
                // Opened for writing: what the push learns about blobs is kept.
                let store = Store::open(dirs, !rootless)?;
                let mut show = |ev: crate::image::PushEvent| {
                    if progress {
                        print_push_progress(&ev);
                    }
                    if events && let Ok(line) = serde_json::to_string(&ev) {
                        eprintln!("{line}");
                    }
                };
                to_json(&store.push(&name, &mut Registry::with_auth(auth), &mut show)?)
            }
            StoreOp::Tag { source, name } => {
                to_json(
                    &Store::open(dirs, !rootless)?.tag(&source, &name, &|| images_in_use(dirs))?,
                )
            }
            StoreOp::Pull {
                name,
                progress,
                events,
                ..
            } => {
                let mut store = Store::open(dirs, !rootless)?;
                let config = config::load(dirs)?;
                store.use_mount_method(rootless, config.image_mount);
                let limit = config.store_limit;
                let in_use = || images_in_use(dirs);
                let mut show = |p: Progress| {
                    if progress {
                        print_progress(&p);
                    }
                    if events {
                        print_event(&p);
                    }
                };
                let (full, record) = store.pull(
                    &name,
                    &mut Registry::with_auth(auth),
                    limit,
                    &in_use,
                    &mut show,
                )?;
                to_json(&Image {
                    name: full,
                    digest: record.digest,
                    size: record.size,
                    config: record.config,
                })
            }
            StoreOp::Load => {
                let mut store = Store::open(dirs, !rootless)?;
                let config = config::load(dirs)?;
                store.use_mount_method(rootless, config.image_mount);
                let mut input = std::io::stdin().lock();
                to_json(&store.load(&mut input, config.store_limit, &|| images_in_use(dirs))?)
            }
            StoreOp::Save { names } => {
                let store = Store::open_existing(dirs)
                    .ok_or_else(|| crate::image::not_pulled(&names.join(" ")))?;
                // Not through std's stdout: a child that ends with _exit never flushes it.
                // SAFETY: fd 1 is this process's stdout; ManuallyDrop leaves it open.
                let mut raw = std::mem::ManuallyDrop::new(unsafe {
                    <std::fs::File as std::os::fd::FromRawFd>::from_raw_fd(1)
                });
                let mut out = std::io::BufWriter::with_capacity(256 * 1024, &mut *raw);
                store.save(&names, &mut out)?;
                std::io::Write::flush(&mut out).map_err(|e| {
                    crate::Error::io(
                        "cannot write the image archive",
                        &e,
                        "check that the reader is still there",
                    )
                })?;
                Ok(Value::Null)
            }
            StoreOp::RemoveImage { name } => to_json(
                &Store::open(dirs, !rootless)?.remove_image(&name, &|| images_in_use(dirs))?,
            ),
            StoreOp::Prune => {
                to_json(&Store::open(dirs, !rootless)?.prune(&|| images_in_use(dirs))?)
            }
            StoreOp::PruneDangling => {
                to_json(&Store::open(dirs, !rootless)?.prune_dangling(&|| images_in_use(dirs))?)
            }
            StoreOp::CheckImage { name } => {
                Store::open_existing(dirs).ok_or_else(|| crate::image::not_pulled(&name))?;
                // Opened for writing: it may build what this mount method needs.
                let mut store = Store::open(dirs, !rootless)?;
                store.use_mount_method(rootless, config::load(dirs)?.image_mount);
                let _lock = store.lock_shared()?;
                let (_, record) = store.find(&name)?;
                let ld_paths: Vec<String> = record
                    .config
                    .env
                    .iter()
                    .filter(|(k, _)| k == "LD_LIBRARY_PATH")
                    .flat_map(|(_, v)| v.split(':').map(String::from).collect::<Vec<_>>())
                    .filter(|d| d.starts_with('/'))
                    .collect();
                store.ensure_artifact(&record.digest)?;
                let check = |root: &std::path::Path| {
                    crate::libc_check::check_tree(root, crate::elf::Machine::host(), &ld_paths)
                };
                let report = if !store.uses_composefs(&record.digest) {
                    check(&store.rootfs_path(&record.digest))
                } else {
                    // Mounted privately for this job: nothing stays on the host.
                    let scratch = store.scratch_dir(&format!("check.{}", std::process::id()));
                    let report = crate::store::composefs::with_mounted(
                        &store.erofs_path(&record.digest),
                        &store.objects_dir(),
                        &scratch,
                        check,
                    );
                    let _ = remove_tree(&scratch);
                    report?
                };
                to_json(&report)
            }
            StoreOp::RemoveVolume { name } => {
                crate::volume::check_name(&name)?;
                let dir = dirs.volume_dir(&name);
                remove_tree(&dir).map_err(|e| {
                    crate::Error::io(
                        format!("cannot remove volume directory {}", dir.display()),
                        &e,
                        "remove it by hand",
                    )
                })?;
                Ok(Value::Null)
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

    /// For the API server, which reads the helper's stderr line by line.
    fn print_event(p: &Progress) {
        use crate::image::PullEvent;
        let ev = match *p {
            Progress::Resolved {
                name,
                digest,
                layers,
            } => PullEvent::Resolved {
                name: name.to_string(),
                digest: digest.to_string(),
                layers,
            },
            Progress::LayerCached { index, digest } => PullEvent::LayerCached {
                index,
                digest: digest.to_string(),
            },
            Progress::LayerDone {
                index,
                digest,
                bytes,
            } => PullEvent::LayerDone {
                index,
                digest: digest.to_string(),
                bytes,
            },
        };
        if let Ok(line) = serde_json::to_string(&ev) {
            eprintln!("{line}");
        }
    }

    /// A push the way `docker push` shows it, one line per layer as it is done.
    fn print_push_progress(ev: &crate::image::PushEvent) {
        use crate::image::PushEvent;
        match ev {
            PushEvent::Started { repository, .. } => {
                eprintln!("The push refers to repository [{repository}]")
            }
            PushEvent::LayerExists { id } => eprintln!("{id}: Layer already exists"),
            PushEvent::LayerMounted { id, from } => eprintln!("{id}: Mounted from {from}"),
            PushEvent::LayerPushed { id, bytes } => {
                eprintln!("{id}: Pushed ({})", crate::image::human_size(*bytes))
            }
            // The CLI prints the result.
            PushEvent::Done { .. } => {}
        }
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
            events: true,
            auth: false,
        };
        assert_eq!(
            serde_json::to_string(&op).unwrap(),
            r#"{"op":"pull","name":"alpine","progress":false,"events":true}"#
        );
        // A login is never in the job, only the word that it comes on stdin.
        let push = StoreOp::Push {
            name: "localhost:5000/app:1".into(),
            progress: true,
            events: false,
            auth: true,
        };
        assert_eq!(
            serde_json::to_string(&push).unwrap(),
            r#"{"op":"push","name":"localhost:5000/app:1","progress":true,"events":false,"auth":true}"#
        );
        let old: StoreOp =
            serde_json::from_str(r#"{"op":"pull","name":"alpine","progress":true}"#).unwrap();
        assert!(matches!(old, StoreOp::Pull { events: false, .. }));
        let tag = StoreOp::Tag {
            source: "alpine".into(),
            name: "mine:1".into(),
        };
        assert_eq!(
            serde_json::to_string(&tag).unwrap(),
            r#"{"op":"tag","source":"alpine","name":"mine:1"}"#
        );
        let back: StoreOp = serde_json::from_str(r#"{"op":"remove_container","id":"x"}"#).unwrap();
        assert_eq!(back, StoreOp::RemoveContainer { id: "x".into() });
    }
}
