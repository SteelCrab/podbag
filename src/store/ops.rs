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
    /// Check every object; `fix`: remove the images that use damaged ones.
    Verify {
        fix: bool,
    },
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
    /// Store the layer tar on stdin, and an image of `base` plus it with `config`
    /// (a build step, `commit`). Answers a [`crate::store::Committed`].
    CommitLayer {
        base: Vec<String>,
        config: crate::image::ImageConfig,
        created: u64,
    },
    /// Like `CommitLayer`, with a `COPY`/`ADD` plan (JSON) on stdin instead of a tar.
    CommitPlan {
        base: Vec<String>,
        config: crate::image::ImageConfig,
        created: u64,
    },
    /// An image of stored layers; with `artifact`, ready for containers. Answers its ID.
    CreateImage {
        config: crate::image::ImageConfig,
        diff_ids: Vec<String>,
        created: u64,
        artifact: bool,
    },
    /// Build what containers of the image need, if it is missing.
    EnsureArtifact {
        digest: String,
    },
    /// Give the image these names. Answers the full names.
    NameImage {
        digest: String,
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
    match run_as_store_owner(op) {
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
            StoreOp::Tag { source, name } => {
                to_json(
                    &Store::open(dirs, !rootless)?.tag(&source, &name, &|| images_in_use(dirs))?,
                )
            }
            StoreOp::Pull {
                name,
                progress,
                events,
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
                let (full, record) =
                    store.pull(&name, &mut Registry::new(), limit, &in_use, &mut show)?;
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
            StoreOp::CommitLayer {
                base,
                config,
                created,
            } => {
                let store = Store::open(dirs, !rootless)?;
                let mut input = std::io::stdin().lock();
                to_json(&store.commit_layer(
                    &base,
                    &mut |w| crate::store::copy_all(&mut input, w, "the layer"),
                    config,
                    created,
                )?)
            }
            StoreOp::CommitPlan {
                base,
                config,
                created,
            } => {
                let store = Store::open(dirs, !rootless)?;
                let plan: Vec<crate::build::plan::PlanEntry> =
                    serde_json::from_reader(std::io::stdin().lock()).map_err(|e| {
                        crate::Error::new(
                            crate::ErrorKind::Internal,
                            "cannot read a copy plan",
                            e.to_string(),
                            "report this bug",
                        )
                    })?;
                to_json(&store.commit_layer(
                    &base,
                    &mut |w| crate::build::plan::write_plan(&plan, w),
                    config,
                    created,
                )?)
            }
            StoreOp::CreateImage {
                config,
                diff_ids,
                created,
                artifact,
            } => {
                let mut store = Store::open(dirs, !rootless)?;
                store.use_mount_method(rootless, config::load(dirs)?.image_mount);
                to_json(&store.create_image(config, diff_ids, created, artifact)?)
            }
            StoreOp::EnsureArtifact { digest } => {
                let mut store = Store::open(dirs, !rootless)?;
                store.use_mount_method(rootless, config::load(dirs)?.image_mount);
                let _lock = store.lock_shared()?;
                store.ensure_artifact(&digest)?;
                Ok(Value::Null)
            }
            StoreOp::NameImage { digest, names } => {
                let store = Store::open(dirs, !rootless)?;
                let limit = config::load(dirs)?.store_limit;
                to_json(&store.name_image(&digest, &names, limit, &|| images_in_use(dirs))?)
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
            StoreOp::Verify { fix } => {
                to_json(&Store::open(dirs, !rootless)?.verify(fix, &|| images_in_use(dirs))?)
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
        };
        assert_eq!(
            serde_json::to_string(&op).unwrap(),
            r#"{"op":"pull","name":"alpine","progress":false,"events":true}"#
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
