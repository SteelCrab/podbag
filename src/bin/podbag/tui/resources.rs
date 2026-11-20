//! Engine selection, registry access, resource inspection and verified transfers.
use super::dialog::{Error, Result, Screen};
use podbag::client::{self, Choice, Client, Kind, Location, Target};
use podbag::dashboard::{
    cluster_migration::{self, Scope},
    kubernetes::{self, Resource},
};
use serde_json::Value;
use std::path::Path;

pub fn engine(ui: &mut Screen, title: &str, exclude: Option<&Choice>) -> Result<Choice> {
    engine_picker(ui, title, exclude, false)?.ok_or(Error::Back)
}

fn engine_picker(
    ui: &mut Screen,
    title: &str,
    exclude: Option<&Choice>,
    include_all: bool,
) -> Result<Option<Choice>> {
    let mut choices = client::choice::engine_choices();
    choices.retain(|c| Some(c) != exclude);
    let mut labels: Vec<_> = choices.iter().map(Choice::to_setting).collect();
    if cfg!(unix) {
        labels.push(ui.t("engine.socket"))
    }
    if include_all {
        labels.insert(0, ui.t("migration.allEngines"));
    }
    let index = ui.pick(&ui.t(title), &labels)?;
    let Some(i) = index.checked_sub(usize::from(include_all)) else {
        return Ok(None);
    };
    if let Some(c) = choices.get(i) {
        return Ok(Some(c.clone()));
    }
    let value = ui.input(&ui.t("settings.socket"), "unix://", false, false)?;
    match Choice::parse(&value) {
        Some(c @ Choice::Socket(_)) if Some(&c) != exclude => Ok(Some(c)),
        _ => Err(Error::Failed(
            "Enter a different absolute Unix socket path".into(),
        )),
    }
}

pub fn volume(ui: &mut Screen, client: &Client) -> Result<()> {
    let title = ui.t("volume.create");
    let name = ui.input(&title, "", false, false)?;
    if name.trim().is_empty() {
        return Err(Error::Failed("Enter a volume name".into()));
    }
    ui.work(&title, |_| {
        client.create_volume(name.trim()).map_err(|e| e.message)
    })
}

pub(super) fn saved_registry_host(place: &Location) -> String {
    crate::preferences::registry_hosts()[place.id()]
        .as_str()
        .unwrap_or("")
        .into()
}

pub(super) fn save_registry_host(place: &Location) -> std::result::Result<(), String> {
    if !place.needs_host() || place.host().is_empty() || saved_registry_host(place) == place.host()
    {
        return Ok(());
    }
    crate::preferences::save_registry_host(place)
}

pub fn choose_registry(ui: &mut Screen) -> Result<Location> {
    let locations = Location::all();
    let hosts = crate::preferences::registry_hosts();
    let labels = locations
        .iter()
        .map(|l| {
            let alias = match l.id() {
                "alibaba" => "Aliyun 阿里云 阿里雲 알리바바 アリババクラウド",
                "acr" => "Microsoft Azure ACR 애저 에저",
                _ => "",
            };
            let host = hosts[l.id()]
                .as_str()
                .unwrap_or_else(|| if l.needs_host() { l.host_example() } else { "" });
            format!("{}   {} {host} {alias}", l.label(), l.host())
        })
        .collect::<Vec<_>>();
    let mut place = locations[ui.pick(&ui.t("pull.location"), &labels)?].clone();
    if place.needs_host() {
        let host = ui.input(
            &format!("{} · {}", place.label(), place.host_example()),
            hosts[place.id()].as_str().unwrap_or(""),
            false,
            false,
        )?;
        if host.trim().is_empty() {
            return Err(Error::Failed("Enter a registry host".into()));
        }
        place = place.with_host(host.trim());
        save_registry_host(&place)?;
    }
    Ok(place)
}
pub fn registry(ui: &mut Screen, exe: &Path) -> Result<Location> {
    let place = choose_registry(ui)?;
    login(ui, exe, &place)?;
    Ok(place)
}
pub(super) fn login(ui: &mut Screen, exe: &Path, place: &Location) -> Result<()> {
    if place.needs_host() && place.host().is_empty() {
        return Err(Error::Failed(
            ui.t("pull.host").replace("{name}", &place.label()),
        ));
    }
    if let Some(hint) = place.login_hint() {
        ui.message(&place.label(), &hint)?;
    } else {
        let server = if *place == Location::DockerHub {
            String::new()
        } else {
            place.host()
        };
        let user = ui.input(&ui.t("pull.user"), "", false, false)?;
        let password = ui.input(&ui.t("pull.password"), "", true, false)?;
        if user.trim().is_empty() || password.is_empty() {
            return Err(Error::Failed(
                "Enter the account and password or token".into(),
            ));
        }
        ui.work(&ui.t("pull.login"), |_| {
            client::login(
                exe,
                client::find_docker_cli().as_deref(),
                &server,
                user.trim(),
                &password,
            )
        })?;
    }
    Ok(())
}

