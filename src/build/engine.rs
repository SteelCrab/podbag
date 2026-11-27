//! Running a build: stages in order, each step through the cache, `RUN` in a container,
//! `COPY` and `ADD` as a planned layer, and the result as an image in the store.
//!
//! Store changes go through the store helper (`podbag __store`), as pulls do: rootless,
//! only the store's owner in its user namespace may write there.

use super::Built;
use super::cache::{self, Entry};
use super::dockerfile::{self, Dockerfile, Instruction};
use super::ignore::Ignore;
use super::plan::{self, Item, Placement, Plan, PlanEntry, PlanKind, Source};
use super::steps::{self, Action, Args};
use crate::image::auth::AuthConfig;
use crate::image::{Image, ImageConfig, manifest::HistoryEntry};
use crate::paths::Dirs;
use crate::store::ops::StoreOp;
use crate::store::tree::Tree;
use crate::store::{Committed, Store};
use crate::{Container, ContainerHandle, Error, ErrorKind, Result};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};

/// What to build, and how.
#[derive(Debug, Clone, Default)]
pub struct BuildOptions {
    /// The build context: a directory.
    pub context: PathBuf,
    /// The Dockerfile's text, and its name for messages.
    pub dockerfile: String,
    pub dockerfile_name: String,
    /// The `.dockerignore` text.
    pub ignore: String,
    /// Names to give the image (`-t`).
    pub tags: Vec<String>,
    pub build_args: BTreeMap<String, String>,
    /// The stage to build (`--target`); the last one when None.
    pub target: Option<String>,
    pub no_cache: bool,
    /// Pull base images even when stored.
    pub pull: bool,
    /// Labels for the image (`--label`).
    pub labels: BTreeMap<String, String>,
    /// The network of `RUN` steps: `default` (bridge), `bridge`, `host`, `none`, or a
    /// network's name.
    pub network: Option<String>,
    /// Only the image ID, no step output.
    pub quiet: bool,
    /// Logins for base images, by registry: the ones the API's `X-Registry-Config`
    /// header gave. A registry not here uses the stored login, as `pull` does. Kept in
    /// memory only.
    pub registry_logins: Vec<(String, AuthConfig)>,
}

/// One stage's state between steps.
#[derive(Debug, Clone)]
struct State {
    /// An image with exactly these layers (None: no layers).
    image: Option<String>,
    diff_ids: Vec<String>,
    config: ImageConfig,
    created: u64,
    history: Vec<HistoryEntry>,
}

impl State {
    fn id(&self) -> String {
        cache::state_id(&self.diff_ids, &self.config)
    }

    /// The ID the image of this state gets.
    fn image_id(&self) -> String {
        let json = crate::image::manifest::config_json(
            &self.config,
            &self.diff_ids,
            self.created,
            &self.history,
        );
        format!("sha256:{}", crate::image::registry::sha256_hex(&json))
    }

    fn from_entry(e: Entry) -> State {
        State {
            image: e.image,
            diff_ids: e.diff_ids,
            config: e.config,
            created: e.created,
            history: Vec::new(),
        }
    }

    fn entry(&self) -> Entry {
        Entry {
            image: self.image.clone(),
            diff_ids: self.diff_ids.clone(),
            config: self.config.clone(),
            created: self.created,
        }
    }
}

