//! Python build backend without external helper modules.
//!
//! This backend intentionally avoids `python -m build` and
//! `python -m installer`. It drives PEP 517 backends directly via
//! `python3` and installs wheel contents in Rust.

use crate::builder::state::{BuildStep, StateTracker};
use crate::cross::CrossConfig;
use crate::package::PackageSpec;
use crate::source::hooks;
use anyhow::{Context, Result, bail};
use std::fs;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
struct Pep517Config {
    backend: String,
    requires: Vec<String>,
    backend_paths: Vec<PathBuf>,
}

enum BuildFrontend {
    Pep517(Pep517Config),
    LegacySetupPy,
}

pub fn build(
    spec: &PackageSpec,
    src_dir: &Path,
    destdir: &Path,
    cross: Option<&CrossConfig>,
    export_compiler_flags: bool,
) -> Result<()> {
    let flags = &spec.build.flags;
    let actual_src = resolve_actual_src(spec, src_dir)?;
    let config_settings = normalize_pep517_config_settings(spec)?;
    fs::create_dir_all(destdir)
        .with_context(|| format!("Failed to create DESTDIR: {}", destdir.display()))?;

    let mut env_vars = crate::builder::standard_build_env(spec, cross, true, export_compiler_flags);
    crate::builder::set_env_var(&mut env_vars, "PYTHONNOUSERSITE", "1");
    crate::builder::set_env_var(&mut env_vars, "PYTHONDONTWRITEBYTECODE", "1");
    crate::builder::set_env_var(&mut env_vars, "SETUPTOOLS_USE_DISTUTILS", "local");

    let mut state = StateTracker::new_with_namespace(
        &actual_src,
        spec.build.flags.lib32_variant.then_some("lib32"),
    )?;
    if !state.is_done(BuildStep::Configured) {
        hooks::run_post_configure_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::Configured)?;
    }

    let dist_dir = actual_src.join("dist");
    if !state.is_done(BuildStep::PostCompileDone) {
        if dist_dir.exists() {
            fs::remove_dir_all(&dist_dir)
                .with_context(|| format!("Failed to clean dist dir: {}", dist_dir.display()))?;
        }
        fs::create_dir_all(&dist_dir)
            .with_context(|| format!("Failed to create dist dir: {}", dist_dir.display()))?;

        match detect_frontend(&actual_src)? {
            BuildFrontend::Pep517(cfg) => {
                build_wheel_pep517(&actual_src, &dist_dir, &env_vars, &cfg, &config_settings)?
            }
            BuildFrontend::LegacySetupPy => {
                if !config_settings.is_empty() {
                    bail!(
                        "build.flags.config_setting is only supported for PEP 517 builds (pyproject.toml)"
                    );
                }
                build_wheel_setup_py(&actual_src, &dist_dir, &env_vars)?;
            }
        }

        hooks::run_post_compile_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostCompileDone)?;
    } else {
        crate::log_info!("Skipping python wheel build and post-compile hooks (already done)");
    }

    if !state.is_done(BuildStep::PostInstallDone) {
        let wheels = collect_wheels(&dist_dir)?;
        let py_version = detect_python_major_minor(&env_vars)?;
        let prefix_rel = normalized_prefix(&flags.prefix)?;
        for wheel in wheels {
            install_wheel(&wheel, destdir, &prefix_rel, &py_version)?;
        }

        hooks::run_post_install_commands(spec, &actual_src, destdir)?;
        state.mark_done(BuildStep::PostInstallDone)?;
    } else {
        crate::log_info!("Skipping python wheel install and post-install hooks (already done)");
    }

    Ok(())
}

