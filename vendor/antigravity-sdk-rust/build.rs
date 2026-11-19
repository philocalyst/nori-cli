#![allow(clippy::unwrap_used)]

fn main() {
    let descriptor_bytes = {
        use protox::prost::Message as _;
        protox::compile(["proto/localharness.proto"], ["proto/"])
            .unwrap()
            .encode_to_vec()
    };

    let fds =
        <prost_types::FileDescriptorSet as prost::Message>::decode(&descriptor_bytes[..]).unwrap();
    let mut config = prost_build::Config::new();
    config.compile_fds(fds).unwrap();

    pbjson_build::Builder::new()
        .register_descriptors(&descriptor_bytes)
        .unwrap()
        .ignore_unknown_fields()
        .build(&[".antigravity.localharness"])
        .unwrap();

    println!(
        "cargo:rustc-env=RUSTC_VERSION={}",
        std::env::var("RUSTC").unwrap_or_else(|_| "rustc".to_string())
    );
}
