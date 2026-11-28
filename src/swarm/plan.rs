//! The pure decisions of swarm mode: IDs, task names, which tasks to start or remove,
//! the batches of a rolling update, and the settings read from a service spec. No files
//! and no containers here, so all of it is easy to test.

use super::store::PortRecord;
use crate::restart::RestartPolicy;
use serde_json::Value;
use std::time::Duration;

/// The length of a swarm object ID (Docker's: 25 lowercase letters and digits).
pub const ID_LEN: usize = 25;

/// An object ID from random bytes: each byte picks one of 36 letters and digits.
pub fn object_id(random: &[u8; ID_LEN]) -> String {
    const ALPHABET: &[u8; 36] = b"0123456789abcdefghijklmnopqrstuvwxyz";
    random
        .iter()
        .map(|b| ALPHABET[usize::from(*b) % 36] as char)
        .collect()
}

/// Whether `s` looks like an object ID podbag made.
pub fn is_object_id(s: &str) -> bool {
    s.len() == ID_LEN
        && s.bytes()
            .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
}

/// Docker's rule for service, secret, config and node names: letters and digits, with
/// `-` or `_` between them, at most 63 characters.
pub fn valid_name(name: &str) -> bool {
    let bytes = name.as_bytes();
    let alnum = |b: &u8| b.is_ascii_alphanumeric();
    !bytes.is_empty()
        && bytes.len() <= 63
        && bytes.first().is_some_and(alnum)
        && bytes.last().is_some_and(alnum)
        && bytes
            .iter()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
}

/// Where a task sits in its service: a slot number (replicated) or the node (global).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Slot {
    Replica(u64),
    Node,
}

impl Slot {
    /// The number Docker shows (`Slot` in a task): 0 for a global task.
    pub fn number(self) -> u64 {
        match self {
            Slot::Replica(n) => n,
            Slot::Node => 0,
        }
    }
}

/// The container name of a task: `<service>.<slot>.<task id>`; a global task has the
/// node ID in place of the slot, as with Docker.
pub fn task_name(service: &str, slot: Slot, node_id: &str, task_id: &str) -> String {
    match slot {
        Slot::Replica(n) => format!("{service}.{n}.{task_id}"),
        Slot::Node => format!("{service}.{node_id}.{task_id}"),
    }
}

/// The slot in a task's container name, given its service's name and the node ID.
pub fn slot_of(name: &str, service: &str, node_id: &str) -> Option<Slot> {
    let rest = name.strip_prefix(service)?.strip_prefix('.')?;
    let (middle, task) = rest.split_once('.')?;
    if !is_object_id(task) {
        return None;
    }
    if middle == node_id {
        return Some(Slot::Node);
    }
    match middle.parse::<u64>() {
        Ok(n) if n > 0 && !middle.starts_with('0') => Some(Slot::Replica(n)),
        _ => None,
    }
}

/// How many tasks a service wants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Replicated(u64),
    Global,
}

/// A live task as reconcile sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Live {
    pub task_id: String,
    pub slot: Slot,
    /// Made from the service's current spec.
    pub up_to_date: bool,
    /// When it was created (seconds): the newest of two in one slot stays.
    pub created: u64,
}

/// What reconcile does: the slots that get a new task, and the tasks to remove.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub start: Vec<Slot>,
    pub remove: Vec<String>,
}

/// Compare the wanted tasks with the live ones: a missing slot gets a task; a slot past
/// the count, a slot of the other mode, and the extra tasks of a slot go. In one slot the
/// up-to-date task wins, then the newest. `active` is false while the node is drained:
/// then no task is wanted.
pub fn reconcile(mode: Mode, active: bool, live: &[Live]) -> Plan {
    let wanted: Vec<Slot> = match (active, mode) {
        (false, _) => Vec::new(),
        (true, Mode::Replicated(n)) => (1..=n).map(Slot::Replica).collect(),
        (true, Mode::Global) => vec![Slot::Node],
    };
    reconcile_slots(&wanted, live)
}

