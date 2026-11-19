//! Reading a Dockerfile: parser directives, comments, line continuations, heredocs for
//! `RUN <<EOF`, instruction flags, and JSON (exec) or shell forms. Variables are not
//! expanded here: that happens when a step runs, with the values it has then.

use crate::{Error, ErrorKind, Result};

/// A parsed Dockerfile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Dockerfile {
    /// The escape character (`# escape=`): `\` or `` ` ``.
    pub escape: char,
    /// `ARG`s before the first `FROM`: usable in `FROM` lines.
    pub global_args: Vec<(String, Option<String>)>,
    pub stages: Vec<Stage>,
}

/// One `FROM` and the instructions after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stage {
    /// The base as written: an image, `scratch`, or an earlier stage's name.
    pub base: String,
    /// `AS name`, in lower case.
    pub name: Option<String>,
    pub platform: Option<String>,
    pub line: usize,
    /// The `FROM` line as written (for progress output).
    pub text: String,
    pub instructions: Vec<Instruction>,
}

/// One instruction after `FROM`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Instruction {
    pub line: usize,
    /// The instruction as written, continuations joined (for progress output).
    pub text: String,
    pub flags: Vec<Flag>,
    pub cmd: Cmd,
}

/// `--name=value` (or `--name` alone) before an instruction's arguments.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Flag {
    pub name: String,
    pub value: Option<String>,
}

/// A command in exec form (`["a", "b"]`) or shell form (`a b`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Form {
    Exec(Vec<String>),
    Shell(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cmd {
    Run(Form),
    Cmd(Form),
    Entrypoint(Form),
    /// Pairs of unexpanded words.
    Env(Vec<(String, String)>),
    Label(Vec<(String, String)>),
    /// Unexpanded words.
    Expose(Vec<String>),
    Volume(Vec<String>),
    User(String),
    Workdir(String),
    Arg(Vec<(String, Option<String>)>),
    /// Sources and the destination, unexpanded.
    Copy {
        sources: Vec<String>,
        dest: String,
    },
    Add {
        sources: Vec<String>,
        dest: String,
    },
    /// The instruction to run in images built on this one, as written.
    Onbuild(String),
    StopSignal(String),
    /// None: `HEALTHCHECK NONE`.
    Healthcheck(Option<Form>),
    Shell(Vec<String>),
    Maintainer(String),
}

impl Instruction {
    /// The value of flag `name`, if given.
    pub fn flag(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .rev()
            .find(|f| f.name == name)
            .map(|f| f.value.as_deref().unwrap_or(""))
    }
}

/// Which flags each instruction takes; others are refused with the line number.
fn allowed_flags(keyword: &str) -> &'static [&'static str] {
    match keyword {
        "FROM" => &["platform"],
        "RUN" => &["network", "mount", "security"],
        "COPY" => &["from", "chown", "chmod", "link", "parents", "exclude"],
        "ADD" => &[
            "chown",
            "chmod",
            "link",
            "checksum",
            "keep-git-dir",
            "exclude",
        ],
        "HEALTHCHECK" => &[
            "interval",
            "timeout",
            "start-period",
            "start-interval",
            "retries",
        ],
        _ => &[],
    }
}

