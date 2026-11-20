//! `pause` and `unpause`: the cgroup v2 freezer on a running container's group. The
//! state records it (`paused`); a stop or kill thaws the group after its signal, so the
//! processes can handle it, as Docker does.

use super::ContainerHandle;
use crate::cgroup::freezer;
use crate::paths::{self, Dirs};
use crate::state::{self, Status};
use crate::{Error, ErrorKind, Result};
use std::path::Path;

fn refused(doing: &str, name: &str, why: &str, next: &str) -> Error {
    Error::new(
        ErrorKind::Conflict,
        format!("cannot {doing} container {name}"),
        why,
        next,
    )
}

/// The Docker words for an exec into a paused container.
pub(crate) fn exec_refused(id: &str) -> Error {
    Error::new(
        ErrorKind::Conflict,
        format!("Container {id} is paused, unpause the container before exec"),
        "its processes are frozen",
        "run `podbag unpause` first",
    )
}

impl ContainerHandle {
    /// Freeze every process of the running container (`docker pause`).
    pub fn pause(&self) -> Result<()> {
        let dirs = paths::dirs()?;
        let info = self.info()?;
        if info.state.status != Status::Running {
            return Err(refused(
                "pause",
                &info.name,
                "it is not running",
                "start it first",
            ));
        }
        if info.state.paused {
            return Err(refused(
                "pause",
                &info.name,
                "it is paused already",
                "run `podbag unpause` to continue it",
            ));
        }
        let group = group_of(&dirs, &self.id, &info.name, info.state.cgroup.as_deref())?;
        freezer::set(Path::new(&group), true, freezer::LIMIT)
            .map_err(|e| freezer_error("pause", &info.name, &e))?;
        let run = info.state.started;
        let recorded = state::change_running_state(&dirs, &self.id, |s| {
            s.started == run && {
                s.paused = true;
                true
            }
        })?;
        if recorded.is_none_or(|s| !s.paused) {
            // It ended (or started again) meanwhile: nothing of that run is left to pause.
            let _ = freezer::set(Path::new(&group), false, freezer::LIMIT);
            return Err(refused(
                "pause",
                &info.name,
                "it stopped meanwhile",
                "start it first",
            ));
        }
        Ok(())
    }

    /// Let the processes of a paused container run again (`docker unpause`).
    pub fn unpause(&self) -> Result<()> {
        let dirs = paths::dirs()?;
        let info = self.info()?;
        if info.state.status != Status::Running {
            return Err(refused(
                "unpause",
                &info.name,
                "it is not running",
                "start it first",
            ));
        }
        let frozen = info
            .state
            .cgroup
            .as_deref()
            .and_then(|g| freezer::frozen(Path::new(g)))
            .unwrap_or(false);
        if !info.state.paused && !frozen {
            return Err(refused(
                "unpause",
                &info.name,
                "it is not paused",
                "nothing to do",
            ));
        }
        let group = group_of(&dirs, &self.id, &info.name, info.state.cgroup.as_deref())?;
        freezer::set(Path::new(&group), false, freezer::LIMIT)
            .map_err(|e| freezer_error("unpause", &info.name, &e))?;
        state::change_running_state(&dirs, &self.id, |s| std::mem::replace(&mut s.paused, false))?;
        Ok(())
    }
}

/// The container's cgroup, or why it has none.
fn group_of(dirs: &Dirs, id: &str, name: &str, cgroup: Option<&str>) -> Result<String> {
    if let Some(g) = cgroup {
        return Ok(g.to_string());
    }
    let why = no_cgroup_reason(dirs, id);
    Err(Error::new(
        ErrorKind::MissingRequirement,
        format!("cannot pause or unpause container {name}"),
        format!("it runs without a cgroup ({why}), and the freezer needs one"),
        "see README \"Requirements\"",
    ))
}

/// Why a container runs without a cgroup, as far as podbag can tell now.
pub(crate) fn no_cgroup_reason(dirs: &Dirs, id: &str) -> String {
    #[cfg(target_os = "linux")]
    {
        let _ = (dirs, id);
        // SAFETY: geteuid has no preconditions.
        let rootless = unsafe { libc::geteuid() } != 0;
        match crate::cgroup::choose_placement(rootless, &Default::default()) {
            Ok((_, Some(why))) => return why,
            Err(e) => return e.why,
            Ok((_, None)) => {}
        }
        "it was created when podbag could not make cgroups".to_string()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (dirs, id);
        "cgroups need Linux".to_string()
    }
}

fn freezer_error(doing: &str, name: &str, e: &std::io::Error) -> Error {
    let kind = if e.kind() == std::io::ErrorKind::Unsupported {
        ErrorKind::Unsupported
    } else {
        ErrorKind::Runtime
    };
    Error::new(
        kind,
        format!("cannot {doing} container {name}"),
        e.to_string(),
        "check that cgroup v2 is mounted at /sys/fs/cgroup",
    )
}

/// After a signal to a paused container: thaw it, so its processes can handle the signal
/// (a frozen process acts on SIGKILL only), and record that it runs again.
pub(crate) fn thaw_after_signal(dirs: &Dirs, id: &str) {
    let Ok(st) = state::read_state(dirs, id) else {
        return;
    };
    if st.status != Status::Running || !st.paused {
        return;
    }
    if let Some(group) = &st.cgroup {
        let _ = freezer::set(Path::new(group), false, freezer::LIMIT);
    }
    let _ = state::change_running_state(dirs, id, |s| std::mem::replace(&mut s.paused, false));
}