fn normalize_pep517_config_settings(spec: &PackageSpec) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for raw in &spec.build.flags.config_settings {
        let expanded = spec.expand_vars(raw);
        let trimmed = expanded.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.contains('\n') || trimmed.contains('\r') {
            bail!(
                "Invalid build.flags.config_setting entry '{}': newlines are not allowed",
                raw
            );
        }

        let key = trimmed.split_once('=').map(|(k, _)| k).unwrap_or(trimmed);
        let key = key.trim();
        if key.is_empty() || key.chars().any(char::is_whitespace) {
            bail!(
                "Invalid build.flags.config_setting entry '{}'; expected KEY=VALUE or KEY",
                raw
            );
        }

        if let Some((_, value)) = trimmed.split_once('=') {
            out.push(format!("{key}={value}"));
        } else {
            out.push(format!("{key}="));
        }
    }
    Ok(out)
}

fn resolve_actual_src(spec: &PackageSpec, src_dir: &Path) -> Result<PathBuf> {
    let source_subdir = spec.expand_vars(&spec.build.flags.source_subdir);
    if source_subdir.is_empty() {
        return Ok(src_dir.to_path_buf());
    }

    let candidate = Path::new(&source_subdir);
    if candidate.is_absolute() {
        if candidate.exists() {
            return Ok(candidate.to_path_buf());
        }
        bail!(
            "Source directory not found: {} (source_subdir: {} -> {})",
            candidate.display(),
            spec.build.flags.source_subdir,
            source_subdir
        );
    }

    let under_src = src_dir.join(&source_subdir);
    if under_src.exists() {
        return Ok(under_src);
    }

    let under_spec = spec.spec_dir.join(&source_subdir);
    if under_spec.exists() {
        return Ok(under_spec);
    }

    if candidate.exists() {
        return Ok(candidate.to_path_buf());
    }

    bail!(
        "Source directory not found: {} (expanded from '{}'; tried src_dir, spec_dir, and absolute path)",
        source_subdir,
        spec.build.flags.source_subdir
    );
}

fn detect_frontend(src_dir: &Path) -> Result<BuildFrontend> {
    let pyproject = src_dir.join("pyproject.toml");
    if !pyproject.exists() {
        if src_dir.join("setup.py").exists() {
            return Ok(BuildFrontend::LegacySetupPy);
        }
        bail!(
            "Python build requires pyproject.toml (PEP 517) or setup.py in {}",
            src_dir.display()
        );
    }

    let content = fs::read_to_string(&pyproject)
        .with_context(|| format!("Failed to read {}", pyproject.display()))?;
    let parsed: toml::Value = toml::from_str(&content)
        .with_context(|| format!("Failed to parse {}", pyproject.display()))?;

    let build_system = parsed.get("build-system").and_then(|v| v.as_table());
    let backend = build_system
        .and_then(|t| t.get("build-backend"))
        .and_then(|v| v.as_str())
        .unwrap_or("setuptools.build_meta:__legacy__")
        .to_string();

    let requires = match build_system.and_then(|t| t.get("requires")) {
        Some(v) => {
            let arr = v.as_array().with_context(|| {
                format!(
                    "Invalid build-system.requires in {}: expected array",
                    pyproject.display()
                )
            })?;
            arr.iter()
                .map(|x| {
                    x.as_str().map(String::from).with_context(|| {
                        format!(
                            "Invalid build-system.requires entry in {}: expected string",
                            pyproject.display()
                        )
                    })
                })
                .collect::<Result<Vec<_>>>()?
        }
        None => Vec::new(),
    };

    let src_root = src_dir
        .canonicalize()
        .with_context(|| format!("Failed to canonicalize source dir {}", src_dir.display()))?;

    let backend_paths = match build_system.and_then(|t| t.get("backend-path")) {
        Some(v) => {
            let arr = v.as_array().with_context(|| {
                format!(
                    "Invalid build-system.backend-path in {}: expected array",
                    pyproject.display()
                )
            })?;
            let mut out = Vec::with_capacity(arr.len());
            for raw in arr {
                let rel = raw.as_str().with_context(|| {
                    format!(
                        "Invalid build-system.backend-path entry in {}: expected string",
                        pyproject.display()
                    )
                })?;
                let rel_path = Path::new(rel);
                if rel_path.is_absolute() {
                    bail!(
                        "Invalid build-system.backend-path entry in {}: absolute paths are not allowed ({})",
                        pyproject.display(),
                        rel
                    );
                }
                let joined = src_dir.join(rel_path);
                let canon = joined.canonicalize().with_context(|| {
                    format!(
                        "build-system.backend-path entry not found in {}: {}",
                        pyproject.display(),
                        joined.display()
                    )
                })?;
                if !canon.starts_with(&src_root) {
                    bail!(
                        "Invalid build-system.backend-path entry in {}: path escapes source tree ({})",
                        pyproject.display(),
                        rel
                    );
                }
                out.push(canon);
            }
            out
        }
        None => Vec::new(),
    };

    Ok(BuildFrontend::Pep517(Pep517Config {
        backend,
        requires,
        backend_paths,
    }))
}

