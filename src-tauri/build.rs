use std::path::Path;
use std::process::Command;

/// Cursor's agent protocol is protobuf over Connect-RPC, so the wire types are
/// generated from `proto/agent.proto` at build time.
///
/// The proto is a verbatim copy of sayknow-cli's
/// `packages/ai/src/providers/cursor/proto/agent.proto` (copied 2026-09-19).
/// Field numbers must never be renumbered: they are the wire contract with
/// `api2.cursor.sh`.
///
/// `protoc` is a documented build prerequisite. Failing here with the install
/// command is the whole point — a raw "protoc not found" from prost is not
/// actionable.
fn main() {
    let proto = Path::new("proto/agent.proto");
    println!("cargo:rerun-if-changed=proto/agent.proto");

    if !proto.exists() {
        panic!(
            "missing {}: copy it from sayknow-cli \
             (packages/ai/src/providers/cursor/proto/agent.proto)",
            proto.display()
        );
    }

    ensure_protoc();

    let mut config = prost_build::Config::new();
    // The proto carries fields (`turns_old`, deprecated summaries) we never
    // read; keeping them generated is cheaper than trimming the file and
    // risking a field-number mistake.
    config.protoc_arg("--experimental_allow_proto3_optional");
    config
        .compile_protos(&["proto/agent.proto"], &["proto"])
        .unwrap_or_else(|e| panic!("failed to generate Cursor protobuf types: {e}"));

    tauri_build::build()
}

fn ensure_protoc() {
    if std::env::var_os("PROTOC").is_some() {
        return;
    }
    let found = Command::new("protoc")
        .arg("--version")
        .output()
        .map(|out| out.status.success())
        .unwrap_or(false);
    if found {
        return;
    }
    panic!(
        "protoc is required to build SayKnow Kit (it generates the Cursor agent \
         protocol types).\n\
         Install it with:  brew install protobuf\n\
         Or point PROTOC at an existing binary:  PROTOC=/path/to/protoc cargo build"
    );
}
