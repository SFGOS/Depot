//! Build system abstraction

mod autotools;
mod bin;
mod cmake;
mod custom;
mod dkms;
mod makefile;
mod meson;
mod perl;
pub(crate) mod python;
mod rust;
pub mod state;

use crate::cross::CrossConfig;
use crate::package::{BuildFlags, BuildType, PackageSpec};
use anyhow::{Context, Result};
use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub type EnvVars = Vec<(String, String)>;
pub(crate) const DEPOT_BUILD_HOST_DIR_ENV: &str = "DEPOT_BUILD_HOST_DIR";
pub(crate) const DEPOT_BUILD_HELPER_CONTEXT_ENV: &str = "DEPOT_BUILD_HELPER_CONTEXT";
pub(crate) const DEPOT_BUILD_HELPER_SOURCE_DIR_ENV: &str = "DEPOT_BUILD_HELPER_SOURCE_DIR";
pub(crate) const DEPOT_BUILD_HELPER_BUILD_DIR_ENV: &str = "DEPOT_BUILD_HELPER_BUILD_DIR";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetBuildKind {
    Primary,
    Lib32,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct InstallDirs {
    pub bindir: String,
    pub sbindir: String,
    pub libdir: String,
    pub libexecdir: String,
    pub sysconfdir: String,
    pub localstatedir: String,
    pub sharedstatedir: String,
    pub includedir: String,
    pub datarootdir: String,
    pub datadir: String,
    pub mandir: String,
    pub infodir: String,
}

#[derive(Debug, Clone, serde::Deserialize, serde::Serialize)]
pub(crate) struct BuildHelperContext {
    pub package_name: String,
    pub package_version: String,
    pub spec_dir: PathBuf,
    pub flags: BuildFlags,
    pub lib32_variant: bool,
    pub host_build_dir: Option<String>,
}

impl BuildHelperContext {
    pub(crate) fn from_spec(spec: &PackageSpec) -> Self {
        Self {
            package_name: spec.package.name.clone(),
            package_version: spec.package.version.clone(),
            spec_dir: spec.spec_dir.clone(),
            flags: spec.build.flags.clone(),
            lib32_variant: spec.build.flags.lib32_variant,
            host_build_dir: spec.build.flags.host_build_dir.clone(),
        }
    }

    pub(crate) fn expand_vars(&self, input: &str) -> String {
        let specdir = self.spec_dir.to_string_lossy();
        input
            .replace("$name", &self.package_name)
            .replace("$version", &self.package_version)
            .replace("$specdir", &specdir)
            .replace("$DEPOT_SPECDIR", &specdir)
    }

    pub(crate) fn build_flags(&self) -> BuildFlags {
        let mut flags = self.flags.clone();
        flags.lib32_variant = self.lib32_variant;
        flags.host_build_dir = self.host_build_dir.clone();
        flags
    }
}

pub(crate) fn apply_build_helper_context_env(
    env_vars: &mut EnvVars,
    spec: &PackageSpec,
) -> Result<()> {
    let encoded = toml::to_string(&BuildHelperContext::from_spec(spec))
        .context("Failed to serialize build helper context")?;
    set_env_var(env_vars, DEPOT_BUILD_HELPER_CONTEXT_ENV, encoded);
    Ok(())
}

pub(crate) fn apply_build_helper_dirs_env(
    env_vars: &mut EnvVars,
    source_dir: Option<&Path>,
    build_dir: Option<&Path>,
) {
    if let Some(source_dir) = source_dir {
        set_env_var(
            env_vars,
            DEPOT_BUILD_HELPER_SOURCE_DIR_ENV,
            source_dir.to_string_lossy().into_owned(),
        );
    }
    if let Some(build_dir) = build_dir {
        set_env_var(
            env_vars,
            DEPOT_BUILD_HELPER_BUILD_DIR_ENV,
            build_dir.to_string_lossy().into_owned(),
        );
    }
}

pub(crate) fn run_autotools_helper_configure(
    context: &BuildHelperContext,
    source_dir: Option<&Path>,
    build_dir: Option<&Path>,
    cross: Option<&CrossConfig>,
    env_vars: &EnvVars,
    extra_args: &[String],
) -> Result<()> {
    autotools::run_helper_configure(context, source_dir, build_dir, cross, env_vars, extra_args)
}

pub(crate) fn run_autotools_helper_install(
    context: &BuildHelperContext,
    build_dir: Option<&Path>,
    env_vars: &EnvVars,
    extra_args: &[String],
) -> Result<()> {
    autotools::run_helper_install(context, build_dir, env_vars, extra_args)
}

pub(crate) fn run_cmake_helper_configure(
    context: &BuildHelperContext,
    source_dir: Option<&Path>,
    build_dir: Option<&Path>,
    cross: Option<&CrossConfig>,
    env_vars: &EnvVars,
    extra_args: &[String],
) -> Result<()> {
    cmake::run_helper_configure(context, source_dir, build_dir, cross, env_vars, extra_args)
}

pub(crate) fn run_cmake_helper_install(
    context: &BuildHelperContext,
    build_dir: Option<&Path>,
    env_vars: &EnvVars,
    extra_args: &[String],
) -> Result<()> {
    cmake::run_helper_install(context, build_dir, env_vars, extra_args)
}

pub(crate) fn run_meson_helper_configure(
    context: &BuildHelperContext,
    source_dir: Option<&Path>,
    build_dir: Option<&Path>,
    cross: Option<&CrossConfig>,
    env_vars: &EnvVars,
    extra_args: &[String],
) -> Result<()> {
    meson::run_helper_configure(context, source_dir, build_dir, cross, env_vars, extra_args)
}

pub(crate) fn run_meson_helper_install(
    context: &BuildHelperContext,
    build_dir: Option<&Path>,
    env_vars: &EnvVars,
    extra_args: &[String],
) -> Result<()> {
    meson::run_helper_install(context, build_dir, env_vars, extra_args)
}

pub(crate) fn run_perl_helper_configure(
    context: &BuildHelperContext,
    source_dir: Option<&Path>,
    env_vars: &EnvVars,
    extra_args: &[String],
) -> Result<()> {
    perl::run_helper_configure(context, source_dir, env_vars, extra_args)
}

pub(crate) fn run_perl_helper_install(
    context: &BuildHelperContext,
    build_dir: Option<&Path>,
    env_vars: &EnvVars,
    extra_args: &[String],
) -> Result<()> {
    perl::run_helper_install(context, build_dir, env_vars, extra_args)
}

pub fn set_env_var(env_vars: &mut EnvVars, key: &str, value: impl Into<String>) {
    let value = value.into();
    if let Some((_, existing)) = env_vars.iter_mut().find(|(k, _)| k == key) {
        *existing = value;
    } else {
        env_vars.push((key.to_string(), value));
    }
}

fn set_expanded_env_var(env_vars: &mut EnvVars, key: &str, value: impl AsRef<str>) {
    let expanded = expand_with_envs(value.as_ref(), env_vars);
    set_env_var(env_vars, key, expanded);
}

fn configured_tool_or_default(configured: &str, default: &str) -> String {
    let configured = configured.trim();
    if configured.is_empty() {
        default.to_string()
    } else {
        configured.to_string()
    }
}

fn configured_defaulted_tool_or_default(
    configured: &str,
    implicit_default: &str,
    default: &str,
) -> String {
    let configured = configured.trim();
    if configured.is_empty() || configured == implicit_default {
        default.to_string()
    } else {
        configured.to_string()
    }
}

fn apply_declared_env_vars(spec: &PackageSpec, env_vars: &mut EnvVars) {
    for raw in &spec.build.flags.env_vars {
        let expanded = expand_with_envs(&spec.expand_vars(raw), env_vars);
        let entry = expanded.trim();
        if entry.is_empty() {
            continue;
        }

        let Some((key, value)) = entry.split_once('=') else {
            crate::log_warn!(
                "Skipping invalid build.flags.env_vars entry '{}'; expected KEY=VALUE",
                raw
            );
            continue;
        };

        let key = key.trim();
        if key.is_empty() || key.chars().any(char::is_whitespace) {
            crate::log_warn!(
                "Skipping invalid build.flags.env_vars entry '{}'; expected KEY=VALUE",
                raw
            );
            continue;
        }

        set_env_var(env_vars, key, value.to_string());
    }
}

/// Expand environment variables in a string using Depot's build env first,
/// falling back to the parent process environment.
pub(crate) fn expand_with_envs(input: &str, envs: &[(String, String)]) -> String {
    let mut result = input.to_string();
    for (key, value) in envs {
        result = result.replace(&format!("${key}"), value);
        result = result.replace(&format!("${{{key}}}"), value);
    }
    for (key, value) in std::env::vars() {
        result = result.replace(&format!("${key}"), &value);
        result = result.replace(&format!("${{{key}}}"), &value);
    }
    result
}

fn default_libdir_for_variant(lib32_variant: bool) -> &'static str {
    if lib32_variant {
        "/usr/lib32"
    } else {
        "/usr/lib"
    }
}