pub fn import(ui: &mut Screen, exe: &Path, to: &Target) -> Result<String> {
    if to.choice != Choice::Podbag {
        return Err(Error::Failed(
            "Select podbag as the destination engine".into(),
        ));
    }
    let from = Client::new(Target::find(Some(&Choice::Docker), exe)?.endpoint);
    if from.endpoint.same_target(&to.endpoint) {
        return Err(Error::Failed("Docker already points to this engine".into()));
    }
    let destination = Client::new(to.endpoint.clone());
    let tags = ui.work(&ui.t("import.title"), |_| {
        podbag::dashboard::importable_images(&from, &destination).map_err(|e| e.message)
    })?;
    if tags.is_empty() {
        return Err(Error::Failed(ui.t("import.none")));
    }
    let labels: Vec<_> = tags
        .iter()
        .map(|image| {
            let tag = &image.tag;
            let detail = if image.present {
                ui.t("import.have")
            } else {
                super::app::size(image.size)
            };
            format!("{tag}   {}   {detail}", Location::of(tag).label())
        })
        .collect();
    let names: Vec<_> = ui
        .select(&ui.t("import.title"), &labels, true)?
        .into_iter()
        .map(|i| tags[i].tag.clone())
        .collect();
    ui.confirm(
        &format!("Docker → podbag   {}", names.join(", ")),
        "import.copy",
    )?;
    ui.work(&ui.t("import.copy"), |_| {
        destination.import_images(&from, &names)
    })?;
    Ok(ui.t("import.done").replace("{n}", &names.len().to_string()))
}

struct MigrationItem {
    label: String,
    id: String,
    image_id: String,
}

fn migration_items(from: &Client, kind: Kind) -> std::result::Result<Vec<MigrationItem>, String> {
    Ok(match kind {
        Kind::Containers => from
            .containers()
            .map_err(|e| e.message)?
            .into_iter()
            .map(|c| MigrationItem {
                label: c.name().into(),
                id: c.id,
                image_id: String::new(),
            })
            .collect(),
        Kind::Volumes => from
            .volumes()
            .map_err(|e| e.message)?
            .into_iter()
            .map(|v| MigrationItem {
                label: v.name.clone(),
                id: v.name,
                image_id: String::new(),
            })
            .collect(),
        Kind::Images => from
            .images()
            .map_err(|e| e.message)?
            .into_iter()
            .flat_map(|i| {
                let tags: Vec<_> = i
                    .repo_tags
                    .iter()
                    .filter(|t| t.as_str() != "<none>:<none>")
                    .cloned()
                    .collect();
                let names = if tags.is_empty() {
                    vec![i.id.clone()]
                } else {
                    tags
                };
                names.into_iter().map(move |name| MigrationItem {
                    label: name.clone(),
                    id: name,
                    image_id: i.id.clone(),
                })
            })
            .collect(),
        Kind::Networks => unreachable!(),
    })
}

