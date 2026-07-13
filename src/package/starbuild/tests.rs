use super::*;

#[test]
fn convert_single_package_starbuild_generates_custom_spec_and_build_script() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let starbuild = temp.path().join("STARBUILD");
    fs::write(
        &starbuild,
        r#"
package_name="meson"
package_version="1.2.3"
description="build system"
license=( "Apache-2.0" )
dependencies=( "python" "ninja" )
build_dependencies=( "git" )
sources=( "helper.sh" "https://github.com/mesonbuild/meson.git#v$package_version" "ne+https://example.com/extra.tar.xz" )
BUILD_ZLIB=True

compile() {
    cd meson
    python -m build
}

assemble() {
    cd meson
    python -m installer --destdir="$pkgdir" dist/*.whl
}
"#,
    )?;

    let converted = convert_starbuild_file(&starbuild, None)?;
    let spec_path = temp.path().join("meson.toml");
    assert_eq!(converted.output_path, spec_path);
    assert!(converted.build_script.is_some());
    assert_eq!(
        converted.build_script_path,
        Some(temp.path().join("build.sh"))
    );
    assert!(converted.toml.contains("type = \"custom\""));
    assert!(
        converted
            .toml
            .contains("url = \"https://github.com/mesonbuild/meson.git#v1.2.3\"")
    );
    assert!(converted.toml.contains("file = \"helper.sh\""));
    assert!(
        converted
            .toml
            .contains("url = \"https://example.com/extra.tar.xz\"")
    );

    fs::write(&spec_path, &converted.toml)?;
    fs::write(
        temp.path().join("build.sh"),
        converted.build_script.unwrap(),
    )?;
    let spec = PackageSpec::from_file(&spec_path)?;
    assert_eq!(spec.package.name, "meson");
    assert_eq!(spec.dependencies.runtime, vec!["python", "ninja"]);
    assert_eq!(spec.dependencies.build, vec!["git"]);
    assert_eq!(spec.manual_sources.len(), 2);
    assert_eq!(spec.source.len(), 1);

    Ok(())
}

#[test]
fn convert_multioutput_starbuild_maps_output_metadata() -> Result<()> {
    let temp = tempfile::tempdir()?;
    let starbuild = temp.path().join("STARBUILD");
    fs::write(
        &starbuild,
        r#"
package_name=( "mesa" "vulkan-intel" )
package_version="25.3.1"
package_descriptions=( "Mesa" "Intel Vulkan" )
license=( "MIT" )
dependencies=( "expat" )
build_dependencies=( "meson" )
sources=( "https://mesa.freedesktop.org/archive/mesa-$package_version.tar.xz" )
dependencies_vulkan-intel=( "vulkan-icd-loader" "mesa" )
gives_vulkan-intel=( "vulkan-driver" )
clashes_vulkan-intel=( "old-vulkan-intel" )
keep_vulkan-intel=( "etc/vulkan/intel.conf" )

compile() {
    cd mesa-$package_version
    meson setup build
}

assemble_mesa() {
    cd mesa-$package_version
    mesoni -C build
}

assemble_vulkan-intel() {
    starmove usr/lib/libvulkan_intel.so
}
"#,
    )?;

    let converted = convert_starbuild_file(&starbuild, None)?;
    assert!(converted.toml.contains("name = \"vulkan-intel\""));
    assert!(
        converted
            .toml
            .contains("keep = [\"etc/vulkan/intel.conf\"]")
    );
    let build_script = converted.build_script.as_deref().unwrap_or("");
    assert!(build_script.contains("depot_install_vulkan_intel()"));
    assert!(build_script.contains("haul \"$DEPOT_OUTPUT_NAME\" \"$@\""));
    assert!(build_script.contains("packages/vulkan-intel/files"));

    let spec_path = temp.path().join("mesa.toml");
    fs::write(&spec_path, &converted.toml)?;
    fs::write(temp.path().join("build.sh"), build_script)?;
    let spec = PackageSpec::from_file(&spec_path)?;
    assert_eq!(spec.packages.len(), 1);
    assert_eq!(spec.packages[0].name, "vulkan-intel");
    assert_eq!(
        spec.package_dependencies["vulkan-intel"].runtime,
        vec!["vulkan-icd-loader", "mesa"]
    );
    assert_eq!(
        spec.package_alternatives["vulkan-intel"].provides,
        vec!["vulkan-driver"]
    );
    assert_eq!(
        spec.package_alternatives["vulkan-intel"].conflicts,
        vec!["old-vulkan-intel"]
    );

    Ok(())
}