fn normalized_arch(arch: &str) -> &str {
    match arch.trim() {
        "amd64" => "x86_64",
        "arm64" => "aarch64",
        other => other,
    }
}

fn normalized_arch_key(arch: &str) -> String {
    normalized_arch(arch).to_ascii_lowercase().replace('-', "_")
}

fn lib32_arch_for(arch: &str) -> String {
    match normalized_arch(arch) {
        "x86_64" => "i686".to_string(),
        other => other.to_string(),
    }
}

pub(crate) fn host_arch() -> &'static str {
    normalized_arch(std::env::consts::ARCH)
}

pub(crate) fn effective_target_arch(
    flags: &crate::package::BuildFlags,
    cross: Option<&CrossConfig>,
    kind: TargetBuildKind,
) -> String {
    match kind {
        TargetBuildKind::Lib32 => {
            if let Some(cc_cfg) = cross {
                return crate::cross::target_arch_from_triple(&crate::cross::lib32_target_triple(
                    cc_cfg.host_triple(),
                ))
                .to_string();
            }
            if !flags.chost.trim().is_empty() {
                return crate::cross::target_arch_from_triple(&crate::cross::lib32_target_triple(
                    flags.chost.trim(),
                ))
                .to_string();
            }
            let base = if flags.carch.trim().is_empty() {
                host_arch()
            } else {
                flags.carch.trim()
            };
            lib32_arch_for(base)
        }
        TargetBuildKind::Primary => {
            if let Some(cc_cfg) = cross {
                return crate::cross::target_arch_from_triple(cc_cfg.host_triple()).to_string();
            }
            if !flags.chost.trim().is_empty() {
                return crate::cross::target_arch_from_triple(flags.chost.trim()).to_string();
            }
            if !flags.carch.trim().is_empty() {
                return flags.carch.trim().to_string();
            }
            host_arch().to_string()
        }
    }
}