fn build_wheel_setup_py(
    src_dir: &Path,
    dist_dir: &Path,
    env_vars: &[(String, String)],
) -> Result<()> {
    crate::log_info!("Building wheel with setup.py...");
    let mut cmd = Command::new("python3");
    cmd.current_dir(src_dir)
        .arg("setup.py")
        .arg("bdist_wheel")
        .arg("--dist-dir")
        .arg(dist_dir);
    crate::builder::prepare_tool_command(&mut cmd, &env_vars.to_vec());

    let status = cmd
        .status()
        .with_context(|| format!("Failed to run setup.py in {}", src_dir.display()))?;
    if !status.success() {
        bail!("setup.py bdist_wheel failed with status {}", status);
    }
    Ok(())
}

fn build_wheel_pep517(
    src_dir: &Path,
    dist_dir: &Path,
    env_vars: &[(String, String)],
    cfg: &Pep517Config,
    config_settings: &[String],
) -> Result<()> {
    crate::log_info!("Building wheel via PEP 517 backend {}...", cfg.backend);

    let mut cmd = Command::new("python3");
    cmd.current_dir(src_dir)
        .arg("-c")
        .arg(PEP517_BUILD_SNIPPET)
        .arg(dist_dir);

    let mut build_env = env_vars.to_vec();
    crate::builder::set_env_var(&mut build_env, "DEPOT_PY_BACKEND", cfg.backend.clone());
    crate::builder::set_env_var(
        &mut build_env,
        "DEPOT_PY_BACKEND_PATHS",
        cfg.backend_paths
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join("\n"),
    );
    crate::builder::set_env_var(
        &mut build_env,
        "DEPOT_PY_CONFIG_SETTINGS",
        config_settings.join("\n"),
    );
    crate::builder::prepare_command(&mut cmd, &build_env);

    let output = cmd.output().with_context(|| {
        format!(
            "Failed to run python3 for PEP 517 build in {}",
            src_dir.display()
        )
    })?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let requires = if cfg.requires.is_empty() {
            "(none declared)".to_string()
        } else {
            cfg.requires.join(", ")
        };
        bail!(
            "PEP 517 wheel build failed with status {}. Backend: {}. Declared build requirements: {}. stderr: {}",
            output.status,
            cfg.backend,
            requires,
            stderr.trim()
        );
    }

    Ok(())
}

fn collect_wheels(dist_dir: &Path) -> Result<Vec<PathBuf>> {
    let mut wheels = Vec::new();
    for entry in fs::read_dir(dist_dir).with_context(|| {
        format!(
            "Failed to read wheel output directory {}",
            dist_dir.display()
        )
    })? {
        let entry = entry?;
        let path = entry.path();
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e == "whl")
        {
            wheels.push(path);
        }
    }
    wheels.sort();
    if wheels.is_empty() {
        bail!("No wheel files produced in {}", dist_dir.display());
    }
    Ok(wheels)
}

fn detect_python_major_minor(env_vars: &[(String, String)]) -> Result<String> {
    let mut cmd = Command::new("python3");
    cmd.arg("-c")
        .arg("import sys;print(f\"{sys.version_info[0]}.{sys.version_info[1]}\")");
    crate::builder::prepare_command(&mut cmd, &env_vars.to_vec());
    let output = cmd
        .output()
        .context("Failed to execute python3 for version detection")?;
    if !output.status.success() {
        bail!("python3 version probe failed with status {}", output.status);
    }
    let version = String::from_utf8(output.stdout)
        .context("python3 version probe output was not valid UTF-8")?
        .trim()
        .to_string();
    if version.split('.').count() != 2 || !version.chars().all(|c| c == '.' || c.is_ascii_digit()) {
        bail!("Unexpected python3 version probe output: {}", version);
    }
    Ok(version)
}

