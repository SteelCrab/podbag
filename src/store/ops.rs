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
    /// Store the images from a raw tar file on stdin, without extracting a copy.
    #[cfg(target_os = "linux")]
    LoadStaged,
    /// Store a root filesystem tar on stdin as one image (`docker import`).
    Import {
        options: crate::image::ImportOptions,
    },
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
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        history: Vec<crate::image::manifest::HistoryEntry>,
    },
    /// Build what containers of the image need, if it is missing.
    EnsureArtifact {
        digest: String,
    },
    /// Make the compressed files of these layers plain, for a build that reads them.
    ThawLayers {
        diff_ids: Vec<String>,
    },
    /// Give the image these names. Answers the full names.
    NameImage {
        digest: String,
        names: Vec<String>,
    },
    /// Compress the files of these images (names or ID prefixes) now; with none, those of
    /// the images unused for `compress_idle_after`. Answers a
    /// [`crate::image::CompressReport`].
    Compress {
        images: Vec<String>,
    },
    /// Make the files of these images plain again. Answers how many images it built a
    /// tree (or EROFS image) for: those that had none for the current mount method.
    Decompress {
        images: Vec<String>,
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
    use crate::store::{FreezeBudget, FreezeSelect, Progress, Store, images_in_use, remove_tree};
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
                to_json(&store.push(&name, &mut Registry::for_engine(dirs, auth)?, &mut show)?)
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
                store.use_compression(config.image_compression, config.compress_idle_after);
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
                    &mut Registry::for_engine(dirs, auth)?,
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
            op @ (StoreOp::Load | StoreOp::LoadStaged) => {
                let mut store = Store::open(dirs, !rootless)?;
                let config = config::load(dirs)?;
                store.use_mount_method(rootless, config.image_mount);
                store.use_compression(config.image_compression, config.compress_idle_after);
                let in_use = || images_in_use(dirs);
                if matches!(op, StoreOp::LoadStaged) {
                    use std::os::fd::AsFd;
                    let input = std::io::stdin().as_fd().try_clone_to_owned().map_err(|e| {
                        crate::Error::io("cannot open the staged archive", &e, "retry the build")
                    })?;
                    to_json(&store.load_staged(input.into(), config.store_limit, &in_use)?)
                } else {
                    let mut input = crate::helper::FramedInput::new(std::io::stdin().lock());
                    to_json(&store.load(&mut input, config.store_limit, &in_use)?)
                }
            }
            StoreOp::Import { options } => {
                let mut store = Store::open(dirs, !rootless)?;
                let config = config::load(dirs)?;
                store.use_mount_method(rootless, config.image_mount);
                store.use_compression(config.image_compression, config.compress_idle_after);
                let mut input = crate::helper::FramedInput::new(std::io::stdin().lock());
                to_json(&store.import(&mut input, options, config.store_limit, &|| {
                    images_in_use(dirs)
                })?)
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
                let store = open_for_change(dirs, rootless)?;
                let mut input = crate::helper::FramedInput::new(std::io::stdin().lock());
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
                let store = open_for_change(dirs, rootless)?;
                let plan: Vec<crate::build::plan::PlanEntry> = serde_json::from_reader(
                    crate::helper::FramedInput::new(std::io::stdin().lock()),
                )
                .map_err(|e| {
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
                history,
            } => {
                let mut store = Store::open(dirs, !rootless)?;
                let settings = config::load(dirs)?;
                store.use_mount_method(rootless, settings.image_mount);
                store.use_compression(settings.image_compression, settings.compress_idle_after);
                to_json(
                    &store
                        .create_image_with_history(config, diff_ids, created, artifact, history)?,
                )
            }
            StoreOp::EnsureArtifact { digest } => {
                let mut store = Store::open(dirs, !rootless)?;
                store.use_mount_method(rootless, config::load(dirs)?.image_mount);
                let _lock = store.lock_shared()?;
                store.ensure_artifact(&digest)?;
                Ok(Value::Null)
            }
            StoreOp::ThawLayers { diff_ids } => {
                Store::open(dirs, !rootless)?.thaw_layers(&diff_ids)?;
                Ok(Value::Null)
            }
            StoreOp::NameImage { digest, names } => {
                let store = open_for_change(dirs, rootless)?;
                let limit = config::load(dirs)?.store_limit;
                to_json(&store.name_image(&digest, &names, limit, &|| images_in_use(dirs))?)
            }
            StoreOp::RemoveImage { name } => {
                let store = open_for_change(dirs, rootless)?;
                to_json(&store.remove_image(&name, &|| images_in_use(dirs))?)
            }
            StoreOp::Prune => {
                to_json(&open_for_change(dirs, rootless)?.prune(&|| images_in_use(dirs))?)
            }
            StoreOp::PruneDangling => {
                let store = open_for_change(dirs, rootless)?;
                to_json(&store.prune_dangling(&|| images_in_use(dirs))?)
            }
            StoreOp::Compress { images } => {
                let store = Store::open(dirs, !rootless)?;
                let idle_after = config::load(dirs)?.compress_idle_after;
                to_json(&compress(&store, dirs, &images, idle_after)?)
            }
            StoreOp::Decompress { images } => {
                let mut store = Store::open(dirs, !rootless)?;
                let config = config::load(dirs)?;
                store.use_mount_method(rootless, config.image_mount);
                let _lock = store.lock_shared()?;
                let mut thawed = 0u64;
                for name in &images {
                    let (_, record) = store.find(name)?;
                    if !store.has_artifact(&record.digest) {
                        store.ensure_artifact(&record.digest)?;
                        thawed += 1;
                    }
                    // Used now: an idle freeze would compress it again at once.
                    if config.image_compression != config::ImageCompression::Off {
                        let _ = store.note_use(&record.digest);
                    }
                }
                to_json(&thawed)
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

    /// The store for a job that adds, makes again or removes images, set up for compression:
    /// it notes the use of an image the build cache makes again, and freezes idle images
    /// after a removal.
    fn open_for_change(dirs: &Dirs, rootless: bool) -> Result<Store> {
        let mut store = Store::open(dirs, !rootless)?;
        // Settings that do not parse must not stop a removal: compression stays off.
        let config = config::load(dirs).unwrap_or_default();
        store.use_compression(config.image_compression, config.compress_idle_after);
        Ok(store)
    }

    /// `podbag system compress`: the images `names`, or with none the idle ones, without
    /// a budget. The names a container uses are reported, not compressed.
    fn compress(
        store: &Store,
        dirs: &Dirs,
        names: &[String],
        idle_after: u64,
    ) -> Result<crate::image::CompressReport> {
        let in_use = || images_in_use(dirs);
        let (select, skipped) = if names.is_empty() {
            let now = crate::state::now();
            let idle = FreezeSelect::Idle {
                now,
                after: idle_after,
            };
            (idle, Vec::new())
        } else {
            let find = |name: &str| Ok(store.find(name)?.1.digest);
            let (digests, skipped) = super::named_images(names, find, &in_use())?;
            (FreezeSelect::Images(digests), skipped)
        };
        let report = store.freeze_images(&select, &in_use, "", FreezeBudget::UNLIMITED)?;
        Ok(crate::image::CompressReport { report, skipped })
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

/// The digests of the images `names` names, each once, and the names a container uses
/// (`in_use`), the first name of each image only.
#[cfg(any(test, target_os = "linux"))]
fn named_images(
    names: &[String],
    digest_of: impl Fn(&str) -> crate::Result<String>,
    in_use: &std::collections::HashMap<String, String>,
) -> crate::Result<(Vec<String>, Vec<String>)> {
    let mut digests: Vec<String> = Vec::new();
    let mut skipped = Vec::new();
    for name in names {
        let digest = digest_of(name)?;
        if digests.contains(&digest) {
            continue;
        }
        if in_use.contains_key(&digest) {
            skipped.push(name.clone());
        }
        digests.push(digest);
    }
    Ok((digests, skipped))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn named_images_go_once_by_digest() {
        let names: Vec<String> = ["a", "b", "a-too"].map(String::from).to_vec();
        let digest_of = |n: &str| Ok(if n == "b" { "d2" } else { "d1" }.to_string());
        let in_use = [("d1".to_string(), "c1".to_string())].into();
        let (digests, skipped) = named_images(&names, digest_of, &in_use).unwrap();
        assert_eq!(digests, ["d1", "d2"]);
        assert_eq!(skipped, ["a"]);
    }

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
        let compress = StoreOp::Compress {
            images: vec!["alpine".into()],
        };
        assert_eq!(
            serde_json::to_string(&compress).unwrap(),
            r#"{"op":"compress","images":["alpine"]}"#
        );
        let back: StoreOp = serde_json::from_str(r#"{"op":"decompress","images":[]}"#).unwrap();
        assert_eq!(back, StoreOp::Decompress { images: Vec::new() });
        let thaw = StoreOp::ThawLayers {
            diff_ids: vec!["sha256:a".into()],
        };
        assert_eq!(
            serde_json::to_string(&thaw).unwrap(),
            r#"{"op":"thaw_layers","diff_ids":["sha256:a"]}"#
        );
    }
}