/// Parse a Dockerfile. Errors name the file and the line.
pub fn parse(text: &str, file: &str) -> Result<Dockerfile> {
    let fail = |line: usize, why: String| {
        Error::new(
            ErrorKind::InvalidInput,
            "cannot read the Dockerfile",
            format!("{file}:{line}: {why}"),
            "fix the Dockerfile",
        )
    };
    let lines: Vec<&str> = text.lines().collect();
    let (escape, first) = directives(&lines).map_err(|(n, why)| fail(n, why))?;
    let logical = join_lines(&lines, first, escape).map_err(|(n, why)| fail(n, why))?;

    let mut df = Dockerfile {
        escape,
        global_args: Vec::new(),
        stages: Vec::new(),
    };
    for (line, raw) in logical {
        let parsed = parse_instruction(&raw, escape).map_err(|why| fail(line, why))?;
        let (keyword, flags, rest) = parsed;
        if keyword == "FROM" {
            let stage = from_stage(&rest, &flags, line, &raw).map_err(|why| fail(line, why))?;
            df.stages.push(stage);
            continue;
        }
        let cmd = command(&keyword, &rest, escape).map_err(|why| fail(line, why))?;
        match df.stages.last_mut() {
            Some(stage) => stage.instructions.push(Instruction {
                line,
                text: raw,
                flags,
                cmd,
            }),
            None => match cmd {
                Cmd::Arg(args) => df.global_args.extend(args),
                _ => {
                    return Err(fail(
                        line,
                        format!("{keyword} comes before the first FROM; only ARG may"),
                    ));
                }
            },
        }
    }
    if df.stages.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "cannot read the Dockerfile",
            format!("{file} has no FROM instruction"),
            "start the Dockerfile with FROM <image>",
        ));
    }
    Ok(df)
}

/// Parse one instruction line (also the text of an `ONBUILD` trigger).
pub fn parse_line(raw: &str, escape: char) -> std::result::Result<Instruction, String> {
    let (keyword, flags, rest) = parse_instruction(raw, escape)?;
    if keyword == "FROM" {
        return Err("FROM is not allowed here".to_string());
    }
    Ok(Instruction {
        line: 0,
        text: raw.to_string(),
        flags,
        cmd: command(&keyword, &rest, escape)?,
    })
}

/// Parser directives at the top: `# escape=`, `# syntax=` (ignored), `# check=`
/// (ignored). Returns the escape character and the index of the first other line.
fn directives(lines: &[&str]) -> std::result::Result<(char, usize), (usize, String)> {
    let mut escape = '\\';
    let mut seen = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        let Some(rest) = line.trim_start().strip_prefix('#') else {
            return Ok((escape, i));
        };
        let Some((key, value)) = rest.split_once('=') else {
            return Ok((escape, i));
        };
        let key = key.trim().to_ascii_lowercase();
        if key.is_empty() || key.contains(char::is_whitespace) {
            return Ok((escape, i));
        }
        if seen.contains(&key) {
            return Err((i + 1, format!("the directive {key} is given twice")));
        }
        match key.as_str() {
            "escape" => {
                escape = match value.trim() {
                    "\\" => '\\',
                    "`" => '`',
                    other => {
                        return Err((
                            i + 1,
                            format!("invalid escape character {other:?}: use \\ or `"),
                        ));
                    }
                }
            }
            "syntax" | "check" => {}
            // An unknown directive is a comment, and ends the directives.
            _ => return Ok((escape, i)),
        }
        seen.push(key);
    }
    Ok((escape, lines.len()))
}

/// Join continued lines, drop comments and empty lines, and read heredocs. Each result
/// has the number of its first line.
fn join_lines(
    lines: &[&str],
    first: usize,
    escape: char,
) -> std::result::Result<Vec<(usize, String)>, (usize, String)> {
    let mut out = Vec::new();
    let mut i = first;
    while i < lines.len() {
        let start = i;
        let line = lines[i];
        i += 1;
        let trimmed = line.trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }
        let mut text = String::new();
        let mut current = trimmed.to_string();
        loop {
            let end = current.trim_end();
            if let Some(body) = end.strip_suffix(escape) {
                text.push_str(body);
                // Continued: skip comment and empty lines in between.
                loop {
                    if i >= lines.len() {
                        current = String::new();
                        break;
                    }
                    let next = lines[i];
                    i += 1;
                    let t = next.trim_start();
                    if t.starts_with('#') || t.is_empty() {
                        continue;
                    }
                    current = next.to_string();
                    break;
                }
                if current.is_empty() && i >= lines.len() {
                    break;
                }
                continue;
            }
            text.push_str(end);
            break;
        }
        // Heredocs: `<<EOF`, `<<-EOF`, `<<"EOF"` take the lines up to the marker.
        for marker in heredoc_markers(&text) {
            let mut body = String::new();
            loop {
                if i >= lines.len() {
                    return Err((
                        start + 1,
                        format!("the heredoc {} has no end line", marker.word),
                    ));
                }
                let l = lines[i];
                i += 1;
                let l = if marker.strip_tabs {
                    l.trim_start_matches('\t')
                } else {
                    l
                };
                if l == marker.word {
                    break;
                }
                body.push_str(l);
                body.push('\n');
            }
            text.push('\n');
            text.push_str(&body);
            text.push_str(&marker.word);
        }
        out.push((start + 1, text));
    }
    Ok(out)
}

