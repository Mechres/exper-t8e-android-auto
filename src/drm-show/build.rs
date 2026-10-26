fn main() {
    println!("cargo:rerun-if-changed=src/yuv_neon.c");
    // yalnizca ARMv7 hedefte NEON cekirdegi derlenir
    if std::env::var("CARGO_CFG_TARGET_ARCH").as_deref() == Ok("arm") {
        cc::Build::new()
            .file("src/yuv_neon.c")
            .flag("-O3")
            .flag("-mfpu=neon")
            .flag("-mfloat-abi=hard")
            .compile("yuv_neon");
    }
}