fn normalized_prefix(prefix: &str) -> Result<PathBuf> {
    let trimmed = prefix.trim();
    let rel = trimmed.trim_start_matches('/');
    let path = PathBuf::from(rel);
    ensure_safe_relative_path(&path, "build.flags.prefix")?;
    Ok(path)
}

fn site_packages_rel(prefix_rel: &Path, py_version: &str) -> PathBuf {
    let mut out = PathBuf::new();
    if !prefix_rel.as_os_str().is_empty() {
        out.push(prefix_rel);
    }
    out.push("lib");
    out.push(format!("python{}", py_version));
    out.push("site-packages");
    out
}

fn install_wheel(
    wheel_path: &Path,
    destdir: &Path,
    prefix_rel: &Path,
    py_version: &str,
) -> Result<()> {
    crate::log_info!("Installing wheel: {}", wheel_path.display());
    let site_packages = site_packages_rel(prefix_rel, py_version);

    let file = fs::File::open(wheel_path)
        .with_context(|| format!("Failed to open wheel {}", wheel_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .with_context(|| format!("Failed to read wheel zip {}", wheel_path.display()))?;

    let mut entry_points_contents: Option<String> = None;

    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).with_context(|| {
            format!(
                "Failed to read entry {} in wheel {}",
                i,
                wheel_path.display()
            )
        })?;

        let rel = entry.enclosed_name().ok_or_else(|| {
            anyhow::anyhow!(
                "Unsafe wheel path in {}: {}",
                wheel_path.display(),
                entry.name()
            )
        })?;
        let target_rel = map_wheel_path(&rel, prefix_rel, &site_packages)?;
        let target_path = destdir.join(&target_rel);

        if let Some(mode) = entry.unix_mode()
            && (mode & 0o170000) == 0o120000
        {
            bail!(
                "Symlink entry is not allowed in wheel {}: {}",
                wheel_path.display(),
                rel.display()
            );
        }

        if entry.is_dir() {
            fs::create_dir_all(&target_path)
                .with_context(|| format!("Failed to create directory {}", target_path.display()))?;
            continue;
        }

        let mut data = Vec::new();
        entry.read_to_end(&mut data).with_context(|| {
            format!(
                "Failed to read file content from wheel {}: {}",
                wheel_path.display(),
                rel.display()
            )
        })?;

        if entry_points_contents.is_none()
            && is_top_level_dist_info_entry_points(&rel)
            && let Ok(s) = String::from_utf8(data.clone())
        {
            entry_points_contents = Some(s);
        }

        if let Some(parent) = target_path.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create {}", parent.display()))?;
        }

        fs::write(&target_path, &data)
            .with_context(|| format!("Failed to write {}", target_path.display()))?;

        #[cfg(unix)]
        if let Some(mode) = entry.unix_mode() {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&target_path)?.permissions();
            perms.set_mode(mode & 0o777);
            fs::set_permissions(&target_path, perms)?;
        }
    }

    if let Some(contents) = entry_points_contents {
        let scripts = parse_console_scripts(&contents)?;
        write_entry_point_scripts(destdir, prefix_rel, &scripts)?;
    }

    Ok(())
}

fn is_top_level_dist_info_entry_points(rel: &Path) -> bool {
    let parts = match path_parts(rel, "wheel entry") {
        Ok(p) => p,
        Err(_) => return false,
    };
    if parts.len() != 2 {
        return false;
    }
    parts[0].ends_with(".dist-info") && parts[1] == "entry_points.txt"
}

