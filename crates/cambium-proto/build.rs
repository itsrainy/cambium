//! Compiles the vendored `build.proto` with protox, a pure-Rust protobuf
//! compiler, so building cambium does not require `protoc` on PATH.

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("cargo:rerun-if-changed=proto/build.proto");
    let fds = protox::compile(["proto/build.proto"], ["proto"])?;
    let mut cfg = prost_build::Config::new();
    cfg.compile_fds(fds)?;
    Ok(())
}
