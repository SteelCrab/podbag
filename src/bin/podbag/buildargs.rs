//! `--build-arg` and `--label` pairs, and the images a build pulls. Not tied to the
//! engine: a front end (macOS, Windows) reads the Dockerfile here to send logins.

use crate::args::BuildArgs;
use std::collections::BTreeMap;

/// `NAME=VALUE` pairs; `NAME` alone takes this process's environment (and is left out
/// when unset), as Docker's `--build-arg` does.
pub fn pairs(list: &[String], from_env: bool) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for item in list {
        match item.split_once('=') {
            Some((k, v)) => {
                out.insert(k.to_string(), v.to_string());
            }
            None if from_env => {
                if let Ok(v) = std::env::var(item) {
                    out.insert(item.clone(), v);
                }
            }
            None => {
                out.insert(item.clone(), String::new());
            }
        }
    }
    out
}

/// The images a build of `a` may pull, from its Dockerfile on this side: a front end
/// (macOS, Windows) sends their registries' logins with the command. Empty when the
/// Dockerfile comes on stdin or cannot be read; the engine then reports any problem.
#[cfg(any(target_os = "macos", windows, test))]
pub fn pulled_images(a: &BuildArgs) -> Vec<String> {
    if a.context == "-" {
        return Vec::new();
    }
    let path = match &a.file {
        Some(f) => std::path::PathBuf::from(f),
        None => std::path::Path::new(&a.context).join("Dockerfile"),
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    podbag::build::dockerfile::parse(&text, "Dockerfile")
        .and_then(|df| podbag::build::steps::pulled_images(&df, pairs(&a.build_arg, true)))
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_args(args: &[&str]) -> BuildArgs {
        use clap::Parser;
        #[derive(Parser)]
        struct Wrap {
            #[command(flatten)]
            a: BuildArgs,
        }
        Wrap::parse_from(std::iter::once("build").chain(args.iter().copied())).a
    }

    #[test]
    fn a_front_end_reads_the_pulled_images_from_the_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        let ctx = tmp.path().display().to_string();
        std::fs::write(
            tmp.path().join("Dockerfile"),
            "ARG BASE=alpine\nFROM $BASE AS a\nFROM ghcr.io/team/app:1\nCOPY --from=a / /\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("other"), "FROM localhost:5000/base\n").unwrap();
        assert_eq!(
            pulled_images(&build_args(&[&ctx])),
            ["alpine", "ghcr.io/team/app:1"]
        );
        assert_eq!(
            pulled_images(&build_args(&["--build-arg", "BASE=quay.io/x/y", &ctx]))[0],
            "quay.io/x/y"
        );
        let other = tmp.path().join("other").display().to_string();
        assert_eq!(
            pulled_images(&build_args(&["-f", &other, &ctx])),
            ["localhost:5000/base"]
        );
        // Stdin, a missing file, or a broken Dockerfile: nothing to send.
        assert!(pulled_images(&build_args(&["-"])).is_empty());
        assert!(pulled_images(&build_args(&["-f", "/no/such/file", &ctx])).is_empty());
        std::fs::write(tmp.path().join("Dockerfile"), "RUN before from\n").unwrap();
        assert!(pulled_images(&build_args(&[&ctx])).is_empty());
    }

    #[test]
    fn build_args_and_labels() {
        let got = pairs(
            &["A=1".into(), "B=x=y".into(), "PODBAG_SURELY_UNSET".into()],
            true,
        );
        assert_eq!(got.get("A").map(String::as_str), Some("1"));
        assert_eq!(got.get("B").map(String::as_str), Some("x=y"));
        assert!(!got.contains_key("PODBAG_SURELY_UNSET"));
        let labels = pairs(&["k".into()], false);
        assert_eq!(labels.get("k").map(String::as_str), Some(""));
    }
}