/// Compare a node's assigned slots with the tasks it already owns.
pub fn reconcile_slots(wanted: &[Slot], live: &[Live]) -> Plan {
    let mut plan = Plan::default();
    for slot in wanted {
        let mut here: Vec<&Live> = live.iter().filter(|t| t.slot == *slot).collect();
        if here.is_empty() {
            plan.start.push(*slot);
            continue;
        }
        // The keeper first: up to date, then the newest.
        here.sort_by_key(|t| std::cmp::Reverse((t.up_to_date, t.created)));
        plan.remove
            .extend(here[1..].iter().map(|t| t.task_id.clone()));
    }
    plan.remove.extend(
        live.iter()
            .filter(|t| !wanted.contains(&t.slot))
            .map(|t| t.task_id.clone()),
    );
    plan
}

/// The batches of a rolling update over `slots`: `parallelism` at a time, all at once
/// for 0.
pub fn batches(slots: &[Slot], parallelism: u64) -> Vec<Vec<Slot>> {
    if slots.is_empty() {
        return Vec::new();
    }
    let size = match usize::try_from(parallelism) {
        Ok(0) | Err(_) => slots.len(),
        Ok(n) => n,
    };
    slots.chunks(size).map(<[Slot]>::to_vec).collect()
}

/// Whether failed tasks are more than the update may bear (Docker's MaxFailureRatio: 0
/// means any failure).
pub fn too_many_failures(failed: usize, updated: usize, max_ratio: f64) -> bool {
    failed > 0 && updated > 0 && failed as f64 / updated as f64 > max_ratio
}

/// What an update does when tasks fail.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailureAction {
    Pause,
    Continue,
    Rollback,
}

/// Which goes first when a task is replaced.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Order {
    StopFirst,
    StartFirst,
}

/// A service's `UpdateConfig` or `RollbackConfig`, with Docker's defaults.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct UpdatePolicy {
    pub parallelism: u64,
    pub delay: Duration,
    pub failure_action: FailureAction,
    pub monitor: Duration,
    pub max_failure_ratio: f64,
    pub order: Order,
}

impl UpdatePolicy {
    /// Read the config; missing fields take Docker's defaults (one at a time, pause, a
    /// 5 second watch, stop first).
    pub fn from_spec(config: &Value) -> UpdatePolicy {
        let nanos = |key: &str| Duration::from_nanos(config[key].as_u64().unwrap_or(0));
        let monitor = match nanos("Monitor") {
            Duration::ZERO => Duration::from_secs(5),
            m => m,
        };
        UpdatePolicy {
            parallelism: config["Parallelism"].as_u64().unwrap_or(1),
            delay: nanos("Delay"),
            failure_action: match config["FailureAction"].as_str() {
                Some("continue") => FailureAction::Continue,
                Some("rollback") => FailureAction::Rollback,
                _ => FailureAction::Pause,
            },
            monitor,
            max_failure_ratio: config["MaxFailureRatio"].as_f64().unwrap_or(0.0),
            order: match config["Order"].as_str() {
                Some("start-first") => Order::StartFirst,
                _ => Order::StopFirst,
            },
        }
    }
}

/// A task's restart policy from the service's `RestartPolicy`: `any` (the default)
/// always, `on-failure` up to MaxAttempts, `none` never.
pub fn restart_policy(policy: &Value) -> RestartPolicy {
    match policy["Condition"].as_str() {
        Some("none") => RestartPolicy::No,
        Some("on-failure") => RestartPolicy::OnFailure {
            max: policy["MaxAttempts"]
                .as_u64()
                .and_then(|m| u32::try_from(m).ok())
                .unwrap_or(0),
        },
        _ => RestartPolicy::Always,
    }
}

/// A service's mode from its spec. Jobs are not supported.
pub fn mode_of(spec: &Value) -> Option<Mode> {
    let mode = &spec["Mode"];
    if mode["Global"].is_object() {
        return Some(Mode::Global);
    }
    if mode["ReplicatedJob"].is_object() || mode["GlobalJob"].is_object() {
        return None;
    }
    Some(Mode::Replicated(
        mode["Replicated"]["Replicas"].as_u64().unwrap_or(1),
    ))
}

