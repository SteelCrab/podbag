//! What each Dockerfile instruction does to a stage, apart from running or copying
//! anything: variable scopes, `ARG` rules, and config changes (`ENV`, `CMD`, `LABEL`…).

use super::dockerfile::{Cmd, Dockerfile, Form, Instruction};
use super::shell;
use crate::image::ImageConfig;
use crate::{Error, ErrorKind, Result};
use std::collections::{BTreeMap, BTreeSet};

/// Build arguments that need no `ARG` to reach `RUN`, and stay out of the cache key.
pub const PROXY_ARGS: [&str; 10] = [
    "HTTP_PROXY",
    "http_proxy",
    "HTTPS_PROXY",
    "https_proxy",
    "FTP_PROXY",
    "ftp_proxy",
    "NO_PROXY",
    "no_proxy",
    "ALL_PROXY",
    "all_proxy",
];

/// The platform arguments BuildKit defines (`ARG TARGETARCH` picks one up). podbag builds
/// for this machine only.
pub fn platform_args() -> Vec<(String, String)> {
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };
    let variant = if arch == "arm64" { "v8" } else { "" };
    let platform = format!("linux/{arch}");
    let mut out = Vec::new();
    for side in ["BUILD", "TARGET"] {
        out.push((format!("{side}PLATFORM"), platform.clone()));
        out.push((format!("{side}OS"), "linux".to_string()));
        out.push((format!("{side}ARCH"), arch.to_string()));
        out.push((format!("{side}VARIANT"), variant.to_string()));
    }
    out
}

/// True for this machine's platform: `linux/arm64`, `linux/arm64/v8`.
pub fn is_this_platform(p: &str) -> bool {
    let arch = platform_args()
        .into_iter()
        .find(|(k, _)| k == "TARGETARCH")
        .map(|(_, v)| v)
        .unwrap_or_default();
    let mut parts = p.split('/');
    parts.next() == Some("linux") && parts.next() == Some(arch.as_str())
}

/// The build arguments given, and which of them a build used.
#[derive(Debug, Default)]
pub struct Args {
    pub given: BTreeMap<String, String>,
    pub used: BTreeSet<String>,
    /// Global `ARG`s (before the first `FROM`) with their values.
    pub globals: BTreeMap<String, Option<String>>,
}

impl Args {
    /// Read the global `ARG`s of a Dockerfile.
    pub fn new(df: &Dockerfile, given: BTreeMap<String, String>) -> Result<Args> {
        let mut args = Args {
            given,
            ..Args::default()
        };
        for (name, default) in &df.global_args {
            let value = args.value(name, default.as_deref(), df.escape, &|n| {
                args.globals.get(n).cloned().flatten()
            })?;
            args.globals.insert(name.clone(), value);
        }
        Ok(args)
    }

    /// The value of `ARG name[=default]`: the given build argument, else the default
    /// (expanded with `lookup`), else a platform argument.
    fn value(
        &self,
        name: &str,
        default: Option<&str>,
        escape: char,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Option<String>> {
        if let Some(v) = self.given.get(name) {
            return Ok(Some(v.clone()));
        }
        if let Some(d) = default {
            return Ok(Some(shell::expand(d, escape, lookup)?));
        }
        Ok(platform_args()
            .into_iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v))
    }

    fn mark(&mut self, name: &str) {
        if self.given.contains_key(name) {
            self.used.insert(name.to_string());
        }
    }

    /// Expand a `FROM` line's word with the global arguments.
    pub fn expand_global(&mut self, word: &str, escape: char) -> Result<String> {
        for n in shell::names_in(word, escape) {
            self.mark(&n);
        }
        let platform = platform_args();
        shell::expand(word, escape, &|n| {
            self.globals.get(n).cloned().flatten().or_else(|| {
                platform
                    .iter()
                    .find(|(k, _)| k == n)
                    .map(|(_, v)| v.clone())
            })
        })
    }

    /// Build arguments given but used by no `ARG` (Docker warns about them).
    pub fn unused(&self) -> Vec<String> {
        self.given
            .keys()
            .filter(|k| !self.used.contains(*k) && !PROXY_ARGS.contains(&k.as_str()))
            .cloned()
            .collect()
    }
}

