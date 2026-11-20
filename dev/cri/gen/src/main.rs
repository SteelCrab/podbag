//! Compiles dev/cri/api.proto with protox (no protoc needed) and writes src/cri/pb.rs.
use std::{env, fs, path::PathBuf};

fn main() {
    let repo = PathBuf::from(env::args().nth(1).expect("usage: podbag-cri-gen REPO"));
    let cri = repo.join("dev/cri");
    let fds = protox::compile([cri.join("api.proto")], [&cri]).expect("compile api.proto");
    let out = tempdir();
    prost_build::Config::new()
        .out_dir(&out)
        .compile_fds(fds)
        .expect("generate Rust code");
    let code = fs::read_to_string(out.join("runtime.v1.rs")).expect("read generated code");
    let header = "// Generated from dev/cri/api.proto (k8s.io/cri-api, runtime.v1) by dev/cri/gen.sh.\n\
                  // Do not edit by hand.\n#![allow(clippy::all, clippy::pedantic)]\n\n";
    fs::write(repo.join("src/cri/pb.rs"), format!("{header}{code}")).expect("write pb.rs");
}

fn tempdir() -> PathBuf {
    let dir = env::temp_dir().join(format!("podbag-cri-gen-{}", std::process::id()));
    fs::create_dir_all(&dir).expect("temp dir");
    dir
}
