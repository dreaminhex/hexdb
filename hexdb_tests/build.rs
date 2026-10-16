// Generates a gRPC client from the server's proto for the gRPC tests.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=../hexdb_api/proto");
    let descriptors = protox::compile(["hexdb/v1/hexdb.proto"], ["../hexdb_api/proto"])?;
    tonic_prost_build::configure().build_server(false).compile_fds(descriptors)?;
    Ok(())
}