/// Build an image. Progress goes to `out` in Docker's words (`Step 1/4 : …`).
pub fn build(opts: &BuildOptions, out: &mut dyn FnMut(&str)) -> Result<Built> {
    let df = dockerfile::parse(&opts.dockerfile, &opts.dockerfile_name)?;
    let mut args = Args::new(&df, opts.build_args.clone())?;
    let bases = df
        .stages
        .iter()
        .map(|s| args.expand_global(&s.base, df.escape))
        .collect::<Result<Vec<_>>>()?;
    let target = match &opts.target {
        Some(t) => {
            let t = t.to_ascii_lowercase();
            df.stages
                .iter()
                .position(|s| s.name.as_deref() == Some(t.as_str()))
                .ok_or_else(|| {
                    Error::new(
                        ErrorKind::InvalidInput,
                        "cannot build the image",
                        format!("the Dockerfile has no stage named {t}"),
                        "give --target the name of a FROM ... AS <name> stage",
                    )
                })?
        }
        None => df.stages.len() - 1,
    };
    let needed = steps::needed_stages(&df, &bases, target);
    for &i in &needed {
        if let Some(p) = &df.stages[i].platform {
            let p = args.expand_global(p, df.escape)?;
            if !p.is_empty() && !steps::is_this_platform(&p) {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "cannot build the image",
                    format!(
                        "stage {} is for {p}; podbag builds for this machine only",
                        i + 1
                    ),
                    "drop --platform, or build with buildx",
                ));
            }
        }
    }
    let dirs = crate::paths::dirs()?;
    let work = dirs.data.join("builds").join(&crate::id::new_id()?[..16]);
    std::fs::create_dir_all(&work).map_err(|e| {
        Error::io(
            format!("cannot create {}", work.display()),
            &e,
            "check the podbag data directory",
        )
    })?;
    let quiet = opts.quiet;
    let mut shown = |text: &str| {
        if !quiet {
            out(text);
        }
    };
    let total = needed
        .iter()
        .map(|&i| 1 + df.stages[i].instructions.len())
        .sum();
    let result = Builder {
        opts,
        cache_dir: dirs.data.join("store").join("build-cache"),
        dirs,
        df: &df,
        args,
        bases,
        stages: vec![None; df.stages.len()],
        out: &mut shown,
        step: 0,
        total,
        ignore: Ignore::parse(&opts.ignore),
        work: work.clone(),
        lock: None,
        thawed: HashSet::new(),
    }
    .run(&needed, target);
    let _ = std::fs::remove_dir_all(&work);
    let built = result?;
    if quiet {
        out(&format!("{}\n", built.id));
    }
    Ok(built)
}

struct Builder<'a> {
    opts: &'a BuildOptions,
    dirs: Dirs,
    df: &'a Dockerfile,
    args: Args,
    /// The `FROM` words, expanded.
    bases: Vec<String>,
    /// Each built stage's final state.
    stages: Vec<Option<State>>,
    cache_dir: PathBuf,
    out: &'a mut dyn FnMut(&str),
    step: usize,
    total: usize,
    ignore: Ignore,
    /// For downloads (`ADD <url>`).
    work: PathBuf,
    /// The store's shared lock while steps run: nothing removes the images steps made.
    lock: Option<crate::store::SharedLock>,
    /// The layer lists whose compressed objects the store helper made plain in this
    /// build. The shared lock keeps them plain: freezing takes the exclusive one.
    thawed: HashSet<Vec<String>>,
}

