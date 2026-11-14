//! Compiles the vendored Spark Connect protos (see `proto/SPARK_VERSION`).
//!
//! Uses `protox` (a pure-Rust protobuf compiler) so building sparkline does not
//! require `protoc` to be installed.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let protos = [
        "proto/spark/connect/base.proto",
        "proto/spark/connect/catalog.proto",
        "proto/spark/connect/commands.proto",
        "proto/spark/connect/common.proto",
        "proto/spark/connect/expressions.proto",
        "proto/spark/connect/ml.proto",
        "proto/spark/connect/ml_common.proto",
        "proto/spark/connect/relations.proto",
        "proto/spark/connect/types.proto",
    ];
    println!("cargo:rerun-if-changed=proto");

    let fds = protox::compile(protos, ["proto"])?;
    tonic_build::configure()
        .build_server(false)
        .compile_fds(fds)?;
    Ok(())
}