#[derive(Debug, PartialEq, Eq)]
struct Heredoc {
    word: String,
    strip_tabs: bool,
}

/// The heredoc markers of a `RUN`, `COPY` or `ADD` line.
fn heredoc_markers(line: &str) -> Vec<Heredoc> {
    let keyword = line
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_ascii_uppercase();
    if !matches!(keyword.as_str(), "RUN" | "COPY" | "ADD") {
        return Vec::new();
    }
    let mut out = Vec::new();
    let mut rest = line;
    while let Some(at) = rest.find("<<") {
        // Only at the start of a word: `$((1<<2))` is no heredoc.
        let starts_word = rest[..at].chars().last().is_none_or(char::is_whitespace);
        let after = &rest[at + 2..];
        rest = after;
        if !starts_word {
            continue;
        }
        let (strip_tabs, after) = match after.strip_prefix('-') {
            Some(a) => (true, a),
            None => (false, after),
        };
        let name = after.trim_start_matches(['"', '\'']);
        let word: String = name
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .collect();
        if word.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_') {
            out.push(Heredoc { word, strip_tabs });
        }
    }
    out
}

type Parsed = (String, Vec<Flag>, String);

/// The keyword (upper case), the flags, and the rest of the line.
fn parse_instruction(raw: &str, _escape: char) -> std::result::Result<Parsed, String> {
    let raw = raw.trim_start();
    let (keyword, mut rest) = match raw.find(char::is_whitespace) {
        Some(i) => (&raw[..i], raw[i..].trim_start()),
        None => (raw, ""),
    };
    let keyword = keyword.to_ascii_uppercase();
    let mut flags = Vec::new();
    while let Some(after) = rest.strip_prefix("--") {
        let end = after.find(char::is_whitespace).unwrap_or(after.len());
        let flag = &after[..end];
        if flag.is_empty() {
            break;
        }
        let (name, value) = match flag.split_once('=') {
            Some((n, v)) => (n.to_string(), Some(v.to_string())),
            None => (flag.to_string(), None),
        };
        if !allowed_flags(&keyword).contains(&name.as_str()) {
            return Err(format!("unknown flag --{name} for {keyword}"));
        }
        flags.push(Flag { name, value });
        rest = after[end..].trim_start();
    }
    Ok((keyword, flags, rest.to_string()))
}

fn from_stage(
    rest: &str,
    flags: &[Flag],
    line: usize,
    raw: &str,
) -> std::result::Result<Stage, String> {
    let words: Vec<&str> = rest.split_whitespace().collect();
    let (base, name) = match words.as_slice() {
        [base] => (*base, None),
        [base, as_, name] if as_.eq_ignore_ascii_case("as") => {
            let ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic())
                && name
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c));
            if !ok {
                return Err(format!("invalid stage name {name:?}"));
            }
            (*base, Some(name.to_ascii_lowercase()))
        }
        _ => return Err("FROM takes an image and, maybe, AS <name>".to_string()),
    };
    Ok(Stage {
        base: base.to_string(),
        name,
        platform: flags
            .iter()
            .find(|f| f.name == "platform")
            .and_then(|f| f.value.clone()),
        line,
        text: raw.to_string(),
        instructions: Vec::new(),
    })
}