impl Builder<'_> {
    fn say(&mut self, text: &str) {
        (self.out)(text);
    }

    fn run(&mut self, needed: &[usize], target: usize) -> Result<Built> {
        // Pulls first: a pull takes the store's exclusive lock at its end.
        for &i in needed {
            let base = self.bases[i].clone();
            if !self.is_stage_or_scratch(&base, i) {
                self.ensure_image(&base)?;
            }
            for ins in &self.df.stages[i].instructions {
                if let Some(from) = ins.flag("from")
                    && !self.is_stage_or_scratch(&from.to_ascii_lowercase(), i)
                {
                    self.ensure_image(from)?;
                }
            }
        }
        if let Some(store) = Store::open_existing(&self.dirs) {
            self.lock = Some(store.hold_shared()?);
        }
        for &i in needed {
            self.run_stage(i)?;
        }
        let mut state = self.stages[target]
            .clone()
            .expect("the target stage was built");
        state
            .config
            .labels
            .extend(self.opts.labels.iter().map(|(k, v)| (k.clone(), v.clone())));
        // Naming may remove images and enforce the store limit: it needs the lock free.
        self.lock = None;
        let id: String = store_op(
            &StoreOp::CreateImage {
                config: state.config.clone(),
                diff_ids: state.diff_ids.clone(),
                created: state.created,
                history: state.history,
                artifact: true,
            },
            None,
        )?;
        let unused = self.args.unused();
        if !unused.is_empty() {
            self.say(&format!(
                "[Warning] One or more build-args [{}] were not consumed\n",
                unused.join(" ")
            ));
        }
        self.say(&format!("Successfully built {}\n", short(&id)));
        let names: Vec<String> = if self.opts.tags.is_empty() {
            Vec::new()
        } else {
            store_op(
                &StoreOp::NameImage {
                    digest: id.clone(),
                    names: self.opts.tags.clone(),
                },
                None,
            )?
        };
        for n in &names {
            let shown = n
                .strip_prefix("docker.io/library/")
                .or_else(|| n.strip_prefix("docker.io/"))
                .unwrap_or(n)
                .to_string();
            self.say(&format!("Successfully tagged {shown}\n"));
        }
        Ok(Built { id, names })
    }

    fn is_stage_or_scratch(&self, word: &str, stage: usize) -> bool {
        let names: Vec<Option<&str>> = self.df.stages.iter().map(|s| s.name.as_deref()).collect();
        word.eq_ignore_ascii_case("scratch")
            || steps::stage_ref(&names, &word.to_ascii_lowercase(), stage).is_some()
    }

    /// Pull an image unless it is stored (and `--pull` was not given).
    fn ensure_image(&mut self, name: &str) -> Result<()> {
        if !self.opts.pull && Image::get(name).is_ok() {
            return Ok(());
        }
        // Private registries: the login given for the registry, else the stored one.
        let login = crate::image::login_for_image(name, &self.opts.registry_logins);
        self.say(&format!("Pulling {name}\n"));
        Image::pull_with_login(name, login.as_ref(), |_| {}).map(drop)
    }

    fn next_step(&mut self, text: &str) {
        self.step += 1;
        let line = format!("Step {}/{} : {}\n", self.step, self.total, one_line(text));
        self.say(&line);
    }

    fn store(&self) -> Option<Store> {
        Store::open_existing(&self.dirs)
    }

    fn run_stage(&mut self, i: usize) -> Result<()> {
        let stage = &self.df.stages[i];
        self.next_step(&stage.text);
        let base = self.bases[i].clone();
        let names: Vec<Option<&str>> = self.df.stages.iter().map(|s| s.name.as_deref()).collect();
        let mut state = if base.eq_ignore_ascii_case("scratch") {
            State {
                image: None,
                diff_ids: Vec::new(),
                config: ImageConfig::default(),
                created: 0,
                history: Vec::new(),
            }
        } else if let Some(j) = steps::stage_ref(&names, &base.to_ascii_lowercase(), i) {
            self.stages[j].clone().expect("an earlier stage was built")
        } else {
            let store = self
                .store()
                .ok_or_else(|| crate::image::not_pulled(&base))?;
            let (_, record) = store.find(&base)?;
            let history = record.history_with_layers();
            State {
                history,
                image: Some(record.digest.clone()),
                diff_ids: record.diff_ids,
                config: record.config,
                created: record.created,
            }
        };
        self.say(&format!(" ---> {}\n", short(&state.image_id())));
        let mut vars = steps::Stage::default();
        let triggers = std::mem::take(&mut state.config.onbuild);
        if !triggers.is_empty() {
            self.say(&format!("# Executing {} build triggers\n", triggers.len()));
        }
        for t in triggers {
            let ins = dockerfile::parse_line(&t, self.df.escape).map_err(|why| {
                Error::new(
                    ErrorKind::InvalidInput,
                    "cannot run an ONBUILD trigger of the base image",
                    format!("{t}: {why}"),
                    "fix the base image",
                )
            })?;
            state = self.instruction(&ins, state, &mut vars, i)?;
        }
        for ins in &self.df.stages[i].instructions {
            self.next_step(&ins.text);
            state = self.instruction(ins, state, &mut vars, i)?;
        }
        self.stages[i] = Some(state);
        Ok(())
    }

    fn instruction(
        &mut self,
        ins: &Instruction,
        mut state: State,
        vars: &mut steps::Stage,
        stage: usize,
    ) -> Result<State> {
        // Cache entries hold the result's filesystem and config. The same result may
        // come from a different Dockerfile, so take its history from this build.
        let mut history = std::mem::take(&mut state.history);
        let before = state.diff_ids.len();
        let parent = state.id();
        let mut config = state.config.clone();
        let action = steps::apply(ins, &mut config, vars, &mut self.args, self.df.escape)?;
        let mut next = match action {
            Action::Config => Ok(State { config, ..state }),
            Action::Workdir(dir) => {
                let next = State { config, ..state };
                let tree = self.tree_of(&next.diff_ids)?;
                let rel = dir.trim_start_matches('/');
                if rel.is_empty() || tree.entries.contains_key(rel) {
                    Ok(next)
                } else {
                    let mut plan = Plan::default();
                    plan.mkdir(&tree, &dir);
                    let entries = std::mem::take(&mut plan.entries);
                    let key = cache::key(&parent, &format!("WORKDIR {dir}"), &[], &plan.digest());
                    self.cached(&key, |b| b.commit_plan(&next, entries))
                }
            }
            Action::Run { argv, network } => {
                let run_args: Vec<(String, String)> = vars
                    .run_args(&state.config, &self.args)
                    .into_iter()
                    .filter(|(k, _)| !steps::PROXY_ARGS.contains(&k.as_str()))
                    .collect();
                let text = format!(
                    "RUN {} network={}",
                    serde_json::to_string(&argv).unwrap_or_default(),
                    network.as_deref().unwrap_or("")
                );
                let key = cache::key(&parent, &text, &run_args, "");
                let env = vars.run_env(&state.config, &self.args);
                self.cached(&key, |b| b.run_step(&state, argv, network, env))
            }
            Action::Copy {
                sources,
                dest,
                from,
                chown,
                chmod,
                add,
            } => {
                if add && from.is_some() {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        "cannot build the image",
                        format!("line {}: ADD does not take --from", ins.line),
                        "use COPY --from",
                    ));
                }
                let (entries, digest, thaw) = self.plan_copy(
                    &state,
                    &sources,
                    &dest,
                    from.as_deref(),
                    chown.as_deref(),
                    chmod,
                    add,
                    stage,
                )?;
                let text = format!(
                    "{} {sources:?} {dest} from={} chown={} chmod={:?}",
                    if add { "ADD" } else { "COPY" },
                    from.as_deref().unwrap_or(""),
                    chown.as_deref().unwrap_or(""),
                    chmod
                );
                let key = cache::key(&parent, &text, &[], &digest);
                self.cached(&key, |b| {
                    if let Some(diff_ids) = thaw {
                        b.thaw(&diff_ids)?;
                    }
                    b.commit_plan(&state, entries)
                })
            }
        }?;
        history.push(HistoryEntry {
            created: (next.created != 0).then(|| crate::shim::logbook::rfc3339(next.created, 0)),
            created_by: ins.text.clone(),
            empty_layer: next.diff_ids.len() == before,
            ..HistoryEntry::default()
        });
        next.history = history;
        self.say(&format!(" ---> {}\n", short(&next.image_id())));
        Ok(next)
    }

    /// A step through the cache: the stored result when it is valid, else `run`'s.
    fn cached(&mut self, key: &str, run: impl FnOnce(&mut Self) -> Result<State>) -> Result<State> {
        if !self.opts.no_cache
            && let Some(e) = cache::get(&self.cache_dir, key)
        {
            let valid = match &e.image {
                Some(d) => self.store().is_some_and(|s| s.has_image(d)),
                None => e.diff_ids.is_empty(),
            };
            if valid {
                self.say(" ---> Using cache\n");
                return Ok(State::from_entry(e));
            }
        }
        let next = run(self)?;
        cache::put(&self.cache_dir, key, &next.entry());
        Ok(next)
    }

    fn tree_of(&self, diff_ids: &[String]) -> Result<Tree> {
        if diff_ids.is_empty() {
            return Ok(Tree::default());
        }
        let store = self
            .store()
            .ok_or_else(|| crate::image::not_pulled("the base image"))?;
        store.tree_of(diff_ids)
    }

    fn run_step(
        &mut self,
        state: &State,
        argv: Vec<String>,
        network: Option<String>,
        env: Vec<(String, String)>,
    ) -> Result<State> {
        let tree_image = match &state.image {
            Some(d) => d.clone(),
            None => store_op(
                &StoreOp::CreateImage {
                    config: ImageConfig::default(),
                    diff_ids: Vec::new(),
                    created: 0,
                    history: Vec::new(),
                    artifact: true,
                },
                None,
            )?,
        };
        store_op::<serde_json::Value>(
            &StoreOp::EnsureArtifact {
                digest: tree_image.clone(),
            },
            None,
        )?;
        let image = Image {
            name: tree_image.clone(),
            digest: tree_image,
            size: 0,
            config: ImageConfig {
                env,
                workdir: state.config.workdir.clone(),
                user: state.config.user.clone(),
                ..ImageConfig::default()
            },
        };
        let mode = network_mode(network.as_deref().or(self.opts.network.as_deref()))?;
        let container = Container::from_image(&image)
            .entrypoint(argv.clone())
            .network(mode);
        let out = RefCell::new(&mut *self.out);
        let mut text = Utf8Text::default();
        let ran = container.run_for_build(
            &mut |id| (out.borrow_mut())(&format!(" ---> Running in {}\n", short(id))),
            &mut |bytes| {
                let t = text.push(bytes);
                if !t.is_empty() {
                    (out.borrow_mut())(&t);
                }
            },
        );
        let rest = text.finish();
        if !rest.is_empty() {
            (out.borrow_mut())(&rest);
        }
        let (id, code) = ran?;
        let handle = ContainerHandle::from_id(id.clone());
        let result = (|| {
            if code != 0 {
                return Err(Error::new(
                    ErrorKind::Runtime,
                    "cannot build the image",
                    format!(
                        "The command '{}' returned a non-zero code: {code}",
                        argv.join(" ")
                    ),
                    "fix the command, then build again",
                ));
            }
            let mut layer = handle.layer()?;
            let committed: Committed = store_op(
                &StoreOp::CommitLayer {
                    base: state.diff_ids.clone(),
                    config: state.config.clone(),
                    created: now(),
                },
                Some(&mut layer),
            )?;
            Ok(after_commit(state, committed))
        })();
        let _ = handle.remove(true);
        (out.borrow_mut())(&format!("Removing intermediate container {}\n", short(&id)));
        result
    }

    /// Have the store helper make the objects of the tree of `diff_ids` plain, once per
    /// build: a compressed object has no file at its path to read or link.
    fn thaw(&mut self, diff_ids: &[String]) -> Result<()> {
        if self.thawed.contains(diff_ids) {
            return Ok(());
        }
        store_op::<serde_json::Value>(
            &StoreOp::ThawLayers {
                diff_ids: diff_ids.to_vec(),
            },
            None,
        )?;
        self.thawed.insert(diff_ids.to_vec());
        Ok(())
    }

    fn commit_plan(&mut self, state: &State, entries: Vec<PlanEntry>) -> Result<State> {
        let json = serde_json::to_vec(&entries).unwrap_or_default();
        let committed: Committed = store_op(
            &StoreOp::CommitPlan {
                base: state.diff_ids.clone(),
                config: state.config.clone(),
                created: now(),
            },
            Some(&mut &json[..]),
        )?;
        Ok(after_commit(state, committed))
    }

    /// The layer entries of a `COPY` or `ADD`, the digest of what it copies, and the
    /// layers of the `--from` tree to thaw before the entries are stored: some of the
    /// files they copy are compressed.
    #[allow(clippy::too_many_arguments)]
    fn plan_copy(
        &mut self,
        state: &State,
        sources: &[String],
        dest: &str,
        from: Option<&str>,
        chown: Option<&str>,
        chmod: Option<u32>,
        add: bool,
        stage: usize,
    ) -> Result<(Vec<PlanEntry>, String, Option<Vec<String>>)> {
        let store = self.store();
        let tree = self.tree_of(&state.diff_ids)?;
        let owner = match chown {
            Some(c) => {
                let object = |path: &str| match &tree.entries.get(path)?.kind {
                    crate::store::layer::EntryKind::File { object, .. } => Some(object.clone()),
                    _ => None,
                };
                let files = [object("etc/passwd"), object("etc/group")];
                if let Some(s) = &store
                    && files.iter().flatten().any(|k| s.is_cold(k))
                {
                    self.thaw(&state.diff_ids)?;
                }
                let read = |key: &Option<String>| {
                    std::fs::read_to_string(store.as_ref()?.object_path(key.as_ref()?)).ok()
                };
                let (passwd, group) = (read(&files[0]), read(&files[1]));
                Some(steps::owner_of(c, passwd.as_deref(), group.as_deref())?)
            }
            None => None,
        };
        // Where sources come from: another stage, an image, or the context.
        let from_ids = match from {
            None => None,
            Some(f) => {
                let names: Vec<Option<&str>> =
                    self.df.stages.iter().map(|s| s.name.as_deref()).collect();
                let diff_ids = match steps::stage_ref(&names, &f.to_ascii_lowercase(), stage) {
                    Some(j) => self.stages[j].as_ref().expect("built").diff_ids.clone(),
                    None => {
                        let s = store.as_ref().ok_or_else(|| crate::image::not_pulled(f))?;
                        s.find(f)?.1.diff_ids
                    }
                };
                Some(diff_ids)
            }
        };
        let from_tree = from_ids.as_ref().map(|ids| self.tree_of(ids)).transpose()?;
        let mut downloads = BTreeMap::new();
        for s in sources {
            if add && (s.starts_with("http://") || s.starts_with("https://")) {
                let got = self.download(s)?;
                downloads.insert(s.clone(), got);
            }
        }
        // Whether a file the copy takes from the tree is compressed: looked for only until
        // one is found, and not in a tree this build has thawed.
        let thawed = from_ids
            .as_ref()
            .is_some_and(|ids| self.thawed.contains(ids));
        let cold = Cell::new(false);
        let object = |k: &str| match &store {
            Some(s) => {
                if !thawed && !cold.get() && s.is_cold(k) {
                    cold.set(true);
                }
                s.object_path(k)
            }
            None => PathBuf::new(),
        };
        let source = match &from_tree {
            Some(t) => Source::Image {
                tree: t,
                object: &object,
            },
            None => Source::Context {
                root: &self.opts.context,
                ignore: &self.ignore,
            },
        };
        let mut found: Vec<(String, Vec<Item>, bool)> = Vec::new();
        for s in sources {
            if let Some(got) = downloads.remove(s) {
                found.push(got);
                continue;
            }
            if add && (s.starts_with("git@") || s.ends_with(".git")) {
                return Err(Error::new(
                    ErrorKind::Unsupported,
                    "cannot build the image",
                    format!("ADD of a Git repository ({s}) needs BuildKit"),
                    "clone it in a RUN step, or build with buildx",
                ));
            }
            for p in source.find(s)? {
                let items = source.items(&p)?;
                let name = p.rsplit('/').next().unwrap_or(&p).to_string();
                let unpack = add
                    && from_tree.is_none()
                    && matches!(&items.first().map(|i| &i.kind), Some(PlanKind::File { from, .. })
                        if plan::packing(&plan::head_of(from)?) != plan::Packing::None);
                found.push((name, items, unpack));
            }
        }
        let at = Placement {
            dest: dest.to_string(),
            tree: &tree,
            owner,
            chmod,
            many: found.len() > 1,
        };
        let mut plan = Plan::default();
        for (name, items, unpack) in found {
            plan.place(&at, &name, items, unpack)?;
        }
        let entries = std::mem::take(&mut plan.entries);
        Ok((entries, plan.digest(), from_ids.filter(|_| cold.get())))
    }

    /// `ADD <url>`: the file, downloaded into the build's work directory.
    fn download(&mut self, url: &str) -> Result<(String, Vec<Item>, bool)> {
        let fail = |why: String| {
            Error::new(
                ErrorKind::Runtime,
                format!("cannot download {url}"),
                why,
                "check the address and the network",
            )
        };
        let name = url
            .split(['?', '#'])
            .next()
            .unwrap_or(url)
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .filter(|n| !n.is_empty() && !n.contains(':'))
            .ok_or_else(|| fail("the address names no file".to_string()))?
            .to_string();
        self.say(&format!("Downloading {url}\n"));
        let path = self.work.join(format!("download-{}", self.step));
        let response = ureq::get(url).call().map_err(|e| fail(e.to_string()))?;
        let mut body = response.into_body().into_reader();
        let mut file = std::fs::File::create(&path).map_err(|e| fail(e.to_string()))?;
        let size = std::io::copy(&mut body, &mut file).map_err(|e| fail(e.to_string()))?;
        let digest = {
            use sha2::{Digest, Sha256};
            let bytes = std::fs::read(&path).map_err(|e| fail(e.to_string()))?;
            cache::hex(&Sha256::digest(&bytes))
        };
        Ok((
            name,
            vec![Item {
                rel: String::new(),
                kind: PlanKind::File { from: path, size },
                mode: 0o600,
                uid: 0,
                gid: 0,
                xattrs: Vec::new(),
                content: digest,
            }],
            false,
        ))
    }
}

