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
    for key in BUILD_OPTION_KEYS {
        println!("cargo:rerun-if-env-changed={key}");
        if let Ok(value) = std::env::var(key) {
            println!("cargo:rustc-env={key}={value}");
        }
    }
}