/// Fill what Docker fills in a new spec: one replica when the mode is missing, and the
/// `vip` endpoint mode.
pub fn with_defaults(spec: &Value) -> Value {
    let mut spec = spec.clone();
    let Some(obj) = spec.as_object_mut() else {
        return spec;
    };
    let mode = obj.entry("Mode").or_insert_with(|| serde_json::json!({}));
    if mode.as_object().is_none_or(|m| m.is_empty()) {
        *mode = serde_json::json!({"Replicated": {}});
    }
    if let Some(replicated) = mode.get_mut("Replicated").and_then(Value::as_object_mut) {
        replicated
            .entry("Replicas")
            .or_insert_with(|| Value::from(1));
    }
    let endpoint = obj
        .entry("EndpointSpec")
        .or_insert_with(|| serde_json::json!({}));
    if let Some(endpoint) = endpoint.as_object_mut() {
        let mode = endpoint.entry("Mode").or_insert(Value::Null);
        if mode.as_str().is_none_or(str::is_empty) {
            *mode = Value::from("vip");
        }
    }
    spec
}

/// Docker's template fields in a hostname, an environment value or a mount source:
/// `{{.Service.Name}}`, `{{.Task.Slot}}` and the like.
pub struct TemplateFacts<'a> {
    pub service_id: &'a str,
    pub service_name: &'a str,
    pub node_id: &'a str,
    pub node_hostname: &'a str,
    pub task_id: &'a str,
    pub task_name: &'a str,
    pub slot: u64,
}

