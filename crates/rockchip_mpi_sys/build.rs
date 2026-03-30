use std::env;
use std::path::PathBuf;

fn main() {
    let buildkit_root = env::var("BUILDKIT_ROOT").expect("BUILDKIT_ROOT not set");
    let buildkit_sysroot =
        format!("{}/arm-rockchip830-linux-uclibcgnueabihf/sysroot", buildkit_root);

    // Tell cargo to look for shared libraries in the specified directory
    println!("cargo:rustc-link-search=/usr/lib");
    println!("cargo:rustc-link-lib=rockit");
    println!("cargo:rustc-link-lib=rockchip_mpp");
    println!("cargo:rustc-link-lib=rga");
    println!("cargo:rustc-link-lib=rockiva");
    println!("cargo:rustc-link-lib=rknnmrt");

    // The bindgen::Builder is the main entry point
    // to bindgen, and lets you build up options for
    // the resulting bindings.
    let bindings = bindgen::Builder::default()
        .header("wrapper.h")
        .parse_callbacks(Box::new(bindgen::CargoCallbacks::new()))
        .clang_arg(format!("--sysroot={}", buildkit_sysroot))
        .clang_arg(format!("-I{}/include", buildkit_sysroot))
        .clang_arg("-DUSE_ROCKCHIP_MPP")
        .clang_arg("-DRKPLATFORM=ON")
        .clang_arg("-DARCH64=OFF")
        .clang_arg("-DUAPI2")
        .clang_arg("-DRV1106_RV1103")
        .clang_arg("-DRKAIQ")
        .clang_arg("-D_LARGEFILE_SOURCE")
        .clang_arg("-D_LARGEFILE64_SOURCE")
        .clang_arg("-D_FILE_OFFSET_BITS=64")
        .generate()
        .expect("Unable to generate bindings");

    // Write the bindings to the $OUT_DIR/bindings.rs file.
    let out_path = PathBuf::from(env::var("OUT_DIR").unwrap());
    bindings.write_to_file(out_path.join("bindings.rs")).expect("Couldn't write bindings!");
}
