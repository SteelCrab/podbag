//! `POST /containers/{id}/exec` and `/exec/…`: commands run in a running container.

use super::http::{Request, peer_closed};
use super::router::{Ctx, Reply, error_text, message, reply_error};
use super::stream::{Sink, Wanted, copy_out, frame, pump};
use crate::containers::ExecOptions;
use crate::state::Status;
use crate::{ContainerHandle, Error, ErrorKind, Result};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::Write;
use std::net::Shutdown;
use std::process::Stdio;
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

/// How long a finished exec stays for `GET /exec/{id}/json`.
const KEEP_FINISHED: Duration = Duration::from_secs(300);

/// A command created by `POST /containers/{id}/exec`, until it is started and after.
pub struct Exec {
    pub id: String,
    pub container: String,
    pub command: Vec<String>,
    pub options: ExecOptions,
    pub wanted: Wanted,
    state: Mutex<ExecState>,
}

#[derive(Default)]
struct ExecState {
    started: bool,
    running: bool,
    exit_code: Option<i32>,
    pid: u32,
    /// The write end of the helper's resize pipe, while it runs with a terminal.
    resize: Option<std::io::PipeWriter>,
    /// A size asked for before the start.
    size: Option<(u16, u16)>,
    finished: Option<Instant>,
}

impl Exec {
    fn lock(&self) -> std::sync::MutexGuard<'_, ExecState> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Mark it started; false if it was started before.
    fn claim(&self) -> bool {
        let mut st = self.lock();
        !std::mem::replace(&mut st.started, true)
    }

    fn running(&self, pid: u32, resize: Option<std::io::PipeWriter>) {
        let mut st = self.lock();
        st.running = true;
        st.pid = pid;
        st.resize = resize;
    }

    fn finish(&self, code: i32) {
        let mut st = self.lock();
        st.running = false;
        st.exit_code = Some(code);
        st.resize = None;
        st.finished = Some(Instant::now());
    }
}

/// Execs by ID, kept in memory: a restart of the service forgets them, like Docker.
#[derive(Default)]
pub struct Execs(Mutex<HashMap<String, Arc<Exec>>>);

impl Execs {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<Exec>>> {
        self.0.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn insert(&self, exec: Exec) {
        let mut map = self.lock();
        map.retain(|_, e| {
            e.lock()
                .finished
                .is_none_or(|t| t.elapsed() < KEEP_FINISHED)
        });
        map.insert(exec.id.clone(), Arc::new(exec));
    }

    fn get(&self, id: &str) -> Option<Arc<Exec>> {
        self.lock().get(id).cloned()
    }
}

/// What an exec create body asks for: the command, its settings, the streams to attach.
pub fn exec_from_body(body: &Value) -> Result<(Vec<String>, ExecOptions, Wanted)> {
    exec_in_container(body, false)
}

fn exec_in_container(
    body: &Value,
    container_privileged: bool,
) -> Result<(Vec<String>, ExecOptions, Wanted)> {
    if body["Privileged"].as_bool().unwrap_or(false) && !container_privileged {
        return Err(Error::new(
            ErrorKind::Unsupported,
            "cannot run a privileged exec",
            "the container was not started with --privileged",
            "drop --privileged",
        ));
    }
    let strings = |v: &Value| -> Vec<String> {
        v.as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x.as_str().map(String::from))
            .collect()
    };
    let command = strings(&body["Cmd"]);
    if command.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "cannot create the exec",
            "No exec command specified",
            "give a command",
        ));
    }
    let text = |key: &str| {
        body[key]
            .as_str()
            .filter(|s| !s.is_empty())
            .map(String::from)
    };
    let tty = body["Tty"].as_bool().unwrap_or(false);
    let wanted = Wanted {
        stdin: body["AttachStdin"].as_bool().unwrap_or(false),
        stdout: body["AttachStdout"].as_bool().unwrap_or(false),
        stderr: body["AttachStderr"].as_bool().unwrap_or(false),
    };
    let options = ExecOptions {
        env: strings(&body["Env"])
            .iter()
            .filter_map(|kv| kv.split_once('='))
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        workdir: text("WorkingDir"),
        user: text("User"),
        tty,
        interactive: wanted.stdin,
        size: tty.then(|| console_size(&body["ConsoleSize"]).unwrap_or((24, 80))),
    };
    Ok((command, options, wanted))
}

/// `ConsoleSize: [rows, cols]`; None when missing or zero.
fn console_size(v: &Value) -> Option<(u16, u16)> {
    let rows = u16::try_from(v[0].as_u64()?).ok()?;
    let cols = u16::try_from(v[1].as_u64()?).ok()?;
    (rows > 0 && cols > 0).then_some((rows, cols))
}

