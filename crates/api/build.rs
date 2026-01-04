fn main() {
    let proto_dir = "proto";
    let out_dir = std::env::var("OUT_DIR").unwrap();

    let mut config = prost_build::Config::new();
    config.out_dir(&out_dir);

    prost_build::compile_protos(&["proto/api.proto"], &[proto_dir]).expect("failed to compile protobuf definitions");
}
