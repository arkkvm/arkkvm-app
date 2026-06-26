use std::env;
use std::path::PathBuf;
use std::process::Command;

fn main() {
    compile_protos();
    cross_compile::build_c_shims();
}

fn compile_protos() {
    use std::env;
    use std::path::PathBuf;

    println!("cargo:rerun-if-changed=../../proto/usb_devices.proto");

    let protoc = protoc_bin_vendored::protoc_bin_path().expect("vendored protoc");
    unsafe { env::set_var("PROTOC", protoc) };

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR"));
    prost_build::Config::new()
        .out_dir(&out_dir)
        .compile_protos(&["../../proto/usb_devices.proto"], &["../../proto/"])
        .expect("compile usb_devices.proto");
}

/// Cross-compilation utilities for C code integration
mod cross_compile {
    use super::*;

    /// Build all C shims required for the project
    pub fn build_c_shims() {
        println!("cargo:rerun-if-changed=../../cshim/getauxval.c");
        println!("cargo:rerun-if-env-changed=CROSS_TOOLCHAIN");
        println!("cargo:rerun-if-env-changed=CROSS_CC");
        println!("cargo:rerun-if-env-changed=CROSS_AR");
        println!("cargo:rerun-if-env-changed=CROSS_SYSROOT");

        let toolchain = env::var("BUILDKIT_ROOT").expect("BUILDKIT_ROOT not set");
        let cc = format!("{}/bin/arm-rockchip830-linux-uclibcgnueabihf-gcc", toolchain);
        let ar = format!("{}/bin/arm-rockchip830-linux-uclibcgnueabihf-ar", toolchain);
        let sysroot = format!("{}/arm-rockchip830-linux-uclibcgnueabihf/sysroot", toolchain);

        let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
        let obj_file = out_dir.join("getauxval.o");
        let lib_file = out_dir.join("libgetauxval.a");

        // Compile C source to object file
        let status = Command::new(&cc)
            .args([
                "-c",
                "-fPIC",
                "../../cshim/getauxval.c",
                "-o",
                obj_file.to_str().expect("Invalid UTF-8 in obj path"),
                &format!("--sysroot={}", sysroot),
                "-O2",
            ])
            .status()
            .expect("Failed to spawn cross-compiler");
        assert!(status.success(), "Cross-compile getauxval.c failed");

        // Create static library
        let status = Command::new(&ar)
            .args([
                "rcs",
                lib_file.to_str().expect("Invalid UTF-8 in lib path"),
                obj_file.to_str().expect("Invalid UTF-8 in obj path"),
            ])
            .status()
            .expect("Failed to spawn archiver");
        assert!(status.success(), "Archive creation failed");

        println!("cargo::rustc-link-arg=-L{}", out_dir.display());
        println!("cargo::rustc-link-arg=-lgetauxval");
        println!("cargo::rustc-link-arg=--sysroot={}", sysroot);
    }
}