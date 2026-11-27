//! A new container from a form, like `docker run -d`: what the user gave, checked, as the
//! body of `POST /containers/create`. Only the Docker API, so it works with any engine.

use super::{ApiError, Client, Kind, Result, encode};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value, json};

/// A published port: `host:container/protocol`. Host port 0: the engine chooses one.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct PublishedPort {
    pub host: u16,
    pub container: u16,
    /// `tcp` or `udp` (empty: `tcp`).
    pub protocol: String,
}

impl PublishedPort {
    fn protocol(&self) -> &str {
        if self.protocol.is_empty() {
            "tcp"
        } else {
            &self.protocol
        }
    }
}

/// A named volume or a host folder, at a path in the container.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct VolumeMount {
    /// A volume name (`data`) or a full host path (`/Users/me/site`, `C:\site`).
    pub source: String,
    /// A full path in the container (`/data`).
    pub target: String,
    pub read_only: bool,
}

impl VolumeMount {
    /// A host path, not a volume name.
    pub fn is_host_path(&self) -> bool {
        is_host_path(self.source.trim())
    }
}

/// What to run.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct RunSpec {
    pub image: String,
    /// Empty: the engine gives a name.
    pub name: String,
    pub ports: Vec<PublishedPort>,
    /// `KEY=VALUE`.
    pub env: Vec<String>,
    pub volumes: Vec<VolumeMount>,
    /// The command and its arguments. Empty: the image's command.
    pub command: Vec<String>,
    /// `no` (or empty), `on-failure`, `always` or `unless-stopped`.
    pub restart: String,
    /// Remove the container when it stops (`--rm`).
    pub auto_remove: bool,
}

pub const RESTART_POLICIES: [&str; 4] = ["no", "on-failure", "always", "unless-stopped"];

fn is_host_path(s: &str) -> bool {
    let b = s.as_bytes();
    s.starts_with('/')
        || s.starts_with("\\\\")
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'\\' | b'/'))
}

