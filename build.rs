const BUILD_OPTION_KEYS: &[&str] = &[
    "BUILD_DEPOT_STATIC",
    "DEPOT_AUTOTOOLS_PACKAGE",
    "DEPOT_CMAKE_PACKAGE",
    "DEPOT_MESON_PACKAGE",
    "DEPOT_PERL_PACKAGE",
    "DEPOT_CUSTOM_PACKAGE",
    "DEPOT_PYTHON_PACKAGE",
    "DEPOT_RUST_PACKAGE",
    "DEPOT_MAKEFILE_PACKAGE",
];

fn main() {
    println!("cargo:rerun-if-env-changed=CC");
    println!("cargo:rerun-if-changed=src/fakeroot_preload.c");
    for key in BUILD_OPTION_KEYS {
        println!("cargo:rerun-if-env-changed={key}");
        if let Ok(value) = std::env::var(key) {
            println!("cargo:rustc-env={key}={value}");
        }
    }

    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux") {
        build_fakeroot_preload();
    }

    if std::env::var_os("CARGO_FEATURE_STATIC_EXCEPT_LIBC").is_some()
        && std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("linux")
    {
        let libgcc_eh = gcc_runtime_archive("libgcc_eh.a").unwrap_or_else(|| {
            panic!("static-except-libc requires libgcc_eh.a for static libgcc linkage")
        });
        println!("cargo:rustc-link-arg-bin=depot=-Wl,--whole-archive");
        println!("cargo:rustc-link-arg-bin=depot={libgcc_eh}");
        println!("cargo:rustc-link-arg-bin=depot=-Wl,--no-whole-archive");
        println!("cargo:rustc-link-arg-bin=depot=-static-libgcc");
    }
}

fn build_fakeroot_preload() {
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let output_path = std::path::PathBuf::from(
        std::env::var_os("OUT_DIR").expect("Cargo must provide OUT_DIR to build.rs"),
    )
    .join("libdepot_fakeroot.so");
    let output = std::process::Command::new(compiler)
        .args([
            "-std=c11", "-shared", "-fPIC", "-O2", "-Wall", "-Wextra", "-Werror", "-o",
        ])
        .arg(&output_path)
        .arg("src/fakeroot_preload.c")
        .output()
        .unwrap_or_else(|error| {
            panic!("failed to launch C compiler for fakeroot preload: {error}")
        });

    if !output.status.success() {
        panic!(
            "failed to build fakeroot preload library:\n{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
}

fn gcc_runtime_archive(name: &str) -> Option<String> {
    let compiler = std::env::var_os("CC").unwrap_or_else(|| "cc".into());
    let output = std::process::Command::new(compiler)
        .arg(format!("-print-file-name={name}"))
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let path = String::from_utf8(output.stdout).ok()?;
    let path = path.trim();
    if path.is_empty() || path == name || !std::path::Path::new(path).exists() {
        None
    } else {
        Some(path.to_string())
    }
}
