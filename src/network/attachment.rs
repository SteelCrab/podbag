//! Changes to a container's secondary bridge networks.

use super::{FixedAddresses, NetworkMode, parse_network_arg};
use crate::spec::{Attachment, ContainerSpec};
use crate::{Error, ErrorKind, Result, id, paths, state};
use serde::{Deserialize, Serialize};

#[cfg(target_os = "linux")]
use super::{Endpoint, Member, Record};
#[cfg(target_os = "linux")]
use crate::{linux, netlink::Socket, paths::Dirs};
#[cfg(target_os = "linux")]
use std::{fs::File, os::unix::fs::MetadataExt, process::Stdio};

#[derive(Serialize, Deserialize)]
pub(crate) enum AttachmentChange {
    Connect {
        aliases: Vec<String>,
        addresses: FixedAddresses,
    },
    Disconnect,
}

impl AttachmentChange {
    pub(crate) fn run(self, id: &str, network: &str) -> Result<()> {
        let network = self.network_name(id, network)?;
        #[cfg(target_os = "linux")]
        {
            let json = serde_json::to_string(&self).map_err(bad_request)?;
            crate::helper::spawn_with_args(
                &["__network-attachment", id, &network, &json],
                Stdio::null(),
                Stdio::null(),
                Stdio::null(),
                None,
            )?
            .wait_done()?;
            Ok(())
        }
        #[cfg(not(target_os = "linux"))]
        {
            let dirs = paths::dirs()?;
            let _names = state::lock_names(&dirs)?;
            state::change_settings(&dirs, id, |spec| {
                if let Self::Connect { addresses, .. } = &self {
                    super::get_in(&dirs, &network)?.check_addresses(*addresses)?;
                }
                self.apply(spec, &network)
            })
        }
    }

    fn network_name(&self, id: &str, name: &str) -> Result<String> {
        match parse_network_arg(name)? {
            NetworkMode::Bridge { network } => Ok(network),
            _ if matches!(self, Self::Disconnect) => Ok(name.to_string()),
            _ => Err(Error::new(
                ErrorKind::InvalidInput,
                self.what(id, name),
                format!("{name} is a network mode, not a network to join"),
                "connect it to a bridge network",
            )),
        }
    }

    fn what(&self, id: &str, network: &str) -> String {
        let (action, direction) = match self {
            Self::Connect { .. } => ("connect", "to"),
            Self::Disconnect => ("disconnect", "from"),
        };
        format!(
            "cannot {action} container {} {direction} network {network}",
            id::short_id(id)
        )
    }

    fn apply(&self, spec: &mut ContainerSpec, network: &str) -> Result<()> {
        match self {
            Self::Connect { aliases, addresses } => {
                if spec.bridge_networks().contains(&network) {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        self.what(&spec.id, network),
                        "it is already connected to that network",
                        "leave it as it is",
                    ));
                }
                spec.extra_networks.push(Attachment {
                    network: network.to_string(),
                    aliases: aliases.clone(),
                });
                if !addresses.is_empty() {
                    spec.fixed_addresses.insert(network.to_string(), *addresses);
                }
            }
            Self::Disconnect => {
                if spec.bridge_networks().first() == Some(&network) {
                    return Err(Error::new(
                        ErrorKind::InvalidInput,
                        self.what(&spec.id, network),
                        "it is the container's first network",
                        "create the container again with the networks it needs",
                    ));
                }
                let at = spec
                    .extra_networks
                    .iter()
                    .position(|a| a.network == network)
                    .ok_or_else(|| {
                        Error::new(
                            ErrorKind::NotFound,
                            self.what(&spec.id, network),
                            "it is not connected to that network",
                            "run `podbag inspect` to see its networks",
                        )
                    })?;
                spec.extra_networks.remove(at);
                spec.fixed_addresses.remove(network);
            }
        }
        spec.validate()
    }
}

#[cfg(target_os = "linux")]
fn bad_request(error: serde_json::Error) -> Error {
    Error::new(
        ErrorKind::Internal,
        "cannot read a network attachment request",
        error.to_string(),
        "report this bug",
    )
}