fn after_commit(state: &State, c: Committed) -> State {
    let mut diff_ids = state.diff_ids.clone();
    diff_ids.push(c.diff_id);
    State {
        image: Some(c.image),
        diff_ids,
        config: state.config.clone(),
        created: now(),
        history: Vec::new(),
    }
}

/// Run a store job and read its answer. `input` goes to its stdin.
fn store_op<T: serde::de::DeserializeOwned>(
    op: &StoreOp,
    input: Option<&mut dyn std::io::Read>,
) -> Result<T> {
    let value = match input {
        Some(i) => crate::helper::run_store_op_with_input(op, i)?,
        None => crate::helper::run_store_op(op, false)?,
    };
    serde_json::from_value(value).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "the store helper sent an unreadable result",
            e.to_string(),
            "report this bug",
        )
    })
}

fn network_mode(name: Option<&str>) -> Result<crate::network::NetworkMode> {
    match name {
        None | Some("" | "default") => Ok(crate::network::NetworkMode::default()),
        Some(n) => crate::network::parse_network_arg(n),
    }
}

/// The first 12 characters of an ID, without `sha256:`.
fn short(id: &str) -> &str {
    let hex = id.strip_prefix("sha256:").unwrap_or(id);
    &hex[..hex.len().min(12)]
}

