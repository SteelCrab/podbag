//! `network create|ls|rm|inspect`.

use crate::args::NetworkCmd;
use crate::image::report;
use podbag::network::Network;

pub fn run(cmd: NetworkCmd) -> i32 {
    match cmd {
        NetworkCmd::Create {
            subnet,
            ipv6,
            subnet6,
            labels,
            name,
        } => match podbag::network::parse_labels(&labels).and_then(|labels| {
            if ipv6 || subnet6.is_some() {
                Network::create_dual_stack(&name, subnet.as_deref(), subnet6, labels)
            } else {
                Network::create_with(&name, subnet.as_deref(), labels)
            }
        }) {
            Ok(n) => {
                println!("{}", n.id);
                0
            }
            Err(e) => report(&e),
        },
        NetworkCmd::Ls => match Network::list() {
            Ok(list) => {
                let mut rows: Vec<[String; 4]> = list
                    .iter()
                    .map(|n| {
                        [
                            n.id.clone(),
                            n.name.clone(),
                            "bridge".into(),
                            "local".into(),
                        ]
                    })
                    .collect();
                // The two modes Docker also lists as networks.
                for (name, driver) in [("host", "host"), ("none", "null")] {
                    rows.push([pseudo_id(name), name.into(), driver.into(), "local".into()]);
                }
                rows.sort_by(|a, b| a[1].cmp(&b[1]));
                println!("{:<15}{:<20}{:<10}SCOPE", "NETWORK ID", "NAME", "DRIVER");
                for [id, name, driver, scope] in rows {
                    println!("{id:<15}{name:<20}{driver:<10}{scope}");
                }
                0
            }
            Err(e) => report(&e),
        },
        NetworkCmd::Rm { names } => {
            let mut code = 0;
            for name in names {
                match Network::remove(&name) {
                    Ok(()) => println!("{name}"),
                    Err(e) => code = report(&e),
                }
            }
            code
        }
        #[cfg(unix)]
        NetworkCmd::Connect {
            aliases,
            ip,
            ip6,
            network,
            container,
        } => match podbag::ContainerHandle::get(&container).and_then(|h| {
            h.connect_with_addresses(
                &network,
                &aliases,
                podbag::network::FixedAddresses {
                    ipv4: ip,
                    ipv6: ip6,
                },
            )
        }) {
            Ok(()) => 0,
            Err(e) => report(&e),
        },
        #[cfg(unix)]
        NetworkCmd::Disconnect { network, container } => {
            match podbag::ContainerHandle::get(&container).and_then(|h| h.disconnect(&network)) {
                Ok(()) => 0,
                Err(e) => report(&e),
            }
        }
        // Containers need Linux.
        #[cfg(not(unix))]
        NetworkCmd::Connect { .. } | NetworkCmd::Disconnect { .. } => crate::unsupported(),
        NetworkCmd::Inspect { name } => match Network::get(&name) {
            Ok(n) => {
                println!("{}", serde_json::to_string_pretty(&n).unwrap_or_default());
                0
            }
            Err(e) => report(&e),
        },
    }
}

/// A stable ID for the `host` and `none` rows.
fn pseudo_id(name: &str) -> String {
    format!(
        "{:0>12}",
        name.bytes().map(|b| format!("{b:02x}")).collect::<String>()
    )
    .chars()
    .take(12)
    .collect()
}
