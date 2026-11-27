use super::{Result, json};
use clap::{Subcommand, ValueEnum};
use podbag::dashboard::{
    cluster_migration::{self, Scope},
    kubernetes::{self, Resource},
};

#[derive(clap::Args)]
pub struct Args {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    #[value(alias = "pods")]
    Pod,
    #[value(alias = "services")]
    Service,
    #[value(alias = "deployments")]
    Deployment,
    #[value(alias = "pvc", alias = "claims", alias = "persistentvolumeclaim")]
    Claim,
    /// OpenShift only
    #[value(alias = "routes")]
    Route,
}

impl From<Kind> for Resource {
    fn from(kind: Kind) -> Self {
        match kind {
            Kind::Pod => Self::Pods,
            Kind::Service => Self::Services,
            Kind::Deployment => Self::Deployments,
            Kind::Claim => Self::Claims,
            Kind::Route => Self::Routes,
        }
    }
}

#[derive(clap::Args)]
struct Selection {
    /// Kubeconfig context. Defaults to the current context
    #[arg(long)]
    context: Option<String>,
    /// Namespace. Defaults to the namespace of the selected context
    #[arg(short, long)]
    namespace: Option<String>,
}

#[derive(Subcommand)]
enum Cmd {
    /// List kubeconfig contexts as JSON
    Contexts,
    /// Count each resource type as JSON. Routes are counted only on OpenShift
    Counts {
        #[arg(long)]
        context: Option<String>,
        #[arg(short = 'A', long)]
        all_namespaces: bool,
    },
    /// List resources as JSON
    #[command(alias = "ls")]
    List {
        kind: Kind,
        #[command(flatten)]
        scope: Selection,
        #[arg(short = 'A', long, conflicts_with = "namespace")]
        all_namespaces: bool,
    },
    /// Print the JSON of one resource
    Inspect {
        kind: Kind,
        name: String,
        #[command(flatten)]
        scope: Selection,
    },
    /// Copy a resource to a different context and verify the destination
    Migrate {
        kind: Kind,
        name: String,
        /// Source kubeconfig context
        #[arg(long)]
        from: String,
        /// Destination kubeconfig context
        #[arg(long)]
        to: String,
        /// Source namespace. Defaults to the source context's namespace
        #[arg(long)]
        from_namespace: Option<String>,
        /// Destination namespace. Defaults to the destination context's namespace
        #[arg(long)]
        to_namespace: Option<String>,
        /// Require the source UID to match a previous inspection
        #[arg(long)]
        uid: Option<String>,
    },
}

pub fn run(args: Args) -> Result {
    match args.cmd {
        Cmd::Contexts => json(kubernetes::kube_contexts()?),
        Cmd::Counts {
            context,
            all_namespaces,
        } => json(kubernetes::kube_counts(context, all_namespaces)?),
        Cmd::List {
            kind,
            scope,
            all_namespaces,
        } => json(kubernetes::kube_list_in(
            kind.into(),
            all_namespaces,
            scope.context.as_deref(),
            scope.namespace.as_deref(),
        )?),
        Cmd::Inspect { kind, name, scope } => json(kubernetes::kube_inspect(
            kind.into(),
            &name,
            scope.context.as_deref(),
            scope.namespace.as_deref(),
        )?),
        Cmd::Migrate {
            kind,
            name,
            from,
            to,
            from_namespace,
            to_namespace,
            uid,
        } => {
            let (from, from_namespace) =
                kubernetes::kube_scope(Some(&from), from_namespace.as_deref())?;
            let (to, to_namespace) = kubernetes::kube_scope(Some(&to), to_namespace.as_deref())?;
            let resource = Resource::from(kind);
            cluster_migration::migrate(
                Scope {
                    context: &from,
                    namespace: &from_namespace,
                },
                Scope {
                    context: &to,
                    namespace: &to_namespace,
                },
                resource.key(),
                &name,
                uid.as_deref(),
                |phase| eprintln!("{phase}"),
            )?;
            json(serde_json::json!({
                "verified": true, "kind": resource.key(), "name": name,
                "context": to, "namespace": to_namespace
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: Args,
    }

    fn parse(args: &[&str]) -> Cmd {
        Cli::try_parse_from(std::iter::once("kube").chain(args.iter().copied()))
            .unwrap()
            .args
            .cmd
    }

    #[test]
    fn route_is_a_resource_kind() {
        for word in ["route", "routes"] {
            let Cmd::List { kind, .. } = parse(&["list", word]) else {
                panic!("not list");
            };
            assert_eq!(Resource::from(kind).key(), "routes");
        }
        let Cmd::Inspect { kind, name, scope } = parse(&["inspect", "route", "web", "-n", "shop"])
        else {
            panic!("not inspect");
        };
        assert_eq!(Resource::from(kind).key(), "routes");
        assert_eq!(
            (name.as_str(), scope.namespace.as_deref()),
            ("web", Some("shop"))
        );
        let Cmd::Migrate { kind, .. } =
            parse(&["migrate", "route", "web", "--from", "a", "--to", "b"])
        else {
            panic!("not migrate");
        };
        assert_eq!(Resource::from(kind).key(), "routes");
    }
}