/// A resize record for the exec helper's resize pipe: rows, then columns, little-endian.
pub fn resize_record(rows: u16, cols: u16) -> [u8; 4] {
    let [r0, r1] = rows.to_le_bytes();
    let [c0, c1] = cols.to_le_bytes();
    [r0, r1, c0, c1]
}

fn no_such_exec(id: &str) -> Reply {
    message(404, format!("No such exec instance: {id}"))
}

fn running_container(name: &str) -> std::result::Result<ContainerHandle, Reply> {
    let handle = ContainerHandle::get(name).map_err(|e| match e.kind {
        ErrorKind::NotFound => message(404, format!("No such container: {name}")),
        _ => reply_error(&e),
    })?;
    match handle.info() {
        Ok(i) if i.state.paused => {
            let e = crate::containers::pause::exec_refused(handle.id());
            Err(message(409, e.what))
        }
        Ok(i) if i.state.status == Status::Running => Ok(handle),
        Ok(_) => Err(message(
            409,
            format!("Container {} is not running", handle.id()),
        )),
        Err(e) => Err(reply_error(&e)),
    }
}

/// `POST /containers/{id}/exec`.
pub fn create(req: &Request, ctx: &Ctx, container: &str) -> Reply {
    let handle = match running_container(container) {
        Ok(h) => h,
        Err(reply) => return reply,
    };
    let body = match req.json() {
        Ok(b) => b,
        Err(e) => return message(400, format!("{e:?}")),
    };
    let privileged = if body["Privileged"].as_bool() == Some(true) {
        match crate::state::read_config(&ctx.dirs, handle.id()) {
            Ok(spec) => spec.security.privileged,
            Err(e) => return reply_error(&e),
        }
    } else {
        false
    };
    // The Linux exec helper applies the container's saved security settings.
    let (command, options, wanted) = match exec_in_container(&body, privileged) {
        Ok(x) => x,
        Err(e) => return reply_error(&e),
    };
    let id = match crate::id::new_id() {
        Ok(id) => id,
        Err(e) => return reply_error(&e),
    };
    ctx.execs.insert(Exec {
        id: id.clone(),
        container: handle.id().to_string(),
        command,
        options,
        wanted,
        state: Mutex::new(ExecState::default()),
    });
    Reply::Json(201, json!({ "Id": id }))
}

/// Routes under `/exec/`.
pub fn route(req: &Request, ctx: &Ctx, rest: &[&str]) -> Option<Reply> {
    Some(match (req.method.as_str(), rest) {
        ("POST", [id, "start"]) => start(req, ctx, id),
        ("GET", [id, "json"]) => inspect(ctx, id),
        ("POST", [id, "resize"]) => resize(req, ctx, id),
        _ => return None,
    })
}

/// `GET /exec/{id}/json`.
fn inspect(ctx: &Ctx, id: &str) -> Reply {
    let Some(exec) = ctx.execs.get(id) else {
        return no_such_exec(id);
    };
    let st = exec.lock();
    Reply::Json(
        200,
        json!({
            "ID": exec.id,
            "Running": st.running,
            "ExitCode": st.exit_code,
            "Pid": st.pid,
            "ProcessConfig": {
                "tty": exec.options.tty,
                "entrypoint": exec.command[0],
                "arguments": &exec.command[1..],
                "privileged": false,
                "user": exec.options.user.clone().unwrap_or_default(),
            },
            "OpenStdin": exec.wanted.stdin,
            "OpenStdout": exec.wanted.stdout,
            "OpenStderr": exec.wanted.stderr,
            "ContainerID": exec.container,
            "CanRemove": false,
            "DetachKeys": "",
        }),
    )
}

/// `POST /exec/{id}/resize?h=…&w=…`.
fn resize(req: &Request, ctx: &Ctx, id: &str) -> Reply {
    let Some(exec) = ctx.execs.get(id) else {
        return no_such_exec(id);
    };
    let size = |key: &str| req.query.int_param(key).and_then(|v| u16::try_from(v).ok());
    let (Some(rows), Some(cols)) = (size("h"), size("w")) else {
        return message(400, "h and w must be numbers");
    };
    let mut st = exec.lock();
    if let Some(pipe) = st.resize.as_mut() {
        // Gone with the helper: nothing to resize any more.
        let _ = pipe.write_all(&resize_record(rows, cols));
    } else if !st.started {
        st.size = Some((rows, cols));
    }
    Reply::Empty(200)
}