fn map_wheel_path(rel: &Path, prefix_rel: &Path, site_packages: &Path) -> Result<PathBuf> {
    let parts = path_parts(rel, "wheel path")?;
    if parts.is_empty() {
        bail!("Invalid empty wheel path");
    }

    if parts[0].ends_with(".data") {
        if parts.len() < 3 {
            bail!(
                "Invalid wheel .data path (expected at least 3 components): {}",
                rel.display()
            );
        }
        let mut out = match parts[1] {
            "purelib" | "platlib" => site_packages.to_path_buf(),
            "scripts" => join_rel(prefix_rel, "bin"),
            "headers" => join_rel(prefix_rel, "include"),
            "data" => prefix_rel.to_path_buf(),
            scheme => {
                bail!(
                    "Unsupported wheel .data scheme '{}' in {}",
                    scheme,
                    rel.display()
                )
            }
        };
        for part in &parts[2..] {
            out.push(part);
        }
        ensure_safe_relative_path(&out, "installed wheel path")?;
        return Ok(out);
    }

    let mut out = site_packages.to_path_buf();
    for part in parts {
        out.push(part);
    }
    ensure_safe_relative_path(&out, "installed wheel path")?;
    Ok(out)
}

fn join_rel(base: &Path, tail: &str) -> PathBuf {
    let mut out = PathBuf::new();
    if !base.as_os_str().is_empty() {
        out.push(base);
    }
    out.push(tail);
    out
}

fn path_parts<'a>(path: &'a Path, label: &str) -> Result<Vec<&'a str>> {
    let mut parts = Vec::new();
    for c in path.components() {
        match c {
            Component::Normal(seg) => {
                let s = seg.to_str().with_context(|| {
                    format!("Non-UTF-8 {} component in {}", label, path.display())
                })?;
                parts.push(s);
            }
            Component::CurDir => {}
            _ => bail!("Unsafe {} component in {}", label, path.display()),
        }
    }
    Ok(parts)
}

fn ensure_safe_relative_path(path: &Path, label: &str) -> Result<()> {
    if path.is_absolute() {
        bail!("{} must be relative: {}", label, path.display());
    }
    for c in path.components() {
        match c {
            Component::Normal(_) => {}
            Component::CurDir => {}
            _ => bail!("Unsafe {} path: {}", label, path.display()),
        }
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConsoleScript {
    name: String,
    module: String,
    attr_path: String,
}

fn parse_console_scripts(entry_points: &str) -> Result<Vec<ConsoleScript>> {
    let mut in_console = false;
    let mut scripts = Vec::new();

    for (idx, raw) in entry_points.lines().enumerate() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            in_console = &line[1..line.len() - 1] == "console_scripts";
            continue;
        }
        if !in_console {
            continue;
        }

        let (name_raw, target_raw) = line
            .split_once('=')
            .with_context(|| format!("Invalid console_scripts entry on line {}", idx + 1))?;
        let name = name_raw.trim();
        if name.is_empty() || name.contains('/') || name.contains('\\') {
            bail!(
                "Invalid console_scripts script name '{}' on line {}",
                name,
                idx + 1
            );
        }

        let mut target = target_raw.trim();
        if let Some(i) = target.find('[') {
            target = &target[..i];
            target = target.trim();
        }

        let (module, attr_path) = target
            .split_once(':')
            .with_context(|| format!("Invalid console_scripts target on line {}", idx + 1))?;
        let module = module.trim();
        let attr_path = attr_path.trim();
        if module.is_empty() || attr_path.is_empty() {
            bail!(
                "Invalid console_scripts target '{}' on line {}",
                target,
                idx + 1
            );
        }

        scripts.push(ConsoleScript {
            name: name.to_string(),
            module: module.to_string(),
            attr_path: attr_path.to_string(),
        });
    }

    Ok(scripts)
}

