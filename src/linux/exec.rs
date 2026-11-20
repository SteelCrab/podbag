//! `podbag __exec <id> '<json>'`: run another command inside a running container.
//!
//! The helper opens the container's namespaces through /proc, joins them (the user
//! namespace first when rootless), and forks so the child lands in the container's PID
//! namespace and, with clone3, directly in its cgroup. The child joins the container's
//! cgroup namespace, enters its root, applies its security settings, and executes the
//! command.

use super::init::{
    connect_stdio, die_with_parent, exec, os_error, reset_signals, resolve, switch_user,
};
use super::spawn::{ContainerStdio, fork_into, wait};
use crate::containers::ExecOptions;
use crate::helper::{StatusChannel, StatusMessage};
use crate::state::{self, Status};
use crate::{Error, ErrorKind, Result, paths};
use rustix::pipe::{PipeFlags, pipe_with};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, OwnedFd};

/// What `__exec` receives on its command line.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct ExecRequest {
    pub command: Vec<String>,
    pub options: ExecOptions,
    /// fd 4 is a pipe of resize records for the terminal (`pty::apply_resizes`).
    #[serde(default)]
    pub resize: bool,
}

/// The descriptor that carries resize records when [`ExecRequest::resize`] is set.
pub const RESIZE_FD: i32 = 4;

const FORWARDED: [i32; 6] = [
    libc::SIGINT,
    libc::SIGTERM,
    libc::SIGHUP,
    libc::SIGQUIT,
    libc::SIGUSR1,
    libc::SIGUSR2,
];

pub fn exec_main(id: &str, json: &str) -> i32 {
    let mut status = StatusChannel::take();
    crate::shim::close_inherited_fds_on_exec();
    crate::signals::block(&[FORWARDED.as_slice(), &[libc::SIGWINCH]].concat());
    match start(id, json) {
        Ok(started) => {
            status.report(&StatusMessage::Started { pid: started.pid });
            finish(started)
        }
        Err(e) => status.fail(e),
    }
}

struct Started {
    pid: u32,
    /// The terminal's controlling side with `-t`; output is relayed from it.
    master: Option<OwnedFd>,
    relay_stdin: bool,
    /// Resize records arrive on [`RESIZE_FD`].
    resize: bool,
}