/// An instruction on one line, as Docker shows it in `Step n/m`.
fn one_line(text: &str) -> String {
    text.lines().next().unwrap_or("").to_string()
}

fn now() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Bytes to text that never splits a character between two chunks.
#[derive(Default)]
struct Utf8Text(Vec<u8>);

impl Utf8Text {
    fn push(&mut self, bytes: &[u8]) -> String {
        self.0.extend_from_slice(bytes);
        let valid = match std::str::from_utf8(&self.0) {
            Ok(_) => self.0.len(),
            Err(e) if e.error_len().is_none() => e.valid_up_to(),
            Err(_) => self.0.len(),
        };
        let text = String::from_utf8_lossy(&self.0[..valid]).into_owned();
        self.0.drain(..valid);
        text
    }

    fn finish(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.0)).into_owned()
    }
}

/// A build context unpacked from a tar (`POST /build`, `podbag build -`): removed when
/// dropped.
pub struct UnpackedContext {
    pub dir: PathBuf,
}

impl Drop for UnpackedContext {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Unpack a context tar (plain, gzip or zstd) into a new directory of the data dir.
pub fn unpack_context(input: &mut dyn std::io::Read) -> Result<UnpackedContext> {
    let dirs = crate::paths::dirs()?;
    let dir = dirs
        .data
        .join("builds")
        .join(format!("context-{}", &crate::id::new_id()?[..16]));
    let ctx = UnpackedContext { dir: dir.clone() };
    let fail = |e: std::io::Error| {
        Error::io(
            "cannot unpack the build context",
            &e,
            "send the context as a tar archive (plain, gzip or zstd)",
        )
    };
    std::fs::create_dir_all(&dir).map_err(fail)?;
    let mut head = [0u8; 6];
    let mut got = 0;
    while got < head.len() {
        match input.read(&mut head[got..]).map_err(fail)? {
            0 => break,
            n => got += n,
        }
    }
    let chained = std::io::Read::chain(&head[..got], input);
    let reader: Box<dyn std::io::Read + '_> = match plan::packing(&head[..got]) {
        plan::Packing::Gzip => Box::new(flate2::read::MultiGzDecoder::new(chained)),
        plan::Packing::Zstd => Box::new(crate::store::zstd::ZstdFrames::new(
            std::io::BufReader::new(chained),
        )),
        plan::Packing::Bzip2 | plan::Packing::Xz => {
            return Err(Error::new(
                ErrorKind::Unsupported,
                "cannot unpack the build context",
                "it is compressed with bzip2 or xz",
                "send it plain or with gzip",
            ));
        }
        _ => Box::new(chained),
    };
    let mut archive = tar::Archive::new(reader);
    archive.set_preserve_permissions(true);
    archive.set_overwrite(true);
    archive.unpack(&dir).map_err(fail)?;
    Ok(ctx)
}