fn write_entry_point_scripts(
    destdir: &Path,
    prefix_rel: &Path,
    scripts: &[ConsoleScript],
) -> Result<()> {
    if scripts.is_empty() {
        return Ok(());
    }
    let scripts_dir = join_rel(prefix_rel, "bin");
    let scripts_path = destdir.join(&scripts_dir);
    fs::create_dir_all(&scripts_path)
        .with_context(|| format!("Failed to create {}", scripts_path.display()))?;

    for script in scripts {
        let target = scripts_path.join(&script.name);
        let module = py_single_quote_escape(&script.module);
        let attrs = py_single_quote_escape(&script.attr_path);
        let content = format!(
            "#!/usr/bin/python3\nimport importlib\nimport sys\n\nobj = importlib.import_module('{module}')\nfor attr in '{attrs}'.split('.'):\n    obj = getattr(obj, attr)\n\nif __name__ == '__main__':\n    raise SystemExit(obj())\n",
        );
        fs::write(&target, content)
            .with_context(|| format!("Failed to write {}", target.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&target)?.permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&target, perms)?;
        }
    }
    Ok(())
}

fn py_single_quote_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

const PEP517_BUILD_SNIPPET: &str = r#"
import importlib
import os
import pathlib
import sys

dist_dir = pathlib.Path(sys.argv[1])
dist_dir.mkdir(parents=True, exist_ok=True)
backend_spec = os.environ.get("DEPOT_PY_BACKEND", "").strip()
if not backend_spec:
    raise RuntimeError("DEPOT_PY_BACKEND is required")

paths = [p for p in os.environ.get("DEPOT_PY_BACKEND_PATHS", "").splitlines() if p]
for p in reversed(paths):
    if p not in sys.path:
        sys.path.insert(0, p)

module_name, _, object_path = backend_spec.partition(":")
backend = importlib.import_module(module_name)
if object_path:
    for attr in object_path.split("."):
        if attr:
            backend = getattr(backend, attr)

config_settings = {}
for raw in [p for p in os.environ.get("DEPOT_PY_CONFIG_SETTINGS", "").splitlines() if p]:
    key, _, value = raw.partition("=")
    key = key.strip()
    if not key:
        raise RuntimeError(f"Invalid DEPOT_PY_CONFIG_SETTINGS entry: {raw!r}")
    existing = config_settings.get(key)
    if existing is None:
        config_settings[key] = value
    elif isinstance(existing, list):
        existing.append(value)
    else:
        config_settings[key] = [existing, value]
if not config_settings:
    config_settings = None
if not hasattr(backend, "build_wheel"):
    raise RuntimeError(f"Backend {backend_spec!r} does not provide build_wheel")
