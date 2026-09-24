fn main() {
    println!("cargo:rerun-if-env-changed=OPENNOW_FFMPEG_DIR");
    println!("cargo:rerun-if-changed=src/windows/nvdec_bridge.c");
    if std::env::var_os("CARGO_FEATURE_NVDEC_EXPERIMENT").is_none()
        || std::env::var("CARGO_CFG_TARGET_OS").as_deref() != Ok("windows")
    {
        return;
    }
    let root = std::path::PathBuf::from(
        std::env::var_os("OPENNOW_FFMPEG_DIR")
            .expect("set OPENNOW_FFMPEG_DIR to FFmpeg shared SDK"),
    );
    let mut build = cc::Build::new();
    build
        .file("src/windows/nvdec_bridge.c")
        .include(root.join("include"))
        .opt_level(2);
    println!("cargo:rerun-if-changed=src/windows/nvdec_gpu_bridge.h");
    println!("cargo:rerun-if-env-changed=OPENNOW_CUDA_DIR");
    if std::env::var_os("CARGO_FEATURE_NVDEC_GPU_INTEROP").is_some() {
        let cuda = std::path::PathBuf::from(
            std::env::var_os("OPENNOW_CUDA_DIR")
                .expect("set OPENNOW_CUDA_DIR to the CUDA driver SDK"),
        );
        build
            .include(cuda.join("include"))
            .define("OPENNOW_GPU_INTEROP", None);
        println!(
            "cargo:rustc-link-search=native={}",
            cuda.join("lib/x64").display()
        );
        println!("cargo:rustc-link-lib=cuda");
    }
    build.compile("opennow_nvdec_bridge");
    println!(
        "cargo:rustc-link-search=native={}",
        root.join("lib").display()
    );
    println!("cargo:rustc-link-lib=avcodec");
    println!("cargo:rustc-link-lib=avutil");
}
