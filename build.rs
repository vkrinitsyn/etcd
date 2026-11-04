use std::path::{Path, PathBuf};

/// The official etcd files, as they are: `proto/etcd/` mirrors the etcd repo (their imports
/// say `etcd/...`), and `.github/proto-compare.sh` compares them with it byte for byte
const PROTOS: [&str; 5] = [
    "proto/etcd/api/authpb/auth.proto",
    "proto/etcd/api/mvccpb/kv.proto",
    "proto/etcd/api/etcdserverpb/rpc.proto",
    "proto/etcd/server/etcdserver/api/v3election/v3electionpb/v3election.proto",
    "proto/etcd/server/etcdserver/api/v3lock/v3lockpb/v3lock.proto",
];
/// The packages the crate has: what else the imports generate - google.api, the OpenAPI
/// options - carries only annotations the server does not read
const PACKAGES: [&str; 5] = ["authpb", "mvccpb", "etcdserverpb", "v3electionpb", "v3lockpb"];

fn main() {
    let proto_root = "proto";
    println!("cargo:rerun-if-changed={}", proto_root);
    println!("cargo:rerun-if-env-changed=ETCDS_GEN_PROTO");
    // [i18n] rust-i18n reads locales/ as the crate compiles: a message edited there rebuilds it
    println!("cargo:rerun-if-changed=locales");

    // [proto] `ETCDS_GEN_PROTO=1 cargo build`: src/etcdpb generated again from proto/. Never on
    // its own: a build must not change the source, and a packaged crate's build cannot. The
    // code is committed, so a build needs no protoc - an old one, on an old build host, cannot
    // read the official files (`optional` in a proto3 extend)
    if std::env::var_os("ETCDS_GEN_PROTO").is_none() {
        return;
    }
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR"));
    tonic_prost_build::configure()
        .out_dir(&out)
        .compile_protos(&PROTOS, &[proto_root, "proto/third_party"])
        .expect("Failed to compile proto files");
    for p in PACKAGES {
        write_package(&out, p);
    }
}

/// One package's generated code into src/etcdpb, tonic's "Service was not ready" in the
/// messages' language (src/i18n.rs)
fn write_package(out: &Path, package: &str) {
    let code = std::fs::read_to_string(out.join(format!("{}.rs", package))).expect("generated code");
    let code = code.replace("format!(\"Service was not ready: {}\", e.into())", "crate::i18n::not_ready(e.into())");
    std::fs::write(format!("src/etcdpb/{}.rs", package), code).expect("src/etcdpb");
}
