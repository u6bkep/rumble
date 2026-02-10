fn main() {
    prost_build::Config::new()
        .compile_protos(&["proto/Mumble.proto"], &["proto/"])
        .expect("Failed to compile Mumble.proto");
}
