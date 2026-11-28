#![cfg(target_os = "linux")]
//! CLI regressions for managed BuildKit arguments and the local exporter.

mod common;

use common::{Sandbox, root_mode, stderr, stdout};
use std::ffi::OsStr;
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::symlink;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn context(sb: &Sandbox) -> PathBuf {
    let context = sb.tmp.path().join("context");
    fs::create_dir(&context).unwrap();
    fs::write(context.join("Dockerfile"), "FROM scratch\nCOPY . /\n").unwrap();
    fs::write(context.join("note"), "included\n").unwrap();
    context
}

fn build(sb: &Sandbox, context: &Path, args: &[&str]) -> Output {
    sb.podbag()
        .args(["build", "--buildkit", "--network", "none"])
        .args(args)
        .arg(context)
        .output()
        .unwrap()
}

fn checked(out: Output) -> Output {
    assert!(out.status.success(), "{}\n{}", stdout(&out), stderr(&out));
    out
}

fn enabled() -> bool {
    if std::env::var("PODBAG_TEST_BUILDKIT").as_deref() != Ok("1") {
        eprintln!("PODBAG_TEST_BUILDKIT is not 1: skipped");
        return false;
    }
    true
}

#[test]
fn the_load_exporter_accepts_buildkit_native_aliases() {
    if !enabled() {
        return;
    }
    let platform = match std::env::consts::ARCH {
        "aarch64" => "linux/aarch64",
        "x86_64" => "linux/x86_64",
        arch => panic!("the BuildKit CI fixture does not have a native alias for {arch}"),
    };
    let sb = Sandbox::new();
    let context = context(&sb);
    let out = checked(build(
        &sb,
        &context,
        &["--platform", platform, "-t", "managed-native-alias:test"],
    ));
    assert!(stdout(&out).contains("sha256:"), "{}", stdout(&out));
}

#[test]
fn the_load_exporter_rejects_foreign_platforms_and_platform_lists() {
    let sb = Sandbox::new();
    let ctx = context(&sb);
    let foreign = match std::env::consts::ARCH {
        "aarch64" => "linux/x86_64",
        _ => "linux/aarch64",
    };
    for platform in [foreign, "linux/amd64,linux/arm64", "windows/amd64"] {
        let out = build(&sb, &ctx, &["--platform", platform]);
        assert!(!out.status.success(), "{platform}: {}", stdout(&out));
        assert!(
            stderr(&out).contains("cannot load a foreign or multi-platform result"),
            "{platform}: {}",
            stderr(&out)
        );
    }
}

#[test]
fn the_local_export_excludes_only_its_own_staging_directory() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new();
    let ctx = context(&sb);
    let parent = ctx.join(" !literal [*]?\\");
    fs::create_dir(&parent).unwrap();
    let alias = sb.tmp.path().join("output-parent");
    symlink(&parent, &alias).unwrap();
    fs::write(parent.join(".podbag-build-user-file"), "ordinary file\n").unwrap();
    fs::write(ctx.join("excluded.txt"), "ignored\n").unwrap();
    fs::write(ctx.join(".dockerignore"), "!**\nexcluded.txt").unwrap();
    let result = alias.join("result");
    checked(build(
        &sb,
        &ctx,
        &["--output", &format!("type=local,dest={}", result.display())],
    ));

    let mut find = if root_mode() {
        let mut command = Command::new("sudo");
        command.args(["-n", "find"]);
        command
    } else {
        Command::new("find")
    };
    let output = checked(find.arg(&result).args(["-print"]).output().unwrap());
    let paths = stdout(&output);
    let copied: Vec<_> = paths
        .lines()
        .map(|line| line.rsplit('/').next().unwrap())
        .filter(|name| name.starts_with(".podbag-build-"))
        .collect();
    assert_eq!(copied, [".podbag-build-user-file"], "{paths}");
    assert!(paths.lines().any(|line| line.ends_with("/note")), "{paths}");
    assert!(
        !paths.lines().any(|line| line.ends_with("/excluded.txt")),
        "{paths}"
    );
}

#[test]
fn duplicate_cache_fields_are_rejected_before_starting_buildkit() {
    let sb = Sandbox::new();
    let ctx = context(&sb);
    let first = sb.tmp.path().join("first-cache");
    let second = sb.tmp.path().join("second-cache");
    for (flag, value) in [
        (
            "--cache-to",
            format!(
                "type=local,dest={},dest={}",
                first.display(),
                second.display()
            ),
        ),
        (
            "--cache-from",
            format!(
                "type=local,src={},src={}",
                first.display(),
                second.display()
            ),
        ),
        (
            "--cache-from",
            "type=registry,ref=127.0.0.1:1/fixture:one,ref=127.0.0.1:1/fixture:two".into(),
        ),
        (
            "--cache-to",
            format!("type=local,dest={},mode=max,mode=min", first.display()),
        ),
    ] {
        let out = build(&sb, &ctx, &[flag, &value]);
        assert!(!out.status.success(), "{flag} {value}");
        assert!(
            stderr(&out).contains("invalid BuildKit cache"),
            "{}",
            stderr(&out)
        );
    }
}

#[test]
fn the_local_export_keeps_a_utf8_path_to_a_non_utf8_parent() {
    if !enabled() {
        return;
    }
    let sb = Sandbox::new();
    let ctx = context(&sb);
    let physical = sb.tmp.path().join(OsStr::from_bytes(b"output-\xff"));
    fs::create_dir(&physical).unwrap();
    let alias = sb.tmp.path().join("output");
    symlink(&physical, &alias).unwrap();
    checked(build(
        &sb,
        &ctx,
        &[
            "--output",
            &format!("type=local,dest={}/result", alias.display()),
        ],
    ));
    assert!(physical.join("result").is_dir());
}