fn start(id: &str, json: &str) -> Result<Started> {
    crate::id::check(id)?;
    let dirs = paths::dirs()?;
    let req: ExecRequest = serde_json::from_str(json).map_err(|e| {
        Error::new(
            ErrorKind::Internal,
            "cannot read an exec request",
            e.to_string(),
            "report this bug",
        )
    })?;
    if req.command.is_empty() {
        return Err(Error::new(
            ErrorKind::InvalidInput,
            "no command to run",
            "exec needs a command",
            "give the command after the container name",
        ));
    }
    let spec = state::read_config(&dirs, id)?;
    let st = state::reconcile(&dirs, id)?;
    let pid = match (st.status, st.pid) {
        (Status::Running, Some(pid)) => pid,
        _ => return Err(not_running(&spec.name)),
    };

    // Open everything first: after joining the mount namespace, /proc paths change meaning.
    // SAFETY: geteuid has no preconditions.
    let rootless = unsafe { libc::geteuid() } != 0;
    let open_ns = |name: &str| {
        File::open(format!("/proc/{pid}/ns/{name}")).map_err(|e| enter_error(&spec.name, e))
    };
    let mut namespaces = Vec::new();
    if rootless {
        namespaces.push((open_ns("user")?, libc::CLONE_NEWUSER));
    }
    for (name, flag) in [
        ("ipc", libc::CLONE_NEWIPC),
        ("uts", libc::CLONE_NEWUTS),
        ("net", libc::CLONE_NEWNET),
        ("pid", libc::CLONE_NEWPID),
        ("mnt", libc::CLONE_NEWNS),
    ] {
        // A container on the host's network shares this namespace: joining it again
        // would need privileges over the host's namespace.
        if same_namespace(pid, name) {
            continue;
        }
        namespaces.push((open_ns(name)?, flag));
    }
    let root = File::open(format!("/proc/{pid}/root")).map_err(|e| enter_error(&spec.name, e))?;
    // The group of the container's main process, opened now: /proc names other PIDs once
    // this process is in the container's namespaces.
    let main_group = group_of(pid);
    // Joined by the child: the helper's own cgroup lies outside the container's cgroup
    // namespace, where clone3 could not place the child.
    // A container made before it had its own cgroup namespace shares ours: skip it.
    let cgroup_ns = if same_namespace(pid, "cgroup") {
        None
    } else {
        Some(open_ns("cgroup")?)
    };
    let cgroup = st
        .cgroup
        .as_deref()
        .map(|path| {
            use std::os::unix::fs::OpenOptionsExt;
            std::fs::OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_DIRECTORY)
                .open(path)
                .map_err(|e| enter_error(&spec.name, e))
        })
        .transpose()?;
    for (file, flag) in &namespaces {
        // SAFETY: setns on a namespace descriptor we opened; this process is single-threaded.
        if unsafe { libc::setns(file.as_raw_fd(), *flag) } != 0 {
            return Err(enter_error(&spec.name, std::io::Error::last_os_error()));
        }
    }

    let opts = &req.options;
    let (master, slave) = if opts.tty {
        let (m, s) = super::pty::open(if opts.size.is_some() { None } else { Some(0) })?;
        if let Some((rows, cols)) = opts.size {
            super::pty::set_size(m.as_raw_fd(), rows, cols);
        }
        (Some(m), Some(s))
    } else {
        (None, None)
    };
    let null = File::open("/dev/null")
        .map_err(|e| Error::io("cannot open /dev/null", &e, "check the system"))?;
    let stdio = match &slave {
        Some(s) => ContainerStdio {
            fds: [s.as_raw_fd(); 3],
            tty: true,
            tty_socket: None,
        },
        None => ContainerStdio {
            fds: [
                if opts.interactive {
                    0
                } else {
                    null.as_raw_fd()
                },
                1,
                2,
            ],
            tty: false,
            tty_socket: None,
        },
    };
    let (status_r, status_w) = pipe_with(PipeFlags::CLOEXEC).map_err(|e| {
        Error::io(
            "cannot start the command",
            &e.into(),
            "check the open file limit",
        )
    })?;

    use std::os::fd::AsFd;
    let forked = match fork_into(cgroup.as_ref().map(|c| c.as_fd())) {
        // The container's processes moved into a group below its own and turned on a
        // controller for the groups below (BuildKit, docker-in-docker): no process may join
        // the top any more. Join its main process's group instead, as runc does.
        Err(e) if e.raw_os_error() == Some(libc::EBUSY) => match &main_group {
            Some(below) => fork_into(Some(below.as_fd())),
            None => Err(e),
        },
        forked => forked,
    };
    let child = forked.map_err(|e| {
        Error::io(
            "cannot start the command in the container",
            &e,
            "check that the container is still running",
        )
    })?;
    if child == 0 {
        drop(status_r);
        let joined = cgroup_ns.as_ref().map_or(Ok(()), join_cgroup_ns);
        let error = match joined
            .and_then(|()| child_setup(&spec, &req, &root, stdio, status_w.as_raw_fd()))
        {
            Err(e) => e,
            Ok(never) => match never {},
        };
        let _ = File::from(status_w).write_all(&serde_json::to_vec(&error).unwrap_or_default());
        // SAFETY: leave the forked child without running destructors.
        unsafe { libc::_exit(error.exit_code()) }
    }
    drop(status_w);
    drop(slave);
    let mut report = Vec::new();
    let _ = File::from(status_r).read_to_end(&mut report);
    if report != [0] {
        let _ = wait(child);
        let report = report.strip_prefix(&[0]).unwrap_or(&report);
        return Err(serde_json::from_slice::<Error>(report).unwrap_or_else(|_| {
            Error::new(
                ErrorKind::Runtime,
                "the command ended before it started",
                "it exited without reporting an error",
                "run it again",
            )
        }));
    }
    Ok(Started {
        pid: child,
        master,
        relay_stdin: opts.tty && opts.interactive,
        resize: req.resize,
    })
}

