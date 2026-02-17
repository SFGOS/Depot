use crate::package::{
    Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source,
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
            BuildType::Rust => write!(f, "Rust"),
            BuildType::Makefile => write!(f, "Makefile"),
            BuildType::Bin => write!(f, "Binary installer"),
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
            if n == 0 { break; }
            hasher.update(&buf[..n]);
        }
        Ok(format!("{:x}", hasher.finalize()))
    }

    // Try to parse as URL first; if parsing fails, treat as local path.
    if let Ok(parsed) = Url::parse(u) {
        match parsed.scheme() {
            "http" | "https" => {
                let client = reqwest::blocking::Client::builder()
                    .timeout(Duration::from_secs(20))
                    .build()
                    .with_context(|| "failed to build http client")?;
                let mut resp = client.get(u).send().with_context(|| format!("failed to GET {}", u))?;
                if !resp.status().is_success() {
                    anyhow::bail!("HTTP status {}", resp.status());
                }
                return hash_reader(&mut resp);
            }
            "ftp" => {
                let host = parsed.host_str().context("ftp url missing host")?;
                let port = parsed.port_or_known_default().unwrap_or(21);
                let addr = format!("{}:{}", host, port);
                let mut ftp_stream = ftp::FtpStream::connect(addr.as_str())
                    .with_context(|| format!("failed to connect to {}", addr))?;
                let user = if parsed.username().is_empty() { "anonymous" } else { parsed.username() };
                let pass = parsed.password().unwrap_or("anonymous@");
                ftp_stream.login(user, pass).with_context(|| "ftp login failed")?;
                let mut result_hex = None;
                let path = parsed.path();
                let candidates = [path.to_string(), path.trim_start_matches('/').to_string()];
                for p in candidates.iter().filter(|s| !s.is_empty()) {
                    if let Ok(res) = ftp_stream.retr(p, |reader| {
                        // reuse hash_reader by adapting reader to trait object
                        let mut r = reader;
                        hash_reader(&mut r).map_err(|e| ftp::FtpError::ConnectionError(std::io::Error::new(std::io::ErrorKind::Other, e.to_string())))
                    }) {
                        result_hex = Some(res);
                        break;
                    }
                }
                ftp_stream.quit().ok();
                if let Some(h) = result_hex { return Ok(h); }
                anyhow::bail!("ftp retrieval failed")
            }
            "file" => {
                if let Ok(fp) = parsed.to_file_path() {
                    let mut f = std::fs::File::open(fp)?;
                    return hash_reader(&mut f);
                }
                anyhow::bail!("invalid file URL")
            }
            _ => anyhow::bail!("unsupported URL scheme")
        }
    }

    // Treat as local path if it exists
    let p = std::path::Path::new(u);
    if p.exists() {
        let mut f = std::fs::File::open(p)?;
        return hash_reader(&mut f);
    }

    anyhow::bail!("could not compute sha256 for '{}': unsupported or unreachable", u)
}