fn command(keyword: &str, rest: &str, escape: char) -> std::result::Result<Cmd, String> {
    let need_args = |what: &str| -> std::result::Result<(), String> {
        if rest.trim().is_empty() {
            Err(format!("{keyword} needs {what}"))
        } else {
            Ok(())
        }
    };
    Ok(match keyword {
        "RUN" => {
            need_args("a command")?;
            Cmd::Run(form(rest))
        }
        "CMD" => Cmd::Cmd(form(rest)),
        "ENTRYPOINT" => Cmd::Entrypoint(form(rest)),
        "ENV" => {
            need_args("a name and a value")?;
            Cmd::Env(pairs(rest, escape, keyword, true)?)
        }
        "LABEL" => {
            need_args("a name and a value")?;
            Cmd::Label(pairs(rest, escape, keyword, false)?)
        }
        "EXPOSE" => {
            need_args("a port")?;
            Cmd::Expose(rest.split_whitespace().map(String::from).collect())
        }
        "VOLUME" => {
            need_args("a path")?;
            Cmd::Volume(match json_list(rest) {
                Some(list) => list,
                None => rest.split_whitespace().map(String::from).collect(),
            })
        }
        "USER" => {
            need_args("a user")?;
            Cmd::User(rest.trim().to_string())
        }
        "WORKDIR" => {
            need_args("a path")?;
            Cmd::Workdir(rest.trim().to_string())
        }
        "ARG" => {
            need_args("a name")?;
            let mut args = Vec::new();
            for word in words(rest, escape) {
                let (name, value) = match word.split_once('=') {
                    Some((n, v)) => (n.to_string(), Some(v.to_string())),
                    None => (word, None),
                };
                if !valid_name(&name) {
                    return Err(format!("invalid ARG name {name:?}"));
                }
                args.push((name, value));
            }
            Cmd::Arg(args)
        }
        "COPY" | "ADD" => {
            if !heredoc_markers(&format!("{keyword} {rest}")).is_empty() {
                return Err(format!(
                    "{keyword} with a heredoc needs BuildKit; build with buildx"
                ));
            }
            let mut list = match json_list(rest) {
                Some(list) => list,
                None => rest.split_whitespace().map(String::from).collect(),
            };
            if list.len() < 2 {
                return Err(format!("{keyword} needs a source and a destination"));
            }
            let dest = list.pop().unwrap_or_default();
            if keyword == "COPY" {
                Cmd::Copy {
                    sources: list,
                    dest,
                }
            } else {
                Cmd::Add {
                    sources: list,
                    dest,
                }
            }
        }
        "ONBUILD" => {
            need_args("an instruction")?;
            let inner = rest.trim();
            let word = inner
                .split_whitespace()
                .next()
                .unwrap_or("")
                .to_ascii_uppercase();
            if matches!(word.as_str(), "ONBUILD" | "FROM" | "MAINTAINER") {
                return Err(format!("{word} is not allowed in ONBUILD"));
            }
            // Checked now, run later.
            parse_line(inner, escape)?;
            Cmd::Onbuild(inner.to_string())
        }
        "STOPSIGNAL" => {
            need_args("a signal")?;
            Cmd::StopSignal(rest.trim().to_string())
        }
        "HEALTHCHECK" => {
            need_args("NONE or CMD <command>")?;
            let trimmed = rest.trim();
            let (first, after) = match trimmed.find(char::is_whitespace) {
                Some(i) => (&trimmed[..i], trimmed[i..].trim_start()),
                None => (trimmed, ""),
            };
            match first.to_ascii_uppercase().as_str() {
                "NONE" if after.is_empty() => Cmd::Healthcheck(None),
                "CMD" if !after.is_empty() => Cmd::Healthcheck(Some(form(after))),
                _ => return Err("HEALTHCHECK takes NONE or CMD <command>".to_string()),
            }
        }
        "SHELL" => match json_list(rest) {
            Some(list) if !list.is_empty() => Cmd::Shell(list),
            _ => return Err("SHELL needs a JSON list, like [\"/bin/sh\", \"-c\"]".to_string()),
        },
        "MAINTAINER" => {
            need_args("a name")?;
            Cmd::Maintainer(rest.trim().to_string())
        }
        "FROM" => unreachable!("FROM is read as a stage"),
        "" => return Err("an empty instruction".to_string()),
        other => return Err(format!("unknown instruction: {other}")),
    })
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && !name.contains(char::is_whitespace)
}

