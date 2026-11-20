//! Registry endpoints offered by the image picker. Unlisted servers use Custom.
use serde::Serialize;

#[derive(Debug, PartialEq, Eq, Hash, Serialize)]
pub struct RegistryPreset {
    pub id: &'static str,
    pub label: &'static str,
    pub example: &'static str,
    // None uses the fixed example host. Some lets users specify their own server.
    hosts: Option<&'static [&'static str]>,
}

impl RegistryPreset {
    const fn fixed(id: &'static str, label: &'static str, host: &'static str) -> Self {
        Self {
            id,
            label,
            example: host,
            hosts: None,
        }
    }

    const fn variable(
        id: &'static str,
        label: &'static str,
        example: &'static str,
        hosts: &'static [&'static str],
    ) -> Self {
        Self {
            id,
            label,
            example,
            hosts: Some(hosts),
        }
    }

    pub(super) fn needs_host(&self) -> bool {
        self.hosts.is_some()
    }

    pub(super) fn recognizes(&self, host: &str) -> bool {
        let Some(patterns) = self.hosts else {
            return self.example == host;
        };
        patterns
            .iter()
            .any(|pattern| match pattern.split_once('*') {
                Some((prefix, suffix)) => host
                    .strip_prefix(prefix)
                    .and_then(|rest| rest.strip_suffix(suffix))
                    .is_some_and(|part| !part.is_empty()),
                None => *pattern == host,
            })
    }
}

use RegistryPreset as P;

pub(super) const PRESETS: &[RegistryPreset] = &[
    P::fixed("ecr-public", "Amazon ECR Public", "public.ecr.aws"),
    P::fixed("mcr", "Microsoft Artifact Registry", "mcr.microsoft.com"),
    P::fixed("dhi", "Docker Hardened Images", "dhi.io"),
    P::variable(
        "gcr",
        "Google Artifact Registry (gcr.io)",
        "gcr.io",
        &["gcr.io", "us.gcr.io", "eu.gcr.io", "asia.gcr.io"],
    ),
    P::fixed(
        "digitalocean",
        "DigitalOcean Container Registry",
        "registry.digitalocean.com",
    ),
    P::variable(
        "ocir",
        "Oracle Cloud Infrastructure Registry",
        "iad.ocir.io",
        &["*.ocir.io", "ocir.*.oci.oraclecloud.com"],
    ),
    P::fixed(
        "oracle",
        "Oracle Container Registry",
        "container-registry.oracle.com",
    ),
    P::variable(
        "ibm",
        "IBM Cloud Container Registry",
        "us.icr.io",
        &["icr.io", "*.icr.io"],
    ),
    P::variable(
        "alibaba",
        "Alibaba Cloud Container Registry",
        "crpi-example.cn-hangzhou.personal.cr.aliyuncs.com",
        &[
            "registry.*.aliyuncs.com",
            "registry-vpc.*.aliyuncs.com",
            "registry-intl.*.aliyuncs.com",
            "registry-intl-vpc.*.aliyuncs.com",
            "*.cr.aliyuncs.com",
        ],
    ),
    P::variable(
        "tencent",
        "Tencent Container Registry",
        "example.tencentcloudcr.com",
        &["*.tencentcloudcr.com", "ccr.ccs.tencentyun.com"],
    ),
    P::variable(
        "huawei",
        "Huawei Cloud SWR",
        "swr.cn-north-4.myhuaweicloud.com",
        &["swr.*.myhuaweicloud.com"],
    ),
    P::variable(
        "scaleway",
        "Scaleway Container Registry",
        "rg.fr-par.scw.cloud",
        &["rg.*.scw.cloud"],
    ),
    P::variable(
        "ovh",
        "OVHcloud Managed Private Registry",
        "example.container-registry.ovh.net",
        &["*.container-registry.ovh.net"],
    ),
    P::variable(
        "redhat",
        "Red Hat Container Registry",
        "registry.redhat.io",
        &[
            "registry.redhat.io",
            "registry.access.redhat.com",
            "registry.connect.redhat.com",
        ],
    ),
    P::fixed("suse", "SUSE Registry", "registry.suse.com"),
    P::fixed("kubernetes", "Kubernetes Registry", "registry.k8s.io"),
    P::fixed("ngc", "NVIDIA NGC", "nvcr.io"),
    P::fixed("chainguard", "Chainguard Registry", "cgr.dev"),
    P::fixed("cloudsmith", "Cloudsmith", "docker.cloudsmith.io"),
    P::variable(
        "jfrog",
        "JFrog Artifactory",
        "example.jfrog.io",
        &["*.jfrog.io"],
    ),
    P::variable("harbor", "Harbor", "harbor.example.com", &[]),
    P::variable(
        "nexus",
        "Sonatype Nexus Repository",
        "nexus.example.com:5000",
        &[],
    ),
    P::variable(
        "gitlab-self",
        "GitLab Self-Managed",
        "registry.example.com",
        &[],
    ),
];