fn target_arch_differs_from_host(
    flags: &crate::package::BuildFlags,
    cross: Option<&CrossConfig>,
    kind: TargetBuildKind,
) -> bool {
    normalized_arch(&effective_target_arch(flags, cross, kind)) != host_arch()
}

pub(crate) fn default_host_build_dir_name(flags: &crate::package::BuildFlags) -> String {
    match flags.build_dir.as_deref().map(str::trim) {
        Some(dir) if !dir.is_empty() => format!("{}-host", dir),
        _ => "build-host".to_string(),
    }
}

pub(crate) fn host_build_dir_for_source(
    src_root: &Path,
    flags: &crate::package::BuildFlags,
) -> PathBuf {
    src_root.join(default_host_build_dir_name(flags))
}

pub(crate) fn host_build_spec(spec: &PackageSpec) -> PackageSpec {
    let mut host_spec = spec.clone();
    host_spec.build.flags.lib32_variant = false;
    host_spec.build.flags.chost.clear();
    host_spec.build.flags.cbuild.clear();
    host_spec.build.flags.carch = host_arch().to_string();
    host_spec.build.flags.host_build_dir = None;
    host_spec.build.flags.build_dir = Some(default_host_build_dir_name(&spec.build.flags));
    append_configure_for_arch(&mut host_spec.build.flags, host_arch());
    host_spec
}

fn append_configure_for_target_arch(
    flags: &mut crate::package::BuildFlags,
    cross: Option<&CrossConfig>,
    kind: TargetBuildKind,
) {
    let arch = effective_target_arch(flags, cross, kind);
    append_configure_for_arch(flags, &arch);
}

fn append_configure_for_arch(flags: &mut crate::package::BuildFlags, arch: &str) {
    if flags.configure_arch.is_empty() {
        return;
    }

    let target_arch = normalized_arch_key(arch);
    let matching_args: Vec<String> = flags
        .configure_arch
        .iter()
        .filter(|(key, _)| normalized_arch_key(key) == target_arch)
        .flat_map(|(_, values)| values.iter().cloned())
        .collect();
    flags.configure.extend(matching_args);
}

fn spec_with_target_configure(
    spec: &PackageSpec,
    cross: Option<&CrossConfig>,
    kind: TargetBuildKind,
) -> Option<PackageSpec> {
    if spec.build.flags.configure_arch.is_empty() {
        return None;
    }

    let mut spec = spec.clone();
    append_configure_for_target_arch(&mut spec.build.flags, cross, kind);
    Some(spec)
}

pub(crate) fn requested_static_build() -> Result<Option<bool>> {
    crate::build_options::requested_static_build()
}