/// A stage's variables while it builds.
#[derive(Debug, Clone, Default)]
pub struct Stage {
    /// `ARG`s declared in this stage, with their values (None: declared, no value).
    pub args: Vec<(String, Option<String>)>,
    /// The stage set `CMD` itself: a later `ENTRYPOINT` keeps it.
    pub cmd_set: bool,
}

impl Stage {
    /// A variable for expansion: `ENV` first, then this stage's `ARG`s.
    pub fn lookup(&self, config: &ImageConfig, name: &str) -> Option<String> {
        config
            .env
            .iter()
            .rev()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.clone())
            .or_else(|| {
                self.args
                    .iter()
                    .rev()
                    .find(|(k, _)| k == name)
                    .and_then(|(_, v)| v.clone())
            })
    }

    pub fn expand(&self, config: &ImageConfig, word: &str, escape: char) -> Result<String> {
        shell::expand(word, escape, &|n| self.lookup(config, n))
    }

    /// The `ARG`s with values that `ENV` does not hide: what `RUN` gets on top of `ENV`,
    /// and (without the proxies) what its cache key holds.
    pub fn run_args(&self, config: &ImageConfig, given: &Args) -> Vec<(String, String)> {
        let mut out: BTreeMap<String, String> = BTreeMap::new();
        for (k, v) in &self.args {
            if let Some(v) = v {
                out.insert(k.clone(), v.clone());
            }
        }
        for p in PROXY_ARGS {
            if let Some(v) = given.given.get(p) {
                out.entry(p.to_string()).or_insert_with(|| v.clone());
            }
        }
        out.into_iter()
            .filter(|(k, _)| !config.env.iter().any(|(e, _)| e == k))
            .collect()
    }

    /// The environment of a `RUN`: `ENV`, then the arguments.
    pub fn run_env(&self, config: &ImageConfig, given: &Args) -> Vec<(String, String)> {
        let mut env = config.env.clone();
        env.extend(self.run_args(config, given));
        env
    }
}

/// What an instruction asks for, beyond config changes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Only the config changed.
    Config,
    /// Run this command. `network`: `RUN --network`.
    Run {
        argv: Vec<String>,
        network: Option<String>,
    },
    /// Copy (or, with `add`, add) the expanded sources to the expanded destination.
    Copy {
        sources: Vec<String>,
        dest: String,
        from: Option<String>,
        chown: Option<String>,
        chmod: Option<u32>,
        add: bool,
    },
    /// Make sure this absolute directory exists.
    Workdir(String),
}

fn refuse(line: usize, what: String) -> Error {
    Error::new(
        ErrorKind::Unsupported,
        "cannot build the image",
        if line > 0 {
            format!("line {line}: {what}")
        } else {
            what
        },
        "podbag's builder has no BuildKit features; build this Dockerfile with buildx \
         (`docker buildx build`, docker-container driver)",
    )
}

fn invalid(line: usize, what: String) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        "cannot build the image",
        if line > 0 {
            format!("line {line}: {what}")
        } else {
            what
        },
        "fix the Dockerfile",
    )
}

/// The shell for shell-form commands.
pub fn shell_of(config: &ImageConfig) -> Vec<String> {
    if config.shell.is_empty() {
        vec!["/bin/sh".to_string(), "-c".to_string()]
    } else {
        config.shell.clone()
    }
}

/// A command as an argument list: exec form as is, shell form through the shell. A bare
/// heredoc (`RUN <<EOF`) is the script itself.
pub fn argv_of(form: &Form, config: &ImageConfig) -> Vec<String> {
    match form {
        Form::Exec(list) => list.clone(),
        Form::Shell(text) => {
            let script = bare_heredoc(text).unwrap_or_else(|| text.clone());
            let mut argv = shell_of(config);
            argv.push(script);
            argv
        }
    }
}