/// Exec form when the text is a JSON list of strings; shell form otherwise.
fn form(rest: &str) -> Form {
    match json_list(rest) {
        Some(list) => Form::Exec(list),
        None => Form::Shell(rest.trim().to_string()),
    }
}

fn json_list(rest: &str) -> Option<Vec<String>> {
    let t = rest.trim();
    if !t.starts_with('[') {
        return None;
    }
    serde_json::from_str::<Vec<String>>(t).ok()
}

/// Split on whitespace outside quotes; quotes and escapes stay for the expansion.
fn words(rest: &str, escape: char) -> Vec<String> {
    let mut out = Vec::new();
    let mut word = String::new();
    let mut quote: Option<char> = None;
    let mut chars = rest.chars();
    while let Some(c) = chars.next() {
        match quote {
            Some(q) => {
                word.push(c);
                if c == escape && q == '"' {
                    if let Some(n) = chars.next() {
                        word.push(n);
                    }
                } else if c == q {
                    quote = None;
                }
            }
            None if c.is_whitespace() => {
                if !word.is_empty() {
                    out.push(std::mem::take(&mut word));
                }
            }
            None => {
                word.push(c);
                if c == escape {
                    if let Some(n) = chars.next() {
                        word.push(n);
                    }
                } else if c == '"' || c == '\'' {
                    quote = Some(c);
                }
            }
        }
    }
    if !word.is_empty() {
        out.push(word);
    }
    out
}

