fn main() {
    // prost-build (which tonic-prost-build wraps) picks up the protoc binary from the `PROTOC`
    // env var - set it to the vendored one so a build doesn't depend on protoc being on PATH.
    unsafe {
        std::env::set_var("PROTOC", protoc_bin_vendored::protoc_bin_path().unwrap());
    }

    tonic_prost_build::configure()
        .build_server(false)
        .compile_protos(
            &["proto/github.com/openconfig/gnmi/proto/gnmi/gnmi.proto"],
            &["proto"],
        )
        .unwrap();

    tauri_build::build()
}