fn static_build_args_for_request(
    build_type: BuildType,
    requested_static: Option<bool>,
    no_delete_static: bool,
) -> Vec<String> {
    let Some(enabled) = requested_static else {
        return Vec::new();
    };

    if !enabled && no_delete_static {
        return Vec::new();
    }

    match build_type {
        BuildType::Autotools => {
            if enabled {
                vec!["--enable-static".to_string()]
            } else {
                vec![
                    "--enable-shared".to_string(),
                    "--disable-static".to_string(),
                ]
            }
        }
        BuildType::CMake => vec![format!(
            "-DBUILD_SHARED_LIBS={}",
            if enabled { "OFF" } else { "ON" }
        )],
        BuildType::Meson => vec![format!(
            "-Ddefault_library={}",
            if enabled { "static" } else { "shared" }
        )],
        BuildType::Perl => vec![format!(
            "LINKTYPE={}",
            if enabled { "static" } else { "dynamic" }
        )],
        _ => Vec::new(),
    }
}

pub(crate) fn static_build_args_for(
    build_type: BuildType,
    flags: &BuildFlags,
) -> Result<Vec<String>> {
    Ok(static_build_args_for_request(
        build_type,
        requested_static_build()?,
        flags.no_delete_static,
    ))
}

pub(crate) fn build_tool_package_option(build_type: BuildType) -> Option<&'static str> {
    crate::build_options::build_tool_package_option(build_type)
}

pub(crate) fn requested_build_tool_package(build_type: BuildType) -> Option<String> {
    crate::build_options::requested_build_tool_package(build_type)
}

pub(crate) fn development_package_option() -> &'static str {
    crate::build_options::development_package_option()
}

pub(crate) fn requested_development_package() -> Option<String> {
    crate::build_options::requested_development_package()
}

pub(crate) fn stage_generated_lifecycle_scripts(spec: &PackageSpec, destdir: &Path) -> Result<()> {
    match spec.build.build_type {
        BuildType::Dkms => dkms::stage_lifecycle_scripts(spec, destdir),
        _ => Ok(()),
    }
}

fn configured_install_dir(value: &str, default: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        default.to_string()
    } else {
        trimmed.to_string()
    }
}

fn split_replacement_spec<'a>(current: &[String], spec: &'a str) -> Option<(&'a str, &'a str)> {
    let trimmed = spec.trim();
    if trimmed.is_empty() {
        return None;
    }

    if let Some((from, to)) = trimmed.split_once("=>") {
        return (!from.is_empty() && !to.is_empty()).then_some((from, to));
    }

    let eq_positions: Vec<usize> = trimmed.match_indices('=').map(|(idx, _)| idx).collect();
    if eq_positions.is_empty() {
        return None;
    }
    if eq_positions.len() == 1 {
        let (from, to) = trimmed.split_once('=')?;
        return (!from.is_empty() && !to.is_empty()).then_some((from, to));
    }

    eq_positions
        .into_iter()
        .filter_map(|idx| {
            let from = &trimmed[..idx];
            let to = &trimmed[idx + 1..];
            (!from.is_empty() && !to.is_empty() && current.iter().any(|flag| flag.contains(from)))
                .then_some((from, to))
        })
        .max_by_key(|(from, _)| from.len())
}

fn apply_replacement_rules(current: &mut [String], replacements: &[String], label: &str) {
    for spec in replacements {
        let Some((from, to)) = split_replacement_spec(current, spec) else {
            if !spec.trim().is_empty() && !current.is_empty() {
                crate::log_warn!(
                    "Skipping invalid {} entry '{}'; expected 'old=>new' or an unambiguous 'old=new'",
                    label,
                    spec
                );
            }
            continue;
        };

        for flag in current.iter_mut() {
            *flag = flag.replace(from, to);
        }
    }
}

fn sanitize_flag_list(values: Vec<String>, label: &str) -> Vec<String> {
    let mut sanitized = Vec::with_capacity(values.len());
    for value in values {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed == "-" {
            crate::log_warn!(
                "Dropping invalid {} entry '-'; compiler/linker flag lists cannot contain a bare dash",
                label
            );
            continue;
        }
        sanitized.push(trimmed.to_string());
    }
    sanitized
}

fn replaced_flags(values: &[String], replacements: &[String], label: &str) -> Vec<String> {
    let mut current = values.to_vec();
    apply_replacement_rules(
        &mut current,
        replacements,
        &format!("{label} replacement rules"),
    );
    sanitize_flag_list(current, label)
}