pub fn migrate_engine(ui: &mut Screen, exe: &Path) -> Result<()> {
    let source = engine_picker(ui, "migration.from", None, true)?;
    let index = ui.menu(
        "migration.kind",
        &["nav.containers", "nav.images", "nav.volumes"],
    )?;
    let kind = [Kind::Containers, Kind::Images, Kind::Volumes][index];
    let (items, errors) = ui.work(&ui.t("migration.resource"), |_| {
        let targets = match &source {
            Some(choice) => vec![Target::find(Some(choice), exe)?],
            None => podbag::dashboard::local_engines(exe),
        };
        let mut items = Vec::new();
        let mut errors = Vec::new();
        for target in targets {
            match migration_items(&Client::new(target.endpoint.clone()), kind) {
                Ok(rows) => items.extend(rows.into_iter().map(|row| (target.clone(), row))),
                Err(e) => errors.push(format!("{} · {e}", target.choice.label())),
            }
        }
        Ok((items, errors))
    })?;
    if !errors.is_empty() {
        ui.message(&ui.t("migration.unavailable"), &errors.join("\n"))?;
    }
    if items.is_empty() {
        return ui.message(&ui.t("migration.resource"), &ui.t("migration.empty"));
    }
    let labels: Vec<_> = items
        .iter()
        .map(|(target, item)| format!("{}   {}", item.label, target.choice.label()))
        .collect();
    let (target, item) = &items[ui.pick(&ui.t("migration.resource"), &labels)?];
    let source = &target.choice;
    let from = Client::new(target.endpoint.clone());
    let MigrationItem {
        label: name,
        id,
        image_id: expected,
    } = item;
    let dst = engine(ui, "migration.to", Some(source))?;
    let to = Client::new(Target::find(Some(&dst), exe)?.endpoint);
    let defaults = kind == Kind::Containers
        && ui.pick(&ui.t("migration.defaults"), &["[ ]".into(), "[✓]".into()])? == 1;
    ui.confirm(
        &format!("{name}   {} → {}", source.to_setting(), dst.to_setting()),
        "migration.start",
    )?;
    ui.work(&ui.t("migration.running"), |log| {
        if !expected.is_empty()
            && from.inspect(kind, id).map_err(|e| e.message)?["Id"] != expected.as_str()
        {
            return Err("The image tag changed. Select it again".into());
        }
        client::migration::migrate(&from, &to, kind, id, defaults, |p| {
            log(&format!("migration.{p}"))
        })
    })?;
    ui.message(&ui.t("migration.done"), &ui.t("migration.verified"))
}

fn list(value: &Value) -> std::slice::Iter<'_, Value> {
    value
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
}

