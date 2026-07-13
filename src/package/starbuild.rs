//! Legacy STARBUILD conversion support.

use super::{
    Alternatives, Build, BuildFlags, BuildType, Dependencies, ManualSource, PackageInfo,
    PackageSpec, Source,
};
use anyhow::{Context, Result, bail};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) struct ConvertedStarbuild {
    pub output_path: PathBuf,
    pub toml: String,
    pub build_script: Option<String>,
    pub build_script_path: Option<PathBuf>,
}

#[derive(Debug, Clone, Default)]
struct ParsedStarbuild {
    package_names: Vec<String>,
    package_descriptions: Vec<String>,
    package_version: String,
    description: String,
    licenses: Vec<String>,
    dependencies: Vec<String>,
    build_dependencies: Vec<String>,
    optional_dependencies: Vec<String>,
    provides: Vec<String>,
    conflicts: Vec<String>,
    keep: Vec<String>,
    options: Vec<String>,
    sources: Vec<String>,
    custom_assignments: Vec<String>,
    custom_vars: HashMap<String, String>,
    custom_functions: Vec<String>,
    prepare: Vec<String>,
    compile: Vec<String>,
    verify: Vec<String>,
    generic_assemble: Vec<String>,
    assemble_functions: BTreeMap<String, Vec<String>>,
    output_dependencies: BTreeMap<String, Vec<String>>,
    output_optional: BTreeMap<String, Vec<String>>,
    output_provides: BTreeMap<String, Vec<String>>,
    output_conflicts: BTreeMap<String, Vec<String>>,
    output_keep: BTreeMap<String, Vec<String>>,
    output_licenses: BTreeMap<String, Vec<String>>,
    symlinks: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PhaseKind {
    Prepare,
    Compile,
    Verify,
    AssembleGeneric,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum FunctionCapture {
    Phase(PhaseKind),
    AssembleSpecific(String),
    Custom,
}

pub(crate) fn convert_starbuild_file(
    input_path: &Path,
    output_override: Option<&Path>,
) -> Result<ConvertedStarbuild> {
    let input_path = input_path
        .canonicalize()
        .with_context(|| format!("Failed to resolve STARBUILD path: {}", input_path.display()))?;
    let input_text = fs::read_to_string(&input_path)
        .with_context(|| format!("Failed to read STARBUILD: {}", input_path.display()))?;

    let parsed = ParsedStarbuild::parse(&input_text)
        .with_context(|| format!("Failed to parse STARBUILD: {}", input_path.display()))?;
    let output_path = output_override.map(PathBuf::from).unwrap_or_else(|| {
        input_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(format!("{}.toml", parsed.main_package_name()))
    });
    let spec_dir = output_path
        .parent()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    let spec = parsed.to_package_spec(spec_dir)?;
    let toml = super::interactive::spec_to_minimal_toml(&spec)?;

    let build_script = (!matches!(spec.build.build_type, BuildType::Meta))
        .then(|| parsed.generate_build_script())
        .transpose()?;
    let build_script_path = build_script.as_ref().map(|_| {
        output_path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("build.sh")
    });

    Ok(ConvertedStarbuild {
        output_path,
        toml,
        build_script,
        build_script_path,
    })
}

impl ParsedStarbuild {
    fn parse(input: &str) -> Result<Self> {
        let mut parsed = Self::default();
        let lines: Vec<&str> = input.lines().collect();
        let mut idx = 0usize;
        let mut capture: Option<FunctionCapture> = None;
        let mut current_lines: Vec<String> = Vec::new();

        while idx < lines.len() {
            let line = lines[idx];
            let trimmed = line.trim();

            if let Some(kind) = capture.clone() {
                match kind {
                    FunctionCapture::Custom => {
                        current_lines.push(line.to_string());
                        if trimmed == "}" {
                            parsed.custom_functions.push(current_lines.join("\n"));
                            current_lines.clear();
                            capture = None;
                        }
                    }
                    FunctionCapture::Phase(phase) => {
                        if trimmed == "}" {
                            parsed.push_phase_lines(phase, std::mem::take(&mut current_lines));
                            capture = None;
                        } else {
                            current_lines.push(line.to_string());
                        }
                    }
                    FunctionCapture::AssembleSpecific(pkg_name) => {
                        if trimmed == "}" {
                            parsed
                                .assemble_functions
                                .insert(pkg_name, std::mem::take(&mut current_lines));
                            capture = None;
                        } else {
                            current_lines.push(line.to_string());
                        }
                    }
                }
                idx += 1;
                continue;
            }

            if trimmed.is_empty() || trimmed.starts_with('#') {
                idx += 1;
                continue;
            }

            if let Some(function_name) = parse_function_name(trimmed) {
                capture = Some(match function_name.as_str() {
                    "prepare" => FunctionCapture::Phase(PhaseKind::Prepare),
                    "compile" => FunctionCapture::Phase(PhaseKind::Compile),
                    "verify" => FunctionCapture::Phase(PhaseKind::Verify),
                    "assemble" => FunctionCapture::Phase(PhaseKind::AssembleGeneric),
                    _ => {
                        if let Some(pkg_name) = function_name.strip_prefix("assemble_") {
                            FunctionCapture::AssembleSpecific(pkg_name.to_string())
                        } else {
                            current_lines.push(line.to_string());
                            FunctionCapture::Custom
                        }
                    }
                });
                idx += 1;
                continue;
            }

            if let Some((key, value)) = parse_simple_assignment(trimmed) {
                if key == "package_name" {
                    parsed.package_names = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if key == "package_version" {
                    parsed.package_version = strip_matching_quotes(&value).to_string();
                    idx += 1;
                    continue;
                }
                if key == "description" {
                    parsed.description = strip_matching_quotes(&value).to_string();
                    idx += 1;
                    continue;
                }
                if key == "package_descriptions" {
                    parsed.package_descriptions = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if key == "license" {
                    parsed.licenses = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if key == "dependencies" {
                    parsed.dependencies = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if key == "build_dependencies" {
                    parsed.build_dependencies = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if matches!(
                    key.as_str(),
                    "optional_dependencies" | "optional_depedencies" | "optional_dependecies"
                ) {
                    parsed.optional_dependencies = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if key == "gives" {
                    parsed.provides = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if matches!(key.as_str(), "clashes" | "clasheses" | "conflicts") {
                    parsed.conflicts = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if key == "keep" {
                    parsed.keep = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if key == "options" {
                    parsed.options = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }
                if key == "sources" || key == "source" {
                    parsed.sources = parse_array_or_string(&value);
                    idx += 1;
                    continue;
                }

                if let Some(pkg_name) = key.strip_prefix("dependencies_") {
                    parsed
                        .output_dependencies
                        .insert(pkg_name.to_string(), parse_array_or_string(&value));
                    idx += 1;
                    continue;
                }
                if let Some(pkg_name) = key.strip_prefix("optional_") {
                    parsed
                        .output_optional
                        .insert(pkg_name.to_string(), parse_array_or_string(&value));
                    idx += 1;
                    continue;
                }
                if let Some(pkg_name) = key.strip_prefix("gives_") {
                    parsed
                        .output_provides
                        .insert(pkg_name.to_string(), parse_array_or_string(&value));
                    idx += 1;
                    continue;
                }
                if let Some(pkg_name) = key.strip_prefix("clashes_") {
                    parsed
                        .output_conflicts
                        .insert(pkg_name.to_string(), parse_array_or_string(&value));
                    idx += 1;
                    continue;
                }
                if let Some(pkg_name) = key.strip_prefix("conflicts_") {
                    parsed
                        .output_conflicts
                        .insert(pkg_name.to_string(), parse_array_or_string(&value));
                    idx += 1;
                    continue;
                }
                if let Some(pkg_name) = key.strip_prefix("keep_") {
                    parsed
                        .output_keep
                        .insert(pkg_name.to_string(), parse_array_or_string(&value));
                    idx += 1;
                    continue;
                }
                if let Some(pkg_name) = key.strip_prefix("license_") {
                    parsed
                        .output_licenses
                        .insert(pkg_name.to_string(), parse_array_or_string(&value));
                    idx += 1;
                    continue;
                }

                parsed.custom_assignments.push(trimmed.to_string());
                parsed
                    .custom_vars
                    .insert(key, strip_matching_quotes(&value).to_string());
                idx += 1;
                continue;
            }

            if let Some(symlink) = trimmed.strip_prefix("symlink:") {
                let pair = strip_matching_quotes(symlink.trim());
                if let Some((link, target)) = pair.split_once(':') {
                    parsed
                        .symlinks
                        .push((link.trim().to_string(), target.trim().to_string()));
                }
            }

            idx += 1;
        }

        if parsed.package_names.is_empty() {
            bail!("STARBUILD does not define package_name");
        }
        if parsed.package_version.trim().is_empty() {
            bail!("STARBUILD does not define package_version");
        }

        parsed.finalize_metadata();
        Ok(parsed)
    }

    fn main_package_name(&self) -> &str {
        &self.package_names[0]
    }

    fn finalize_metadata(&mut self) {
        let main_pkg = self.main_package_name().to_string();
        self.description = expand_legacy_vars(
            &self.description,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );

        finalize_vec(
            &mut self.package_names,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.package_descriptions,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.licenses,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.dependencies,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.build_dependencies,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.optional_dependencies,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.provides,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.conflicts,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.keep,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.options,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_vec(
            &mut self.sources,
            &main_pkg,
            &self.package_version,
            &self.custom_vars,
        );

        finalize_map(
            &mut self.output_dependencies,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_map(
            &mut self.output_optional,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_map(
            &mut self.output_provides,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_map(
            &mut self.output_conflicts,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_map(
            &mut self.output_keep,
            &self.package_version,
            &self.custom_vars,
        );
        finalize_map(
            &mut self.output_licenses,
            &self.package_version,
            &self.custom_vars,
        );

        for (link, target) in &mut self.symlinks {
            *link = expand_legacy_vars(link, &main_pkg, &self.package_version, &self.custom_vars);
            *target =
                expand_legacy_vars(target, &main_pkg, &self.package_version, &self.custom_vars);
        }
    }

    fn to_package_spec(&self, spec_dir: PathBuf) -> Result<PackageSpec> {
        let homepage = infer_homepage(&self.sources).unwrap_or_default();
        let primary_description = self
            .package_descriptions
            .first()
            .cloned()
            .unwrap_or_else(|| {
                if self.description.is_empty() {
                    self.main_package_name().to_string()
                } else {
                    self.description.clone()
                }
            });
        let default_license = if self.licenses.is_empty() {
            vec!["Unknown".to_string()]
        } else {
            self.licenses.clone()
        };

        let mut primary = PackageInfo {
            name: self.main_package_name().to_string(),
            real_name: None,
            version: self.package_version.clone(),
            revision: 1,
            description: primary_description,
            homepage: homepage.clone(),
            abi_breaking: false,
            built_against: Vec::new(),
            license: self
                .output_licenses
                .get(self.main_package_name())
                .cloned()
                .unwrap_or_else(|| default_license.clone()),
        };
        if primary.license.is_empty() {
            primary.license = default_license.clone();
        }

        let multiple_outputs = self.package_names.len() > 1;
        let mut extra_packages = Vec::new();
        for (idx, pkg_name) in self.package_names.iter().enumerate().skip(1) {
            let mut pkg = PackageInfo {
                name: pkg_name.clone(),
                real_name: None,
                version: self.package_version.clone(),
                revision: 1,
                description: self
                    .package_descriptions
                    .get(idx)
                    .cloned()
                    .unwrap_or_else(|| primary.description.clone()),
                homepage: homepage.clone(),
                abi_breaking: false,
                built_against: Vec::new(),
                license: self
                    .output_licenses
                    .get(pkg_name)
                    .cloned()
                    .unwrap_or_else(|| default_license.clone()),
            };
            if pkg.license.is_empty() {
                pkg.license = default_license.clone();
            }
            extra_packages.push(pkg);
        }

        let mut dependencies = Dependencies {
            build: dedupe_preserve_order(self.build_dependencies.clone()),
            runtime: dedupe_preserve_order(self.dependencies.clone()),
            test: Vec::new(),
            optional: dedupe_preserve_order(self.optional_dependencies.clone()),
            groups: Vec::new(),
            lib32: None,
        };
        let mut alternatives = Alternatives {
            provides: dedupe_preserve_order(self.provides.clone()),
            conflicts: dedupe_preserve_order(self.conflicts.clone()),
            replaces: Vec::new(),
            lib32: None,
        };
        let mut package_dependencies = BTreeMap::new();
        let mut package_alternatives = BTreeMap::new();

        if multiple_outputs {
            for pkg_name in self.package_names.iter().skip(1) {
                package_dependencies.insert(
                    pkg_name.clone(),
                    Dependencies {
                        build: Vec::new(),
                        runtime: dedupe_preserve_order(
                            self.output_dependencies
                                .get(pkg_name)
                                .cloned()
                                .unwrap_or_default(),
                        ),
                        test: Vec::new(),
                        optional: dedupe_preserve_order(
                            self.output_optional
                                .get(pkg_name)
                                .cloned()
                                .unwrap_or_default(),
                        ),
                        groups: Vec::new(),
                        lib32: None,
                    },
                );
                package_alternatives.insert(
                    pkg_name.clone(),
                    Alternatives {
                        provides: dedupe_preserve_order(
                            self.output_provides
                                .get(pkg_name)
                                .cloned()
                                .unwrap_or_default(),
                        ),
                        conflicts: dedupe_preserve_order(
                            self.output_conflicts
                                .get(pkg_name)
                                .cloned()
                                .unwrap_or_default(),
                        ),
                        replaces: Vec::new(),
                        lib32: None,
                    },
                );
            }
        }

        let mut keep = self.keep.clone();
        for values in self.output_keep.values() {
            keep.extend(values.clone());
        }
        keep = dedupe_preserve_order(keep);

        let extractable_sources = self.extractable_sources()?;
        let manual_sources = self.manual_sources();
        let has_build_steps = self.has_build_steps();
        let is_meta =
            extractable_sources.is_empty() && manual_sources.is_empty() && !has_build_steps;

        if multiple_outputs {
            dependencies.runtime = dedupe_preserve_order(self.dependencies.clone());
            dependencies.optional = dedupe_preserve_order(self.optional_dependencies.clone());
            alternatives.provides = dedupe_preserve_order(self.provides.clone());
            alternatives.conflicts = dedupe_preserve_order(self.conflicts.clone());
        }

        Ok(PackageSpec {
            package: primary,
            packages: extra_packages,
            alternatives,
            manual_sources,
            source: extractable_sources,
            build: Build {
                build_type: if is_meta {
                    BuildType::Meta
                } else {
                    BuildType::Custom
                },
                flags: BuildFlags {
                    keep,
                    use_lto: !self.option_enabled("!lto"),
                    no_strip: self.option_enabled("no-strip")
                        || self.option_enabled("nostrip")
                        || self.option_enabled("no-strip-binaries"),
                    no_delete_static: self.option_enabled("no-remove-a"),
                    no_compress_man: self.option_enabled("no-compress-man"),
                    split_docs: self.option_enabled("docs"),
                    ..BuildFlags::default()
                },
            },
            dependencies,
            package_alternatives,
            package_dependencies,
            spec_dir,
        })
    }

    fn generate_build_script(&self) -> Result<String> {
        let primary_name = self.main_package_name();
        let install_handlers = self.install_handler_map()?;
        let mut script = String::new();
        script.push_str("#!/bin/sh\nset -eu\n\n");
        script.push_str("makei() {\n");
        script.push_str("  echo \"==> Running: make DESTDIR=\\\"$pkgdir\\\" install $*\"\n");
        script.push_str("  make DESTDIR=\"$pkgdir\" install \"$@\"\n");
        script.push_str("}\n\n");
        script.push_str("mesoni() {\n");
        script.push_str("  echo \"==> Running: meson install --destdir \\\"$pkgdir\\\" $*\"\n");
        script.push_str("  meson install --destdir \"$pkgdir\" \"$@\"\n");
        script.push_str("}\n\n");
        script.push_str("cmakei() {\n");
        script.push_str("  echo \"==> Running: cmake --install $*\"\n");
        script.push_str("  DESTDIR=\"$pkgdir\" cmake --install \"$@\"\n");
        script.push_str("}\n\n");
        script.push_str("starmove() {\n");
        script.push_str("  [ \"$#\" -ge 1 ] || {\n");
        script.push_str("    echo \"starmove: requires at least one path pattern\" >&2\n");
        script.push_str("    return 1\n");
        script.push_str("  }\n");
        script.push_str("  [ \"${DEPOT_OUTPUT_NAME:-}\" != \"\" ] || {\n");
        script.push_str("    echo \"starmove: DEPOT_OUTPUT_NAME is not set\" >&2\n");
        script.push_str("    return 1\n");
        script.push_str("  }\n");
        script.push_str("  haul \"$DEPOT_OUTPUT_NAME\" \"$@\"\n");
        script.push_str("}\n\n");
        script.push_str("depot_starbuild_sync_srcdir() {\n");
        script.push_str("  workdir=${DEPOT_STARBUILD_WORKDIR:?}\n");
        script.push_str("  compat_root=\"$workdir/.depot-starbuild\"\n");
        script.push_str("  mkdir -p \"$compat_root/packages\"\n");
        script
            .push_str("  for entry in \"$workdir\"/* \"$workdir\"/.[!.]* \"$workdir\"/..?*; do\n");
        script.push_str("    [ -e \"$entry\" ] || [ -L \"$entry\" ] || continue\n");
        script.push_str("    base=$(basename \"$entry\")\n");
        script.push_str("    [ \"$base\" = \".depot-starbuild\" ] && continue\n");
        script.push_str("    [ \"$base\" = \"packages\" ] && continue\n");
        script.push_str("    ln -snf \"$entry\" \"$compat_root/$base\"\n");
        script.push_str("  done\n");
        for pkg_name in &self.package_names {
            let dest_expr = if pkg_name == primary_name {
                "\"${DEPOT_PRIMARY_DESTDIR:-$DESTDIR}\"".to_string()
            } else {
                format!("\"$(subdestdir '{}')\"", sh_single_quote(pkg_name))
            };
            script.push_str(&format!(
                "  mkdir -p \"$compat_root/packages/{}/\"\n",
                pkg_name
            ));
            script.push_str(&format!(
                "  ln -snf {} \"$compat_root/packages/{}/files\"\n",
                dest_expr, pkg_name
            ));
        }
        script.push_str("  srcdir=\"$compat_root\"\n");
        script.push_str("  export srcdir\n");
        script.push_str("}\n\n");
        script.push_str("depot_starbuild_setup_env() {\n");
        script.push_str("  package_name=$1\n");
        script.push_str(&format!(
            "  package_version='{}'\n",
            sh_single_quote(&self.package_version)
        ));
        script.push_str("  pkgdir=${DESTDIR:?}\n");
        script.push_str("  export package_name package_version pkgdir\n");
        script.push_str("  LANG=C\n");
        script.push_str("  LC_ALL=C\n");
        script.push_str("  export LANG LC_ALL\n");
        script.push_str("  depot_starbuild_sync_srcdir\n");
        for assignment in &self.custom_assignments {
            script.push_str("  ");
            script.push_str(assignment.trim());
            script.push('\n');
        }
        script.push_str("}\n\n");

        for custom_function in &self.custom_functions {
            script.push_str(custom_function);
            script.push_str("\n\n");
        }

        if self.has_build_steps() {
            script.push_str("depot_build() {\n");
            script.push_str(&format!(
                "  depot_starbuild_setup_env '{}'\n",
                sh_single_quote(primary_name)
            ));
            append_shell_body(
                &mut script,
                &self.prepare,
                &self.compile,
                &self.verify,
                None,
            );
            script.push_str("}\n\n");
        }

        let primary_install = install_handlers
            .get(primary_name)
            .cloned()
            .unwrap_or_default();
        script.push_str("depot_install() {\n");
        script.push_str(&format!(
            "  depot_starbuild_setup_env '{}'\n",
            sh_single_quote(primary_name)
        ));
        append_shell_body(&mut script, &[], &[], &[], Some(&primary_install));
        for (link, target) in &self.symlinks {
            script.push_str(&format!(
                "  mkdir -p \"$(dirname \"$pkgdir/{0}\")\"\n  ln -snf '{1}' \"$pkgdir/{0}\"\n",
                link,
                sh_single_quote(target)
            ));
        }
        script.push_str("}\n\n");

        for pkg_name in self.package_names.iter().skip(1) {
            let fn_suffix = shell_fn_suffix(pkg_name);
            let body = install_handlers.get(pkg_name).cloned().unwrap_or_default();
            script.push_str(&format!("depot_install_{fn_suffix}() {{\n"));
            script.push_str(&format!(
                "  depot_starbuild_setup_env '{}'\n",
                sh_single_quote(pkg_name)
            ));
            append_shell_body(&mut script, &[], &[], &[], Some(&body));
            script.push_str("}\n\n");
        }

        Ok(script)
    }

    fn has_build_steps(&self) -> bool {
        !self.prepare.is_empty() || !self.compile.is_empty() || !self.verify.is_empty()
    }

    fn option_enabled(&self, option: &str) -> bool {
        self.options.iter().any(|value| value == option)
    }

    fn extractable_sources(&self) -> Result<Vec<Source>> {
        let mut out = Vec::new();
        for raw in &self.sources {
            if let Some(source) = convert_extractable_source(raw, self.main_package_name())? {
                out.push(source);
            }
        }
        Ok(out)
    }

    fn manual_sources(&self) -> Vec<ManualSource> {
        let mut out = Vec::new();
        for raw in &self.sources {
            if let Some(manual) = convert_manual_source(raw) {
                out.push(manual);
            }
        }
        out
    }

    fn install_handler_map(&self) -> Result<BTreeMap<String, Vec<String>>> {
        let mut handlers = BTreeMap::new();
        for pkg_name in &self.package_names {
            if let Some(body) = self.assemble_functions.get(pkg_name) {
                handlers.insert(pkg_name.clone(), body.clone());
                continue;
            }
            if !self.generic_assemble.is_empty() {
                handlers.insert(pkg_name.clone(), self.generic_assemble.clone());
                continue;
            }
            if pkg_name == self.main_package_name() {
                handlers.insert(pkg_name.clone(), Vec::new());
                continue;
            }
            bail!(
                "STARBUILD defines output '{}' but has no matching assemble() or assemble_{}()",
                pkg_name,
                pkg_name
            );
        }
        Ok(handlers)
    }

    fn push_phase_lines(&mut self, phase: PhaseKind, lines: Vec<String>) {
        match phase {
            PhaseKind::Prepare => self.prepare = lines,
            PhaseKind::Compile => self.compile = lines,
            PhaseKind::Verify => self.verify = lines,
            PhaseKind::AssembleGeneric => self.generic_assemble = lines,
        }
    }
}

fn append_shell_body(
    out: &mut String,
    prepare: &[String],
    compile: &[String],
    verify: &[String],
    install: Option<&[String]>,
) {
    let mut wrote = false;
    for line in prepare {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
        wrote = true;
    }
    for line in compile {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
        wrote = true;
    }
    for line in verify {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
        wrote = true;
    }
    if let Some(install_lines) = install {
        for line in install_lines {
            out.push_str("  ");
            out.push_str(line);
            out.push('\n');
            wrote = true;
        }
    }
    if !wrote {
        out.push_str("  :\n");
    }
}

fn finalize_vec(
    values: &mut Vec<String>,
    pkg_name: &str,
    package_version: &str,
    custom_vars: &HashMap<String, String>,
) {
    for value in values {
        *value = expand_legacy_vars(value, pkg_name, package_version, custom_vars);
    }
}

fn finalize_map(
    values: &mut BTreeMap<String, Vec<String>>,
    package_version: &str,
    custom_vars: &HashMap<String, String>,
) {
    for (pkg_name, items) in values {
        finalize_vec(items, pkg_name, package_version, custom_vars);
    }
}

fn convert_extractable_source(raw: &str, pkg_name: &str) -> Result<Option<Source>> {
    if is_local_source(raw) || raw.starts_with("ne+") {
        return Ok(None);
    }
    if !is_extractable_remote_source(raw) {
        return Ok(None);
    }

    let normalized = normalize_source_url(raw);
    Ok(Some(Source {
        url: normalized.clone(),
        sha256: "skip".to_string(),
        extract_dir: derive_extract_dir(&normalized, pkg_name),
        patches: Vec::new(),
        post_extract: Vec::new(),
        cherry_pick: Vec::new(),
    }))
}

fn convert_manual_source(raw: &str) -> Option<ManualSource> {
    if is_local_source(raw) {
        return Some(ManualSource {
            file: Some(raw.to_string()),
            files: Vec::new(),
            url: None,
            urls: Vec::new(),
            sha256: None,
            dest: None,
        });
    }

    if raw.starts_with("ne+") || !is_extractable_remote_source(raw) {
        return Some(ManualSource {
            file: None,
            files: Vec::new(),
            url: Some(normalize_source_url(raw)),
            urls: Vec::new(),
            sha256: None,
            dest: None,
        });
    }

    None
}

fn parse_function_name(line: &str) -> Option<String> {
    let open = line.find('(')?;
    let close = line[open..].find(')')? + open;
    let brace = line[close..].find('{')? + close;
    let name = line[..open].trim();
    if brace <= close || name.is_empty() {
        return None;
    }
    if !name
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    Some(name.to_string())
}

fn parse_simple_assignment(line: &str) -> Option<(String, String)> {
    let eq = line.find('=')?;
    let key = line[..eq].trim();
    if key.is_empty() {
        return None;
    }
    if !key
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
    {
        return None;
    }
    Some((key.to_string(), line[eq + 1..].trim().to_string()))
}

fn parse_array_or_string(value: &str) -> Vec<String> {
    let trimmed = value.trim();
    if let Some(inner) = trimmed.strip_prefix('(').and_then(|v| v.strip_suffix(')')) {
        return tokenize_shell_words(inner);
    }
    if let Some(inner) = trimmed.strip_prefix('{').and_then(|v| v.strip_suffix('}')) {
        return tokenize_shell_words(inner);
    }
    let unquoted = strip_matching_quotes(trimmed);
    if unquoted.is_empty() {
        Vec::new()
    } else {
        vec![unquoted.to_string()]
    }
}

fn tokenize_shell_words(input: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut quote: Option<char> = None;

    for ch in input.chars() {
        if let Some(active_quote) = quote {
            if ch == active_quote {
                quote = None;
            } else {
                current.push(ch);
            }
            continue;
        }

        match ch {
            '"' | '\'' => quote = Some(ch),
            c if c.is_whitespace() => {
                if !current.is_empty() {
                    out.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if !current.is_empty() {
        out.push(current);
    }

    out
}

fn strip_matching_quotes(value: &str) -> &str {
    if value.len() >= 2
        && ((value.starts_with('"') && value.ends_with('"'))
            || (value.starts_with('\'') && value.ends_with('\'')))
    {
        &value[1..value.len() - 1]
    } else {
        value
    }
}

fn expand_legacy_vars(
    input: &str,
    pkg_name: &str,
    package_version: &str,
    custom_vars: &HashMap<String, String>,
) -> String {
    let mut current = input.to_string();
    for _ in 0..16 {
        let next = expand_legacy_vars_once(&current, pkg_name, package_version, custom_vars);
        if next == current {
            break;
        }
        current = next;
    }
    current
}

fn expand_legacy_vars_once(
    input: &str,
    pkg_name: &str,
    package_version: &str,
    custom_vars: &HashMap<String, String>,
) -> String {
    let chars: Vec<char> = input.chars().collect();
    let mut idx = 0usize;
    let mut out = String::new();
    while idx < chars.len() {
        if chars[idx] != '$' {
            out.push(chars[idx]);
            idx += 1;
            continue;
        }

        if idx + 1 < chars.len() && chars[idx + 1] == '{' {
            let mut end = idx + 2;
            while end < chars.len() && chars[end] != '}' {
                end += 1;
            }
            if end >= chars.len() {
                out.push(chars[idx]);
                idx += 1;
                continue;
            }
            let key: String = chars[idx + 2..end].iter().collect();
            out.push_str(&legacy_var_value(
                &key,
                pkg_name,
                package_version,
                custom_vars,
            ));
            idx = end + 1;
            continue;
        }

        let mut end = idx + 1;
        while end < chars.len() && (chars[end].is_ascii_alphanumeric() || chars[end] == '_') {
            end += 1;
        }
        if end == idx + 1 {
            out.push(chars[idx]);
            idx += 1;
            continue;
        }

        let key: String = chars[idx + 1..end].iter().collect();
        out.push_str(&legacy_var_value(
            &key,
            pkg_name,
            package_version,
            custom_vars,
        ));
        idx = end;
    }
    out
}

fn legacy_var_value(
    key: &str,
    pkg_name: &str,
    package_version: &str,
    custom_vars: &HashMap<String, String>,
) -> String {
    match key {
        "package_name" => pkg_name.to_string(),
        "package_version" => package_version.to_string(),
        _ => custom_vars
            .get(key)
            .cloned()
            .unwrap_or_else(|| format!("${key}")),
    }
}

fn infer_homepage(sources: &[String]) -> Option<String> {
    for source in sources {
        if is_local_source(source) {
            continue;
        }
        let normalized = normalize_source_url(source);
        let base = normalized
            .strip_prefix("hg+")
            .unwrap_or(&normalized)
            .split('#')
            .next()
            .unwrap_or(&normalized);
        if let Some(with_scheme) = base
            .strip_prefix("http://")
            .map(|rest| format!("http://{}", rest))
            .or_else(|| {
                base.strip_prefix("https://")
                    .map(|rest| format!("https://{}", rest))
            })
            .or_else(|| {
                base.strip_prefix("ftp://")
                    .map(|rest| format!("ftp://{}", rest))
            })
        {
            return Some(with_scheme);
        }
    }
    None
}

fn normalize_source_url(raw: &str) -> String {
    raw.strip_prefix("git+")
        .or_else(|| raw.strip_prefix("ne+"))
        .map(str::to_string)
        .unwrap_or_else(|| raw.to_string())
}

fn derive_extract_dir(url: &str, fallback_pkg_name: &str) -> String {
    let base = url.split('#').next().unwrap_or(url);
    let filename = base.rsplit('/').next().unwrap_or(base);
    let without_query = filename.split('?').next().unwrap_or(filename);

    if url.starts_with("hg+") {
        return without_query
            .trim_end_matches(".hg")
            .trim_end_matches(".git")
            .to_string();
    }

    if base.ends_with(".git") || (url.contains('#') && !looks_like_archive(base)) {
        return without_query.trim_end_matches(".git").to_string();
    }

    strip_archive_suffixes(without_query)
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| fallback_pkg_name.to_string())
}

fn strip_archive_suffixes(filename: &str) -> Option<String> {
    const SUFFIXES: &[&str] = &[
        ".tar.gz", ".tar.xz", ".tar.bz2", ".tar.zst", ".tar.lz", ".tar.lz4", ".tgz", ".tbz2",
        ".txz", ".zip", ".tar", ".gz", ".xz", ".bz2", ".zst", ".deb", ".rpm",
    ];
    for suffix in SUFFIXES {
        if let Some(stripped) = filename.strip_suffix(suffix) {
            return Some(stripped.to_string());
        }
    }
    None
}

fn looks_like_archive(url: &str) -> bool {
    strip_archive_suffixes(url.rsplit('/').next().unwrap_or(url)).is_some()
}

fn is_local_source(raw: &str) -> bool {
    !raw.contains("://")
        && !raw.starts_with("git+")
        && !raw.starts_with("hg+")
        && !raw.starts_with("ne+")
}

fn is_extractable_remote_source(raw: &str) -> bool {
    if raw.starts_with("ne+") {
        return false;
    }
    if raw.starts_with("git+") || raw.starts_with("hg+") {
        return true;
    }
    let normalized = normalize_source_url(raw);
    let base = normalized.split('#').next().unwrap_or(&normalized);
    base.ends_with(".git") || normalized.contains('#') || looks_like_archive(base)
}

fn dedupe_preserve_order(values: Vec<String>) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for value in values {
        if seen.insert(value.clone()) {
            out.push(value);
        }
    }
    out
}

fn shell_fn_suffix(pkg_name: &str) -> String {
    let mut out = String::with_capacity(pkg_name.len().max(1));
    for ch in pkg_name.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    if out.as_bytes().first().is_some_and(|b| b.is_ascii_digit()) {
        out.insert(0, '_');
    }
    out
}

fn sh_single_quote(value: &str) -> String {
    value.replace('\'', "'\"'\"'")
}

#[cfg(test)]
mod tests;
