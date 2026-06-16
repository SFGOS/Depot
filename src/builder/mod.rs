//! Build system abstraction

mod autotools;
mod bin;
mod cmake;
mod custom;
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
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec};
    use crate::test_support::TestEnv;
    use std::collections::HashMap;
    use std::ffi::OsStr;
    use std::fs;
    use std::path::PathBuf;

    fn mk_spec(cflags: Vec<&str>, ldflags: Vec<&str>) -> PackageSpec {
        let flags = BuildFlags {
            cflags: cflags.into_iter().map(String::from).collect(),
            ldflags: ldflags.into_iter().map(String::from).collect(),
            ..BuildFlags::default()
        };
        PackageSpec {
            package: PackageInfo {
                name: "env-test".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                built_against: Vec::new(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![crate::package::Source {
                url: "https://example.test/src.tar.gz".into(),
                sha256: "abc".into(),
                extract_dir: "src".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Custom,
                flags,
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        }
    }

    fn env_value<'a>(env: &'a EnvVars, key: &str) -> Option<&'a str> {
        env.iter()
            .find(|(candidate, _)| candidate == key)
            .map(|(_, value)| value.as_str())
    }

    #[test]
    fn test_prepare_command() {
        let mut cmd = Command::new("ls");
        // Set an env var that should be cleared
        cmd.env("FORBIDDEN", "value");
        // Set PATH manually in the current process to ensure it's picked up if it exists
        let mut env = TestEnv::new();
        env.set_var("PATH", "/usr/bin");
        env.set_var("HOME", "/home/test");
        env.set_var("SHELL", "/bin/zsh");
        env.set_var("DEPOT_ROOTFS", "/my/rootfs");
        env.set_var("TERM", "xterm-256color");
        env.set_var("CLICOLOR_FORCE", "1");

        prepare_command(&mut cmd, &vec![("MYVAR".to_string(), "myval".to_string())]);

        let envs: HashMap<_, _> = cmd.get_envs().collect();
        assert!(envs.contains_key(OsStr::new("PATH")));
        assert!(envs.contains_key(OsStr::new("HOME")));
        assert!(!envs.contains_key(OsStr::new("FORBIDDEN")));
        assert_eq!(
            envs.get(OsStr::new("SHELL")),
            Some(&Some(std::ffi::OsString::from("/bin/sh").as_os_str()))
        );
        assert_eq!(
            envs.get(OsStr::new("MYVAR")),
            Some(&Some(std::ffi::OsString::from("myval").as_os_str()))
        );
        // DEPOT_ROOTFS should be preserved from the parent environment
        assert_eq!(
            envs.get(OsStr::new("DEPOT_ROOTFS")),
            Some(&Some(std::ffi::OsString::from("/my/rootfs").as_os_str()))
        );
        assert_eq!(
            envs.get(OsStr::new("TERM")),
            Some(&Some(
                std::ffi::OsString::from("xterm-256color").as_os_str()
            ))
        );
        assert_eq!(
            envs.get(OsStr::new("CLICOLOR_FORCE")),
            Some(&Some(std::ffi::OsString::from("1").as_os_str()))
        );
    }

    #[test]
    fn test_prepare_command_preserves_destdir() {
        let mut cmd = std::process::Command::new("ls");
        let mut env = TestEnv::new();
        env.set_var("DESTDIR", "/tmp/dest");
        prepare_command(&mut cmd, &Vec::new());
        let envs: HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(
            envs.get(OsStr::new("DESTDIR")),
            Some(&Some(std::ffi::OsString::from("/tmp/dest").as_os_str()))
        );
    }

    #[test]
    fn test_prepare_command_preserves_rust_toolchain_homes() {
        let mut cmd = std::process::Command::new("ls");
        let mut env = TestEnv::new();
        env.set_var("CARGO_HOME", "/var/cache/cargo-home");
        env.set_var("RUSTUP_HOME", "/var/cache/rustup-home");
        prepare_command(&mut cmd, &Vec::new());
        let envs: HashMap<_, _> = cmd.get_envs().collect();
        assert_eq!(
            envs.get(OsStr::new("CARGO_HOME")),
            Some(&Some(
                std::ffi::OsString::from("/var/cache/cargo-home").as_os_str()
            ))
        );
        assert_eq!(
            envs.get(OsStr::new("RUSTUP_HOME")),
            Some(&Some(
                std::ffi::OsString::from("/var/cache/rustup-home").as_os_str()
            ))
        );
    }

    #[test]
    fn test_build_tool_package_option_maps_supported_builders() {
        assert_eq!(
            build_tool_package_option(BuildType::Meson),
            Some("DEPOT_MESON_PACKAGE")
        );
        assert_eq!(
            build_tool_package_option(BuildType::CMake),
            Some("DEPOT_CMAKE_PACKAGE")
        );
        assert_eq!(build_tool_package_option(BuildType::Bin), None);
    }

    #[test]
    fn test_static_build_args_skip_disable_static_when_no_delete_static_enabled() {
        let args = static_build_args_for_request(BuildType::Autotools, Some(false), true);
        assert!(args.is_empty());

        let args = static_build_args_for_request(BuildType::CMake, Some(false), true);
        assert!(args.is_empty());
    }

    #[test]
    fn test_static_build_args_keep_other_requested_modes() {
        assert_eq!(
            static_build_args_for_request(BuildType::Autotools, Some(false), false),
            vec![
                "--enable-shared".to_string(),
                "--disable-static".to_string()
            ]
        );
        assert_eq!(
            static_build_args_for_request(BuildType::CMake, Some(false), false),
            vec!["-DBUILD_SHARED_LIBS=ON".to_string()]
        );
        assert_eq!(
            static_build_args_for_request(BuildType::Meson, Some(false), false),
            vec!["-Ddefault_library=shared".to_string()]
        );
        assert_eq!(
            static_build_args_for_request(BuildType::Perl, Some(false), false),
            vec!["LINKTYPE=dynamic".to_string()]
        );
        assert_eq!(
            static_build_args_for_request(BuildType::Meson, Some(true), true),
            vec!["-Ddefault_library=static".to_string()]
        );
    }

    #[test]
    fn test_standard_build_env_exports_native_linker_and_cpp() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.ranlib = "llvm-ranlib".to_string();
        spec.build.flags.strip = "llvm-strip".to_string();
        spec.build.flags.ld = "ld.lld".to_string();
        spec.build.flags.nm = "llvm-nm".to_string();
        spec.build.flags.objcopy = "llvm-objcopy".to_string();
        spec.build.flags.objdump = "llvm-objdump".to_string();
        spec.build.flags.readelf = "llvm-readelf".to_string();
        spec.build.flags.cpp = "clang-cpp".to_string();

        let env = standard_build_env(&spec, None, true, true);
        assert!(env.iter().any(|(k, v)| k == "RANLIB" && v == "llvm-ranlib"));
        assert!(env.iter().any(|(k, v)| k == "STRIP" && v == "llvm-strip"));
        assert!(env.iter().any(|(k, v)| k == "LD" && v == "ld.lld"));
        assert!(env.iter().any(|(k, v)| k == "NM" && v == "llvm-nm"));
        assert!(
            env.iter()
                .any(|(k, v)| k == "OBJCOPY" && v == "llvm-objcopy")
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "OBJDUMP" && v == "llvm-objdump")
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "READELF" && v == "llvm-readelf")
        );
        assert!(env.iter().any(|(k, v)| k == "CPP" && v == "clang-cpp"));
    }

    #[test]
    fn test_standard_build_env_exports_tool_dir_and_expands_tool_commands() {
        let mut spec = mk_spec(
            vec!["--gcc-toolchain=$TOOL_DIR"],
            vec!["-B$TOOL_DIR", "-Wl,--as-needed"],
        );
        spec.build.flags.tool_dir = "/opt/depot-tools/bin".to_string();
        spec.build.flags.cc = "$TOOL_DIR/clang".to_string();
        spec.build.flags.cxx = "$TOOL_DIR/clang++".to_string();
        spec.build.flags.ar = "$TOOL_DIR/llvm-ar".to_string();
        spec.build.flags.ranlib = "$TOOL_DIR/llvm-ranlib".to_string();
        spec.build.flags.ld = "$TOOL_DIR/ld.lld".to_string();
        spec.build.flags.env_vars = vec!["LLVM_CONFIG=$TOOL_DIR/llvm-config".to_string()];

        let env = standard_build_env(&spec, None, true, true);

        assert_eq!(env_value(&env, "TOOL_DIR"), Some("/opt/depot-tools/bin"));
        assert_eq!(env_value(&env, "CC"), Some("/opt/depot-tools/bin/clang"));
        assert_eq!(env_value(&env, "CXX"), Some("/opt/depot-tools/bin/clang++"));
        assert_eq!(env_value(&env, "AR"), Some("/opt/depot-tools/bin/llvm-ar"));
        assert_eq!(
            env_value(&env, "RANLIB"),
            Some("/opt/depot-tools/bin/llvm-ranlib")
        );
        assert_eq!(env_value(&env, "LD"), Some("/opt/depot-tools/bin/ld.lld"));
        assert_eq!(
            env_value(&env, "CFLAGS"),
            Some("--gcc-toolchain=/opt/depot-tools/bin")
        );
        assert_eq!(
            env_value(&env, "LDFLAGS"),
            Some("-B/opt/depot-tools/bin -Wl,--as-needed")
        );
        assert_eq!(
            env_value(&env, "LLVM_CONFIG"),
            Some("/opt/depot-tools/bin/llvm-config")
        );
    }

    #[test]
    fn test_standard_build_env_cross_uses_package_tool_overrides() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.cc = "/tools/bin/clang".to_string();
        spec.build.flags.cxx = "/tools/bin/clang++".to_string();
        spec.build.flags.ar = "/tools/bin/llvm-ar".to_string();
        spec.build.flags.ranlib = "/tools/bin/llvm-ranlib".to_string();
        spec.build.flags.strip = "/tools/bin/llvm-strip".to_string();
        spec.build.flags.ld = "/tools/bin/ld.lld".to_string();
        spec.build.flags.nm = "/tools/bin/llvm-nm".to_string();
        spec.build.flags.objcopy = "/tools/bin/llvm-objcopy".to_string();
        spec.build.flags.objdump = "/tools/bin/llvm-objdump".to_string();
        spec.build.flags.readelf = "/tools/bin/llvm-readelf".to_string();
        spec.build.flags.cpp = "/tools/bin/clang-cpp".to_string();
        let cross = CrossConfig {
            prefix: "x86_64-test-linux-gnu".into(),
            cc: "x86_64-test-linux-gnu-gcc".into(),
            cxx: "x86_64-test-linux-gnu-g++".into(),
            ar: "x86_64-test-linux-gnu-ar".into(),
            ranlib: "x86_64-test-linux-gnu-ranlib".into(),
            strip: "x86_64-test-linux-gnu-strip".into(),
            ld: "x86_64-test-linux-gnu-ld".into(),
            nm: "x86_64-test-linux-gnu-nm".into(),
            objcopy: "x86_64-test-linux-gnu-objcopy".into(),
            objdump: "x86_64-test-linux-gnu-objdump".into(),
            readelf: "x86_64-test-linux-gnu-readelf".into(),
        };

        let env = standard_build_env(&spec, Some(&cross), true, true);

        assert_eq!(env_value(&env, "CC"), Some("/tools/bin/clang"));
        assert_eq!(env_value(&env, "CXX"), Some("/tools/bin/clang++"));
        assert_eq!(env_value(&env, "AR"), Some("/tools/bin/llvm-ar"));
        assert_eq!(env_value(&env, "RANLIB"), Some("/tools/bin/llvm-ranlib"));
        assert_eq!(env_value(&env, "STRIP"), Some("/tools/bin/llvm-strip"));
        assert_eq!(env_value(&env, "LD"), Some("/tools/bin/ld.lld"));
        assert_eq!(env_value(&env, "NM"), Some("/tools/bin/llvm-nm"));
        assert_eq!(env_value(&env, "OBJCOPY"), Some("/tools/bin/llvm-objcopy"));
        assert_eq!(env_value(&env, "OBJDUMP"), Some("/tools/bin/llvm-objdump"));
        assert_eq!(env_value(&env, "READELF"), Some("/tools/bin/llvm-readelf"));
        assert_eq!(env_value(&env, "CPP"), Some("/tools/bin/clang-cpp"));
        assert_eq!(
            env_value(&env, "CROSS_PREFIX"),
            Some("x86_64-test-linux-gnu")
        );
    }

    #[test]
    fn test_standard_build_env_exports_effective_carch_for_cross_and_lib32() {
        let spec = mk_spec(Vec::new(), Vec::new());
        let cross = CrossConfig {
            prefix: "aarch64-linux-gnu".into(),
            cc: "aarch64-linux-gnu-gcc".into(),
            cxx: "aarch64-linux-gnu-g++".into(),
            ar: "aarch64-linux-gnu-ar".into(),
            ranlib: "aarch64-linux-gnu-ranlib".into(),
            strip: "aarch64-linux-gnu-strip".into(),
            ld: "aarch64-linux-gnu-ld".into(),
            nm: "aarch64-linux-gnu-nm".into(),
            objcopy: "aarch64-linux-gnu-objcopy".into(),
            objdump: "aarch64-linux-gnu-objdump".into(),
            readelf: "aarch64-linux-gnu-readelf".into(),
        };

        let cross_env = standard_build_env(&spec, Some(&cross), true, true);
        assert!(
            cross_env
                .iter()
                .any(|(k, v)| k == "CARCH" && v == "aarch64"),
            "expected cross builds to export target CARCH"
        );
        assert!(
            cross_env
                .iter()
                .any(|(k, v)| k == "OBJCOPY" && v == "aarch64-linux-gnu-objcopy"),
            "expected cross builds to export OBJCOPY"
        );
        assert!(
            cross_env
                .iter()
                .any(|(k, v)| k == "OBJDUMP" && v == "aarch64-linux-gnu-objdump"),
            "expected cross builds to export OBJDUMP"
        );
        assert!(
            cross_env
                .iter()
                .any(|(k, v)| k == "READELF" && v == "aarch64-linux-gnu-readelf"),
            "expected cross builds to export READELF"
        );

        let mut lib32_spec = spec.clone();
        lib32_spec.build.flags.lib32_variant = true;
        lib32_spec.build.flags.carch = "x86_64".into();
        let lib32_env = standard_build_env(&lib32_spec, None, true, true);
        assert!(
            lib32_env.iter().any(|(k, v)| k == "CARCH" && v == "i686"),
            "expected lib32 builds to export i686 CARCH"
        );
    }

    #[test]
    fn test_spec_with_target_configure_appends_matching_arch_args() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.configure = vec!["--base".to_string()];
        spec.build
            .flags
            .configure_arch
            .insert("aarch64".to_string(), vec!["--for-aarch64".to_string()]);
        spec.build
            .flags
            .configure_arch
            .insert("x86_64".to_string(), vec!["--for-x86".to_string()]);
        let cross = CrossConfig {
            prefix: "aarch64-linux-gnu".into(),
            cc: "aarch64-linux-gnu-gcc".into(),
            cxx: "aarch64-linux-gnu-g++".into(),
            ar: "aarch64-linux-gnu-ar".into(),
            ranlib: "aarch64-linux-gnu-ranlib".into(),
            strip: "aarch64-linux-gnu-strip".into(),
            ld: "aarch64-linux-gnu-ld".into(),
            nm: "aarch64-linux-gnu-nm".into(),
            objcopy: "aarch64-linux-gnu-objcopy".into(),
            objdump: "aarch64-linux-gnu-objdump".into(),
            readelf: "aarch64-linux-gnu-readelf".into(),
        };

        let adjusted = spec_with_target_configure(&spec, Some(&cross), TargetBuildKind::Primary)
            .expect("expected arch-specific configure args");

        assert_eq!(
            adjusted.build.flags.configure,
            vec!["--base".to_string(), "--for-aarch64".to_string()]
        );
    }

    #[test]
    fn test_spec_with_target_configure_uses_lib32_arch() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.lib32_variant = true;
        spec.build.flags.carch = "x86_64".to_string();
        spec.build
            .flags
            .configure_arch
            .insert("i686".to_string(), vec!["--for-lib32".to_string()]);

        let adjusted = spec_with_target_configure(&spec, None, TargetBuildKind::Lib32)
            .expect("expected lib32 configure args");

        assert_eq!(
            adjusted.build.flags.configure,
            vec!["--for-lib32".to_string()]
        );
    }

    #[test]
    fn test_standard_build_env_respects_export_compiler_flags_toggle() {
        let mut spec = mk_spec(vec!["-O2"], vec!["-Wl,--as-needed"]);
        spec.build.flags.cxxflags = vec!["-O2".into(), "-fno-exceptions".into()];

        let enabled = standard_build_env(&spec, None, true, true);
        assert!(
            enabled.iter().any(|(k, v)| k == "CFLAGS" && v == "-O2"),
            "expected CFLAGS to be exported when enabled"
        );
        assert!(
            enabled
                .iter()
                .any(|(k, v)| k == "CXXFLAGS" && v == "-O2 -fno-exceptions"),
            "expected CXXFLAGS to be exported when enabled"
        );
        assert!(
            enabled
                .iter()
                .any(|(k, v)| k == "LDFLAGS" && v == "-Wl,--as-needed"),
            "expected LDFLAGS to be exported when enabled"
        );

        let disabled = standard_build_env(&spec, None, true, false);
        assert!(
            !disabled.iter().any(|(k, _)| k == "CFLAGS"),
            "expected CFLAGS to be omitted when disabled"
        );
        assert!(
            !disabled.iter().any(|(k, _)| k == "CXXFLAGS"),
            "expected CXXFLAGS to be omitted when disabled"
        );
        assert!(
            !disabled.iter().any(|(k, _)| k == "LDFLAGS"),
            "expected LDFLAGS to be omitted when disabled"
        );

        let mut disabled_by_spec = spec.clone();
        disabled_by_spec.build.flags.no_flags = true;
        let disabled_env = standard_build_env(&disabled_by_spec, None, true, true);
        assert!(
            !disabled_env.iter().any(|(k, _)| k == "CFLAGS"),
            "expected CFLAGS to be omitted when no_flags is set in spec"
        );
        assert!(
            !disabled_env.iter().any(|(k, _)| k == "CXXFLAGS"),
            "expected CXXFLAGS to be omitted when no_flags is set in spec"
        );
        assert!(
            !disabled_env.iter().any(|(k, _)| k == "LDFLAGS"),
            "expected LDFLAGS to be omitted when no_flags is set in spec"
        );
    }

    #[test]
    fn test_standard_build_env_injects_ltoflags_into_compiler_and_linker_flags() {
        let mut spec = mk_spec(vec!["-O2"], vec!["-Wl,--as-needed"]);
        spec.build.flags.cxxflags = vec!["-O2".into()];
        spec.build.flags.ltoflags = vec!["-flto=auto".into(), "-fuse-linker-plugin".into()];
        spec.build.flags.use_lto = true;

        let env = standard_build_env(&spec, None, true, true);
        assert!(
            env.iter()
                .any(|(k, v)| { k == "CFLAGS" && v == "-O2 -flto=auto -fuse-linker-plugin" }),
            "expected LTOFLAGS to be appended to CFLAGS"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "CXXFLAGS" && v == "-O2 -flto=auto -fuse-linker-plugin"),
            "expected LTOFLAGS to be appended to CXXFLAGS"
        );
        assert!(
            env.iter().any(|(k, v)| {
                k == "LDFLAGS" && v == "-Wl,--as-needed -flto=auto -fuse-linker-plugin"
            }),
            "expected LTOFLAGS to be appended to LDFLAGS"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "LTOFLAGS" && v == "-flto=auto -fuse-linker-plugin"),
            "expected LTOFLAGS variable to be exported"
        );
    }

    #[test]
    fn test_standard_build_env_injects_fuse_ld_into_ldflags() {
        let mut spec = mk_spec(Vec::new(), vec!["-Wl,--as-needed"]);
        spec.build.flags.fuse_ld = "/usr/bin/ld.lld".into();

        let env = standard_build_env(&spec, None, true, true);

        assert_eq!(
            env_value(&env, "LDFLAGS"),
            Some("-fuse-ld=/usr/bin/ld.lld -Wl,--as-needed")
        );
    }

    #[test]
    fn test_standard_build_env_normalizes_fuse_ld_tool_names() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.fuse_ld = "ld.lld".into();

        let env = standard_build_env(&spec, None, true, true);

        assert_eq!(env_value(&env, "LDFLAGS"), Some("-fuse-ld=lld"));
    }

    #[test]
    fn test_standard_build_env_applies_replace_flag_rules() {
        let mut spec = mk_spec(vec!["-D_FORTIFY_SOURCE=3", "-O2"], vec!["-Wl,-O3"]);
        spec.build.flags.cxxflags = vec!["-O2".into(), "-stdlib=libc++".into()];
        spec.build.flags.replace_cflags = vec!["_FORTIFY_SOURCE=3=_FORTIFY_SOURCE=2".into()];
        spec.build.flags.replace_cxxflags = vec!["-stdlib=libc++=>-stdlib=libstdc++".into()];
        spec.build.flags.replace_ldflags = vec!["-O3=>-O2".into()];
        spec.build.flags.ltoflags = vec!["-flto=auto".into()];
        spec.build.flags.replace_ltoflags = vec!["auto=>thin".into()];
        spec.build.flags.use_lto = true;

        let env = standard_build_env(&spec, None, true, true);
        assert!(
            env.iter()
                .any(|(k, v)| k == "CFLAGS" && v == "-D_FORTIFY_SOURCE=2 -O2 -flto=thin"),
            "expected replace_cflags and replace_ltoflags to be applied"
        );
        assert!(
            env.iter()
                .any(|(k, v)| { k == "CXXFLAGS" && v == "-O2 -stdlib=libstdc++ -flto=thin" }),
            "expected replace_cxxflags to be applied"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "LDFLAGS" && v == "-Wl,-O2 -flto=thin"),
            "expected replace_ldflags to be applied"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "LTOFLAGS" && v == "-flto=thin"),
            "expected replace_ltoflags to affect exported LTOFLAGS"
        );
    }

    #[test]
    fn test_standard_build_env_drops_bare_dash_flags() {
        let mut spec = mk_spec(vec!["-O2", "-", ""], vec!["-Wl,--as-needed", "  "]);
        spec.build.flags.cxxflags = vec!["-O2".into(), "-".into(), "-fno-exceptions".into()];
        spec.build.flags.ltoflags = vec!["-".into(), "-flto=thin".into()];
        spec.build.flags.use_lto = true;

        let env = standard_build_env(&spec, None, true, true);
        assert!(
            env.iter()
                .any(|(k, v)| k == "CFLAGS" && v == "-O2 -flto=thin"),
            "expected bare dash entries to be removed from CFLAGS"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "CXXFLAGS" && v == "-O2 -fno-exceptions -flto=thin"),
            "expected bare dash entries to be removed from CXXFLAGS"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "LDFLAGS" && v == "-Wl,--as-needed -flto=thin"),
            "expected blank and bare dash entries to be removed from LDFLAGS"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "LTOFLAGS" && v == "-flto=thin"),
            "expected bare dash entries to be removed from LTOFLAGS"
        );
    }

    #[test]
    fn test_standard_build_env_skips_lto_injection_when_disabled() {
        let mut spec = mk_spec(vec!["-O2"], vec!["-Wl,--as-needed"]);
        spec.build.flags.cxxflags = vec!["-O2".into()];
        spec.build.flags.ltoflags = vec!["-flto=auto".into()];
        spec.build.flags.rustltoflags = vec!["-Clinker-plugin-lto".into()];
        spec.build.flags.use_lto = false;

        let env = standard_build_env(&spec, None, true, true);
        assert!(
            env.iter().any(|(k, v)| k == "CFLAGS" && v == "-O2"),
            "expected CFLAGS to remain unchanged when use_lto is false"
        );
        assert!(
            env.iter().any(|(k, v)| k == "CXXFLAGS" && v == "-O2"),
            "expected CXXFLAGS to remain unchanged when use_lto is false"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "LDFLAGS" && v == "-Wl,--as-needed"),
            "expected LDFLAGS to remain unchanged when use_lto is false"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "LTOFLAGS" && v == "-flto=auto"),
            "expected LTOFLAGS variable to be exported even when use_lto is false"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "RUSTLTOFLAGS" && v == "-Clinker-plugin-lto"),
            "expected RUSTLTOFLAGS variable to be exported even when use_lto is false"
        );
        assert_eq!(effective_rustflags(&spec.build.flags), Vec::<String>::new());
    }

    #[test]
    fn test_standard_build_env_exports_makeflags() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.makeflags = "-j12 --output-sync=target".to_string();

        let env = standard_build_env(&spec, None, true, true);
        assert!(
            env.iter()
                .any(|(k, v)| k == "MAKEFLAGS" && v == "-j12 --output-sync=target"),
            "expected MAKEFLAGS to be exported from build flags"
        );
    }

    #[test]
    fn test_standard_build_env_exports_install_dir_vars() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.prefix = "/opt/vertex".into();
        spec.build.flags.bindir = "/opt/vertex/bin".into();
        spec.build.flags.sbindir = "/opt/vertex/sbin".into();
        spec.build.flags.libdir = "/opt/vertex/lib".into();
        spec.build.flags.libexecdir = "/opt/vertex/libexec".into();
        spec.build.flags.sysconfdir = "/etc/vertex".into();
        spec.build.flags.localstatedir = "/var".into();
        spec.build.flags.sharedstatedir = "/var/lib".into();
        spec.build.flags.includedir = "/opt/vertex/include".into();
        spec.build.flags.datarootdir = "/opt/vertex/share".into();
        spec.build.flags.datadir = "/opt/vertex/share/data".into();
        spec.build.flags.mandir = "/opt/vertex/share/man".into();
        spec.build.flags.infodir = "/opt/vertex/share/info".into();

        let env = standard_build_env(&spec, None, false, true);

        assert_eq!(env_value(&env, "PREFIX"), Some("/opt/vertex"));
        assert_eq!(env_value(&env, "BINDIR"), Some("/opt/vertex/bin"));
        assert_eq!(env_value(&env, "SBINDIR"), Some("/opt/vertex/sbin"));
        assert_eq!(env_value(&env, "LIBDIR"), Some("/opt/vertex/lib"));
        assert_eq!(env_value(&env, "LIBEXECDIR"), Some("/opt/vertex/libexec"));
        assert_eq!(env_value(&env, "SYSCONFDIR"), Some("/etc/vertex"));
        assert_eq!(env_value(&env, "LOCALSTATEDIR"), Some("/var"));
        assert_eq!(env_value(&env, "SHAREDSTATEDIR"), Some("/var/lib"));
        assert_eq!(env_value(&env, "INCLUDEDIR"), Some("/opt/vertex/include"));
        assert_eq!(env_value(&env, "DATAROOTDIR"), Some("/opt/vertex/share"));
        assert_eq!(env_value(&env, "DATADIR"), Some("/opt/vertex/share/data"));
        assert_eq!(env_value(&env, "MANDIR"), Some("/opt/vertex/share/man"));
        assert_eq!(env_value(&env, "INFODIR"), Some("/opt/vertex/share/info"));
    }

    #[test]
    fn test_standard_build_env_install_dir_vars_use_effective_defaults() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.lib32_variant = true;

        let env = standard_build_env(&spec, None, false, true);

        assert_eq!(env_value(&env, "LIBDIR"), Some("/usr/lib32"));
        assert_eq!(env_value(&env, "LIBEXECDIR"), Some("/usr/lib32"));
        assert_eq!(env_value(&env, "DATAROOTDIR"), Some("/usr/share"));
        assert_eq!(env_value(&env, "DATADIR"), Some("/usr/share"));
    }

    #[test]
    fn test_standard_build_env_exports_passthrough_env() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.passthrough_env = vec!["RUSTFLAGS".into()];

        let mut env = TestEnv::new();
        env.set_var("RUSTFLAGS", "-C target-cpu=native");

        let env = standard_build_env(&spec, None, false, true);
        assert!(
            env.iter()
                .any(|(k, v)| k == "RUSTFLAGS" && v == "-C target-cpu=native"),
            "expected RUSTFLAGS to be copied from parent environment"
        );
    }

    #[test]
    fn test_standard_build_env_exports_declared_env_vars() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.package.version = "2.4.1".to_string();
        spec.spec_dir = PathBuf::from("/tmp/specs/demo");
        spec.build.flags.env_vars = vec![
            "SETUPTOOLS_SCM_PRETEND_VERSION=$version".into(),
            "PYO3_CONFIG_FILE=$specdir/pyo3.toml".into(),
            "PKG_CONFIG_PATH=$LIBDIR/pkgconfig".into(),
        ];

        let env = standard_build_env(&spec, None, false, true);
        assert!(
            env.iter()
                .any(|(k, v)| k == "SETUPTOOLS_SCM_PRETEND_VERSION" && v == "2.4.1"),
            "expected env_vars values to expand package variables"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "PYO3_CONFIG_FILE" && v == "/tmp/specs/demo/pyo3.toml"),
            "expected env_vars values to expand specdir variables"
        );
        assert!(
            env.iter()
                .any(|(k, v)| k == "PKG_CONFIG_PATH" && v == "/usr/lib/pkgconfig"),
            "expected env_vars values to expand install directory variables"
        );
    }

    #[test]
    fn test_standard_build_env_declared_env_vars_override_defaults_and_passthrough() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.cc = "spec-cc".to_string();
        spec.build.flags.passthrough_env = vec!["CC".into()];
        spec.build.flags.env_vars = vec!["CC=custom-cc".into()];

        let mut env = TestEnv::new();
        env.set_var("CC", "host-cc");

        let env = standard_build_env(&spec, None, true, true);
        assert!(
            env.iter().any(|(k, v)| k == "CC" && v == "custom-cc"),
            "expected explicit env_vars assignments to override default and passthrough values"
        );
    }

    #[test]
    fn test_effective_rustflags_applies_replace_rules() {
        let flags = BuildFlags {
            rustflags: vec!["-C".into(), "debuginfo=2".into()],
            replace_rustflags: vec!["debuginfo=2=>opt-level=2".into()],
            ..BuildFlags::default()
        };

        assert_eq!(effective_rustflags(&flags), vec!["-C", "opt-level=2"]);
    }

    #[test]
    fn test_effective_rustflags_appends_rustltoflags_when_enabled() {
        let flags = BuildFlags {
            rustflags: vec!["-C".into(), "opt-level=3".into()],
            rustltoflags: vec!["-Clinker-plugin-lto".into(), "-Cembed-bitcode=yes".into()],
            use_lto: true,
            ..BuildFlags::default()
        };

        assert_eq!(
            effective_rustflags(&flags),
            vec![
                "-C",
                "opt-level=3",
                "-Clinker-plugin-lto",
                "-Cembed-bitcode=yes"
            ]
        );
    }

    #[test]
    fn test_standard_build_env_passthrough_does_not_override_default_vars() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.cc = "spec-cc".to_string();
        spec.build.flags.passthrough_env = vec!["CC".into()];

        let mut env = TestEnv::new();
        env.set_var("CC", "host-cc");

        let env = standard_build_env(&spec, None, true, true);
        assert!(
            env.iter().any(|(k, v)| k == "CC" && v == "spec-cc"),
            "expected default CC to take precedence over passthrough CC"
        );
    }

    #[test]
    fn test_install_dirs_use_defaults_and_lib32_fallbacks() {
        let default_dirs = install_dirs(&BuildFlags::default());
        assert_eq!(default_dirs.bindir, "/usr/bin");
        assert_eq!(default_dirs.sbindir, "/usr/bin");
        assert_eq!(default_dirs.libdir, "/usr/lib");
        assert_eq!(default_dirs.libexecdir, "/usr/lib");
        assert_eq!(default_dirs.datarootdir, "/usr/share");
        assert_eq!(default_dirs.datadir, "/usr/share");

        let lib32_dirs = install_dirs(&BuildFlags {
            lib32_variant: true,
            ..BuildFlags::default()
        });
        assert_eq!(lib32_dirs.libdir, "/usr/lib32");
        assert_eq!(lib32_dirs.libexecdir, "/usr/lib32");
    }

    #[test]
    fn test_build_helper_context_restores_runtime_build_flags() {
        let mut spec = mk_spec(Vec::new(), Vec::new());
        spec.build.flags.lib32_variant = true;
        spec.build.flags.host_build_dir = Some("/tmp/build-host".into());

        let restored = BuildHelperContext::from_spec(&spec).build_flags();
        assert!(restored.lib32_variant);
        assert_eq!(restored.host_build_dir.as_deref(), Some("/tmp/build-host"));
    }

    #[test]
    fn test_install_dirs_respect_explicit_overrides_and_derived_defaults() {
        let dirs = install_dirs(&BuildFlags {
            bindir: "/opt/bin".into(),
            libdir: "/opt/lib64".into(),
            datarootdir: "/opt/share-root".into(),
            ..BuildFlags::default()
        });

        assert_eq!(dirs.bindir, "/opt/bin");
        assert_eq!(dirs.libdir, "/opt/lib64");
        assert_eq!(dirs.libexecdir, "/opt/lib64");
        assert_eq!(dirs.datarootdir, "/opt/share-root");
        assert_eq!(dirs.datadir, "/opt/share-root");
    }

    #[test]
    fn test_install_destdir_path_uses_build_dir_for_lib32() {
        let build_dir = Path::new("/tmp/build");
        let destdir = Path::new("/tmp/pkg");
        assert_eq!(install_destdir_path(build_dir, destdir, false), destdir);
        assert_eq!(
            install_destdir_path(build_dir, destdir, true),
            build_dir.join("destdir")
        );
    }

    #[test]
    fn test_stage_lib32_install_tree_uses_usr_lib32_when_present() -> Result<()> {
        let temp = tempfile::tempdir()?;
        let staging = temp.path().join("staging");
        let dest = temp.path().join("dest");
        fs::create_dir_all(staging.join("usr/lib32"))?;
        fs::create_dir_all(staging.join("usr/bin"))?;
        fs::write(staging.join("usr/lib32/libfoo.so.1"), "lib32")?;
        fs::write(staging.join("usr/bin/foo"), "bin")?;

        stage_lib32_install_tree(&staging, &dest)?;

        assert_eq!(
            fs::read_to_string(dest.join("usr/lib32/libfoo.so.1"))?,
            "lib32"
        );
        assert!(!dest.join("usr/bin/foo").exists());
        Ok(())
    }

    #[test]
    #[cfg(unix)]
    fn test_stage_lib32_install_tree_relocates_usr_lib_when_needed() -> Result<()> {
        use std::os::unix::fs as unix_fs;

        let temp = tempfile::tempdir()?;
        let staging = temp.path().join("staging");
        let dest = temp.path().join("dest");
        fs::create_dir_all(staging.join("usr/lib"))?;
        fs::create_dir_all(staging.join("usr/share/man/man1"))?;
        fs::write(staging.join("usr/lib/libfoo.so.1"), "relocated")?;
        fs::write(staging.join("usr/share/man/man1/foo.1"), "manpage")?;
        unix_fs::symlink("libfoo.so.1", staging.join("usr/lib/libfoo.so"))?;

        stage_lib32_install_tree(&staging, &dest)?;

        assert_eq!(
            fs::read_to_string(dest.join("usr/lib32/libfoo.so.1"))?,
            "relocated"
        );
        assert_eq!(
            fs::read_link(dest.join("usr/lib32/libfoo.so"))?,
            PathBuf::from("libfoo.so.1")
        );
        assert!(!dest.join("usr/share/man/man1/foo.1").exists());
        assert!(!dest.join("usr/lib").exists());
        Ok(())
    }

    #[test]
    fn test_stage_lib32_install_tree_preserves_hardlinks() -> Result<()> {
        use std::os::unix::fs::MetadataExt;

        let temp = tempfile::tempdir()?;
        let staging = temp.path().join("staging");
        let dest = temp.path().join("dest");
        fs::create_dir_all(staging.join("usr/lib32"))?;
        fs::write(staging.join("usr/lib32/libfoo.so.1"), "lib32")?;
        fs::hard_link(
            staging.join("usr/lib32/libfoo.so.1"),
            staging.join("usr/lib32/libfoo-current.so"),
        )?;

        stage_lib32_install_tree(&staging, &dest)?;

        let first = dest.join("usr/lib32/libfoo.so.1").metadata()?;
        let second = dest.join("usr/lib32/libfoo-current.so").metadata()?;
        assert_eq!(first.ino(), second.ino());
        assert_eq!(first.nlink(), 2);
        assert_eq!(second.nlink(), 2);
        Ok(())
    }
}