pub(crate) fn install_dirs(flags: &crate::package::BuildFlags) -> InstallDirs {
    let libdir = configured_install_dir(
        &flags.libdir,
        default_libdir_for_variant(flags.lib32_variant),
    );
    let datarootdir = configured_install_dir(&flags.datarootdir, "/usr/share");
    let default_mandir = format!("{datarootdir}/man");
    let default_infodir = format!("{datarootdir}/info");

    InstallDirs {
        bindir: configured_install_dir(&flags.bindir, "/usr/bin"),
        sbindir: configured_install_dir(&flags.sbindir, "/usr/bin"),
        libdir: libdir.clone(),
        libexecdir: configured_install_dir(&flags.libexecdir, &libdir),
        sysconfdir: configured_install_dir(&flags.sysconfdir, "/etc"),
        localstatedir: configured_install_dir(&flags.localstatedir, "/var"),
        sharedstatedir: configured_install_dir(&flags.sharedstatedir, "/var/lib"),
        includedir: configured_install_dir(&flags.includedir, "/usr/include"),
        datarootdir: datarootdir.clone(),
        datadir: configured_install_dir(&flags.datadir, &datarootdir),
        mandir: configured_install_dir(&flags.mandir, &default_mandir),
        infodir: configured_install_dir(&flags.infodir, &default_infodir),
    }
}

fn apply_install_dir_env_vars(env_vars: &mut EnvVars, flags: &crate::package::BuildFlags) {
    let dirs = install_dirs(flags);
    set_env_var(env_vars, "PREFIX", flags.prefix.clone());
    set_env_var(env_vars, "BINDIR", dirs.bindir);
    set_env_var(env_vars, "SBINDIR", dirs.sbindir);
    set_env_var(env_vars, "LIBDIR", dirs.libdir);
    set_env_var(env_vars, "LIBEXECDIR", dirs.libexecdir);
    set_env_var(env_vars, "SYSCONFDIR", dirs.sysconfdir);
    set_env_var(env_vars, "LOCALSTATEDIR", dirs.localstatedir);
    set_env_var(env_vars, "SHAREDSTATEDIR", dirs.sharedstatedir);
    set_env_var(env_vars, "INCLUDEDIR", dirs.includedir);
    set_env_var(env_vars, "DATAROOTDIR", dirs.datarootdir);
    set_env_var(env_vars, "DATADIR", dirs.datadir);
    set_env_var(env_vars, "MANDIR", dirs.mandir);
    set_env_var(env_vars, "INFODIR", dirs.infodir);
}

pub(crate) fn install_destdir_path(
    build_dir: &Path,
    destdir: &Path,
    lib32_variant: bool,
) -> PathBuf {
    if lib32_variant {
        build_dir.join("destdir")
    } else {
        destdir.to_path_buf()
    }
}

pub(crate) fn stage_lib32_install_tree(staging_destdir: &Path, destdir: &Path) -> Result<()> {
    let lib_rel = lib32_stage_source_rel(staging_destdir)?;
    crate::fs_copy::copy_tree_preserving_links(
        &staging_destdir.join(&lib_rel),
        &destdir.join("usr/lib32"),
    )
}
fn lib32_stage_source_rel(staging_destdir: &Path) -> Result<PathBuf> {
    let staged_lib32 = PathBuf::from("usr/lib32");
    if staging_destdir.join(&staged_lib32).exists() {
        return Ok(staged_lib32);
    }

    let staged_lib = PathBuf::from("usr/lib");
    if staging_destdir.join(&staged_lib).exists() {
        crate::log_warn!(
            "lib32 install populated {} instead of usr/lib32; relocating staged libraries",
            staging_destdir.join(&staged_lib).display()
        );
        return Ok(staged_lib);
    }

    anyhow::bail!(
        "lib32 install did not populate {} or {}",
        staging_destdir.join("usr/lib32").display(),
        staging_destdir.join("usr/lib").display()
    );
}

fn compiler_flag_sets(
    flags: &crate::package::BuildFlags,
) -> (Vec<String>, Vec<String>, Vec<String>, Vec<String>) {
    let mut cflags = replaced_flags(&flags.cflags, &flags.replace_cflags, "build.flags.cflags");
    let mut cxxflags = replaced_flags(
        &flags.cxxflags,
        &flags.replace_cxxflags,
        "build.flags.cxxflags",
    );
    let mut ldflags = replaced_flags(
        &flags.ldflags,
        &flags.replace_ldflags,
        "build.flags.ldflags",
    );
    let ltoflags = replaced_flags(
        &flags.ltoflags,
        &flags.replace_ltoflags,
        "build.flags.ltoflags",
    );

    if let Some(fuse_ld) = fuse_ld_flag(&flags.fuse_ld) {
        ldflags.insert(0, fuse_ld);
    }

    if flags.use_lto && !ltoflags.is_empty() {
        cflags.extend(ltoflags.iter().cloned());
        cxxflags.extend(ltoflags.iter().cloned());
        ldflags.extend(ltoflags.iter().cloned());
    }

    (cflags, cxxflags, ldflags, ltoflags)
}