/// A name as Docker allows it: a letter or digit, then letters, digits, `_`, `.` or `-`.
/// `min` is the shortest length.
fn plain_name(s: &str, min: usize) -> bool {
    s.len() >= min
        && s.starts_with(|c: char| c.is_ascii_alphanumeric())
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

impl RunSpec {
    fn restart(&self) -> &str {
        if self.restart.is_empty() {
            "no"
        } else {
            &self.restart
        }
    }

    /// Find what is wrong before anything is sent.
    pub fn check(&self) -> std::result::Result<(), String> {
        let image = self.image.trim();
        if image.is_empty() || image.chars().any(char::is_whitespace) {
            return Err(format!("{image:?} is not an image name"));
        }
        let name = self.name.trim();
        if !name.is_empty() && !plain_name(name, 2) {
            return Err(format!(
                "{name:?} is not a container name: use 2 or more letters, digits, _ . or -, starting with a letter or digit"
            ));
        }
        let mut hosts = Vec::new();
        for p in &self.ports {
            if p.container == 0 {
                return Err("a container port is a number from 1 to 65535".into());
            }
            if !matches!(p.protocol(), "tcp" | "udp") {
                return Err(format!("protocol {} is not tcp or udp", p.protocol));
            }
            if p.host != 0 {
                let key = (p.host, p.protocol());
                if hosts.contains(&key) {
                    return Err(format!("host port {}/{} is used twice", key.0, key.1));
                }
                hosts.push(key);
            }
        }
        for kv in &self.env {
            match kv.split_once('=') {
                Some((key, _)) if !key.is_empty() && !key.chars().any(char::is_whitespace) => {}
                _ => return Err(format!("{kv:?} is not KEY=VALUE")),
            }
        }
        for v in &self.volumes {
            let (source, target) = (v.source.trim(), v.target.trim());
            if !is_host_path(source) && !plain_name(source, 1) {
                return Err(format!(
                    "{source:?} is not a volume name or a full host path"
                ));
            }
            if !target.starts_with('/') || target == "/" {
                return Err(format!(
                    "{target:?} is not a full container path, like /data"
                ));
            }
        }
        if !RESTART_POLICIES.contains(&self.restart()) {
            return Err(format!("unknown restart policy {}", self.restart));
        }
        if self.auto_remove && self.restart() != "no" {
            return Err("remove when it stops works only with the restart policy no".into());
        }
        Ok(())
    }

    /// The body of `POST /containers/create`.
    pub fn config(&self) -> Value {
        let mut exposed = Map::new();
        let mut bindings = Map::new();
        for p in &self.ports {
            let key = format!("{}/{}", p.container, p.protocol());
            exposed.insert(key.clone(), json!({}));
            let host = if p.host == 0 {
                String::new()
            } else {
                p.host.to_string()
            };
            let list = bindings.entry(key).or_insert_with(|| json!([]));
            if let Some(list) = list.as_array_mut() {
                list.push(json!({"HostIp": "", "HostPort": host}));
            }
        }
        let mounts: Vec<Value> = self
            .volumes
            .iter()
            .map(|v| {
                json!({
                    "Type": if v.is_host_path() { "bind" } else { "volume" },
                    "Source": v.source.trim(),
                    "Target": v.target.trim(),
                    "ReadOnly": v.read_only,
                })
            })
            .collect();
        let mut body = json!({
            "Image": self.image.trim(),
            "Env": self.env,
            "ExposedPorts": exposed,
            "HostConfig": {
                "PortBindings": bindings,
                "Mounts": mounts,
                "RestartPolicy": {"Name": self.restart(), "MaximumRetryCount": 0},
                "AutoRemove": self.auto_remove,
            },
        });
        if !self.command.is_empty() {
            body["Cmd"] = json!(self.command);
        }
        body
    }
}

impl Client {
    /// Create and start a container (`docker run -d`). Gives its id. When the start fails,
    /// the new container is removed again, so its name stays free for another try.
    pub fn run_container(&self, spec: &RunSpec) -> Result<String> {
        spec.check().map_err(|message| ApiError {
            status: 400,
            message,
        })?;
        let mut path = String::from("/containers/create");
        let name = spec.name.trim();
        if !name.is_empty() {
            path.push_str(&format!("?name={}", encode(name)));
        }
        let created = self.call("POST", &path, Some(&spec.config()))?;
        let id = created["Id"]
            .as_str()
            .filter(|id| !id.is_empty())
            .ok_or_else(|| ApiError {
                status: 0,
                message: "the engine gave no container id".into(),
            })?
            .to_string();
        if let Err(e) = self.container_action(&id, "start") {
            let _ = self.remove(Kind::Containers, &id, true);
            return Err(e);
        }
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(image: &str) -> RunSpec {
        RunSpec {
            image: image.into(),
            ..RunSpec::default()
        }
    }

    #[test]
    fn a_bare_image_uses_the_image_defaults() {
        let body = spec(" nginx:alpine ").config();
        assert_eq!(
            body,
            json!({
                "Image": "nginx:alpine",
                "Env": [],
                "ExposedPorts": {},
                "HostConfig": {
                    "PortBindings": {},
                    "Mounts": [],
                    "RestartPolicy": {"Name": "no", "MaximumRetryCount": 0},
                    "AutoRemove": false,
                },
            })
        );
        assert!(
            body.get("Cmd").is_none(),
            "no Cmd: the image's command runs"
        );
    }

    #[test]
    fn every_field_goes_into_the_request() {
        let s = RunSpec {
            image: "nginx:alpine".into(),
            name: "web".into(),
            ports: vec![
                PublishedPort {
                    host: 8080,
                    container: 80,
                    protocol: "tcp".into(),
                },
                PublishedPort {
                    host: 8443,
                    container: 80,
                    protocol: String::new(),
                },
                PublishedPort {
                    host: 0,
                    container: 53,
                    protocol: "udp".into(),
                },
            ],
            env: vec!["MODE=dev".into(), "EMPTY=".into(), "URL=a=b".into()],
            volumes: vec![
                VolumeMount {
                    source: "site".into(),
                    target: "/usr/share/nginx/html".into(),
                    read_only: true,
                },
                VolumeMount {
                    source: "/Users/me/logs".into(),
                    target: "/var/log/nginx".into(),
                    read_only: false,
                },
                VolumeMount {
                    source: "C:\\site".into(),
                    target: "/srv".into(),
                    read_only: false,
                },
            ],
            command: vec!["nginx".into(), "-g".into(), "daemon off;".into()],
            restart: "unless-stopped".into(),
            auto_remove: false,
        };
        assert_eq!(s.check(), Ok(()));
        assert_eq!(
            s.config(),
            json!({
                "Image": "nginx:alpine",
                "Env": ["MODE=dev", "EMPTY=", "URL=a=b"],
                "Cmd": ["nginx", "-g", "daemon off;"],
                "ExposedPorts": {"80/tcp": {}, "53/udp": {}},
                "HostConfig": {
                    "PortBindings": {
                        "80/tcp": [{"HostIp": "", "HostPort": "8080"}, {"HostIp": "", "HostPort": "8443"}],
                        "53/udp": [{"HostIp": "", "HostPort": ""}],
                    },
                    "Mounts": [
                        {"Type": "volume", "Source": "site", "Target": "/usr/share/nginx/html", "ReadOnly": true},
                        {"Type": "bind", "Source": "/Users/me/logs", "Target": "/var/log/nginx", "ReadOnly": false},
                        {"Type": "bind", "Source": "C:\\site", "Target": "/srv", "ReadOnly": false},
                    ],
                    "RestartPolicy": {"Name": "unless-stopped", "MaximumRetryCount": 0},
                    "AutoRemove": false,
                },
            })
        );
    }

    #[test]
    fn auto_remove_and_restart_policies() {
        let mut s = spec("alpine");
        s.auto_remove = true;
        assert_eq!(s.check(), Ok(()));
        assert_eq!(s.config()["HostConfig"]["AutoRemove"], true);
        for policy in RESTART_POLICIES {
            let mut s = spec("alpine");
            s.restart = policy.into();
            assert_eq!(s.check(), Ok(()), "{policy}");
            assert_eq!(s.config()["HostConfig"]["RestartPolicy"]["Name"], policy);
        }
        s.restart = "always".into();
        assert!(s.check().unwrap_err().contains("restart policy no"));
        s.restart = "sometimes".into();
        s.auto_remove = false;
        assert!(s.check().unwrap_err().contains("sometimes"));
    }

    #[test]
    fn bad_input_is_refused() {
        let bad = |change: fn(&mut RunSpec)| {
            let mut s = spec("nginx");
            change(&mut s);
            s.check().unwrap_err()
        };
        assert!(bad(|s| s.image = "  ".into()).contains("image"));
        assert!(bad(|s| s.image = "my image".into()).contains("image"));
        assert!(bad(|s| s.name = "a".into()).contains("container name"));
        assert!(bad(|s| s.name = "-web".into()).contains("container name"));
        assert!(bad(|s| s.name = "web app".into()).contains("container name"));
        assert!(bad(|s| s.ports = vec![PublishedPort::default()]).contains("container port"));
        assert!(
            bad(|s| s.ports = vec![PublishedPort {
                host: 1,
                container: 1,
                protocol: "sctp".into()
            }])
            .contains("sctp")
        );
        fn twice(host: u16) -> PublishedPort {
            PublishedPort {
                host,
                container: 80,
                protocol: "tcp".into(),
            }
        }
        assert!(bad(|s| s.ports = vec![twice(8080), twice(8080)]).contains("8080/tcp"));
        assert!(bad(|s| s.env = vec!["MODE".into()]).contains("KEY=VALUE"));
        assert!(bad(|s| s.env = vec!["=x".into()]).contains("KEY=VALUE"));
        assert!(bad(|s| s.env = vec!["MY KEY=x".into()]).contains("KEY=VALUE"));
        let volume = |source: &str, target: &str| VolumeMount {
            source: source.into(),
            target: target.into(),
            read_only: false,
        };
        for source in ["", "./site", "~/site", "site/data"] {
            let mut s = spec("nginx");
            s.volumes = vec![volume(source, "/data")];
            assert!(s.check().unwrap_err().contains("volume name"), "{source:?}");
        }
        for target in ["", "data", "/"] {
            let mut s = spec("nginx");
            s.volumes = vec![volume("data", target)];
            assert!(
                s.check().unwrap_err().contains("container path"),
                "{target:?}"
            );
        }
        // A port may be published more than once: on any host port, or with another protocol.
        let mut s = spec("nginx");
        s.ports = vec![twice(0), twice(0), twice(8080), {
            let mut p = twice(8080);
            p.protocol = "udp".into();
            p
        }];
        assert_eq!(s.check(), Ok(()));
    }
}