/// `<<EOF\nbody\nEOF` → `body`.
fn bare_heredoc(text: &str) -> Option<String> {
    let (first, rest) = text.split_once('\n')?;
    let marker = first.trim().strip_prefix("<<")?;
    let marker = marker.strip_prefix('-').unwrap_or(marker);
    let word = marker.trim_matches(['"', '\'']);
    if word.is_empty() || !word.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return None;
    }
    let body = rest.strip_suffix(word)?;
    Some(body.to_string())
}

/// Apply one instruction to `config`. Returns what else the step must do.
pub fn apply(
    ins: &Instruction,
    config: &mut ImageConfig,
    stage: &mut Stage,
    args: &mut Args,
    escape: char,
) -> Result<Action> {
    let line = ins.line;
    let x = |stage: &Stage, config: &ImageConfig, w: &str| stage.expand(config, w, escape);
    match &ins.cmd {
        Cmd::Env(pairs) => {
            for (k, v) in pairs {
                let k = x(stage, config, k)?;
                let v = x(stage, config, v)?;
                match config.env.iter_mut().find(|(e, _)| *e == k) {
                    Some(slot) => slot.1 = v,
                    None => config.env.push((k, v)),
                }
            }
        }
        Cmd::Label(pairs) => {
            for (k, v) in pairs {
                let k = x(stage, config, k)?;
                let v = x(stage, config, v)?;
                config.labels.insert(k, v);
            }
        }
        Cmd::Arg(list) => {
            for (name, default) in list {
                args.mark(name);
                let from_global = args.globals.get(name).cloned().flatten();
                let value = match args.value(name, default.as_deref(), escape, &|n| {
                    stage.lookup(config, n)
                })? {
                    Some(v) => Some(v),
                    None => from_global,
                };
                stage.args.push((name.clone(), value));
            }
        }
        Cmd::Expose(ports) => {
            for p in ports {
                let p = x(stage, config, p)?;
                for p in p.split_whitespace() {
                    let p = if p.contains('/') {
                        p.to_ascii_lowercase()
                    } else {
                        format!("{p}/tcp")
                    };
                    if !config.exposed_ports.contains(&p) {
                        config.exposed_ports.push(p);
                    }
                }
            }
            config.exposed_ports.sort();
        }
        Cmd::Volume(paths) => {
            for p in paths {
                let p = x(stage, config, p)?;
                if p.is_empty() {
                    return Err(invalid(line, "VOLUME needs a path".to_string()));
                }
                if !config.volumes.contains(&p) {
                    config.volumes.push(p);
                }
            }
        }
        Cmd::User(u) => config.user = Some(x(stage, config, u)?).filter(|u| !u.is_empty()),
        Cmd::Workdir(w) => {
            let w = x(stage, config, w)?;
            let base = config.workdir.clone().unwrap_or_else(|| "/".to_string());
            let joined = if w.starts_with('/') {
                w
            } else {
                format!("{base}/{w}")
            };
            let dir = super::ignore::clean(&joined);
            config.workdir = Some(dir.clone());
            return Ok(Action::Workdir(dir));
        }
        Cmd::Cmd(form) => {
            config.cmd = argv_of(form, config);
            stage.cmd_set = true;
        }
        Cmd::Entrypoint(form) => {
            config.entrypoint = argv_of(form, config);
            if !stage.cmd_set {
                config.cmd.clear();
            }
        }
        Cmd::Shell(list) => config.shell = list.clone(),
        Cmd::StopSignal(s) => config.stop_signal = Some(x(stage, config, s)?),
        Cmd::Onbuild(text) => config.onbuild.push(text.clone()),
        Cmd::Maintainer(_) => {}
        Cmd::Healthcheck(None) => config.healthcheck = Some(crate::health::HealthSettings::none()),
        Cmd::Healthcheck(Some(form)) => {
            let test = match form {
                Form::Exec(list) => [vec!["CMD".to_string()], list.clone()].concat(),
                Form::Shell(text) => vec!["CMD-SHELL".to_string(), text.clone()],
            };
            let ns = |flag: &str| -> Result<u64> {
                match ins.flag(flag) {
                    None | Some("") => Ok(0),
                    Some(v) => Ok(crate::health::parse_duration(v)?.as_nanos() as u64),
                }
            };
            let retries: u64 = match ins.flag("retries") {
                None | Some("") => 0,
                Some(v) => v
                    .parse()
                    .map_err(|_| invalid(line, format!("invalid --retries {v:?}")))?,
            };
            let json = serde_json::json!({
                "Test": test,
                "Interval": ns("interval")?,
                "Timeout": ns("timeout")?,
                "StartPeriod": ns("start-period")?,
                "StartInterval": ns("start-interval")?,
                "Retries": retries,
            });
            config.healthcheck = crate::health::HealthSettings::from_docker(&json)?;
        }
        Cmd::Run(form) => {
            if ins.flag("mount").is_some() {
                return Err(refuse(line, "RUN --mount needs BuildKit".to_string()));
            }
            if ins.flag("security").is_some() {
                return Err(refuse(line, "RUN --security needs BuildKit".to_string()));
            }
            return Ok(Action::Run {
                argv: argv_of(form, config),
                network: ins.flag("network").map(String::from),
            });
        }
        Cmd::Copy { sources, dest } | Cmd::Add { sources, dest } => {
            let add = matches!(ins.cmd, Cmd::Add { .. });
            for flag in ["parents", "exclude", "checksum", "keep-git-dir"] {
                if ins.flag(flag).is_some() {
                    return Err(refuse(line, format!("--{flag} needs BuildKit")));
                }
            }
            let mut expanded = Vec::new();
            for s in sources {
                expanded.push(x(stage, config, s)?);
            }
            let dest = x(stage, config, dest)?;
            let workdir = config.workdir.clone().unwrap_or_else(|| "/".to_string());
            let dest = if dest.starts_with('/') {
                dest
            } else {
                let slash = if dest.ends_with('/') || dest == "." || dest.is_empty() {
                    "/"
                } else {
                    ""
                };
                let joined = super::ignore::clean(&format!("{workdir}/{dest}"));
                if joined == "/" {
                    joined
                } else {
                    format!("{joined}{slash}")
                }
            };
            let chmod = match ins.flag("chmod") {
                None | Some("") => None,
                Some(m) => Some(
                    u32::from_str_radix(m, 8)
                        .ok()
                        .filter(|m| *m <= 0o7777)
                        .ok_or_else(|| invalid(line, format!("invalid --chmod {m:?}")))?,
                ),
            };
            let chown = match ins.flag("chown") {
                None | Some("") => None,
                Some(c) => Some(x(stage, config, c)?),
            };
            return Ok(Action::Copy {
                sources: expanded,
                dest,
                from: ins.flag("from").map(String::from),
                chown,
                chmod,
                add,
            });
        }
    }
    Ok(Action::Config)
}