/// In the forked child, inside the container's namespaces: enter its root, become the
/// user, and execute the command. Never returns on success.
fn child_setup(
    spec: &crate::spec::ContainerSpec,
    req: &ExecRequest,
    root: &File,
    stdio: ContainerStdio,
    status_fd: i32,
) -> Result<std::convert::Infallible> {
    // SAFETY: prctl with these arguments has no memory preconditions.
    unsafe { libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL) };
    let fail = |what: &str| {
        os_error(
            what,
            std::io::Error::last_os_error(),
            "check that the container is still running",
        )
    };
    // SAFETY: fchdir to the container's root directory, then make it this process's root.
    unsafe {
        if libc::fchdir(root.as_raw_fd()) != 0 || libc::chroot(c".".as_ptr()) != 0 {
            return Err(fail("cannot enter the container's root"));
        }
    }
    connect_stdio(stdio)?;
    let opts = &req.options;
    let creds = crate::credentials::resolve_user(
        opts.user.as_deref().or(spec.user.as_deref()),
        &std::fs::read_to_string("/etc/passwd").unwrap_or_default(),
        &std::fs::read_to_string("/etc/group").unwrap_or_default(),
    )?;
    super::security::apply(&spec.security)?;
    switch_user(&super::init::with_groups(creds.clone(), &spec.security))?;
    die_with_parent(status_fd);
    let workdir = opts
        .workdir
        .as_deref()
        .or(spec.workdir.as_deref())
        .unwrap_or("/");
    std::env::set_current_dir(workdir).map_err(|e| {
        os_error(
            &format!("cannot enter the working directory {workdir}"),
            e,
            "choose a directory that exists in the container",
        )
    })?;
    let mut with_env = spec.clone();
    with_env.env.extend(opts.env.iter().cloned());
    let env = with_env.effective_env(&creds.home);
    let program = resolve(&req.command[0], &env)?;
    reset_signals();
    // SAFETY: one byte from a valid buffer to our own descriptor: "set up, executing now".
    unsafe { libc::write(status_fd, [0u8].as_ptr().cast(), 1) };
    exec(&program, &req.command, &env)
}

/// Forward signals, relay a terminal if there is one, and return the command's exit code.
fn finish(started: Started) -> i32 {
    crate::signals::forward_all_to(started.pid, &FORWARDED);
    if let Some(master) = &started.master {
        let resize = started.resize.then_some(RESIZE_FD);
        super::pty::relay(master, started.relay_stdin, started.pid, resize);
    }
    wait(started.pid).unwrap_or(-1)
}

/// Whether process `pid` is in this process's namespace of kind `name`.
fn same_namespace(pid: u32, name: &str) -> bool {
    let link = |p: String| std::fs::read_link(p).ok();
    let theirs = link(format!("/proc/{pid}/ns/{name}"));
    theirs.is_some() && theirs == link(format!("/proc/self/ns/{name}"))
}

/// The cgroup directory process `pid` is in now (cgroup v2), as this process sees it.
fn group_of(pid: u32) -> Option<File> {
    use std::os::unix::fs::OpenOptionsExt;
    let text = std::fs::read_to_string(format!("/proc/{pid}/cgroup")).ok()?;
    let path = text.lines().find_map(|l| l.strip_prefix("0::"))?;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY)
        .open(format!("/sys/fs/cgroup{path}"))
        .ok()
}

/// In the forked child: join the container's cgroup namespace.
fn join_cgroup_ns(ns: &File) -> Result<()> {
    // SAFETY: setns on a namespace descriptor we opened.
    if unsafe { libc::setns(ns.as_raw_fd(), libc::CLONE_NEWCGROUP) } != 0 {
        return Err(os_error(
            "cannot enter the container's cgroup namespace",
            std::io::Error::last_os_error(),
            "check that the container is still running",
        ));
    }
    Ok(())
}

fn not_running(name: &str) -> Error {
    Error::new(
        ErrorKind::InvalidInput,
        format!("container {name} is not running"),
        "exec needs a running container",
        "start a new container, or run `podbag ps -a` to see its state",
    )
}

fn enter_error(name: &str, e: std::io::Error) -> Error {
    Error::io(
        format!("cannot enter container {name}"),
        &e,
        "check that the container is still running",
    )
}