fn fuse_ld_flag(fuse_ld: &str) -> Option<String> {
    let fuse_ld = fuse_ld.trim();
    if fuse_ld.is_empty() {
        None
    } else if fuse_ld.starts_with("-fuse-ld=") || fuse_ld.starts_with("--ld-path=") {
        Some(fuse_ld.to_string())
    } else if fuse_ld.contains('/') {
        Some(format!("-fuse-ld={fuse_ld}"))
    } else {
        let driver_name = match fuse_ld {
            "ld.lld" => "lld",
            "ld.mold" => "mold",
            "ld.gold" => "gold",
            "ld.bfd" => "bfd",
            _ => fuse_ld,
        };
        Some(format!("-fuse-ld={driver_name}"))
    }
}

fn rust_ltoflags(flags: &crate::package::BuildFlags) -> Vec<String> {
    sanitize_flag_list(flags.rustltoflags.clone(), "build.flags.rustltoflags")
}

pub(crate) fn effective_rustflags(flags: &crate::package::BuildFlags) -> Vec<String> {
    let mut rustflags = replaced_flags(
        &flags.rustflags,
        &flags.replace_rustflags,
        "build.flags.rustflags",
    );
    let rust_ltoflags = rust_ltoflags(flags);
    if flags.use_lto && !rust_ltoflags.is_empty() {
        rustflags.extend(rust_ltoflags);
    }
    rustflags
}