/// Turn `--chown` (`user`, `user:group`, numbers or names) into numbers, with the image's
/// `/etc/passwd` and `/etc/group` (None when the image lacks them).
pub fn owner_of(spec: &str, passwd: Option<&str>, group: Option<&str>) -> Result<(u32, u32)> {
    let bad = |why: String| {
        Error::new(
            ErrorKind::InvalidInput,
            format!("cannot use --chown={spec}"),
            why,
            "use numbers, or names the image's /etc/passwd and /etc/group know",
        )
    };
    let find = |table: Option<&str>, name: &str, field: usize| -> Option<u32> {
        table?.lines().find_map(|l| {
            let parts: Vec<&str> = l.split(':').collect();
            (parts.first() == Some(&name))
                .then(|| parts.get(field)?.parse().ok())
                .flatten()
        })
    };
    let (user, group_name) = match spec.split_once(':') {
        Some((u, g)) => (u, Some(g)),
        None => (spec, None),
    };
    let uid = match user.parse::<u32>() {
        Ok(n) => n,
        Err(_) => {
            find(passwd, user, 2).ok_or_else(|| bad(format!("no user {user} in the image")))?
        }
    };
    let gid = match group_name {
        None => match user.parse::<u32>() {
            Ok(n) => n,
            Err(_) => find(passwd, user, 3).unwrap_or(uid),
        },
        Some(g) => match g.parse::<u32>() {
            Ok(n) => n,
            Err(_) => find(group, g, 2).ok_or_else(|| bad(format!("no group {g} in the image")))?,
        },
    };
    Ok((uid, gid))
}

