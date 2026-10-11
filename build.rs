fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/csi.proto");

    tonic_prost_build::configure()
        .build_server(true)
        .build_client(false)
        .out_dir("src/csi")
        .compile_protos(&["proto/csi.proto"], &["proto"])?;

    Ok(())
}