/// `POST /exec/{id}/start`: attached through the hijacked connection, or detached.
fn start(req: &Request, ctx: &Ctx, id: &str) -> Reply {
    let Some(exec) = ctx.execs.get(id) else {
        return no_such_exec(id);
    };
    let body = req.json().unwrap_or(Value::Null);
    let handle = match running_container(&exec.container) {
        Ok(h) => h,
        Err(reply) => return reply,
    };
    if !exec.claim() {
        return message(409, format!("Exec {id} has already been started"));
    }
    let mut options = exec.options.clone();
    if options.tty {
        let asked = exec.lock().size;
        if let Some(size) = console_size(&body["ConsoleSize"]).or(asked) {
            options.size = Some(size);
        }
    }
    if body["Detach"].as_bool().unwrap_or(false) {
        return match spawn(
            &handle,
            &exec,
            &options,
            Stdio::null(),
            Stdio::null(),
            Stdio::null(),
        ) {
            Ok(running) => {
                std::thread::spawn(move || {
                    let code = running.wait().unwrap_or(-1);
                    exec.finish(code);
                });
                Reply::Empty(200)
            }
            Err(e) => {
                exec.finish(exit_code_of(&e));
                reply_error(&e)
            }
        };
    }
    let tty = options.tty;
    Reply::Hijack {
        content_type: if tty {
            "application/vnd.docker.raw-stream"
        } else {
            "application/vnd.docker.multiplexed-stream"
        },
        run: Box::new(move |sock, pending, head| {
            attached(sock, pending, head, handle, exec, options)
        }),
    }
}

/// 127 for a command that is not there, 126 for one that cannot run, like Docker.
fn exit_code_of(e: &Error) -> i32 {
    match e.kind {
        ErrorKind::CommandNotFound => 127,
        _ => 126,
    }
}

/// Start the exec helper with these streams and, with a terminal, a resize pipe.
fn spawn(
    handle: &ContainerHandle,
    exec: &Exec,
    options: &ExecOptions,
    stdin: Stdio,
    stdout: Stdio,
    stderr: Stdio,
) -> Result<crate::Running> {
    let pipe = if options.tty {
        Some(std::io::pipe().map_err(|e| Error::io("cannot make a resize pipe", &e, "try again"))?)
    } else {
        None
    };
    let (resize_r, resize_w) = match pipe {
        Some((r, w)) => (Some(std::os::fd::OwnedFd::from(r)), Some(w)),
        None => (None, None),
    };
    let running = handle.exec_with(&exec.command, options, stdin, stdout, stderr, resize_r)?;
    exec.running(running.helper_pid(), resize_w);
    Ok(running)
}