/// The stages a target needs, in order: its own, the ones it starts `FROM`, and the ones
/// it copies `--from`, and theirs. `bases` are the expanded `FROM` words.
pub fn needed_stages(df: &Dockerfile, bases: &[String], target: usize) -> Vec<usize> {
    let names: Vec<Option<&str>> = df.stages.iter().map(|s| s.name.as_deref()).collect();
    let mut needed = BTreeSet::new();
    let mut todo = vec![target];
    while let Some(i) = todo.pop() {
        if !needed.insert(i) {
            continue;
        }
        let mut refs = vec![bases[i].to_ascii_lowercase()];
        for ins in &df.stages[i].instructions {
            if let Some(from) = ins.flag("from") {
                refs.push(from.to_ascii_lowercase());
            }
        }
        for r in refs {
            if let Some(j) = stage_ref(&names, &r, i) {
                todo.push(j);
            }
        }
    }
    needed.into_iter().collect()
}

/// An earlier stage named `r` (by name or number), seen from stage `i`.
pub fn stage_ref(names: &[Option<&str>], r: &str, i: usize) -> Option<usize> {
    if let Ok(n) = r.parse::<usize>() {
        return (n < i).then_some(n);
    }
    names[..i].iter().rposition(|n| *n == Some(r))
}

/// The images a build may pull, each once: the expanded `FROM` words and the `--from`
/// values that name no earlier stage and are not `scratch`.
pub fn pulled_images(df: &Dockerfile, given: BTreeMap<String, String>) -> Result<Vec<String>> {
    let mut args = Args::new(df, given)?;
    let names: Vec<Option<&str>> = df.stages.iter().map(|s| s.name.as_deref()).collect();
    let mut out: Vec<String> = Vec::new();
    for (i, stage) in df.stages.iter().enumerate() {
        let base = args.expand_global(&stage.base, df.escape)?;
        let froms = stage.instructions.iter().filter_map(|ins| ins.flag("from"));
        for word in std::iter::once(base.as_str()).chain(froms) {
            let pulled = !word.eq_ignore_ascii_case("scratch")
                && stage_ref(&names, &word.to_ascii_lowercase(), i).is_none();
            if pulled && !out.iter().any(|o| o == word) {
                out.push(word.to_string());
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build::dockerfile::{parse, parse_line};

    #[test]
    fn pulled_images_leave_out_stages_and_scratch() {
        let text = "ARG BASE=ghcr.io/team/base:1\n\
                    FROM $BASE AS build\n\
                    COPY --from=localhost:5000/tools:2 /bin/t /t\n\
                    FROM scratch AS empty\n\
                    FROM build\n\
                    COPY --from=empty / /\n\
                    COPY --from=0 / /\n\
                    COPY --from=alpine /etc/os-release /\n\
                    FROM ghcr.io/team/base:1\n";
        let df = parse(text, "Dockerfile").unwrap();
        assert_eq!(
            pulled_images(&df, BTreeMap::new()).unwrap(),
            ["ghcr.io/team/base:1", "localhost:5000/tools:2", "alpine"]
        );
        let given = BTreeMap::from([("BASE".to_string(), "quay.io/x/y".to_string())]);
        assert_eq!(pulled_images(&df, given).unwrap()[0], "quay.io/x/y");
    }

    fn run(lines: &[&str], given: &[(&str, &str)]) -> (ImageConfig, Stage, Args, Vec<Action>) {
        let text = format!("FROM base\n{}\n", lines.join("\n"));
        let df = parse(&text, "Dockerfile").unwrap();
        let given = given
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();
        let mut args = Args::new(&df, given).unwrap();
        let mut config = ImageConfig::default();
        let mut stage = Stage::default();
        let mut actions = Vec::new();
        for ins in &df.stages[0].instructions {
            actions.push(apply(ins, &mut config, &mut stage, &mut args, df.escape).unwrap());
        }
        (config, stage, args, actions)
    }

    #[test]
    fn env_label_expose_volume_user() {
        let (c, _, _, _) = run(
            &[
                "ENV A=1 B=$A-x",
                "ENV A=2",
                "LABEL v=\"$B\" k=${A}",
                "EXPOSE 80 53/UDP",
                "VOLUME /data $A",
                "USER app:$A",
                "STOPSIGNAL SIGINT",
            ],
            &[],
        );
        assert_eq!(
            c.env,
            [("A".into(), "2".into()), ("B".into(), "1-x".into())]
        );
        assert_eq!(c.labels.get("v").unwrap(), "1-x");
        assert_eq!(c.labels.get("k").unwrap(), "2");
        assert_eq!(c.exposed_ports, ["53/udp", "80/tcp"]);
        assert_eq!(c.volumes, ["/data", "2"]);
        assert_eq!(c.user.as_deref(), Some("app:2"));
        assert_eq!(c.stop_signal.as_deref(), Some("SIGINT"));
    }

    #[test]
    fn args_defaults_given_values_and_env_wins() {
        let (c, stage, args, actions) = run(
            &[
                "ARG V=1 W",
                "ARG TARGETARCH",
                "ENV E=$V",
                "ARG X=$E",
                "RUN echo",
                "ENV V=env",
            ],
            &[("W", "given"), ("UNUSED", "u"), ("HTTP_PROXY", "p")],
        );
        assert_eq!(stage.lookup(&c, "V").as_deref(), Some("env"));
        assert_eq!(stage.lookup(&c, "W").as_deref(), Some("given"));
        assert_eq!(stage.lookup(&c, "X").as_deref(), Some("1"));
        assert!(stage.lookup(&c, "TARGETARCH").is_some());
        assert_eq!(args.unused(), ["UNUSED"]);
        let run_args = stage.run_args(&c, &args);
        assert!(run_args.contains(&("W".into(), "given".into())));
        assert!(run_args.contains(&("HTTP_PROXY".into(), "p".into())));
        assert!(!run_args.iter().any(|(k, _)| k == "V"), "ENV hides the ARG");
        assert!(
            matches!(&actions[4], Action::Run { argv, .. } if argv == &["/bin/sh", "-c", "echo"])
        );
    }

    #[test]
    fn global_args_reach_from_and_arg_without_value() {
        let df = parse(
            "ARG TAG=3.19\nARG BASE\nFROM alpine:$TAG\nARG TAG\nRUN x\n",
            "Dockerfile",
        )
        .unwrap();
        let mut args = Args::new(&df, [("BASE".to_string(), "b".to_string())].into()).unwrap();
        assert_eq!(
            args.expand_global(&df.stages[0].base, df.escape).unwrap(),
            "alpine:3.19"
        );
        assert_eq!(args.expand_global("${BASE}", df.escape).unwrap(), "b");
        assert!(args.unused().is_empty());
        let mut config = ImageConfig::default();
        let mut stage = Stage::default();
        apply(
            &df.stages[0].instructions[0],
            &mut config,
            &mut stage,
            &mut args,
            '\\',
        )
        .unwrap();
        assert_eq!(stage.lookup(&config, "TAG").as_deref(), Some("3.19"));
    }

    #[test]
    fn cmd_entrypoint_shell_and_heredoc() {
        let (c, _, _, actions) = run(
            &[
                "CMD [\"a\"]",
                "ENTRYPOINT [\"e\"]",
                "SHELL [\"/bin/bash\", \"-c\"]",
                "RUN echo $HOME",
                "RUN <<EOF\necho one\necho two\nEOF",
            ],
            &[],
        );
        assert_eq!(c.cmd, ["a"], "CMD of this stage stays after ENTRYPOINT");
        assert_eq!(c.entrypoint, ["e"]);
        assert!(
            matches!(&actions[3], Action::Run { argv, .. } if argv == &["/bin/bash", "-c", "echo $HOME"])
        );
        assert!(
            matches!(&actions[4], Action::Run { argv, .. } if argv[2] == "echo one\necho two\n")
        );
        // An inherited CMD goes when ENTRYPOINT is set.
        let mut config = ImageConfig {
            cmd: vec!["old".into()],
            ..ImageConfig::default()
        };
        let ins = parse_line("ENTRYPOINT top", '\\').unwrap();
        apply(
            &ins,
            &mut config,
            &mut Stage::default(),
            &mut Args::default(),
            '\\',
        )
        .unwrap();
        assert!(config.cmd.is_empty());
        assert_eq!(config.entrypoint, ["/bin/sh", "-c", "top"]);
    }

    #[test]
    fn workdir_copy_and_healthcheck() {
        let (c, _, _, actions) = run(
            &[
                "WORKDIR /srv",
                "WORKDIR app",
                "COPY --chown=1:2 --chmod=640 a b ./",
                "ADD x.tar sub",
                "COPY --from=build /out /",
                "HEALTHCHECK --interval=5s --retries=2 CMD curl -f localhost",
            ],
            &[],
        );
        assert_eq!(actions[1], Action::Workdir("/srv/app".into()));
        assert_eq!(c.workdir.as_deref(), Some("/srv/app"));
        assert_eq!(
            actions[2],
            Action::Copy {
                sources: vec!["a".into(), "b".into()],
                dest: "/srv/app/".into(),
                from: None,
                chown: Some("1:2".into()),
                chmod: Some(0o640),
                add: false,
            }
        );
        assert!(
            matches!(&actions[3], Action::Copy { dest, add: true, .. } if dest == "/srv/app/sub")
        );
        assert!(
            matches!(&actions[4], Action::Copy { from: Some(f), dest, .. } if f == "build" && dest == "/")
        );
        let h = c.healthcheck.unwrap();
        assert_eq!(h.test, ["CMD-SHELL", "curl -f localhost"]);
        assert_eq!(h.interval_ms, Some(5000));
        assert_eq!(h.retries, Some(2));
    }

    #[test]
    fn buildkit_only_features_point_to_buildx() {
        let df = parse(
            "FROM a\nRUN --mount=type=cache,target=/c make\nCOPY --parents a /b\n",
            "Dockerfile",
        )
        .unwrap();
        for ins in &df.stages[0].instructions {
            let e = apply(
                ins,
                &mut ImageConfig::default(),
                &mut Stage::default(),
                &mut Args::default(),
                '\\',
            )
            .unwrap_err();
            assert_eq!(e.kind, ErrorKind::Unsupported);
            assert!(e.next.contains("buildx"), "{e}");
        }
    }

    #[test]
    fn owners_from_numbers_and_names() {
        let passwd = "root:x:0:0::/root:/bin/sh\napp:x:1000:1001::/home/app:/bin/sh\n";
        let group = "root:x:0:\nstaff:x:50:\n";
        assert_eq!(owner_of("5", None, None).unwrap(), (5, 5));
        assert_eq!(owner_of("5:6", None, None).unwrap(), (5, 6));
        assert_eq!(
            owner_of("app", Some(passwd), Some(group)).unwrap(),
            (1000, 1001)
        );
        assert_eq!(
            owner_of("app:staff", Some(passwd), Some(group)).unwrap(),
            (1000, 50)
        );
        assert!(owner_of("nobody", Some(passwd), Some(group)).is_err());
        assert!(owner_of("app:nogroup", Some(passwd), Some(group)).is_err());
    }

    #[test]
    fn only_needed_stages_are_built() {
        let df = parse(
            "FROM a AS one\nFROM b AS two\nRUN x\nFROM one AS three\nCOPY --from=1 /x /\n\
             FROM c AS unused\n",
            "Dockerfile",
        )
        .unwrap();
        let bases: Vec<String> = df.stages.iter().map(|s| s.base.clone()).collect();
        assert_eq!(needed_stages(&df, &bases, 2), [0, 1, 2]);
        assert_eq!(needed_stages(&df, &bases, 1), [1]);
        let names: Vec<Option<&str>> = df.stages.iter().map(|s| s.name.as_deref()).collect();
        assert_eq!(stage_ref(&names, "one", 2), Some(0));
        assert_eq!(stage_ref(&names, "three", 2), None, "not an earlier stage");
        assert_eq!(stage_ref(&names, "alpine", 2), None);
    }
}