/// Run in a fresh process so a rootless change can join the session's user namespace.
#[cfg(target_os = "linux")]
pub fn attachment_main(id: &str, network: &str, json: &str) -> i32 {
    use crate::helper::{StatusChannel, StatusMessage};
    let mut status = StatusChannel::take();
    crate::shim::close_inherited_fds_on_exec();
    // Once the locks are held, finish a change even if the invoking terminal closes.
    crate::signals::block(&[libc::SIGHUP, libc::SIGINT, libc::SIGQUIT, libc::SIGTERM]);
    let result = serde_json::from_str::<AttachmentChange>(json)
        .map_err(bad_request)
        .and_then(|change| {
            let network = change.network_name(id, network)?;
            change.apply_in(&paths::dirs()?, id, &network)
        });
    match result {
        Ok(()) => {
            status.report(&StatusMessage::Done {
                output: serde_json::Value::Null,
            });
            0
        }
        Err(error) => status.fail(error),
    }
}

#[cfg(target_os = "linux")]
impl AttachmentChange {
    fn busy(&self, id: &str, network: &str, why: &str) -> Error {
        Error::new(
            ErrorKind::Conflict,
            self.what(id, network),
            why,
            "retry when the container is running or stopped",
        )
    }

    fn apply_in(&self, dirs: &Dirs, id: &str, network: &str) -> Result<()> {
        use std::time::{Duration, Instant};
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            {
                // Rename and resource updates also write the settings under the name lock.
                let _names = state::lock_names(dirs)?;
                let idle = state::try_shim_lock(dirs, id)?;
                let _state = state::lock_state(dirs, id)?;
                let before = state::read_config(dirs, id)?;
                let mut after = before.clone();
                self.apply(&mut after, network)?;
                if idle.is_some() {
                    if let Self::Connect { addresses, .. } = self {
                        super::get_in(dirs, network)?.check_addresses(*addresses)?;
                    }
                    return state::write_config(dirs, &after);
                }
                let running = state::read_state(dirs, id)?;
                if running.status == state::Status::Running {
                    return self.live(dirs, network, &before, &after, &running);
                }
            }
            if Instant::now() >= deadline {
                return Err(self.busy(id, network, "a start or stop is still in progress"));
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn live(
        &self,
        dirs: &Dirs,
        network: &str,
        before: &ContainerSpec,
        after: &ContainerSpec,
        running: &state::State,
    ) -> Result<()> {
        let id = &before.id;
        let pid = running
            .pid
            .ok_or_else(|| self.busy(id, network, "the running container has no process ID"))?;
        let Some(inode) = running.network_inode else {
            return Err(Error::new(
                ErrorKind::Conflict,
                self.what(id, network),
                "the runtime did not record a bridge namespace for this run",
                "stop and start the container again",
            ));
        };
        // SAFETY: geteuid has no preconditions.
        let rootless = unsafe { libc::geteuid() } != 0;
        let holder = if rootless {
            let pid = linux::session::existing(dirs, id)?;
            linux::session::join_userns(pid)?;
            Some(linux::net::namespace(pid).map_err(|error| self.io(id, network, error))?)
        } else {
            None
        };
        // All netlink operations use this descriptor. If the process exits and Linux
        // reuses its PID, the descriptor still refers to the original namespace.
        let ns = linux::net::namespace(pid).map_err(|error| self.io(id, network, error))?;
        let actual = ns.metadata().map_err(|error| self.io(id, network, error))?;
        if actual.ino() != inode {
            return Err(self.busy(id, network, "the running network namespace changed"));
        }

        match holder {
            Some(holder) => {
                linux::net::in_netns_file(&holder, || -> std::io::Result<Result<()>> {
                    Ok(self.transaction(dirs, network, before, after, &ns, None))
                })
                .map_err(|error| self.io(id, network, error))?
            }
            None => {
                let host_bridge = if matches!(self, Self::Connect { .. }) {
                    super::lock_host_bridges()?
                } else {
                    None
                };
                self.transaction(dirs, network, before, after, &ns, host_bridge)
            }
        }
    }

    fn io(&self, id: &str, network: &str, error: std::io::Error) -> Error {
        Error::io(
            self.what(id, network),
            &error,
            "check the bridge and container interfaces",
        )
    }

    fn transaction(
        &self,
        dirs: &Dirs,
        name: &str,
        before: &ContainerSpec,
        after: &ContainerSpec,
        ns: &File,
        host_bridge: Option<super::HostBridgeLock>,
    ) -> Result<()> {
        let id = &after.id;
        let _networks = super::lock(dirs)?;
        let mut changed = self.records(dirs, name, before, after)?;
        let io = |error| self.io(id, name, error);
        let mut socket = Socket::route().map_err(io)?;

        // Hold the lock through rollback as well, so no other container can take an
        // address while its previous owner might still have a live interface.
        match self {
            Self::Connect { .. } => {
                let target = &changed[0];
                let network = &target.record.network;
                let addresses = target.record.containers[id].addresses();
                let bridge = linux::net::prepare_bridge_with_lock(network, host_bridge)?;
                let (existing, vacant) = interfaces(&mut socket, id, bridge).map_err(io)?;
                if existing.is_some() {
                    return Err(Error::new(
                        ErrorKind::Conflict,
                        self.what(id, name),
                        "an interface from an earlier change is still attached",
                        "stop and start the container again",
                    ));
                }
                let Some(slot) = vacant else {
                    return Err(self.busy(id, name, "there is no free secondary interface"));
                };
                let host_side = linux::net::veth_name(id, slot);
                // Reserve the address before creating the link. If interrupted, its
                // address remains reserved until the container next stops or starts.
                if let Err((saved, mut error)) = save_records(dirs, &changed) {
                    restore_records(dirs, id, &mut changed[..saved], &mut error);
                    return Err(error);
                }
                if let Err(error) =
                    linux::net::connect_peer(network, bridge, &host_side, ns, addresses, slot)
                {
                    let mut error = io(error);
                    if socket
                        .link_index(&host_side)
                        .is_ok_and(|link| link.is_none())
                    {
                        restore_records(dirs, id, &mut changed, &mut error);
                    }
                    return Err(error);
                }
                if let Err(mut error) = state::write_config(dirs, after) {
                    let undo = remove_interface(&mut socket, &host_side, bridge).map_err(io);
                    let removed = undo.is_ok();
                    note_rollback(&mut error, undo);
                    if removed {
                        restore_records(dirs, id, &mut changed, &mut error);
                    }
                    return Err(error);
                }
            }
            Self::Disconnect => {
                let (target, other_records) = changed
                    .split_first_mut()
                    .expect("a record for the target bridge");
                let network = &target.record.network;
                let bridge = socket.link_index(&network.bridge).map_err(io)?;
                let existing = match bridge {
                    Some(bridge) => interfaces(&mut socket, id, bridge).map_err(io)?.0,
                    None => None,
                };
                let restore = || -> Result<()> {
                    if let (Some((slot, _)), Some(bridge), Some(member)) =
                        (existing, bridge, &target.previous)
                    {
                        linux::net::connect_peer(
                            network,
                            bridge,
                            &linux::net::veth_name(id, slot),
                            ns,
                            member.addresses(),
                            slot,
                        )
                        .map_err(io)?;
                    }
                    Ok(())
                };
                if let Some((_, index)) = existing {
                    linux::net::delete_veth(&mut socket, index).map_err(io)?;
                }
                if let Err(mut error) = state::write_config(dirs, after) {
                    note_rollback(&mut error, restore());
                    return Err(error);
                }
                // Return the address only after the other network records and the
                // config are committed. The detached address cannot be reused on failure.
                let result = save_records(dirs, other_records).and_then(|()| {
                    super::write_record(dirs, &target.record)
                        .map_err(|error| (other_records.len(), error))
                });
                if let Err((saved, mut error)) = result {
                    note_rollback(&mut error, state::write_config(dirs, before));
                    note_rollback(&mut error, restore());
                    restore_records(dirs, id, &mut other_records[..saved], &mut error);
                    return Err(error);
                }
            }
        }

        for change in &changed {
            super::refresh_hosts_locked(dirs, &change.record.network.name);
        }
        if after.bridge_networks() == [super::DEFAULT] {
            // The default bridge has no names to announce. The changed container still
            // needs its own former names and peers removed from the mounted file.
            let mut loads = super::Loads::default();
            loads.read(dirs, super::DEFAULT);
            if let Some(text) = super::compose_hosts(&loads, dirs, id, &[super::DEFAULT]) {
                let _ =
                    crate::netfiles::rewrite_in_place(&dirs.container_dir(id).join("hosts"), &text);
            }
        }
        Ok(())
    }

    // The first record is the network being changed. The others already hold an address
    // for the container; update only the information used to refresh its hosts file.
    fn records(
        &self,
        dirs: &Dirs,
        name: &str,
        before: &ContainerSpec,
        after: &ContainerSpec,
    ) -> Result<Vec<Edit>> {
        let id = &before.id;
        let old_names = before.bridge_networks();
        let new_names = after.bridge_networks();
        let names: Vec<String> = if new_names.len() > 1 {
            new_names.iter().map(ToString::to_string).collect()
        } else {
            Vec::new()
        };
        let mut target = super::load(dirs, name)?;
        let previous = target.containers.get(id).cloned();
        let mut changed = Vec::new();
        for other in old_names.into_iter().filter(|&n| n != name) {
            let mut record = super::load(dirs, other)?;
            let member = record.containers.get_mut(id).ok_or_else(|| {
                self.busy(id, name, "an existing interface has no recorded address")
            })?;
            let previous = Some(member.clone());
            member.networks.clone_from(&names);
            changed.push(Edit { record, previous });
        }
        let Some(primary) = changed.first().and_then(|edit| edit.previous.as_ref()) else {
            return Err(self.busy(id, name, "there is no primary bridge"));
        };
        match self {
            Self::Connect { aliases, addresses } => {
                super::join_record(
                    &mut target,
                    id,
                    &Endpoint {
                        name: &after.name,
                        hostname: &after.hostname,
                        aliases,
                        networks: &names,
                        extra_hosts: &primary.extra_hosts,
                        fixed: *addresses,
                    },
                )?;
            }
            Self::Disconnect => {
                if target.containers.remove(id).is_none() {
                    return Err(self.busy(id, name, "the interface has no recorded address"));
                }
            }
        }
        changed.insert(
            0,
            Edit {
                record: target,
                previous,
            },
        );
        Ok(changed)
    }
}

#[cfg(target_os = "linux")]
struct Edit {
    record: Record,
    previous: Option<Member>,
}

#[cfg(target_os = "linux")]
fn save_records(dirs: &Dirs, changed: &[Edit]) -> std::result::Result<(), (usize, Error)> {
    for (index, change) in changed.iter().enumerate() {
        super::write_record(dirs, &change.record).map_err(|error| (index, error))?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn restore_records(dirs: &Dirs, id: &str, changed: &mut [Edit], error: &mut Error) {
    for edit in changed.iter_mut().rev() {
        match edit.previous.take() {
            Some(member) => edit.record.containers.insert(id.to_string(), member),
            None => edit.record.containers.remove(id),
        };
        note_rollback(error, super::write_record(dirs, &edit.record));
    }
}

#[cfg(target_os = "linux")]
fn note_rollback(error: &mut Error, undo: Result<()>) {
    if let Err(undo) = undo {
        error
            .why
            .push_str(&format!("; rollback failed: {} ({})", undo.what, undo.why));
        error.next = "stop and start the container after correcting the reported error".to_string();
    }
}

#[cfg(target_os = "linux")]
fn interfaces(
    socket: &mut Socket,
    id: &str,
    bridge: u32,
) -> std::io::Result<(Option<(usize, u32)>, Option<usize>)> {
    let mut existing = None;
    let mut vacant = None;
    for slot in 1..crate::spec::MAX_NETWORKS {
        match socket.link(&linux::net::veth_name(id, slot))? {
            None if vacant.is_none() => vacant = Some(slot),
            Some((index, Some(master))) if master == bridge => {
                if existing.replace((slot, index)).is_some() {
                    return Err(std::io::Error::other(
                        "multiple interfaces on the same bridge",
                    ));
                }
            }
            _ => (),
        }
    }
    Ok((existing, vacant))
}

#[cfg(target_os = "linux")]
fn remove_interface(socket: &mut Socket, name: &str, bridge: u32) -> std::io::Result<()> {
    match socket.link(name)? {
        Some((index, Some(master))) if master == bridge => linux::net::delete_veth(socket, index),
        Some(_) => Err(std::io::Error::other("the veth is on a different bridge")),
        None => Ok(()),
    }
}