/// Run the exec attached to `sock` until it ends; a client that leaves hangs it up.
fn attached(
    mut sock: std::os::unix::net::UnixStream,
    pending: Vec<u8>,
    head: super::router::Upgrade,
    handle: ContainerHandle,
    exec: Arc<Exec>,
    options: ExecOptions,
) {
    let tty = options.tty;
    let pipes = (|| -> std::io::Result<_> {
        let stdin = if options.interactive {
            Some(std::io::pipe()?)
        } else {
            None
        };
        let out = std::io::pipe()?;
        let err = if tty { None } else { Some(std::io::pipe()?) };
        Ok((stdin, out, err))
    })();
    let started = pipes
        .map_err(|e| Error::io("cannot make pipes for the exec", &e, "try again"))
        .and_then(|(stdin, (out_r, out_w), err)| {
            let (stdin_r, stdin_w) = match stdin {
                Some((r, w)) => (Stdio::from(r), Some(w)),
                None => (Stdio::null(), None),
            };
            let (err_r, err_w) = match err {
                Some((r, w)) => (Some(r), Stdio::from(w)),
                None => (None, Stdio::null()),
            };
            let running = spawn(&handle, &exec, &options, stdin_r, Stdio::from(out_w), err_w)?;
            Ok((running, stdin_w, out_r, err_r))
        });
    let (running, stdin_w, out_r, err_r) = match started {
        Ok(s) => s,
        Err(e) => {
            // Like Docker: the stream carries the message, the exit code tells the rest.
            exec.finish(exit_code_of(&e));
            if head.send(&mut sock).is_ok() {
                let text = format!("{}\n", error_text(&e));
                let bytes = if tty {
                    text.into_bytes()
                } else {
                    frame(2, text.as_bytes())
                };
                let _ = sock.write_all(&bytes);
            }
            return;
        }
    };
    if head.send(&mut sock).is_err() {
        let _ = running.wait();
        exec.finish(-1);
        return;
    }
    if let (Some(mut w), Ok(mut from)) = (stdin_w, sock.try_clone()) {
        std::thread::spawn(move || {
            if w.write_all(&pending).is_ok() {
                pump(&mut from, &mut w);
            }
        });
    }
    let sinks = match sock.try_clone() {
        Ok(s) => Arc::new(vec![Sink::new(s, exec.wanted)]),
        Err(_) => Arc::new(Vec::new()),
    };
    let relay = |from: std::io::PipeReader, stream_id: u8| {
        let sinks = sinks.clone();
        std::thread::spawn(move || copy_out(from, stream_id, tty, &sinks))
    };
    let out = relay(out_r, 1);
    let err = err_r.map(|r| relay(r, 2));
    let pid = running.helper_pid() as i32;
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(running.wait().unwrap_or(-1));
    });
    let mut hung_up: Option<Instant> = None;
    let code = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(code) => break code,
            Err(mpsc::RecvTimeoutError::Disconnected) => break -1,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
        // A client that left cannot see the command any more: hang it up, as a closed
        // terminal would; kill the helper (and with it the command) if that is ignored.
        match hung_up {
            None if peer_closed(&sock) => {
                // SAFETY: a signal to our own child, not reaped yet (the waiter reports first).
                unsafe { libc::kill(pid, libc::SIGHUP) };
                hung_up = Some(Instant::now());
            }
            Some(t) if t.elapsed() > Duration::from_secs(2) => {
                // SAFETY: as above.
                unsafe { libc::kill(pid, libc::SIGKILL) };
            }
            _ => {}
        }
    };
    let _ = out.join();
    if let Some(e) = err {
        let _ = e.join();
    }
    // Recorded before the stream closes: the docker CLI asks for it right after.
    exec.finish(code);
    let _ = sock.shutdown(Shutdown::Both);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn exec_bodies_map_to_options() {
        let body = json!({
            "Cmd": ["sh", "-c", "x"],
            "Env": ["A=1", "BARE"],
            "WorkingDir": "/w",
            "User": "nobody",
            "Tty": true,
            "AttachStdin": true,
            "AttachStdout": true,
            "AttachStderr": false,
            "ConsoleSize": [30, 100]
        });
        let (cmd, opts, wanted) = exec_from_body(&body).unwrap();
        assert_eq!(cmd, ["sh", "-c", "x"]);
        assert_eq!(opts.env, vec![("A".to_string(), "1".to_string())]);
        assert_eq!(opts.workdir.as_deref(), Some("/w"));
        assert_eq!(opts.user.as_deref(), Some("nobody"));
        assert!(opts.tty);
        assert!(opts.interactive);
        assert_eq!(opts.size, Some((30, 100)));
        assert_eq!(
            wanted,
            Wanted {
                stdin: true,
                stdout: true,
                stderr: false
            }
        );
    }

    #[test]
    fn a_terminal_always_gets_a_size() {
        let (_, opts, _) = exec_from_body(&json!({"Cmd": ["sh"], "Tty": true})).unwrap();
        assert_eq!(opts.size, Some((24, 80)));
        let (_, opts, _) =
            exec_from_body(&json!({"Cmd": ["sh"], "Tty": true, "ConsoleSize": [0, 0]})).unwrap();
        assert_eq!(opts.size, Some((24, 80)));
        let (_, opts, _) = exec_from_body(&json!({"Cmd": ["sh"]})).unwrap();
        assert_eq!(opts.size, None);
    }

    #[test]
    fn exec_bodies_that_cannot_run() {
        for (body, kind) in [
            (json!({}), ErrorKind::InvalidInput),
            (json!({"Cmd": []}), ErrorKind::InvalidInput),
            (
                json!({"Cmd": ["sh"], "Privileged": true}),
                ErrorKind::Unsupported,
            ),
        ] {
            let e = exec_from_body(&body).unwrap_err();
            assert_eq!(e.kind, kind, "{body}");
        }
    }

    #[test]
    fn privileged_exec_requires_an_already_privileged_container() {
        let body = json!({"Cmd": ["id"], "Privileged": true});
        assert_eq!(
            exec_in_container(&body, false).unwrap_err().kind,
            ErrorKind::Unsupported
        );
        assert_eq!(exec_in_container(&body, true).unwrap().0, ["id"]);
    }

    #[test]
    fn resize_records_are_little_endian() {
        assert_eq!(resize_record(40, 120), [40, 0, 120, 0]);
        assert_eq!(resize_record(300, 1), [44, 1, 1, 0]);
    }
}