/// Replace the template fields podbag knows; anything else stays as written.
pub fn expand(text: &str, facts: &TemplateFacts) -> String {
    let slot = facts.slot.to_string();
    let fields: [(&str, &str); 7] = [
        (".Service.ID", facts.service_id),
        (".Service.Name", facts.service_name),
        (".Node.ID", facts.node_id),
        (".Node.Hostname", facts.node_hostname),
        (".Task.ID", facts.task_id),
        (".Task.Name", facts.task_name),
        (".Task.Slot", &slot),
    ];
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            out.push_str(&rest[start..]);
            return out;
        };
        let field = after[..end].trim();
        match fields.iter().find(|(name, _)| *name == field) {
            Some((_, value)) => out.push_str(value),
            None => out.push_str(&rest[start..start + 2 + end + 2]),
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Host ports a service gets when it publishes a port without one (Docker's range).
pub const AUTO_PORTS: std::ops::RangeInclusive<u16> = 30000..=32767;

/// The published ports of an `EndpointSpec`: each with its host port, picked from
/// [`AUTO_PORTS`] when none is given (the old pick is kept). `taken` are the ports of the
/// other services, as (protocol, port). An error says why a port cannot be had.
pub fn assign_ports(
    wanted: &[Value],
    taken: &[(String, u16)],
    previous: &[PortRecord],
) -> Result<Vec<PortRecord>, String> {
    let mut out: Vec<PortRecord> = Vec::with_capacity(wanted.len());
    let used = |out: &[PortRecord], protocol: &str, port: u16| {
        taken.iter().any(|(p, n)| p == protocol && *n == port)
            || out
                .iter()
                .any(|r| r.protocol == protocol && r.published == port)
    };
    for w in wanted {
        let protocol = w["Protocol"]
            .as_str()
            .filter(|p| !p.is_empty())
            .unwrap_or("tcp");
        if !matches!(protocol, "tcp" | "udp") {
            return Err(format!(
                "protocol {protocol} is not supported (use tcp or udp)"
            ));
        }
        let target = w["TargetPort"]
            .as_u64()
            .and_then(|t| u16::try_from(t).ok())
            .filter(|t| *t > 0)
            .ok_or_else(|| "a published port needs a target port".to_string())?;
        let mode = w["PublishMode"]
            .as_str()
            .filter(|m| !m.is_empty())
            .unwrap_or("ingress");
        let given = w["PublishedPort"]
            .as_u64()
            .and_then(|p| u16::try_from(p).ok())
            .unwrap_or(0);
        let published = if given > 0 {
            if used(&out, protocol, given) {
                return Err(format!("port {given}/{protocol} is already in use"));
            }
            given
        } else {
            let old = previous
                .iter()
                .find(|r| r.protocol == protocol && r.target == target)
                .map(|r| r.published)
                .filter(|p| !used(&out, protocol, *p));
            match old {
                Some(p) => p,
                None => AUTO_PORTS
                    .clone()
                    .find(|p| !used(&out, protocol, *p))
                    .ok_or_else(|| "no free port is left to publish".to_string())?,
            }
        };
        out.push(PortRecord {
            protocol: protocol.to_string(),
            target,
            published,
            mode: mode.to_string(),
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ports_are_given_or_picked() {
        let wanted = [
            json!({"Protocol": "tcp", "TargetPort": 80, "PublishedPort": 8080}),
            json!({"TargetPort": 443}),
            json!({"Protocol": "udp", "TargetPort": 53, "PublishedPort": 30000, "PublishMode": "host"}),
        ];
        let taken = [("tcp".to_string(), 30000)];
        let got = assign_ports(&wanted, &taken, &[]).unwrap();
        let rec = |protocol: &str, target, published, mode: &str| PortRecord {
            protocol: protocol.into(),
            target,
            published,
            mode: mode.into(),
        };
        assert_eq!(
            got,
            [
                rec("tcp", 80, 8080, "ingress"),
                rec("tcp", 443, 30001, "ingress"),
                rec("udp", 53, 30000, "host"),
            ]
        );
        // The old pick stays.
        let again = assign_ports(&wanted[1..2], &[], &[rec("tcp", 443, 31234, "ingress")]).unwrap();
        assert_eq!(again, [rec("tcp", 443, 31234, "ingress")]);
        // Another service's port, the same port twice, and no target.
        let e = assign_ports(&wanted[..1], &[("tcp".into(), 8080)], &[]).unwrap_err();
        assert!(e.contains("8080"), "{e}");
        let twice = [wanted[0].clone(), wanted[0].clone()];
        assert!(assign_ports(&twice, &[], &[]).is_err());
        assert!(assign_ports(&[json!({"PublishedPort": 1})], &[], &[]).is_err());
        assert!(assign_ports(&[json!({"TargetPort": 1, "Protocol": "sctp"})], &[], &[]).is_err());
        assert_eq!(assign_ports(&[], &[], &[]).unwrap(), []);
    }

    #[test]
    fn object_ids_have_docker_form() {
        let id = object_id(&[0; ID_LEN]);
        assert_eq!(id, "0".repeat(ID_LEN));
        let id = object_id(&[255; ID_LEN]);
        assert_eq!(id.len(), ID_LEN);
        assert!(is_object_id(&id), "{id}");
        let mut bytes = [0u8; ID_LEN];
        for (i, b) in bytes.iter_mut().enumerate() {
            *b = (i * 7) as u8;
        }
        let id = object_id(&bytes);
        assert!(
            id.bytes()
                .all(|b| b.is_ascii_digit() || b.is_ascii_lowercase())
        );
        assert!(is_object_id(&id));
        assert!(!is_object_id("short"));
        assert!(!is_object_id(&"A".repeat(ID_LEN)));
        assert!(!is_object_id(&"../".repeat(9)[..ID_LEN]));
    }

    #[test]
    fn names_follow_docker() {
        for ok in ["web", "my_stack_web", "a-b", "a1", "A"] {
            assert!(valid_name(ok), "{ok}");
        }
        let long = "a".repeat(64);
        for bad in [
            "",
            "-web",
            "web-",
            "_x",
            "a b",
            "a.b",
            "a/b",
            "..",
            long.as_str(),
        ] {
            assert!(!valid_name(bad), "{bad}");
        }
        assert!(valid_name(&"a".repeat(63)));
    }

    #[test]
    fn task_names_carry_slot_or_node() {
        let task = "t".repeat(ID_LEN);
        let node = "n".repeat(ID_LEN);
        let name = task_name("web", Slot::Replica(3), &node, &task);
        assert_eq!(name, format!("web.3.{task}"));
        assert_eq!(slot_of(&name, "web", &node), Some(Slot::Replica(3)));
        let name = task_name("agent", Slot::Node, &node, &task);
        assert_eq!(name, format!("agent.{node}.{task}"));
        assert_eq!(slot_of(&name, "agent", &node), Some(Slot::Node));
        // Another service whose name starts the same, and broken names.
        assert_eq!(slot_of(&format!("web.1.{task}"), "we", &node), None);
        assert_eq!(slot_of(&format!("web.x.{task}"), "web", &node), None);
        assert_eq!(slot_of(&format!("web.0.{task}"), "web", &node), None);
        assert_eq!(slot_of("web.1", "web", &node), None);
        assert_eq!(Slot::Node.number(), 0);
        assert_eq!(Slot::Replica(2).number(), 2);
    }

    fn live(id: &str, slot: Slot, up_to_date: bool, created: u64) -> Live {
        Live {
            task_id: id.to_string(),
            slot,
            up_to_date,
            created,
        }
    }

    #[test]
    fn reconcile_starts_missing_and_removes_extra_slots() {
        use Slot::Replica as R;
        // Nothing yet: every slot.
        let plan = reconcile(Mode::Replicated(3), true, &[]);
        assert_eq!(plan.start, [R(1), R(2), R(3)]);
        assert!(plan.remove.is_empty());
        // A gap is filled; slots past the count go (scale down).
        let tasks = [
            live("a", R(1), true, 1),
            live("c", R(3), true, 1),
            live("d", R(4), true, 1),
        ];
        let plan = reconcile(Mode::Replicated(3), true, &tasks);
        assert_eq!(plan.start, [R(2)]);
        assert_eq!(plan.remove, ["d"]);
        // Two in one slot: the up-to-date one stays, then the newest.
        let tasks = [
            live("old", R(1), false, 9),
            live("new", R(1), true, 1),
            live("x", R(2), true, 1),
            live("y", R(2), true, 5),
        ];
        let plan = reconcile(Mode::Replicated(2), true, &tasks);
        assert!(plan.start.is_empty());
        let mut removed = plan.remove.clone();
        removed.sort();
        assert_eq!(removed, ["old", "x"]);
        // Outdated tasks are the update's work, not reconcile's.
        let plan = reconcile(Mode::Replicated(1), true, &[live("a", R(1), false, 1)]);
        assert_eq!(plan, Plan::default());
        // Zero replicas, or a drained node: none.
        let plan = reconcile(Mode::Replicated(0), true, &[live("a", R(1), true, 1)]);
        assert_eq!(plan.remove, ["a"]);
        let plan = reconcile(Mode::Replicated(2), false, &[live("a", R(1), true, 1)]);
        assert_eq!(plan.remove, ["a"]);
        assert!(plan.start.is_empty());
    }

    #[test]
    fn reconcile_keeps_one_global_task() {
        let plan = reconcile(Mode::Global, true, &[]);
        assert_eq!(plan.start, [Slot::Node]);
        let tasks = [
            live("g", Slot::Node, true, 1),
            live("r", Slot::Replica(1), true, 1),
        ];
        let plan = reconcile(Mode::Global, true, &tasks);
        assert!(plan.start.is_empty());
        assert_eq!(plan.remove, ["r"]);
        let plan = reconcile(Mode::Global, false, &tasks[..1]);
        assert_eq!(plan.remove, ["g"]);
        // Replicated after global: the node's task goes.
        let plan = reconcile(Mode::Replicated(1), true, &tasks[..1]);
        assert_eq!(plan.start, [Slot::Replica(1)]);
        assert_eq!(plan.remove, ["g"]);
    }

    #[test]
    fn updates_go_in_batches() {
        use Slot::Replica as R;
        let slots = [R(1), R(2), R(3), R(4), R(5)];
        assert_eq!(
            batches(&slots, 2),
            vec![vec![R(1), R(2)], vec![R(3), R(4)], vec![R(5)]]
        );
        assert_eq!(batches(&slots, 1).len(), 5);
        assert_eq!(batches(&slots, 0), vec![slots.to_vec()]);
        assert_eq!(batches(&slots, 9), vec![slots.to_vec()]);
        assert!(batches(&[], 1).is_empty());
    }

    #[test]
    fn failure_ratio() {
        assert!(!too_many_failures(0, 3, 0.0));
        assert!(too_many_failures(1, 3, 0.0));
        assert!(!too_many_failures(1, 3, 0.5));
        assert!(too_many_failures(2, 3, 0.5));
        assert!(!too_many_failures(3, 3, 1.0));
        assert!(!too_many_failures(0, 0, 0.0));
    }

    #[test]
    fn update_policy_defaults_and_fields() {
        let p = UpdatePolicy::from_spec(&Value::Null);
        assert_eq!(p.parallelism, 1);
        assert_eq!(p.delay, Duration::ZERO);
        assert_eq!(p.failure_action, FailureAction::Pause);
        assert_eq!(p.monitor, Duration::from_secs(5));
        assert_eq!(p.max_failure_ratio, 0.0);
        assert_eq!(p.order, Order::StopFirst);
        let p = UpdatePolicy::from_spec(&json!({
            "Parallelism": 0, "Delay": 2_000_000_000u64, "FailureAction": "rollback",
            "Monitor": 1_000_000_000u64, "MaxFailureRatio": 0.25, "Order": "start-first"
        }));
        assert_eq!(p.parallelism, 0);
        assert_eq!(p.delay, Duration::from_secs(2));
        assert_eq!(p.failure_action, FailureAction::Rollback);
        assert_eq!(p.monitor, Duration::from_secs(1));
        assert_eq!(p.max_failure_ratio, 0.25);
        assert_eq!(p.order, Order::StartFirst);
        let p = UpdatePolicy::from_spec(&json!({"FailureAction": "continue", "Monitor": 0}));
        assert_eq!(p.failure_action, FailureAction::Continue);
        assert_eq!(p.monitor, Duration::from_secs(5), "0 means the default");
    }

    #[test]
    fn restart_policies_map() {
        assert_eq!(restart_policy(&Value::Null), RestartPolicy::Always);
        assert_eq!(
            restart_policy(&json!({"Condition": "any"})),
            RestartPolicy::Always
        );
        assert_eq!(
            restart_policy(&json!({"Condition": "none"})),
            RestartPolicy::No
        );
        assert_eq!(
            restart_policy(&json!({"Condition": "on-failure", "MaxAttempts": 3})),
            RestartPolicy::OnFailure { max: 3 }
        );
        assert_eq!(
            restart_policy(&json!({"Condition": "on-failure"})),
            RestartPolicy::OnFailure { max: 0 }
        );
    }

    #[test]
    fn modes_and_defaults() {
        let spec = with_defaults(&json!({"Name": "web"}));
        assert_eq!(spec["Mode"], json!({"Replicated": {"Replicas": 1}}));
        assert_eq!(spec["EndpointSpec"]["Mode"], "vip");
        assert_eq!(mode_of(&spec), Some(Mode::Replicated(1)));
        let spec = with_defaults(&json!({"Mode": {"Replicated": {}}}));
        assert_eq!(mode_of(&spec), Some(Mode::Replicated(1)));
        let spec =
            json!({"Mode": {"Replicated": {"Replicas": 4}}, "EndpointSpec": {"Mode": "dnsrr"}});
        assert_eq!(with_defaults(&spec), spec, "what is given stays");
        assert_eq!(mode_of(&spec), Some(Mode::Replicated(4)));
        assert_eq!(
            mode_of(&json!({"Mode": {"Global": {}}})),
            Some(Mode::Global)
        );
        assert_eq!(mode_of(&json!({"Mode": {"ReplicatedJob": {}}})), None);
        assert_eq!(mode_of(&json!({"Mode": {"GlobalJob": {}}})), None);
    }

    #[test]
    fn templates_expand() {
        let facts = TemplateFacts {
            service_id: "sid",
            service_name: "web",
            node_id: "nid",
            node_hostname: "host",
            task_id: "tid",
            task_name: "web.2.tid",
            slot: 2,
        };
        assert_eq!(expand("{{.Service.Name}}-{{.Task.Slot}}", &facts), "web-2");
        assert_eq!(
            expand(
                "{{ .Node.Hostname }}/{{.Node.ID}}/{{.Service.ID}}/{{.Task.ID}}/{{.Task.Name}}",
                &facts
            ),
            "host/nid/sid/tid/web.2.tid"
        );
        assert_eq!(expand("plain {{.Other}}", &facts), "plain {{.Other}}");
    }
}
