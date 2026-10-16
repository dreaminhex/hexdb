// Generates the gRPC server from proto/hexdb/v1/hexdb.proto. protox compiles
// the proto in Rust, so building needs no protoc.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto");
    let descriptors = protox::compile(["hexdb/v1/hexdb.proto"], ["proto"])?;
    tonic_prost_build::configure().build_client(false).build_transport(false).compile_fds(descriptors)?;
    Ok(())
}