pub fn create_interactive() -> Result<PackageSpec> {
    println!("Interactive Package Specification Creator");
    println!("-----------------------------------------");

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

    let license = Text::new("License:")
        .with_help_message("e.g. MIT, GPL-3.0, Apache-2.0")
        .with_placeholder("MIT")
        .prompt()?;

    // Present core build system choices only (keep interactive concise)
    let build_types = vec![
        BuildType::Autotools,
        BuildType::CMake,
        BuildType::Meson,
        BuildType::Makefile,
        BuildType::Rust,
    ];

    let build_type = Select::new("Build System:", build_types)
        .with_help_message("Select the build system used by the package (common choices)")
        .prompt()?;

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

    // Ask whether to show advanced fields — if yes, prompt for them later.
    let show_advanced = Confirm::new("Show advanced options?")
        .with_help_message("Enable prompts for optional build flags and advanced fields")
        .with_default(false)
        .prompt()?;

    let build_dir = if Confirm::new("Use separate build directory?")
        .with_help_message("e.g., build/ or out/")
        .with_default(false)
        .prompt()?
    {
        Some(
            Text::new("Build directory name:")
                .with_default("build")
                .prompt()?,
        )
    } else {
        None
    };

    // Advanced fields (only shown when requested)
    let mut cflags = Vec::new();
    let mut ldflags = Vec::new();
    let mut chost = String::new();
    let mut cbuild = String::new();
    let mut carch = String::new();
    let mut bindir = String::new();

    if show_advanced {
        let c = Text::new("CFLAGS (optional, empty to skip):")
            .with_help_message("Space-separated CFLAGS")
            .prompt()?;
        if !c.trim().is_empty() {
            cflags = c.split_whitespace().map(String::from).collect();
        }
        let l = Text::new("LDFLAGS (optional, empty to skip):")
            .with_help_message("Space-separated LDFLAGS")
            .prompt()?;
        if !l.trim().is_empty() {
            ldflags = l.split_whitespace().map(String::from).collect();
        }
        chost = Text::new("CHOST (optional):").prompt()?;
        cbuild = Text::new("CBUILD (optional):").prompt()?;
        carch = Text::new("CARCH (optional):").prompt()?;
        bindir = Text::new("Binary install dir (optional, default /usr/bin):")
            .with_default("/usr/bin")
            .prompt()?;
    }

    let source_url = Text::new("Source URL:")
        .with_help_message("URL to the source tarball or git repository")
        .with_default(gnu_source_default.as_str())
        .prompt()?;

    // Attempt to compute SHA256 automatically when online — use as the default if available.
    let computed_sha_default = match compute_sha256_for_url(&source_url) {
        Ok(hex) => {
            // Use raw hex as the default so pressing Enter accepts it
            hex
        }
        Err(_) => "skip".to_string(),
    };

    let source_sha256 = Text::new("Source checksum:")
        .with_help_message("Accepts sha256:, sha512:, md5:, or raw SHA256 hex (use 'skip' to bypass)")
        .with_default(computed_sha_default.as_str())
        .prompt()?;

    let extract_dir = Text::new("Extract Directory:")
        .with_help_message("Directory created after extraction (supports $name, $version)")
        .with_default("$name-$version")
        .prompt()?;

    let mut sources = Vec::new();
    if !source_url.is_empty() {
        sources.push(Source {
            url: source_url,
            sha256: source_sha256,
            extract_dir,
            patches: Vec::new(),
            post_extract: Vec::new(),
        });
    }

    let mut runtime_deps = Vec::new();
    loop {
        let dep = Text::new("Runtime Dependency (empty to finish):").prompt()?;
        if dep.is_empty() {
            break;
        }
        runtime_deps.push(dep);
    }

    let mut build_deps = Vec::new();
    loop {
        let dep = Text::new("Build-time Dependency (empty to finish):").prompt()?;
        if dep.is_empty() {
            break;
        }
        build_deps.push(dep);
    }

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
        manual_sources: Vec::new(),
        source: sources,
        build: Build {
            build_type,
            flags: BuildFlags {
                build_dir,
                cflags,
                ldflags,
                chost,
                cbuild,
                carch,
                bindir: if bindir.is_empty() { BuildFlags::default().bindir } else { bindir },
                ..BuildFlags::default()
            },
        },
        dependencies: Dependencies {
            build: build_deps,
            runtime: runtime_deps,
        },
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
    pkg.insert("version".into(), Value::String(spec.package.version.clone()));
    if !spec.package.description.is_empty() {
        pkg.insert("description".into(), Value::String(spec.package.description.clone()));
    }
    if !spec.package.homepage.is_empty() {
        pkg.insert("homepage".into(), Value::String(spec.package.homepage.clone()));
    }
    if !spec.package.license.is_empty() {
        pkg.insert("license".into(), Value::String(spec.package.license.clone()));
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
            if !p.license.is_empty() {
                pt.insert("license".into(), Value::String(p.license.clone()));
            }
            arr.push(Value::Table(pt));
        }
        root.insert("packages".into(), Value::Array(arr));
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
            let default_extract = format!("{}-{}", spec.package.name, spec.package.version);
            if s.extract_dir != default_extract {
                st.insert("extract_dir".into(), Value::String(s.extract_dir.clone()));
            }
            if !s.patches.is_empty() {
                st.insert(
                    "patches".into(),
                    Value::Array(s.patches.iter().map(|p| Value::String(p.clone())).collect()),
                );
            }
            if !s.post_extract.is_empty() {
                st.insert(
                    "post_extract".into(),
                    Value::Array(s.post_extract.iter().map(|p| Value::String(p.clone())).collect()),
                );
            }
            arr.push(Value::Table(st));
        }
        root.insert("source".into(), Value::Array(arr));
    }

    // build (only include set fields)
    let mut build_tbl = Table::new();
    build_tbl.insert("type".into(), Value::String(format!("{}", format!("{:?}", spec.build.build_type).to_lowercase())));

    let mut flags_tbl = Table::new();
    if let Some(bdir) = &spec.build.flags.build_dir {
        flags_tbl.insert("build_dir".into(), Value::String(bdir.clone()));
    }
    if !spec.build.flags.cflags.is_empty() {
        flags_tbl.insert(
            "cflags".into(),
            Value::Array(spec.build.flags.cflags.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    if !spec.build.flags.ldflags.is_empty() {
        flags_tbl.insert(
            "ldflags".into(),
            Value::Array(spec.build.flags.ldflags.iter().map(|s| Value::String(s.clone())).collect()),
        );
    }
    if !spec.build.flags.chost.is_empty() {
        flags_tbl.insert("chost".into(), Value::String(spec.build.flags.chost.clone()));
    }
    if !spec.build.flags.cbuild.is_empty() {
        flags_tbl.insert("cbuild".into(), Value::String(spec.build.flags.cbuild.clone()));
    }
    if !spec.build.flags.carch.is_empty() && spec.build.flags.carch != BuildFlags::default().carch {
        flags_tbl.insert("carch".into(), Value::String(spec.build.flags.carch.clone()));
    }
    if spec.build.flags.bindir != BuildFlags::default().bindir {
        flags_tbl.insert("bindir".into(), Value::String(spec.build.flags.bindir.clone()));
    }

    if !flags_tbl.is_empty() {
        build_tbl.insert("flags".into(), Value::Table(flags_tbl));
    }
    root.insert("build".into(), Value::Table(build_tbl));

    // dependencies
    if !spec.dependencies.build.is_empty() || !spec.dependencies.runtime.is_empty() {
        let mut dep_tbl = Table::new();
        if !spec.dependencies.build.is_empty() {
            dep_tbl.insert(
                "build".into(),
                Value::Array(spec.dependencies.build.iter().map(|s| Value::String(s.clone())).collect()),
            );
        }
        if !spec.dependencies.runtime.is_empty() {
            dep_tbl.insert(
                "runtime".into(),
                Value::Array(spec.dependencies.runtime.iter().map(|s| Value::String(s.clone())).collect()),
            );
        }
        root.insert("dependencies".into(), Value::Table(dep_tbl));
    }

    Ok(toml::to_string_pretty(&Value::Table(root))?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::package::{BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source};

    #[test]
    fn spec_to_minimal_toml_omits_defaults() {
        let spec = PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                version: "1.0".into(),
                revision: 1,
                description: "A test".into(),
                homepage: "".into(),
                license: "MIT".into(),
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
                license: "MIT".into(),
            },
            packages: vec![PackageInfo {
                name: "foo-dev".into(),
                version: "1.0".into(),
                revision: 1,
                description: "dev files".into(),
                homepage: "".into(),
                license: "MIT".into(),
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
            spec_dir: PathBuf::from("."),
        };

        let toml = spec_to_minimal_toml(&spec).unwrap();
        assert!(toml.contains("[[packages]]"));
        assert!(toml.contains("name = \"foo-dev\""));
    }

    #[test]
    fn compute_sha256_for_local_path_and_file_url() {
        use tempfile::NamedTempFile;
        use sha2::Sha256 as TestSha256;
        use sha2::Digest as TestDigest;

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
}