pub fn standard_build_env(
    spec: &PackageSpec,
    cross: Option<&CrossConfig>,
    include_compiler_env: bool,
    export_compiler_flags: bool,
) -> EnvVars {
    let flags = &spec.build.flags;
    let mut env_vars: EnvVars = Vec::new();
    let export_compiler_flags = export_compiler_flags && !flags.no_flags;

    if !flags.tool_dir.trim().is_empty() {
        set_expanded_env_var(&mut env_vars, "TOOL_DIR", flags.tool_dir.trim());
    }

    if include_compiler_env && export_compiler_flags {
        let (cflags, cxxflags, ldflags, ltoflags) = compiler_flag_sets(flags);

        if !cflags.is_empty() {
            set_expanded_env_var(&mut env_vars, "CFLAGS", cflags.join(" "));
        }
        if !cxxflags.is_empty() {
            set_expanded_env_var(&mut env_vars, "CXXFLAGS", cxxflags.join(" "));
        }
        if !ltoflags.is_empty() {
            set_expanded_env_var(&mut env_vars, "LTOFLAGS", ltoflags.join(" "));
        }
        let rust_ltoflags = rust_ltoflags(flags);
        if !rust_ltoflags.is_empty() {
            set_expanded_env_var(&mut env_vars, "RUSTLTOFLAGS", rust_ltoflags.join(" "));
        }

        let ldflags = if !ldflags.is_empty() || !flags.libc.is_empty() {
            if flags.libc.is_empty() {
                ldflags.join(" ")
            } else if ldflags.is_empty() {
                format!("-Wl,--dynamic-linker={}", flags.libc)
            } else {
                format!("{} -Wl,--dynamic-linker={}", ldflags.join(" "), flags.libc)
            }
        } else {
            String::new()
        };
        if !ldflags.is_empty() {
            set_expanded_env_var(&mut env_vars, "LDFLAGS", ldflags);
        }
    }

    if !flags.chost.is_empty() {
        set_env_var(&mut env_vars, "CHOST", flags.chost.clone());
    }
    if !flags.cbuild.is_empty() {
        set_env_var(&mut env_vars, "CBUILD", flags.cbuild.clone());
    }
    let target_kind = if flags.lib32_variant {
        TargetBuildKind::Lib32
    } else {
        TargetBuildKind::Primary
    };
    let effective_carch = effective_target_arch(flags, cross, target_kind);
    if !effective_carch.is_empty() {
        set_env_var(&mut env_vars, "CARCH", effective_carch);
    }
    apply_install_dir_env_vars(&mut env_vars, flags);
    if !flags.makeflags.trim().is_empty() {
        set_expanded_env_var(&mut env_vars, "MAKEFLAGS", flags.makeflags.trim());
    }

    set_env_var(&mut env_vars, "DEPOT_ROOTFS", flags.rootfs.clone());
    set_env_var(
        &mut env_vars,
        "DEPOT_SPECDIR",
        spec.spec_dir.to_string_lossy().into_owned(),
    );

    if include_compiler_env {
        if let Some(cc_cfg) = cross {
            let default_flags = BuildFlags::default();
            set_expanded_env_var(
                &mut env_vars,
                "CC",
                configured_defaulted_tool_or_default(&flags.cc, &default_flags.cc, &cc_cfg.cc),
            );
            set_expanded_env_var(
                &mut env_vars,
                "CXX",
                configured_defaulted_tool_or_default(&flags.cxx, &default_flags.cxx, &cc_cfg.cxx),
            );
            set_expanded_env_var(
                &mut env_vars,
                "AR",
                configured_defaulted_tool_or_default(&flags.ar, &default_flags.ar, &cc_cfg.ar),
            );
            set_expanded_env_var(
                &mut env_vars,
                "RANLIB",
                configured_tool_or_default(&flags.ranlib, &cc_cfg.ranlib),
            );
            set_expanded_env_var(
                &mut env_vars,
                "STRIP",
                configured_tool_or_default(&flags.strip, &cc_cfg.strip),
            );
            set_expanded_env_var(
                &mut env_vars,
                "LD",
                configured_tool_or_default(&flags.ld, &cc_cfg.ld),
            );
            set_expanded_env_var(
                &mut env_vars,
                "NM",
                configured_tool_or_default(&flags.nm, &cc_cfg.nm),
            );
            set_expanded_env_var(
                &mut env_vars,
                "OBJCOPY",
                configured_tool_or_default(&flags.objcopy, &cc_cfg.objcopy),
            );
            set_expanded_env_var(
                &mut env_vars,
                "OBJDUMP",
                configured_tool_or_default(&flags.objdump, &cc_cfg.objdump),
            );
            set_expanded_env_var(
                &mut env_vars,
                "READELF",
                configured_tool_or_default(&flags.readelf, &cc_cfg.readelf),
            );
            if !flags.cpp.trim().is_empty() {
                set_expanded_env_var(&mut env_vars, "CPP", flags.cpp.trim());
            }
            set_env_var(&mut env_vars, "CROSS_PREFIX", cc_cfg.prefix.clone());
            set_env_var(
                &mut env_vars,
                "CROSS_COMPILE",
                format!("{}-", cc_cfg.prefix),
            );
        } else {
            set_expanded_env_var(&mut env_vars, "CC", flags.cc.trim());
            set_expanded_env_var(&mut env_vars, "CXX", flags.cxx.trim());
            set_expanded_env_var(&mut env_vars, "AR", flags.ar.trim());
            if !flags.ranlib.trim().is_empty() {
                set_expanded_env_var(&mut env_vars, "RANLIB", flags.ranlib.trim());
            }
            if !flags.strip.trim().is_empty() {
                set_expanded_env_var(&mut env_vars, "STRIP", flags.strip.trim());
            }
            if !flags.ld.trim().is_empty() {
                set_expanded_env_var(&mut env_vars, "LD", flags.ld.trim());
            }
            if !flags.nm.trim().is_empty() {
                set_expanded_env_var(&mut env_vars, "NM", flags.nm.trim());
            }
            if !flags.objcopy.trim().is_empty() {
                set_expanded_env_var(&mut env_vars, "OBJCOPY", flags.objcopy.trim());
            }
            if !flags.objdump.trim().is_empty() {
                set_expanded_env_var(&mut env_vars, "OBJDUMP", flags.objdump.trim());
            }
            if !flags.readelf.trim().is_empty() {
                set_expanded_env_var(&mut env_vars, "READELF", flags.readelf.trim());
            }
            if !flags.cpp.trim().is_empty() {
                set_expanded_env_var(&mut env_vars, "CPP", flags.cpp.trim());
            }
        }
    }

    for key in &flags.passthrough_env {
        let key = key.trim();
        if key.is_empty() || key.contains('=') {
            continue;
        }
        if env_vars.iter().any(|(existing, _)| existing == key) {
            continue;
        }
        if let Ok(value) = std::env::var(key) {
            set_env_var(&mut env_vars, key, value);
        }
    }

    apply_declared_env_vars(spec, &mut env_vars);

    env_vars
}