fn object_label(obj: &Value) -> String {
    let name = obj["metadata"]["name"].as_str().unwrap_or("");
    let ns = obj["metadata"]["namespace"].as_str().unwrap_or("");
    let status = &obj["status"];
    let spec = &obj["spec"];
    let fields = match obj["kind"].as_str() {
        Some("Deployment") => format!(
            "{}/{}  {}",
            status["readyReplicas"].as_u64().unwrap_or(0),
            spec["replicas"].as_u64().unwrap_or(1),
            obj.pointer("/spec/template/spec/containers")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|v| v["image"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Some("Pod") => {
            let containers = list(&spec["containers"])
                .chain(list(&spec["initContainers"]).filter(|c| c["restartPolicy"] == "Always"));
            let ready: std::collections::HashSet<_> = list(&status["containerStatuses"])
                .chain(list(&status["initContainerStatuses"]))
                .filter(|c| c["ready"] == true)
                .filter_map(|c| c["name"].as_str())
                .collect();
            let total = containers.clone().count();
            let count = containers
                .filter_map(|c| c["name"].as_str())
                .filter(|n| ready.contains(n))
                .count();
            format!(
                "{}  {count}/{total}  {}",
                status["phase"].as_str().unwrap_or(""),
                spec["nodeName"].as_str().unwrap_or("")
            )
        }
        Some("Service") => format!(
            "{}  {}  {}",
            spec["type"].as_str().unwrap_or(""),
            spec["externalName"]
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| {
                    let ips = list(&spec["clusterIPs"])
                        .filter_map(Value::as_str)
                        .collect::<Vec<_>>();
                    if ips.is_empty() {
                        spec["clusterIP"].as_str().unwrap_or("").into()
                    } else {
                        ips.join(", ")
                    }
                }),
            spec["ports"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|p| format!(
                    "{}{}/{}",
                    p["port"],
                    p["nodePort"]
                        .as_u64()
                        .map(|n| format!(":{n}"))
                        .unwrap_or_default(),
                    p["protocol"].as_str().unwrap_or("TCP")
                ))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        Some("PersistentVolumeClaim") => format!(
            "{}  {}  {}",
            status["phase"].as_str().unwrap_or(""),
            status["capacity"]["storage"].as_str().unwrap_or(""),
            spec["storageClassName"].as_str().unwrap_or("")
        ),
        _ => format!(
            "{}  {}",
            status["phase"].as_str().unwrap_or(""),
            spec["nodeName"].as_str().unwrap_or("")
        ),
    };
    format!("{ns}/{name}   {fields}")
}

pub fn cluster(ui: &mut Screen, migrate: bool) -> Result<()> {
    let contexts = ui.work(&ui.t("kube.cluster"), |_| kubernetes::kube_contexts())?;
    let mut choices = vec![ui.t("kube.current")];
    choices.extend(contexts.clone());
    let idx = ui.pick(&ui.t("kube.cluster"), &choices)?;
    let selected = idx.checked_sub(1).map(|i| contexts[i].clone());
    let all = ui.menu("kube.namespace", &["kube.current", "kube.allNamespaces"])? == 1;
    let counts = ui
        .work(&ui.t("kube.resources"), |_| {
            kubernetes::kube_counts(selected.clone(), all)
        })
        .ok();
    let kinds = [
        ("pods", Resource::Pods),
        ("services", Resource::Services),
        ("deployments", Resource::Deployments),
        ("claims", Resource::Claims),
    ];
    let labels = kinds
        .iter()
        .map(|(key, _)| {
            let label = ui.t(&format!("kube.{key}"));
            counts
                .as_ref()
                .and_then(|c| c.get(*key))
                .map_or(label.clone(), |n| format!("{label} ({n})"))
        })
        .collect::<Vec<_>>();
    let (key, kind) = kinds[ui.pick(&ui.t("kube.resources"), &labels)?];
    let mut namespace = None;
    loop {
        let mut snap = ui.work(&ui.t("kube.resources"), |_| {
            kubernetes::kube_list(kind, all, selected.clone())
        })?;
        if migrate && all {
            if namespace.is_none() {
                let mut names = vec![ui.t("kube.allNamespaces")];
                names.extend(
                    snap.items
                        .iter()
                        .filter_map(|item| item["metadata"]["namespace"].as_str())
                        .map(str::to_owned)
                        .collect::<std::collections::BTreeSet<_>>(),
                );
                let index = ui.pick(&ui.t("kube.namespace"), &names)?;
                namespace = Some(if index == 0 {
                    String::new()
                } else {
                    names.swap_remove(index)
                });
            }
            if let Some(ns) = namespace.as_deref().filter(|n| !n.is_empty()) {
                snap.items
                    .retain(|item| item["metadata"]["namespace"] == ns);
            }
        }
        let labels: Vec<_> = snap
            .items
            .iter()
            .map(object_label)
            .chain(std::iter::once(ui.t("action.refresh")))
            .collect();
        let i = ui.pick(
            &format!("{} · {}", snap.context, ui.t(&format!("kube.{key}"))),
            &labels,
        )?;
        let Some(obj) = snap.items.get(i) else {
            continue;
        };
        if !migrate {
            let json = serde_json::to_string_pretty(obj).map_err(|e| e.to_string())?;
            match ui.message(&labels[i], &json) {
                Ok(()) | Err(Error::Back) => continue,
                Err(e) => return Err(e),
            }
        }
        let dsts: Vec<_> = contexts
            .iter()
            .filter(|c| **c != snap.context)
            .cloned()
            .collect();
        let dst = &dsts[ui.pick(&ui.t("migration.to"), &dsts)?];
        let ns = obj["metadata"]["namespace"]
            .as_str()
            .unwrap_or(&snap.namespace);
        let dest_ns = ui.input(&ui.t("migration.namespace"), ns, false, false)?;
        let name = obj["metadata"]["name"].as_str().unwrap_or("");
        let uid = obj["metadata"]["uid"].as_str().unwrap_or("");
        ui.confirm(
            &format!("{name}   {}/{ns} → {dst}/{dest_ns}", snap.context),
            "migration.start",
        )?;
        ui.work(&ui.t("migration.running"), |log| {
            cluster_migration::migrate(
                Scope {
                    context: &snap.context,
                    namespace: ns,
                },
                Scope {
                    context: dst,
                    namespace: &dest_ns,
                },
                key,
                name,
                Some(uid),
                |s| log(&format!("migration.{s}")),
            )
        })?;
        return ui.message(&ui.t("migration.done"), &ui.t("migration.verified"));
    }
}
