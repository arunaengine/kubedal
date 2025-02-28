extern crate tonic_build;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/csi.proto");

    tonic_build::configure()
        .build_server(true)
        .out_dir("src/csi")
        .compile_protos(&["proto/csi.proto"], &["proto"])?;

    Ok(())
}