/// `ENV`/`LABEL` pairs: `a=1 b="2 3"`, or (ENV only, the old form) `NAME value with spaces`.
fn pairs(
    rest: &str,
    escape: char,
    keyword: &str,
    old_form: bool,
) -> std::result::Result<Vec<(String, String)>, String> {
    let list = words(rest, escape);
    let first_has_eq = list.first().is_some_and(|w| w.contains('='));
    if !first_has_eq {
        if !old_form {
            return Err(format!("{keyword} needs name=value pairs"));
        }
        let trimmed = rest.trim();
        let (name, value) = match trimmed.find(char::is_whitespace) {
            Some(i) => (&trimmed[..i], trimmed[i..].trim_start()),
            None => return Err(format!("{keyword} {trimmed} has no value")),
        };
        return Ok(vec![(name.to_string(), value.to_string())]);
    }
    list.into_iter()
        .map(|w| match w.split_once('=') {
            Some((k, v)) if !k.is_empty() => Ok((k.to_string(), v.to_string())),
            _ => Err(format!("{keyword} needs name=value pairs, not {w:?}")),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(text: &str) -> Instruction {
        let df = parse(&format!("FROM alpine\n{text}\n"), "Dockerfile").unwrap();
        df.stages[0].instructions[0].clone()
    }

    fn error(text: &str) -> String {
        parse(text, "Dockerfile").unwrap_err().why
    }

    #[test]
    fn stages_with_names_args_and_platform() {
        let df = parse(
            "ARG BASE=alpine\nARG V\nFROM --platform=linux/arm64 ${BASE}:3 AS Build\nRUN make\n\
             FROM scratch\nCOPY --from=build /out /\n",
            "Dockerfile",
        )
        .unwrap();
        assert_eq!(
            df.global_args,
            [
                ("BASE".to_string(), Some("alpine".to_string())),
                ("V".to_string(), None)
            ]
        );
        assert_eq!(df.stages.len(), 2);
        assert_eq!(df.stages[0].base, "${BASE}:3");
        assert_eq!(df.stages[0].name.as_deref(), Some("build"));
        assert_eq!(df.stages[0].platform.as_deref(), Some("linux/arm64"));
        assert_eq!(df.stages[0].line, 3);
        assert_eq!(df.stages[1].instructions[0].flag("from"), Some("build"));
        assert_eq!(df.stages[1].instructions[0].line, 6);
    }

    #[test]
    fn exec_and_shell_forms() {
        assert_eq!(
            one("RUN [\"echo\", \"a b\"]").cmd,
            Cmd::Run(Form::Exec(vec!["echo".into(), "a b".into()]))
        );
        assert_eq!(
            one("CMD echo $HOME").cmd,
            Cmd::Cmd(Form::Shell("echo $HOME".into()))
        );
        // Not valid JSON: the shell form, as Docker reads it.
        assert_eq!(
            one("ENTRYPOINT [\"a\", 'b']").cmd,
            Cmd::Entrypoint(Form::Shell("[\"a\", 'b']".into()))
        );
        assert_eq!(
            one("SHELL [\"/bin/bash\", \"-c\"]").cmd,
            Cmd::Shell(vec!["/bin/bash".into(), "-c".into()])
        );
    }

    #[test]
    fn continuations_comments_and_case() {
        let i = one("run echo a \\\n  # a comment\n\n  && echo b");
        assert_eq!(i.cmd, Cmd::Run(Form::Shell("echo a   && echo b".into())));
        assert_eq!(i.line, 2);
        let df = parse(
            "# a comment\nFROM alpine\n  # indented\nENV A=1\n",
            "Dockerfile",
        )
        .unwrap();
        assert_eq!(df.stages[0].line, 2);
        assert_eq!(df.stages[0].instructions[0].line, 4);
    }

    #[test]
    fn escape_directive() {
        let df = parse(
            "# escape=`\n# syntax=docker/dockerfile:1\nFROM alpine\nRUN dir C:\\ `\n  && echo",
            "Dockerfile",
        )
        .unwrap();
        assert_eq!(df.escape, '`');
        assert_eq!(
            df.stages[0].instructions[0].cmd,
            Cmd::Run(Form::Shell("dir C:\\   && echo".into()))
        );
        // After the first instruction, a directive is a comment.
        let df = parse("FROM alpine\n# escape=`\nRUN a \\\nb", "Dockerfile").unwrap();
        assert_eq!(df.escape, '\\');
        assert!(error("# escape=x\nFROM a").contains("escape"));
        assert!(error("# escape=`\n# escape=\\\nFROM a").contains("twice"));
    }

    #[test]
    fn env_and_label_pairs() {
        assert_eq!(
            one("ENV A=1 B=\"two words\" C=x\\ y").cmd,
            Cmd::Env(vec![
                ("A".into(), "1".into()),
                ("B".into(), "\"two words\"".into()),
                ("C".into(), "x\\ y".into()),
            ])
        );
        assert_eq!(
            one("ENV NAME some value").cmd,
            Cmd::Env(vec![("NAME".into(), "some value".into())])
        );
        assert_eq!(
            one("LABEL a.b=c \"d e\"=f").cmd,
            Cmd::Label(vec![
                ("a.b".into(), "c".into()),
                ("\"d e\"".into(), "f".into())
            ])
        );
        assert!(error("FROM a\nLABEL x").contains("Dockerfile:2"));
    }

    #[test]
    fn copy_add_and_flags() {
        let i = one("COPY --chown=1:2 --chmod=755 a b /dest/");
        assert_eq!(
            i.cmd,
            Cmd::Copy {
                sources: vec!["a".into(), "b".into()],
                dest: "/dest/".into()
            }
        );
        assert_eq!(i.flag("chown"), Some("1:2"));
        assert_eq!(i.flag("chmod"), Some("755"));
        assert_eq!(i.flag("from"), None);
        assert_eq!(
            one("ADD [\"a b\", \"/c\"]").cmd,
            Cmd::Add {
                sources: vec!["a b".into()],
                dest: "/c".into()
            }
        );
        assert!(error("FROM a\nCOPY onlyone").contains("source and a destination"));
        assert!(error("FROM a\nCOPY --nope=1 a b").contains("unknown flag --nope"));
    }

    #[test]
    fn other_instructions() {
        assert_eq!(
            one("ARG A B=2").cmd,
            Cmd::Arg(vec![("A".into(), None), ("B".into(), Some("2".into()))])
        );
        assert_eq!(
            one("EXPOSE 80/tcp 53/udp").cmd,
            Cmd::Expose(vec!["80/tcp".into(), "53/udp".into()])
        );
        assert_eq!(
            one("VOLUME [\"/a\", \"/b\"]").cmd,
            Cmd::Volume(vec!["/a".into(), "/b".into()])
        );
        assert_eq!(
            one("VOLUME /a /b").cmd,
            Cmd::Volume(vec!["/a".into(), "/b".into()])
        );
        assert_eq!(one("USER app:app").cmd, Cmd::User("app:app".into()));
        assert_eq!(one("WORKDIR /srv").cmd, Cmd::Workdir("/srv".into()));
        assert_eq!(
            one("STOPSIGNAL SIGINT").cmd,
            Cmd::StopSignal("SIGINT".into())
        );
        assert_eq!(one("HEALTHCHECK NONE").cmd, Cmd::Healthcheck(None));
        let h = one("HEALTHCHECK --interval=5s CMD [\"true\"]");
        assert_eq!(
            h.cmd,
            Cmd::Healthcheck(Some(Form::Exec(vec!["true".into()])))
        );
        assert_eq!(h.flag("interval"), Some("5s"));
        assert_eq!(
            one("ONBUILD RUN echo hi").cmd,
            Cmd::Onbuild("RUN echo hi".into())
        );
        assert_eq!(one("MAINTAINER me").cmd, Cmd::Maintainer("me".into()));
    }

    #[test]
    fn heredoc_for_run() {
        let df = parse(
            "FROM alpine\nRUN <<EOF\necho one\necho two\nEOF\nRUN <<-END\n\techo three\n\tEND\nCMD x\n",
            "Dockerfile",
        )
        .unwrap();
        let i = &df.stages[0].instructions;
        assert_eq!(
            i[0].cmd,
            Cmd::Run(Form::Shell("<<EOF\necho one\necho two\nEOF".into()))
        );
        assert_eq!(
            i[1].cmd,
            Cmd::Run(Form::Shell("<<-END\necho three\nEND".into()))
        );
        assert_eq!(i[2].line, 9);
        assert!(error("FROM a\nRUN <<EOF\necho").contains("Dockerfile:2: the heredoc EOF"));
        // Not heredocs: a shift, and a name that starts with a digit.
        assert_eq!(
            one("RUN echo $((1<<2)) <<9").cmd,
            Cmd::Run(Form::Shell("echo $((1<<2)) <<9".into()))
        );
    }

    #[test]
    fn errors_name_the_line() {
        assert_eq!(
            error("FROM a\nRUN x\nBOGUS y"),
            "Dockerfile:3: unknown instruction: BOGUS"
        );
        assert!(error("RUN x\nFROM a").starts_with("Dockerfile:1: RUN comes before"));
        assert!(error("# only a comment\n").contains("no FROM"));
        assert!(error("FROM a b c").contains("Dockerfile:1"));
        assert!(error("FROM a\nRUN").contains("RUN needs a command"));
        assert!(error("FROM a\nONBUILD FROM b").contains("not allowed in ONBUILD"));
        assert!(error("FROM a\nSHELL sh -c").contains("JSON"));
    }
}