/// The Dockerfile and `.dockerignore` of a context directory: `name` relative to it.
pub fn read_dockerfile(context: &Path, name: &str) -> Result<(String, String)> {
    let inside = super::ignore::clean(&format!("/{name}"));
    let path = context.join(inside.trim_start_matches('/'));
    // Through links too, it must stay in the context.
    let outside = match (std::fs::canonicalize(&path), std::fs::canonicalize(context)) {
        (Ok(real), Ok(root)) => !real.starts_with(root),
        _ => false,
    };
    if outside {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            format!("cannot read the Dockerfile {name}"),
            "it leads out of the build context",
            "send the Dockerfile itself in the build context",
        ));
    }
    let text = std::fs::read_to_string(&path).map_err(|e| {
        Error::io(
            format!("cannot read the Dockerfile {name}"),
            &e,
            "check its name (-f) and the build context",
        )
    })?;
    Ok((text, read_ignore(context, &path)))
}

/// `<Dockerfile>.dockerignore` next to the Dockerfile, else `.dockerignore` in the context.
pub fn read_ignore(context: &Path, dockerfile: &Path) -> String {
    let beside = PathBuf::from(format!("{}.dockerignore", dockerfile.display()));
    std::fs::read_to_string(beside)
        .or_else(|_| std::fs::read_to_string(context.join(".dockerignore")))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_chunks_keep_characters_whole() {
        let mut t = Utf8Text::default();
        let bytes = "é ok".as_bytes();
        assert_eq!(t.push(&bytes[..1]), "");
        assert_eq!(t.push(&bytes[1..]), "é ok");
        assert_eq!(t.push(&[0xff, b'a']), "\u{fffd}a");
        assert_eq!(t.push(&[0xc3]), "");
        assert_eq!(t.finish(), "\u{fffd}");
    }

    #[test]
    fn dockerfiles_stay_in_the_context() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = tmp.path().join("ctx");
        std::fs::create_dir(&ctx).unwrap();
        std::fs::write(tmp.path().join("secret"), "FROM x").unwrap();
        std::fs::write(ctx.join("Dockerfile"), "FROM a").unwrap();
        std::fs::write(ctx.join(".dockerignore"), "*.log").unwrap();
        std::os::unix::fs::symlink("../secret", ctx.join("link")).unwrap();
        let (text, ignore) = read_dockerfile(&ctx, "Dockerfile").unwrap();
        assert_eq!((text.as_str(), ignore.as_str()), ("FROM a", "*.log"));
        assert!(read_dockerfile(&ctx, "link").is_err());
        assert!(read_dockerfile(&ctx, "../secret").is_err());
    }

    #[test]
    fn short_ids_and_one_line() {
        assert_eq!(short("sha256:0123456789abcdef"), "0123456789ab");
        assert_eq!(short("abc"), "abc");
        assert_eq!(one_line("RUN <<EOF\necho\nEOF"), "RUN <<EOF");
    }

    #[test]
    fn networks_for_run() {
        assert_eq!(
            network_mode(None).unwrap(),
            crate::network::NetworkMode::default()
        );
        assert_eq!(
            network_mode(Some("none")).unwrap(),
            crate::network::NetworkMode::None
        );
        assert_eq!(
            network_mode(Some("host")).unwrap(),
            crate::network::NetworkMode::Host
        );
    }
}