wheel_name = backend.build_wheel(str(dist_dir), config_settings, None)
if not wheel_name:
    raise RuntimeError(f"Backend {backend_spec!r} returned an empty wheel name")
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{Build, BuildFlags, BuildType, Dependencies, PackageInfo, Source};
    use std::io::Write;
    use tempfile::tempdir;
    use zip::write::SimpleFileOptions;

    fn mk_spec() -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: "py-test".into(),
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Default::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.test/src.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "src".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Python,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        }
    }

    #[test]
    fn normalize_pep517_config_settings_accepts_key_value_and_key_only() -> Result<()> {
        let mut spec = mk_spec();
        spec.package.version = "1.2.3".into();
        spec.build.flags.config_settings = vec![
            "editable_mode=compat".into(),
            "setup-args=--plat-name=x86_64".into(),
            "builddir".into(),
            "version=$version".into(),
        ];

        let normalized = normalize_pep517_config_settings(&spec)?;
        assert_eq!(
            normalized,
            vec![
                "editable_mode=compat".to_string(),
                "setup-args=--plat-name=x86_64".to_string(),
                "builddir=".to_string(),
                "version=1.2.3".to_string(),
            ]
        );
        Ok(())
    }

    #[test]
    fn normalize_pep517_config_settings_rejects_invalid_key() {
        let mut spec = mk_spec();
        spec.build.flags.config_settings = vec!["bad key=value".into()];
        let err = normalize_pep517_config_settings(&spec)
            .expect_err("invalid config setting key should fail");
        assert!(err.to_string().contains("expected KEY=VALUE or KEY"));
    }

    #[test]
    fn parse_console_scripts_basic() -> Result<()> {
        let text = r#"
[console_scripts]
hello = demo.cli:main
tool = pkg.mod:run [extra]
"#;
        let scripts = parse_console_scripts(text)?;
        assert_eq!(scripts.len(), 2);
        assert_eq!(scripts[0].name, "hello");
        assert_eq!(scripts[0].module, "demo.cli");
        assert_eq!(scripts[0].attr_path, "main");
        assert_eq!(scripts[1].name, "tool");
        assert_eq!(scripts[1].attr_path, "run");
        Ok(())
    }

    #[test]
    fn map_wheel_data_paths() -> Result<()> {
        let prefix = PathBuf::from("usr");
        let site = PathBuf::from("usr/lib/python3.12/site-packages");
        let mapped = map_wheel_path(Path::new("pkg-1.0.data/scripts/tool"), &prefix, &site)?;
        assert_eq!(mapped, PathBuf::from("usr/bin/tool"));
        Ok(())
    }

    #[test]
    fn install_wheel_extracts_files_and_scripts() -> Result<()> {
        let _spec = mk_spec();
        let tmp = tempdir()?;
        let wheel_path = tmp.path().join("demo-1.0-py3-none-any.whl");

        let file = fs::File::create(&wheel_path)?;
        let mut zip = zip::ZipWriter::new(file);
        let file_opts = SimpleFileOptions::default().unix_permissions(0o644);
        let exec_opts = SimpleFileOptions::default().unix_permissions(0o755);

        zip.start_file("demo/__init__.py", file_opts)?;
        zip.write_all(b"__all__ = []\n")?;
        zip.start_file("demo-1.0.dist-info/WHEEL", file_opts)?;
        zip.write_all(b"Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n")?;
        zip.start_file("demo-1.0.dist-info/entry_points.txt", file_opts)?;
        zip.write_all(b"[console_scripts]\ndemo = demo.__main__:main\n")?;
        zip.start_file("demo-1.0.data/scripts/raw-script", exec_opts)?;
        zip.write_all(b"#!/bin/sh\necho raw\n")?;
        zip.finish()?;

        let dest = tmp.path().join("dest");
        fs::create_dir_all(&dest)?;
        let prefix = PathBuf::from("usr");
        install_wheel(&wheel_path, &dest, &prefix, "3.12")?;

        assert!(
            dest.join("usr/lib/python3.12/site-packages/demo/__init__.py")
                .exists()
        );
        assert!(dest.join("usr/bin/raw-script").exists());
        assert!(dest.join("usr/bin/demo").exists());
        Ok(())
    }

    #[test]
    fn install_wheel_ignores_nested_dist_info_entry_points() -> Result<()> {
        let tmp = tempdir()?;
        let wheel_path = tmp.path().join("demo-1.0-py3-none-any.whl");

        let file = fs::File::create(&wheel_path)?;
        let mut zip = zip::ZipWriter::new(file);
        let file_opts = SimpleFileOptions::default().unix_permissions(0o644);

        zip.start_file("demo/__init__.py", file_opts)?;
        zip.write_all(b"__all__ = []\n")?;
        zip.start_file("demo-1.0.dist-info/WHEEL", file_opts)?;
        zip.write_all(b"Wheel-Version: 1.0\nRoot-Is-Purelib: true\nTag: py3-none-any\n")?;
        zip.start_file("demo-1.0.dist-info/entry_points.txt", file_opts)?;
        zip.write_all(b"[console_scripts]\ndemo = demo.__main__:main\n")?;
        zip.start_file(
            "demo/vendor/wheel-0.1.dist-info/entry_points.txt",
            file_opts,
        )?;
        zip.write_all(b"[console_scripts]\nwheel = wheel.cli:main\n")?;
        zip.finish()?;

        let dest = tmp.path().join("dest");
        fs::create_dir_all(&dest)?;
        let prefix = PathBuf::from("usr");
        install_wheel(&wheel_path, &dest, &prefix, "3.12")?;

        assert!(dest.join("usr/bin/demo").exists());
        assert!(!dest.join("usr/bin/wheel").exists());
        Ok(())
    }
}
