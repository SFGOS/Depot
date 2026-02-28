use crate::package::{
    Alternatives, Build, BuildFlags, BuildType, Dependencies, ManualSource, PackageInfo,
    PackageSpec, Source,
};
use anyhow::{Context, Result};
use inquire::{Confirm, Select, Text};
use sha2::{Digest, Sha256};
use std::fmt;
use std::io::Read;
use std::path::PathBuf;
use std::time::Duration;
use url::Url;

impl fmt::Display for BuildType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            BuildType::Autotools => write!(f, "Autotools"),
            BuildType::CMake => write!(f, "CMake"),
            BuildType::Meson => write!(f, "Meson"),
            BuildType::Custom => write!(f, "Custom"),
            BuildType::Python => write!(f, "Python"),
            BuildType::Rust => write!(f, "Rust"),
            BuildType::Makefile => write!(f, "Makefile"),
            BuildType::Bin => write!(f, "Binary installer"),
            BuildType::Meta => write!(f, "Metapackage"),
        }
    }
}

/// Try to compute the SHA256 hex for the given URL or local path.
///
/// Supports: `http`, `https`, `ftp`, `file`, and plain local filesystem paths.
/// Returns Ok(hex) on success or Err on any failure (caller should treat failure as "no default").
fn compute_sha256_for_url(u: &str) -> anyhow::Result<String> {
    // helper to hash a Read
    fn hash_reader<R: Read>(r: &mut R) -> anyhow::Result<String> {
        let mut hasher = Sha256::new();
        let mut buf = [0u8; 8192];
        loop {
            let n = r.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    // Try to parse as URL first; if parsing fails, treat as local path.
    if let Ok(parsed) = Url::parse(u) {
        match parsed.scheme() {
            "http" | "https" => {
                let ua = format!("{}/{}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                let client =
                    crate::source::build_blocking_client(&ua, Some(Duration::from_secs(20)))
                        .with_context(|| "failed to build http client")?;
                let mut resp = client
                    .get(u)
                    .send()
                    .with_context(|| format!("failed to GET {}", u))?;
                if !resp.status().is_success() {
                    anyhow::bail!("HTTP status {}", resp.status());
                }
                return hash_reader(&mut resp);
            }
            "ftp" => {
                let host = parsed.host_str().context("ftp url missing host")?;
                let port = parsed.port_or_known_default().unwrap_or(21);
                let addr = format!("{}:{}", host, port);
                let mut ftp_stream = suppaftp::FtpStream::connect(addr.as_str())
                    .with_context(|| format!("failed to connect to {}", addr))?;
                let user = if parsed.username().is_empty() {
                    "anonymous"
                } else {
                    parsed.username()
                };
                let pass = parsed.password().unwrap_or("anonymous@");
                ftp_stream
                    .login(user, pass)
                    .with_context(|| "ftp login failed")?;
                let mut result_hex = None;
                let path = parsed.path();
                let candidates = [path.to_string(), path.trim_start_matches('/').to_string()];
                for p in candidates.iter().filter(|s| !s.is_empty()) {
                    if let Ok(res) = ftp_stream.retr(p, |reader| {
                        // reuse hash_reader by adapting reader to trait object
                        let mut r = reader;
                        hash_reader(&mut r).map_err(|e| {
                            suppaftp::FtpError::ConnectionError(std::io::Error::other(
                                e.to_string(),
                            ))
                        })
                    }) {
                        result_hex = Some(res);
                        break;
                    }
                }
                ftp_stream.quit().ok();
                if let Some(h) = result_hex {
                    return Ok(h);
                }
                anyhow::bail!("ftp retrieval failed")
            }
            "file" => {
                if let Ok(fp) = parsed.to_file_path() {
                    let mut f = std::fs::File::open(fp)?;
                    return hash_reader(&mut f);
                }
                anyhow::bail!("invalid file URL")
            }
            _ => anyhow::bail!("unsupported URL scheme"),
        }
    }

    // Treat as local path if it exists
    let p = std::path::Path::new(u);
    if p.exists() {
        let mut f = std::fs::File::open(p)?;
        return hash_reader(&mut f);
    }

    anyhow::bail!(
        "could not compute sha256 for '{}': unsupported or unreachable",
        u
    )
}

fn prompt_repeating_list(item_label: &str, help: &str) -> Result<Vec<String>> {
    let mut values = Vec::new();
    loop {
        let prompt = format!("{} #{} (empty to finish):", item_label, values.len() + 1);
        let entry = Text::new(&prompt).with_help_message(help).prompt()?;
        let trimmed = entry.trim();
        if trimmed.is_empty() {
            break;
        }
        values.push(trimmed.to_string());
    }
    Ok(values)
}

fn prompt_optional_text(prompt: &str, help: &str) -> Result<String> {
    Ok(Text::new(prompt)
        .with_help_message(help)
        .prompt()?
        .trim()
        .to_string())
}

fn prompt_manual_sources() -> Result<Vec<ManualSource>> {
    let mut out = Vec::new();
    loop {
        let add = Confirm::new("Add a manual source?")
            .with_help_message(
                "Use for local files (keys, patches, configs) or direct URLs copied into the build dir",
            )
            .with_default(out.is_empty())
            .prompt()?;
        if !add {
            break;
        }

        let modes = vec!["Local file(s)", "Remote URL(s)"];
        let mode = Select::new("Manual source mode:", modes)
            .with_help_message("Local files are resolved relative to the spec directory")
            .prompt()?;

        let mut manual = ManualSource {
            file: None,
            files: Vec::new(),
            url: None,
            urls: Vec::new(),
            sha256: None,
            dest: None,
        };

        let entries = if mode == "Local file(s)" {
            prompt_repeating_list(
                "Manual source file",
                "Path relative to spec dir, e.g. depot.pub, keys/repo.pub",
            )?
        } else {
            prompt_repeating_list(
                "Manual source URL",
                "Supports http(s), ftp, and file:// URLs",
            )?
        };

        if entries.is_empty() {
            crate::log_warn!("Manual source block skipped (no entries provided).");
            continue;
        }

        if mode == "Local file(s)" {
            if entries.len() == 1 {
                manual.file = Some(entries[0].clone());
            } else {
                manual.files = entries;
            }
        } else if entries.len() == 1 {
            manual.url = Some(entries[0].clone());
        } else {
            manual.urls = entries;
        }

        let single_entry = matches!(
            (
                &manual.file,
                manual.files.len(),
                &manual.url,
                manual.urls.len()
            ),
            (Some(_), 0, None, 0) | (None, 0, Some(_), 0)
        );

        if single_entry {
            let checksum = prompt_optional_text(
                "Manual source checksum (optional):",
                "sha256:/sha512:/md5:, raw SHA256 hex, or 'skip' (empty omits field)",
            )?;
            if !checksum.is_empty() {
                manual.sha256 = Some(checksum);
            }

            let dest = prompt_optional_text(
                "Manual source destination (optional):",
                "Path relative to build work dir (default derives from file/url name)",
            )?;
            if !dest.is_empty() {
                manual.dest = Some(dest);
            }
        } else {
            crate::log_info!(
                "Multiple entries in one manual_sources block cannot use a shared checksum or dest; skipping those prompts."
            );
        }

        out.push(manual);
    }
    Ok(out)
}

fn parse_license_list(input: &str) -> Vec<String> {
    input
        .split(',')
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

fn licenses_to_toml_value(licenses: &[String]) -> Option<toml::Value> {
    if licenses.is_empty() {
        None
    } else if licenses.len() == 1 {
        Some(toml::Value::String(licenses[0].clone()))
    } else {
        Some(toml::Value::Array(
            licenses
                .iter()
                .map(|license| toml::Value::String(license.clone()))
                .collect(),
        ))
    }
}

fn expand_known_package_vars(input: &str, name: &str, version: &str) -> String {
    input
        .replace("$name", name)
        .replace("${name}", name)
        .replace("$version", version)
        .replace("${version}", version)
}

pub fn create_interactive() -> Result<PackageSpec> {
    crate::log_info!("Interactive Package Specification Creator");
    crate::log_info!("-----------------------------------------");

    // Ask early whether this is a GNU project so we can pre-fill homepage and
    // avoid repeating the same question later for autotools sources.
    let is_gnu_project = Confirm::new("Is this a GNU project?")
        .with_help_message("Automatically fill GNU-specific defaults (homepage, mirrors)")
        .with_default(false)
        .prompt()?;

    // Default package name to the current directory basename when possible.
    let default_name = std::env::current_dir()
        .ok()
        .and_then(|p| p.file_name().map(|s| s.to_string_lossy().into_owned()))
        .unwrap_or_else(|| "zlib".into());

    let name = Text::new("Package Name:")
        .with_help_message("e.g. zlib, bash, linux")
        .with_default(default_name.as_str())
        .prompt()?;

    if name.is_empty() {
        anyhow::bail!("Package name cannot be empty");
    }

    let version = Text::new("Version:")
        .with_help_message("e.g. 1.2.11, 5.1")
        .with_placeholder("1.0.0")
        .prompt()?;

    let description = Text::new("Description:")
        .with_help_message("A short description of the package")
        .prompt()?;

    let homepage_default = if is_gnu_project {
        format!("https://www.gnu.org/software/{}/", name)
    } else {
        String::new()
    };

    let homepage = Text::new("Homepage:")
        .with_help_message("Project website URL")
        .with_default(homepage_default.as_str())
        .prompt()?;

    let license_input = Text::new("License(s):")
        .with_help_message("Single SPDX id or comma-separated list, e.g. MIT, Apache-2.0")
        .with_placeholder("MIT")
        .prompt()?;
    let license = parse_license_list(&license_input);
    if license.is_empty() {
        anyhow::bail!("At least one license is required");
    }

    // Present all supported build systems.
    let build_types = vec![
        BuildType::Autotools,
        BuildType::CMake,
        BuildType::Meson,
        BuildType::Makefile,
        BuildType::Python,
        BuildType::Rust,
        BuildType::Custom,
        BuildType::Bin,
        BuildType::Meta,
    ];

    let build_type = Select::new("Build System:", build_types)
        .with_help_message("Select the build system used by the package (common choices)")
        .prompt()?;
    let is_metapackage = matches!(build_type, BuildType::Meta);

    // If the build system is Autotools, offer a convenience prompt for GNU-hosted tarballs.
    let mut gnu_source_default = String::new();
    if let BuildType::Autotools = build_type {
        // Rely on the initial `is_gnu_project` answer (do not re-ask).
        if is_gnu_project {
            let suffix = Text::new("GNU tarball suffix:")
                .with_help_message("e.g. .tar.gz, .tar.xz")
                .with_default(".tar.gz")
                .prompt()?;
            gnu_source_default = format!(
                "https://mirrors.kernel.org/gnu/{name}/{name}-{version}{suffix}",
                name = name,
                version = version,
                suffix = suffix
            );
        }
    }

    // Ask whether to show advanced fields.
    let show_advanced = if is_metapackage {
        false
    } else {
        Confirm::new("Show advanced options?")
            .with_help_message(
                "Includes source subdir, toolchain overrides, hooks, and build tuning",
            )
            .with_default(false)
            .prompt()?
    };

    let mut sources = Vec::new();
    let mut manual_sources = Vec::new();
    if !is_metapackage {
        let source_url = Text::new("Source URL:")
            .with_help_message(
                "URL to the source tarball or git repository (supports file://; leave empty if you will add manual_sources later)",
            )
            .with_default(gnu_source_default.as_str())
            .prompt()?;

        if source_url.trim().is_empty() {
            crate::log_warn!(
                "No source URL provided. Generated spec will omit [source]; add manual_sources or source entries before building (unless this becomes a metapackage)."
            );
            if Confirm::new("Add manual sources now?")
                .with_help_message("Useful for keyrings/patch-only/custom packages")
                .with_default(true)
                .prompt()?
            {
                manual_sources = prompt_manual_sources()?;
            }
        } else {
            // Attempt to compute SHA256 automatically when online — use as the default if available.
            let checksum_source_url = expand_known_package_vars(&source_url, &name, &version);
            let computed_sha_default = match compute_sha256_for_url(&checksum_source_url) {
                Ok(hex) => {
                    // Use raw hex as the default so pressing Enter accepts it
                    hex
                }
                Err(_) => "skip".to_string(),
            };

            let source_sha256 = Text::new("Source checksum:")
                .with_help_message(
                    "Accepts sha256:, sha512:, md5:, or raw SHA256 hex (use 'skip' to bypass)",
                )
                .with_default(computed_sha_default.as_str())
                .prompt()?;

            let extract_dir = Text::new("Extract Directory:")
                .with_help_message("Directory created after extraction (supports $name, $version)")
                .with_default("$name-$version")
                .prompt()?;

            let mut source = Source {
                url: source_url,
                sha256: source_sha256,
                extract_dir,
                patches: Vec::new(),
                post_extract: Vec::new(),
            };

            if show_advanced {
                source.patches = prompt_repeating_list(
                    "Patch path/URL",
                    "Patch file path (relative to spec dir) or direct URL",
                )?;
                source.post_extract = prompt_repeating_list(
                    "post_extract command",
                    "Runs in extracted source directory using sh -c",
                )?;
                if Confirm::new("Add manual sources?")
                    .with_help_message(
                        "Optional local files or URLs copied into the build dir before source fetching",
                    )
                    .with_default(false)
                    .prompt()?
                {
                    manual_sources = prompt_manual_sources()?;
                }
            }

            sources.push(source);
        }
    } else {
        crate::log_info!(
            "Metapackage selected: skipping source/build prompts. Define runtime dependencies to pull in."
        );
    }

    let mut flags = BuildFlags::default();
    if !is_metapackage {
        if !matches!(build_type, BuildType::Bin) {
            flags.prefix = Text::new("Install prefix:")
                .with_help_message("Most packages should use /usr")
                .with_default(flags.prefix.as_str())
                .prompt()?;
        }

        let supports_separate_build_dir = matches!(
            build_type,
            BuildType::Autotools | BuildType::CMake | BuildType::Meson | BuildType::Custom
        );
        if supports_separate_build_dir {
            let default_bdir = if matches!(build_type, BuildType::Meson) {
                "builddir"
            } else {
                "build"
            };
            let default_enabled = matches!(build_type, BuildType::CMake | BuildType::Meson);
            if Confirm::new("Use separate build directory?")
                .with_help_message("Recommended for CMake/Meson and often useful for Autotools")
                .with_default(default_enabled)
                .prompt()?
            {
                flags.build_dir = Some(
                    Text::new("Build directory name:")
                        .with_default(default_bdir)
                        .prompt()?,
                );
            }
        }

        if show_advanced {
            flags.source_subdir = prompt_optional_text(
                "Source subdirectory (optional):",
                "Use for monorepos (e.g. llvm-project/clang, src)",
            )?;
            flags.cc = Text::new("CC:")
                .with_help_message("C compiler")
                .with_default(flags.cc.as_str())
                .prompt()?;
            flags.cxx = Text::new("CXX:")
                .with_help_message("C++ compiler")
                .with_default(flags.cxx.as_str())
                .prompt()?;
            flags.ar = Text::new("AR:")
                .with_help_message("Archiver tool")
                .with_default(flags.ar.as_str())
                .prompt()?;
            flags.chost = prompt_optional_text(
                "CHOST target triple (optional):",
                "Example: x86_64-sfg-linux-gnu",
            )?;
            flags.cbuild = prompt_optional_text(
                "CBUILD build triple (optional):",
                "Example: x86_64-pc-linux-gnu",
            )?;
            flags.carch = Text::new("CARCH:")
                .with_help_message("CPU architecture short name")
                .with_default(flags.carch.as_str())
                .prompt()?;
            flags.cflags = prompt_repeating_list(
                "CFLAG",
                "One flag per entry, e.g. -O2, -fPIC, -D_GNU_SOURCE",
            )?;
            flags.ldflags =
                prompt_repeating_list("LDFLAG", "One flag per entry, e.g. -Wl,-z,relro")?;
        }

        if matches!(
            build_type,
            BuildType::Autotools | BuildType::CMake | BuildType::Meson
        ) {
            let help = match build_type {
                BuildType::Autotools => "Examples: --disable-static, --enable-nls, --with-zlib",
                BuildType::CMake => "Examples: -DENABLE_TESTS=OFF, -DUSE_SYSTEM_LIBS=ON",
                BuildType::Meson => "Examples: -Dtests=false, -Ddefault_library=static",
                _ => "",
            };
            if Confirm::new("Add configure/setup options?")
                .with_help_message("Adds entries to build.flags.configure")
                .with_default(show_advanced)
                .prompt()?
            {
                flags.configure = prompt_repeating_list("Configure option", help)?;
            }
        }

        if show_advanced && matches!(build_type, BuildType::Autotools) {
            flags.configure_file = prompt_optional_text(
                "Configure script path (optional):",
                "Relative to source root, e.g. build-aux/configure",
            )?;
            flags.make_dirs = prompt_repeating_list(
                "Make dir (build phase)",
                "Relative to build dir, e.g. lib, libelf (empty = build root)",
            )?;
            flags.make_test_dirs = prompt_repeating_list(
                "Make dir (test phase)",
                "Relative to build dir, e.g. tests (empty = build root)",
            )?;
            flags.make_install_dirs = prompt_repeating_list(
                "Make dir (install phase)",
                "Relative to build dir, e.g. lib, apps (empty = build root)",
            )?;
            flags.skip_tests = Confirm::new("Skip automatic tests (make check/test)?")
                .with_help_message(
                    "If enabled, Depot will not auto-run detected Autotools test targets",
                )
                .with_default(false)
                .prompt()?;
        }

        if matches!(build_type, BuildType::Makefile) {
            crate::log_info!("Define Makefile build and install commands.");
            flags.makefile_commands = prompt_repeating_list(
                "makefile build command",
                "Examples: make -j$(nproc), make all",
            )?;
            if flags.makefile_commands.is_empty() {
                anyhow::bail!("Makefile build type requires at least one build command");
            }
            flags.makefile_install_commands = prompt_repeating_list(
                "makefile install command",
                "Examples: make DESTDIR=$DESTDIR install",
            )?;
            if flags.makefile_install_commands.is_empty() {
                anyhow::bail!("Makefile build type requires at least one install command");
            }
        }

        if matches!(build_type, BuildType::Rust) {
            let profiles = vec!["release", "debug"];
            let profile = Select::new("Cargo profile:", profiles)
                .with_help_message("Release is recommended for production packages")
                .prompt()?;
            flags.profile = profile.to_string();
            flags.target = prompt_optional_text(
                "Rust target triple (optional):",
                "Example: x86_64-unknown-linux-gnu",
            )?;
            if show_advanced {
                flags.rustflags = prompt_repeating_list(
                    "RUSTFLAG",
                    "One flag per entry, e.g. -Ctarget-cpu=native",
                )?;
                flags.cargs = prompt_repeating_list(
                    "Extra cargo arg",
                    "Examples: --locked, --features, serde, --no-default-features",
                )?;
            }
            flags.bindir = Text::new("Binary install dir:")
                .with_help_message("Destination inside package image")
                .with_default(flags.bindir.as_str())
                .prompt()?;
        }

        if matches!(build_type, BuildType::Bin) {
            flags.binary_type = prompt_optional_text(
                "Binary type (optional):",
                "Metadata only, e.g. deb, rpm, tarball",
            )?;
        }

        if show_advanced {
            flags.post_configure = prompt_repeating_list(
                "post_configure command",
                "Runs after configure/setup, before build",
            )?;
            flags.post_compile =
                prompt_repeating_list("post_compile command", "Runs after build, before install")?;
            flags.post_install =
                prompt_repeating_list("post_install command", "Runs after install step")?;
        }
    }

    let runtime_deps =
        prompt_repeating_list("Runtime dependency", "Package name (e.g. zlib, openssl)")?;
    let build_deps =
        prompt_repeating_list("Build dependency", "Package needed only at build time")?;
    let test_deps = prompt_repeating_list(
        "Test dependency",
        "Package needed only for running package test suites",
    )?;
    let optional_deps = prompt_repeating_list(
        "Optional dependency",
        "Package that enables optional runtime functionality",
    )?;

    Ok(PackageSpec {
        package: PackageInfo {
            name,
            version,
            revision: 1,
            description,
            homepage,
            license,
        },
        packages: Vec::new(),
        alternatives: Alternatives::default(),
        manual_sources,
        source: sources,
        build: Build { build_type, flags },
        dependencies: Dependencies {
            build: build_deps,
            runtime: runtime_deps,
            test: test_deps,
            optional: optional_deps,
        },
        package_alternatives: Default::default(),
        package_dependencies: Default::default(),
        spec_dir: PathBuf::from("."),
    })
}

/// Produce a compact TOML string for a spec created interactively.
/// This omits default/empty fields so generated `pkg.toml` is concise.
pub fn spec_to_minimal_toml(spec: &PackageSpec) -> anyhow::Result<String> {
    use toml::value::{Table, Value};

    let mut root = Table::new();

    // package block
    let mut pkg = Table::new();
    pkg.insert("name".into(), Value::String(spec.package.name.clone()));
    pkg.insert(
        "version".into(),
        Value::String(spec.package.version.clone()),
    );
    if !spec.package.description.is_empty() {
        pkg.insert(
            "description".into(),
            Value::String(spec.package.description.clone()),
        );
    }
    if !spec.package.homepage.is_empty() {
        pkg.insert(
            "homepage".into(),
            Value::String(spec.package.homepage.clone()),
        );
    }
    if let Some(v) = licenses_to_toml_value(&spec.package.license) {
        pkg.insert("license".into(), v);
    }
    root.insert("package".into(), Value::Table(pkg));

    // additional package outputs (if any)
    if !spec.packages.is_empty() {
        let mut arr = Vec::new();
        for p in &spec.packages {
            let mut pt = Table::new();
            pt.insert("name".into(), Value::String(p.name.clone()));
            pt.insert("version".into(), Value::String(p.version.clone()));
            if !p.description.is_empty() {
                pt.insert("description".into(), Value::String(p.description.clone()));
            }
            if !p.homepage.is_empty() {
                pt.insert("homepage".into(), Value::String(p.homepage.clone()));
            }
            if let Some(v) = licenses_to_toml_value(&p.license) {
                pt.insert("license".into(), v);
            }
            arr.push(Value::Table(pt));
        }
        root.insert("packages".into(), Value::Array(arr));
    }

    // manual sources
    if !spec.manual_sources.is_empty() {
        let mut arr = Vec::new();
        for m in &spec.manual_sources {
            let mut mt = Table::new();
            if let Some(file) = m.file.as_ref().filter(|s| !s.trim().is_empty()) {
                mt.insert("file".into(), Value::String(file.clone()));
            }
            if !m.files.is_empty() {
                mt.insert(
                    "files".into(),
                    Value::Array(m.files.iter().map(|s| Value::String(s.clone())).collect()),
                );
            }
            if let Some(url) = m.url.as_ref().filter(|s| !s.trim().is_empty()) {
                mt.insert("url".into(), Value::String(url.clone()));
            }
            if !m.urls.is_empty() {
                mt.insert(
                    "urls".into(),
                    Value::Array(m.urls.iter().map(|s| Value::String(s.clone())).collect()),
                );
            }
            if let Some(sha256) = m.sha256.as_ref().filter(|s| {
                let trimmed = s.trim();
                !trimmed.is_empty() && !trimmed.eq_ignore_ascii_case("skip")
            }) {
                mt.insert("sha256".into(), Value::String(sha256.clone()));
            }
            if let Some(dest) = m.dest.as_ref().filter(|s| !s.trim().is_empty()) {
                mt.insert("dest".into(), Value::String(dest.clone()));
            }
            if !mt.is_empty() {
                arr.push(Value::Table(mt));
            }
        }
        if !arr.is_empty() {
            root.insert("manual_sources".into(), Value::Array(arr));
        }
    }

    // sources
    if !spec.source.is_empty() {
        let mut arr = Vec::new();
        for s in &spec.source {
            let mut st = Table::new();
            st.insert("url".into(), Value::String(s.url.clone()));
            if !s.sha256.is_empty() && s.sha256.to_lowercase() != "skip" {
                st.insert("sha256".into(), Value::String(s.sha256.clone()));
            }
            st.insert("extract_dir".into(), Value::String(s.extract_dir.clone()));
            if !s.patches.is_empty() {
                st.insert(
                    "patches".into(),
                    Value::Array(s.patches.iter().map(|p| Value::String(p.clone())).collect()),
                );
            }
            if !s.post_extract.is_empty() {
                st.insert(
                    "post_extract".into(),
                    Value::Array(
                        s.post_extract
                            .iter()
                            .map(|p| Value::String(p.clone()))
                            .collect(),
                    ),
                );
            }
            arr.push(Value::Table(st));
        }
        root.insert("source".into(), Value::Array(arr));
    }

    // build (only include set fields)
    let mut build_tbl = Table::new();
    let build_type = match spec.build.build_type {
        BuildType::Autotools => "autotools",
        BuildType::CMake => "cmake",
        BuildType::Meson => "meson",
        BuildType::Custom => "custom",
        BuildType::Python => "python",
        BuildType::Rust => "rust",
        BuildType::Makefile => "makefile",
        BuildType::Bin => "bin",
        BuildType::Meta => "meta",
    };
    build_tbl.insert("type".into(), Value::String(build_type.to_string()));

    let defaults = BuildFlags::default();
    let mut flags_tbl = Table::new();
    if let Some(bdir) = &spec.build.flags.build_dir {
        flags_tbl.insert("build_dir".into(), Value::String(bdir.clone()));
    }
    if !spec.build.flags.source_subdir.is_empty() {
        flags_tbl.insert(
            "source_subdir".into(),
            Value::String(spec.build.flags.source_subdir.clone()),
        );
    }
    if !spec.build.flags.cflags.is_empty() {
        flags_tbl.insert(
            "cflags".into(),
            Value::Array(
                spec.build
                    .flags
                    .cflags
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.cxxflags.is_empty() {
        flags_tbl.insert(
            "cxxflags".into(),
            Value::Array(
                spec.build
                    .flags
                    .cxxflags
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.ldflags.is_empty() {
        flags_tbl.insert(
            "ldflags".into(),
            Value::Array(
                spec.build
                    .flags
                    .ldflags
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.keep.is_empty() {
        flags_tbl.insert(
            "keep".into(),
            Value::Array(
                spec.build
                    .flags
                    .keep
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if spec.build.flags.no_flags != defaults.no_flags {
        flags_tbl.insert("no_flags".into(), Value::Boolean(spec.build.flags.no_flags));
    }
    if spec.build.flags.no_strip != defaults.no_strip {
        flags_tbl.insert("no_strip".into(), Value::Boolean(spec.build.flags.no_strip));
    }
    if spec.build.flags.no_delete_static != defaults.no_delete_static {
        flags_tbl.insert(
            "no_delete_static".into(),
            Value::Boolean(spec.build.flags.no_delete_static),
        );
    }
    if spec.build.flags.no_compress_man != defaults.no_compress_man {
        flags_tbl.insert(
            "no_compress_man".into(),
            Value::Boolean(spec.build.flags.no_compress_man),
        );
    }
    if spec.build.flags.skip_tests != defaults.skip_tests {
        flags_tbl.insert(
            "skip_tests".into(),
            Value::Boolean(spec.build.flags.skip_tests),
        );
    }
    if !spec.build.flags.configure.is_empty() {
        flags_tbl.insert(
            "configure".into(),
            Value::Array(
                spec.build
                    .flags
                    .configure
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.configure_file.is_empty() {
        flags_tbl.insert(
            "configure_file".into(),
            Value::String(spec.build.flags.configure_file.clone()),
        );
    }
    if spec.build.flags.prefix != defaults.prefix {
        flags_tbl.insert(
            "prefix".into(),
            Value::String(spec.build.flags.prefix.clone()),
        );
    }
    if spec.build.flags.cc != defaults.cc {
        flags_tbl.insert("cc".into(), Value::String(spec.build.flags.cc.clone()));
    }
    if spec.build.flags.cxx != defaults.cxx {
        flags_tbl.insert("cxx".into(), Value::String(spec.build.flags.cxx.clone()));
    }
    if spec.build.flags.ar != defaults.ar {
        flags_tbl.insert("ar".into(), Value::String(spec.build.flags.ar.clone()));
    }
    if !spec.build.flags.libc.is_empty() {
        flags_tbl.insert("libc".into(), Value::String(spec.build.flags.libc.clone()));
    }
    if !spec.build.flags.chost.is_empty() {
        flags_tbl.insert(
            "chost".into(),
            Value::String(spec.build.flags.chost.clone()),
        );
    }
    if !spec.build.flags.cbuild.is_empty() {
        flags_tbl.insert(
            "cbuild".into(),
            Value::String(spec.build.flags.cbuild.clone()),
        );
    }
    if !spec.build.flags.carch.is_empty() && spec.build.flags.carch != defaults.carch {
        flags_tbl.insert(
            "carch".into(),
            Value::String(spec.build.flags.carch.clone()),
        );
    }
    if !spec.build.flags.make_vars.is_empty() {
        flags_tbl.insert(
            "make_vars".into(),
            Value::Array(
                spec.build
                    .flags
                    .make_vars
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.make_exec.is_empty() {
        flags_tbl.insert(
            "make_exec".into(),
            Value::String(spec.build.flags.make_exec.clone()),
        );
    }
    if !spec.build.flags.make_target.is_empty() {
        flags_tbl.insert(
            "make_target".into(),
            Value::String(spec.build.flags.make_target.clone()),
        );
    }
    if !spec.build.flags.make_targets.is_empty() {
        flags_tbl.insert(
            "make_targets".into(),
            Value::Array(
                spec.build
                    .flags
                    .make_targets
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.make_dirs.is_empty() {
        flags_tbl.insert(
            "make_dirs".into(),
            Value::Array(
                spec.build
                    .flags
                    .make_dirs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.make_test_vars.is_empty() {
        flags_tbl.insert(
            "make_test_vars".into(),
            Value::Array(
                spec.build
                    .flags
                    .make_test_vars
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.make_test_target.is_empty() {
        flags_tbl.insert(
            "make_test_target".into(),
            Value::String(spec.build.flags.make_test_target.clone()),
        );
    }
    if !spec.build.flags.make_test_targets.is_empty() {
        flags_tbl.insert(
            "make_test_targets".into(),
            Value::Array(
                spec.build
                    .flags
                    .make_test_targets
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.make_test_dirs.is_empty() {
        flags_tbl.insert(
            "make_test_dirs".into(),
            Value::Array(
                spec.build
                    .flags
                    .make_test_dirs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.make_install_vars.is_empty() {
        flags_tbl.insert(
            "make_install_vars".into(),
            Value::Array(
                spec.build
                    .flags
                    .make_install_vars
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.make_install_target.is_empty() {
        flags_tbl.insert(
            "make_install_target".into(),
            Value::String(spec.build.flags.make_install_target.clone()),
        );
    }
    if !spec.build.flags.make_install_targets.is_empty() {
        flags_tbl.insert(
            "make_install_targets".into(),
            Value::Array(
                spec.build
                    .flags
                    .make_install_targets
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.make_install_dirs.is_empty() {
        flags_tbl.insert(
            "make_install_dirs".into(),
            Value::Array(
                spec.build
                    .flags
                    .make_install_dirs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.passthrough_env.is_empty() {
        flags_tbl.insert(
            "passthrough_env".into(),
            Value::Array(
                spec.build
                    .flags
                    .passthrough_env
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.post_compile.is_empty() {
        flags_tbl.insert(
            "post_compile".into(),
            Value::Array(
                spec.build
                    .flags
                    .post_compile
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.post_configure.is_empty() {
        flags_tbl.insert(
            "post_configure".into(),
            Value::Array(
                spec.build
                    .flags
                    .post_configure
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.post_install.is_empty() {
        flags_tbl.insert(
            "post_install".into(),
            Value::Array(
                spec.build
                    .flags
                    .post_install
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.makefile_commands.is_empty() {
        flags_tbl.insert(
            "makefile_commands".into(),
            Value::Array(
                spec.build
                    .flags
                    .makefile_commands
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.makefile_install_commands.is_empty() {
        flags_tbl.insert(
            "makefile_install_commands".into(),
            Value::Array(
                spec.build
                    .flags
                    .makefile_install_commands
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if spec.build.flags.profile != defaults.profile {
        flags_tbl.insert(
            "profile".into(),
            Value::String(spec.build.flags.profile.clone()),
        );
    }
    if !spec.build.flags.target.is_empty() {
        flags_tbl.insert(
            "target".into(),
            Value::String(spec.build.flags.target.clone()),
        );
    }
    if !spec.build.flags.rustflags.is_empty() {
        flags_tbl.insert(
            "rustflags".into(),
            Value::Array(
                spec.build
                    .flags
                    .rustflags
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if !spec.build.flags.cargs.is_empty() {
        flags_tbl.insert(
            "cargs".into(),
            Value::Array(
                spec.build
                    .flags
                    .cargs
                    .iter()
                    .map(|s| Value::String(s.clone()))
                    .collect(),
            ),
        );
    }
    if spec.build.flags.bindir != defaults.bindir {
        flags_tbl.insert(
            "bindir".into(),
            Value::String(spec.build.flags.bindir.clone()),
        );
    }
    if !spec.build.flags.binary_type.is_empty() {
        flags_tbl.insert(
            "binary_type".into(),
            Value::String(spec.build.flags.binary_type.clone()),
        );
    }

    if !flags_tbl.is_empty() {
        build_tbl.insert("flags".into(), Value::Table(flags_tbl));
    }
    root.insert("build".into(), Value::Table(build_tbl));

    // dependencies
    if !spec.dependencies.build.is_empty()
        || !spec.dependencies.runtime.is_empty()
        || !spec.dependencies.test.is_empty()
        || !spec.dependencies.optional.is_empty()
    {
        let mut dep_tbl = Table::new();
        if !spec.dependencies.build.is_empty() {
            dep_tbl.insert(
                "build".into(),
                Value::Array(
                    spec.dependencies
                        .build
                        .iter()
                        .map(|s| Value::String(s.clone()))
                        .collect(),
                ),
            );
        }
        if !spec.dependencies.runtime.is_empty() {
            dep_tbl.insert(
                "runtime".into(),
                Value::Array(
                    spec.dependencies
                        .runtime
                        .iter()
                        .map(|s| Value::String(s.clone()))
                        .collect(),
                ),
            );
        }
        if !spec.dependencies.test.is_empty() {
            dep_tbl.insert(
                "test".into(),
                Value::Array(
                    spec.dependencies
                        .test
                        .iter()
                        .map(|s| Value::String(s.clone()))
                        .collect(),
                ),
            );
        }
        if !spec.dependencies.optional.is_empty() {
            dep_tbl.insert(
                "optional".into(),
                Value::Array(
                    spec.dependencies
                        .optional
                        .iter()
                        .map(|s| Value::String(s.clone()))
                        .collect(),
                ),
            );
        }
        root.insert("dependencies".into(), Value::Table(dep_tbl));
    }

    Ok(toml::to_string_pretty(&Value::Table(root))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{
        BuildFlags, BuildType, Dependencies, ManualSource, PackageInfo, PackageSpec, Source,
    };

    #[test]
    fn spec_to_minimal_toml_omits_defaults() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "A test".into(),
                homepage: "".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo-1.0.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo-1.0".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let toml = spec_to_minimal_toml(&spec).unwrap();
        assert!(toml.contains("name = \"foo\""));
        assert!(toml.contains("version = \"1.0\""));
        assert!(toml.contains("description = \"A test\""));
        // defaults should not be present
        assert!(!toml.contains("cflags"));
        assert!(!toml.contains("rustflags"));
        // sha256="skip" should be omitted
        assert!(!toml.contains("sha256"));
    }

    #[test]
    fn spec_to_minimal_toml_includes_additional_packages() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "A test".into(),
                homepage: "".into(),
                license: vec!["MIT".into()],
            },
            packages: vec![PackageInfo {
                name: "foo-dev".into(),
                version: "1.0".into(),
                revision: 1,
                description: "dev files".into(),
                homepage: "".into(),
                license: vec!["MIT".into()],
            }],
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo-1.0.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo-1.0".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let toml = spec_to_minimal_toml(&spec).unwrap();
        assert!(toml.contains("[[packages]]"));
        assert!(toml.contains("name = \"foo-dev\""));
    }

    #[test]
    fn spec_to_minimal_toml_includes_multiple_licenses_as_array() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "A test".into(),
                homepage: "".into(),
                license: vec!["MIT".into(), "Apache-2.0".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo-1.0.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo-1.0".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let toml = spec_to_minimal_toml(&spec).unwrap();
        assert!(toml.contains("license = ["));
        assert!(toml.contains("\"MIT\""));
        assert!(toml.contains("\"Apache-2.0\""));
    }

    #[test]
    fn spec_to_minimal_toml_includes_extended_build_flags() {
        let mut flags = BuildFlags::default();
        flags.source_subdir = "project/subdir".into();
        flags.configure = vec!["--disable-static".into(), "--enable-foo".into()];
        flags.configure_file = "build-aux/configure".into();
        flags.post_configure = vec!["./configure-helper.sh".into()];
        flags.post_compile = vec!["make check".into()];
        flags.post_install = vec!["strip $DESTDIR/usr/bin/foo".into()];
        flags.makefile_commands = vec!["make".into()];
        flags.makefile_install_commands = vec!["make DESTDIR=$DESTDIR install".into()];
        flags.cargs = vec!["--locked".into()];
        flags.rustflags = vec!["-Ctarget-cpu=native".into()];
        flags.cxxflags = vec!["-O2".into(), "-fno-rtti".into()];
        flags.target = "x86_64-unknown-linux-gnu".into();
        flags.keep = vec!["etc/locale.gen".into()];
        flags.no_flags = true;
        flags.no_strip = true;
        flags.no_delete_static = true;
        flags.no_compress_man = true;
        flags.skip_tests = true;
        flags.make_vars = vec!["V=1".into()];
        flags.make_dirs = vec!["lib".into(), "libelf".into()];
        flags.make_test_vars = vec!["TESTS=unit".into()];
        flags.make_test_dirs = vec!["tests".into()];
        flags.make_install_vars = vec!["STRIPPROG=true".into()];
        flags.make_install_dirs = vec!["lib".into(), "apps".into()];

        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "A test".into(),
                homepage: "".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo-1.0.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo-1.0".into(),
                patches: vec!["fix.patch".into()],
                post_extract: vec!["autoreconf -fi".into()],
            }],
            build: Build {
                build_type: BuildType::Autotools,
                flags,
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let toml = spec_to_minimal_toml(&spec).unwrap();
        assert!(toml.contains("source_subdir = \"project/subdir\""));
        assert!(toml.contains("configure = ["));
        assert!(toml.contains("configure_file = \"build-aux/configure\""));
        assert!(toml.contains("post_configure = ["));
        assert!(toml.contains("post_compile = ["));
        assert!(toml.contains("post_install = ["));
        assert!(toml.contains("makefile_commands = ["));
        assert!(toml.contains("makefile_install_commands = ["));
        assert!(toml.contains("cargs = ["));
        assert!(toml.contains("rustflags = ["));
        assert!(toml.contains("cxxflags = ["));
        assert!(toml.contains("target = \"x86_64-unknown-linux-gnu\""));
        assert!(toml.contains("keep = ["));
        assert!(toml.contains("\"etc/locale.gen\""));
        assert!(toml.contains("no_flags = true"));
        assert!(toml.contains("no_strip = true"));
        assert!(toml.contains("no_delete_static = true"));
        assert!(toml.contains("no_compress_man = true"));
        assert!(toml.contains("skip_tests = true"));
        assert!(toml.contains("make_vars = ["));
        assert!(toml.contains("make_dirs = ["));
        assert!(toml.contains("make_test_vars = ["));
        assert!(toml.contains("make_test_dirs = ["));
        assert!(toml.contains("make_install_vars = ["));
        assert!(toml.contains("make_install_dirs = ["));
        assert!(toml.contains("patches = ["));
        assert!(toml.contains("post_extract = ["));
    }

    #[test]
    fn spec_to_minimal_toml_includes_extract_dir_for_variable_default() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "A test".into(),
                homepage: "".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo-1.0.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "$name-$version".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let toml = spec_to_minimal_toml(&spec).unwrap();
        assert!(toml.contains("extract_dir = \"$name-$version\""));
    }

    #[test]
    fn spec_to_minimal_toml_includes_test_and_optional_dependencies() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "A test".into(),
                homepage: "".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.com/foo-1.0.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo-1.0".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies {
                build: vec![],
                runtime: vec![],
                test: vec!["python".into(), "bats".into()],
                optional: vec!["gtk-doc".into()],
            },
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let toml = spec_to_minimal_toml(&spec).unwrap();
        let val: toml::Value = toml::from_str(&toml).unwrap();
        let test_deps = val
            .get("dependencies")
            .and_then(|d| d.get("test"))
            .and_then(|t| t.as_array())
            .expect("expected dependencies.test array");
        assert_eq!(test_deps.len(), 2);
        assert_eq!(test_deps[0].as_str(), Some("python"));
        assert_eq!(test_deps[1].as_str(), Some("bats"));
        let optional_deps = val
            .get("dependencies")
            .and_then(|d| d.get("optional"))
            .and_then(|t| t.as_array())
            .expect("expected dependencies.optional array");
        assert_eq!(optional_deps.len(), 1);
        assert_eq!(optional_deps[0].as_str(), Some("gtk-doc"));
    }

    #[test]
    fn spec_to_minimal_toml_supports_metapackage_without_sources() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo-meta".into(),
                version: "1.0".into(),
                revision: 1,
                description: "Meta package".into(),
                homepage: "".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: Vec::new(),
            build: Build {
                build_type: BuildType::Meta,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies {
                build: Vec::new(),
                runtime: vec!["foo".into(), "bar".into()],
                test: Vec::new(),
                optional: Vec::new(),
            },
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let toml = spec_to_minimal_toml(&spec).unwrap();
        assert!(toml.contains("type = \"meta\""));
        assert!(!toml.contains("[[source]]"));

        let val: toml::Value = toml::from_str(&toml).unwrap();
        assert!(val.get("source").is_none());
    }

    #[test]
    fn spec_to_minimal_toml_includes_manual_sources() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "vertex-keyring".into(),
                version: "1.0.0".into(),
                revision: 1,
                description: "keyring".into(),
                homepage: "https://www.vertexlinux.net".into(),
                license: vec!["MIT".into()],
            },
            packages: Vec::new(),
            alternatives: Alternatives::default(),
            manual_sources: vec![
                ManualSource {
                    file: Some("vertex.pub".into()),
                    files: Vec::new(),
                    url: None,
                    urls: Vec::new(),
                    sha256: None,
                    dest: Some("usr/share/depot/keys/public/vertex.pub".into()),
                },
                ManualSource {
                    file: None,
                    files: Vec::new(),
                    url: Some("file:///tmp/vertex.minisig".into()),
                    urls: Vec::new(),
                    sha256: Some("skip".into()),
                    dest: Some("usr/share/depot/keys/sign/vertex.minisig".into()),
                },
            ],
            source: Vec::new(),
            build: Build {
                build_type: BuildType::Custom,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies::default(),
            package_alternatives: Default::default(),
            package_dependencies: Default::default(),
            spec_dir: PathBuf::from("."),
        };

        let toml = spec_to_minimal_toml(&spec).unwrap();
        assert!(toml.contains("[[manual_sources]]"));
        assert!(toml.contains("file = \"vertex.pub\""));
        assert!(toml.contains("url = \"file:///tmp/vertex.minisig\""));
        assert!(toml.contains("dest = \"usr/share/depot/keys/public/vertex.pub\""));
        assert!(!toml.contains("sha256 = \"skip\""));

        let val: toml::Value = toml::from_str(&toml).unwrap();
        let arr = val
            .get("manual_sources")
            .and_then(|v| v.as_array())
            .expect("expected manual_sources array");
        assert_eq!(arr.len(), 2);
    }

    #[test]
    fn compute_sha256_for_local_path_and_file_url() {
        use sha2::Digest as TestDigest;
        use sha2::Sha256 as TestSha256;
        use tempfile::NamedTempFile;

        let mut tmp = NamedTempFile::new().unwrap();
        std::io::Write::write_all(&mut tmp, b"abc").unwrap();
        let expected = {
            let mut h = TestSha256::new();
            h.update(b"abc");
            format!("{:x}", h.finalize())
        };

        // plain path
        let p = tmp.path().to_str().unwrap().to_string();
        assert_eq!(compute_sha256_for_url(&p).unwrap(), expected);

        // file:// URL
        let file_url = format!("file://{}", tmp.path().to_str().unwrap());
        assert_eq!(compute_sha256_for_url(&file_url).unwrap(), expected);
    }

    #[test]
    fn expand_known_package_vars_replaces_name_and_version_patterns() {
        let input = "https://example.com/$name/${name}-$version-${version}.tar.xz";
        let out = expand_known_package_vars(input, "python", "3.13.1");
        assert_eq!(
            out,
            "https://example.com/python/python-3.13.1-3.13.1.tar.xz"
        );
    }
}