pub fn ensure_host_build(
    spec: &PackageSpec,
    src_dir: &Path,
    cross: Option<&CrossConfig>,
    export_compiler_flags: bool,
    kind: TargetBuildKind,
) -> Result<Option<PathBuf>> {
    if !spec.build.flags.host_build
        || !target_arch_differs_from_host(&spec.build.flags, cross, kind)
    {
        return Ok(None);
    }

    let host_dir = match spec.build.build_type {
        BuildType::Autotools => autotools::ensure_host_build(spec, src_dir, export_compiler_flags)?,
        BuildType::CMake => cmake::ensure_host_build(spec, src_dir, export_compiler_flags)?,
        BuildType::Meson => meson::ensure_host_build(spec, src_dir, export_compiler_flags)?,
        other => {
            anyhow::bail!(
                "build.flags.host_build is currently supported only for autotools/cmake/meson (got {:?})",
                other
            );
        }
    };

    Ok(Some(host_dir))
}

/// Prepare a Command with a hermetic environment and some essential variables preserved.
pub fn prepare_command(cmd: &mut Command, env_vars: &EnvVars) {
    cmd.env_clear();

    if let Some(path) = sanitized_build_path() {
        cmd.env("PATH", path);
    }

    // Preserve essential environment variables
    for var in &[
        "LANG",
        "HOME",
        "DESTDIR",
        "DEPOT_ROOTFS",
        "CARGO_HOME",
        "RUSTUP_HOME",
        "RUSTUP_TOOLCHAIN",
        "RUSTC",
        "RUSTDOC",
        "TERM",
        "COLORTERM",
        "NO_COLOR",
        "CLICOLOR",
        "CLICOLOR_FORCE",
        "FORCE_COLOR",
    ] {
        if let Ok(val) = std::env::var(var) {
            cmd.env(var, val);
        }
    }
    // Use a deterministic POSIX shell for build tooling. Inheriting an
    // interactive shell (e.g. zsh) can make Autotools-generated scripts
    // produce non-reproducible or incompatible shell fragments.
    cmd.env("SHELL", "/bin/sh");
    // Set requested environment variables
    for (key, val) in env_vars {
        cmd.env(key, val);
    }
}

fn sanitized_build_path() -> Option<OsString> {
    use std::path::PathBuf;

    let mut parts: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|raw| std::env::split_paths(&raw).collect())
        .unwrap_or_default();

    for dir in ["/bin", "/usr/bin", "/sbin", "/usr/sbin"] {
        let path = PathBuf::from(dir);
        if path.exists() && !parts.iter().any(|p| p == &path) {
            parts.push(path);
        }
    }

    if parts.is_empty() {
        return None;
    }

    std::env::join_paths(parts).ok()
}

/// Prepare a Command for interactive tool execution with live terminal output.
pub fn prepare_tool_command(cmd: &mut Command, env_vars: &EnvVars) {
    prepare_command(cmd, env_vars);
    cmd.stdin(Stdio::inherit());
    cmd.stdout(Stdio::inherit());
    cmd.stderr(Stdio::inherit());
}

/// Build a package using the appropriate build system
pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
    export_compiler_flags: bool,
    host_build_dir: Option<&Path>,
) -> Result<()> {
    let target_kind = if spec.build.flags.lib32_variant {
        TargetBuildKind::Lib32
    } else {
        TargetBuildKind::Primary
    };
    let target_configured_spec = spec_with_target_configure(spec, cross, target_kind);
    let spec = target_configured_spec.as_ref().unwrap_or(spec);

    if let Some(cc) = cross {
        crate::log_info!(
            "Cross-compiling for {} with {:?}...",
            cc.prefix,
            spec.build.build_type
        );
    } else {
        crate::log_info!("Building with {:?}...", spec.build.build_type);
    }

    // Clean destdir to prevent stale files/directories (e.g., directories where symlinks should be)
    if destdir.exists() {
        std::fs::remove_dir_all(destdir)?;
    }

    match spec.build.build_type {
        BuildType::Autotools => autotools::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
        BuildType::CMake => cmake::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
        BuildType::Meson => meson::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
        BuildType::Perl => perl::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
        BuildType::Custom => custom::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
        BuildType::Python => python::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
        BuildType::Rust => rust::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
        BuildType::Dkms => dkms::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
        BuildType::Bin => bin::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
        BuildType::Meta => {
            // Metapackages are metadata-only; create an empty staging root and let
            // packaging/installation metadata carry dependencies.
            std::fs::create_dir_all(destdir)?;
            Ok(())
        }
        BuildType::Makefile => makefile::build(
            spec,
            src_dir,
            destdir,
            cross,
            export_compiler_flags,
            host_build_dir,
        ),
    }
}
#[cfg(test)]
mod tests;
