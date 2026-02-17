//! Depot - Not Your Average Package Manager
//! A source-based package manager for Linux

mod builder;
mod config;
mod cross;
mod db;
mod deps;
mod fakeroot;
mod index;
mod package;
mod source;
mod staging;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "Depot")]
#[command(about = "Depot - Source-based package manager for Linux", long_about = None)]
#[command(version)]
struct Cli {
    /// Custom root filesystem path
    #[arg(long, short = 'r', default_value = "/", global = true)]
    rootfs: PathBuf,

    /// Skip dependency checks
    #[arg(long, global = true)]
    no_deps: bool,

    /// Cross-compilation prefix (e.g., x86_64-linux-musl, aarch64-linux-gnu)
    #[arg(long, global = true)]
    cross_prefix: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Build and install a package from a spec file
    Install {
        /// Path to package spec (.toml) or package archive (.tar.zst)
        #[arg(value_name = "SPEC_OR_ARCHIVE")]
        spec_or_archive: PathBuf,

        /// Explicitly specify path to package spec (.toml file)
        #[arg(short, long = "spec", visible_alias = "package", alias = "p")]
        spec: Option<PathBuf>,
    },
    /// Remove an installed package
    Remove {
        /// Package name to remove
        package: String,
    },
    /// Build a package without installing
    Build {
        /// Path to package spec (.toml file)
        #[arg(value_name = "SPEC")]
        spec_pos: Option<PathBuf>,

        /// Explicitly specify path to package spec (.toml file)
        #[arg(short, long = "spec", visible_alias = "package", alias = "p")]
        spec: Option<PathBuf>,
    },
    /// Show information about a package
    Info {
        /// Path to package spec or installed package name
        package: String,
    },
    /// List installed packages
    List,
    /// Repository management
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    /// Show current configuration
    Config,
    /// Create a new package specification interactively
    MakeSpec {
        /// Output file path (defaults to <name>.toml)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum RepoCommands {
    /// Create a repository database from a directory of packages
    Create {
        /// Directory containing .depot.pkg.tar.zst files
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Sync git mirrors configured in /etc/depot.d/mirrors.toml into /usr/src/depot
    Sync,
    /// Show status of configured git mirrors
    Status,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    match cli.command {
        Commands::Install {
            spec_or_archive,
            spec,
        } => {
            let mut spec_path = spec.unwrap_or(spec_or_archive);

            // Load configuration early so we can use the configured repo clone dir
            let config = config::Config::for_rootfs(&cli.rootfs);

            // Repo clone dir is available via `config.repo_clone_dir` and
            // is passed explicitly to index builders below.

            // If the provided path doesn't exist, treat it as a package name and
            // try to locate a spec under configured repo dir or local packages/.
            if !spec_path.exists() {
                let name = spec_path.to_string_lossy().to_string();
                println!("Looking up package '{}' in local indexes...", name);
                let pkg_index = index::PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));
                if let Some(found) = pkg_index.find(&name) {
                    spec_path = found;
                }
            }

            println!("Installing package from: {}", spec_path.display());

            let (pkg_spec, staging_dir): (package::PackageSpec, Option<tempfile::TempDir>) =
                if spec_path.to_string_lossy().ends_with(".tar.zst") {
                    // Install from archive
                    println!("Detected package archive: {}", spec_path.display());
                    let tmp_dir = tempfile::TempDir::new()?;
                    let extract_dir = tmp_dir.path().to_path_buf();

                    // Extract metadata.toml first to get spec
                    let file = fs::File::open(&spec_path)?;
                    let zstd_decoder = zstd::stream::read::Decoder::new(file)?;
                    let mut archive = tar::Archive::new(zstd_decoder);

                    let mut metadata_content = String::new();
                    for entry in archive.entries()? {
                        let mut entry = entry?;
                        if entry.path()?.to_string_lossy() == ".metadata.toml" {
                            use std::io::Read;
                            entry.read_to_string(&mut metadata_content)?;
                            break;
                        }
                    }

                    if metadata_content.is_empty() {
                        anyhow::bail!(
                            "Package archive does not contain .metadata.toml: {}",
                            spec_path.display()
                        );
                    }

                    // We need to parse the metadata.toml but we don't have a direct "from_metadata"
                    // Let's implement a minimal reconstruction or use the metadata to fill a spec.
                    // Actually, PackageSpec needs a lot of fields.
                    // Let's extract the WHOLE archive to a temporary staging dir and use it.
                    let file = fs::File::open(&spec_path)?;
                    let zstd_decoder = zstd::stream::read::Decoder::new(file)?;
                    let mut archive = tar::Archive::new(zstd_decoder);
                    archive.unpack(&extract_dir)?;

                    let metadata: toml::Value = toml::from_str(&metadata_content)?;

                    // Create a minimal spec from metadata
                    let mut spec = package::PackageSpec {
                        package: package::PackageInfo {
                            name: metadata
                                .get("name")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            version: metadata
                                .get("version")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            revision: metadata
                                .get("revision")
                                .and_then(|v| v.as_integer())
                                .unwrap_or(1) as u32,
                            description: metadata
                                .get("description")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            homepage: metadata
                                .get("homepage")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                            license: metadata
                                .get("license")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string(),
                        },
                        packages: Vec::new(),
                        alternatives: package::Alternatives::default(),
                        manual_sources: Vec::new(),
                        source: Vec::new(),
                        build: package::Build {
                            build_type: package::BuildType::Bin,
                            flags: package::BuildFlags::default(),
                        },
                        dependencies: package::Dependencies {
                            build: Vec::new(),
                            runtime: if let Some(deps) = metadata
                                .get("dependencies")
                                .and_then(|v| v.get("runtime"))
                                .and_then(|v| v.as_array())
                            {
                                deps.iter()
                                    .filter_map(|v| v.as_str())
                                    .map(String::from)
                                    .collect()
                            } else {
                                Vec::new()
                            },
                        },
                        spec_dir: PathBuf::from("."),
                    };

                    if let Some(provides) = metadata.get("provides").and_then(|v| v.as_array()) {
                        spec.alternatives.provides = provides
                            .iter()
                            .filter_map(|v| v.as_str())
                            .map(String::from)
                            .collect();
                    }

                    (spec, Some(tmp_dir))
                } else {
                    // Install from spec (normal build)
                    let mut pkg_spec = package::PackageSpec::from_file(&spec_path)?;
                    pkg_spec.apply_config(&config);

                    // ... existing build logic ...
                    // Jump to the part where build actually happens.
                    // To keep the code clean, I'll move the build/stage logic into a helper or similar?
                    // Actually, I'll just structure it so we can skip build if staging_dir is Some.
                    (pkg_spec, None)
                };

            println!(
                "Package: {} v{}-{}",
                pkg_spec.package.name, pkg_spec.package.version, pkg_spec.package.revision
            );

            // Ensure database directory exists
            std::fs::create_dir_all(&config.db_dir).with_context(|| {
                format!(
                    "Failed to create database directory: {}",
                    config.db_dir.display()
                )
            })?;
            let db_path = config.db_dir.join("packages.db");

            // Check dependencies and prompt for auto-install if needed
            if !cli.no_deps {
                deps::print_dep_status(&pkg_spec, &db_path)?;

                // Collect all missing dependencies (build + runtime)
                let mut missing = deps::check_build_deps(&pkg_spec, &db_path)?;
                let missing_runtime = deps::check_runtime_deps(&pkg_spec, &db_path)?;

                for dep in missing_runtime {
                    if !missing.contains(&dep) {
                        missing.push(dep);
                    }
                }

                if !missing.is_empty() {
                    // Check for dependency cycles via DEPOT_DEPCHAIN env var
                    let dep_chain = std::env::var("DEPOT_DEPCHAIN").unwrap_or_default();
                    let chain_set: std::collections::HashSet<&str> =
                        dep_chain.split(',').filter(|s| !s.is_empty()).collect();

                    if chain_set.contains(pkg_spec.package.name.as_str()) {
                        anyhow::bail!(
                            "Dependency cycle detected! {} is already in chain: {}",
                            pkg_spec.package.name,
                            dep_chain
                        );
                    }

                    println!("\nMissing dependencies: {}", missing.join(", "));
                    println!("Do you want to attempt to install them? [Y/n] ");

                    let mut input = String::new();
                    std::io::stdin().read_line(&mut input)?;
                    let input = input.trim().to_lowercase();

                    if input == "y" || input == "yes" || input.is_empty() {
                        // Build package index for fast lookups
                        let pkg_index = index::PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));

                        // Build new dep chain
                        let new_chain = if dep_chain.is_empty() {
                            pkg_spec.package.name.clone()
                        } else {
                            format!("{},{}", dep_chain, pkg_spec.package.name)
                        };

                        // Attempt to install missing deps
                        for dep in missing {
                            // Use package index for O(1) lookup
                            let candidate = pkg_index.find(&dep);

                            if let Some(dep_spec_path) = candidate {
                                println!("Installing dependency: {}...", dep);

                                let mut cmd = std::process::Command::new(std::env::current_exe()?);
                                cmd.arg("-r").arg(&cli.rootfs);

                                if cli.no_deps {
                                    cmd.arg("--no-deps");
                                }
                                if let Some(ref p) = cli.cross_prefix {
                                    cmd.arg("--cross-prefix").arg(p);
                                }

                                cmd.arg("install").arg(&dep_spec_path);
                                cmd.env("DEPOT_DEPCHAIN", &new_chain);

                                let status = cmd.status()?;

                                if !status.success() {
                                    anyhow::bail!("Failed to install dependency: {}", dep);
                                }
                            } else {
                                anyhow::bail!(
                                    "Could not find package spec for dependency: {}",
                                    dep
                                );
                            }
                        }
                    }
                }

                // Enforce build dependencies (runtime deps are warnings only if not installed/prompt declined)
                deps::require_build_deps(&pkg_spec, &db_path)?;
            }

            // Ensure database directory exists
            std::fs::create_dir_all(&config.db_dir).with_context(|| {
                format!(
                    "Failed to create database directory: {}",
                    config.db_dir.display()
                )
            })?;
            let db_path = config.db_dir.join("packages.db");

            let destdir = if let Some(dir) = &staging_dir {
                dir.path().to_path_buf()
            } else {
                // 1-2. Fetch + extract sources (supports archives and git URL#rev)
                let src_dir = source::prepare(&pkg_spec, &config.cache_dir, &config.build_dir)?;

                // 3. Build
                let destdir = config
                    .build_dir
                    .join("destdir")
                    .join(&pkg_spec.package.name);

                // Build with optional cross-compilation
                let cross_config = cli
                    .cross_prefix
                    .as_ref()
                    .map(|p| cross::CrossConfig::from_prefix(p))
                    .transpose()?;
                builder::build(&pkg_spec, &src_dir, &destdir, cross_config.as_ref())?;

                // 3.1 Copy license files into staged tree
                staging::add_licenses(&src_dir, &destdir, &pkg_spec.package.name)?;

                destdir
            };

            // 4. Stage (clean .la files, etc.)
            staging::process(&destdir, &pkg_spec)?;

            // 5. Install/update to rootfs (atomic)
            let new_files = staging::generate_manifest_with_dirs(&destdir)?;

            let remove_paths =
                db::calculate_upgrade_paths(&db_path, &pkg_spec.package.name, &new_files.files)?;

            let tx_base = config.build_dir.join("tx");
            let tx = staging::install_atomic(&destdir, &cli.rootfs, &tx_base, &remove_paths)?;

            // 6. Register in database (rollback install on DB error)
            for out in pkg_spec.outputs() {
                let mut spec_for_out = pkg_spec.clone();
                spec_for_out.package = out;
                if let Err(e) = db::register_package(&db_path, &spec_for_out, &destdir) {
                    let _ = tx.rollback();
                    return Err(e);
                }
            }
            tx.commit()?;

            // 7. Check runtime dependencies (warn only)
            if !cli.no_deps {
                let missing_runtime = deps::check_runtime_deps(&pkg_spec, &db_path)?;
                if !missing_runtime.is_empty() {
                    eprintln!(
                        "\x1b[33mWarning: Missing runtime dependencies: {}\x1b[0m",
                        missing_runtime.join(", ")
                    );
                    eprintln!(
                        "\x1b[33mContinuing without runtime deps; binaries may not run correctly.\x1b[0m"
                    );
                    eprintln!("\x1b[33mUse --no-deps to suppress this warning.\x1b[0m");
                }
            }

            println!(
                "Successfully installed {} v{}",
                pkg_spec.package.name, pkg_spec.package.version
            );
        }
        Commands::Remove { package } => {
            println!("Removing package: {}", package);
            let config = config::Config::for_rootfs(&cli.rootfs);
            let db_path = config.db_dir.join("packages.db");
            db::remove_package(&db_path, &package, &cli.rootfs)?;
            println!("Successfully removed {}", package);
        }
        Commands::Build { spec_pos, spec } => {
            if crate::fakeroot::is_root() {
                anyhow::bail!(
                    "The 'build' command must be run as a non-root user to ensure a clean build environment."
                );
            }
            let spec_path = spec.or(spec_pos).context("No spec file provided")?;
            println!("Building package from: {}", spec_path.display());
            let mut pkg_spec = package::PackageSpec::from_file(&spec_path)?;

            let config = config::Config::for_rootfs(&cli.rootfs);

            // Apply system overrides
            pkg_spec.apply_config(&config);

            // Ensure database directory exists
            std::fs::create_dir_all(&config.db_dir).with_context(|| {
                format!(
                    "Failed to create database directory: {}",
                    config.db_dir.display()
                )
            })?;
            let db_path = config.db_dir.join("packages.db");

            // Check build dependencies
            if !cli.no_deps {
                deps::print_dep_status(&pkg_spec, &db_path)?;
                deps::require_build_deps(&pkg_spec, &db_path)?;
            }

            let src_dir = source::prepare(&pkg_spec, &config.cache_dir, &config.build_dir)?;

            let destdir = config
                .build_dir
                .join("destdir")
                .join(&pkg_spec.package.name);
            // Build with optional cross-compilation
            let cross_config = cli
                .cross_prefix
                .as_ref()
                .map(|p| cross::CrossConfig::from_prefix(p))
                .transpose()?;
            builder::build(&pkg_spec, &src_dir, &destdir, cross_config.as_ref())?;

            staging::add_licenses(&src_dir, &destdir, &pkg_spec.package.name)?;

            staging::process(&destdir, &pkg_spec)?;

            // Create package archive(s) — support multiple outputs from a single spec.
            let arch = cli
                .cross_prefix
                .as_deref()
                .unwrap_or(std::env::consts::ARCH);

            let mut created_files = Vec::new();
            for out in pkg_spec.outputs() {
                let mut spec_for_out = pkg_spec.clone();
                spec_for_out.package = out;
                let packager = package::Packager::new(spec_for_out.clone(), destdir.clone(), config.clone());
                let pkg_file = packager.create_package(Path::new("."), arch)?;
                created_files.push(pkg_file);
            }

            for f in &created_files {
                println!("Build complete. Package created: {}", f.display());
            }
        }
        Commands::Info { package } => {
            // Try as file first, then as installed package name
            let path = PathBuf::from(&package);
            if path.exists() {
                let pkg_spec = package::PackageSpec::from_file(&path)?;
                println!("{}", pkg_spec);

                // Also show dependency status
                let config = config::Config::for_rootfs(&cli.rootfs);
                let db_path = config.db_dir.join("packages.db");
                deps::print_dep_status(&pkg_spec, &db_path)?;
            } else {
                let config = config::Config::for_rootfs(&cli.rootfs);
                let db_path = config.db_dir.join("packages.db");
                db::show_package_info(&db_path, &package)?;
            }
        }
        Commands::List => {
            let config = config::Config::for_rootfs(&cli.rootfs);
            let db_path = config.db_dir.join("packages.db");
            db::list_packages(&db_path)?;
        }
        Commands::Repo { command } => match command {
            RepoCommands::Create { dir } => {
                let repo = db::repo::RepoManager::new(dir);
                let db_path = repo.create_repo_db()?;
                println!("Created repository database: {}", db_path.display());
            }
            RepoCommands::Sync => {
                // Only root may run sync
                if !crate::fakeroot::is_root() {
                    anyhow::bail!("The 'repo sync' command must be run as root");
                }
                let config = config::Config::for_rootfs(&cli.rootfs);
                if config.mirrors.is_empty() {
                    println!("No mirrors configured in /etc/depot.d/mirrors.toml");
                } else {
                    db::repo::sync_mirrors(&config.repo_clone_dir, &config.mirrors)?;
                    println!("Mirrors synchronized into {}", config.repo_clone_dir.display());
                }
            }
            RepoCommands::Status => {
                let config = config::Config::for_rootfs(&cli.rootfs);
                if config.mirrors.is_empty() {
                    println!("No mirrors configured in /etc/depot.d/mirrors.toml");
                } else {
                    db::repo::mirrors_status(&config.repo_clone_dir, &config.mirrors)?;
                }
            }
        },
        Commands::Config => {
            let config = config::Config::for_rootfs(&cli.rootfs);
            println!("Cache Directory: {}", config.cache_dir.display());
            println!("Build Directory: {}", config.build_dir.display());
            println!("Database Directory: {}", config.db_dir.display());
            println!("\nBuild Overrides: {}", config.build_overrides);
            println!("Package Overrides: {}", config.package_overrides);
            if !config.appends.is_empty() {
                println!("\nAppends:");
                for (k, v) in &config.appends {
                    println!("  {} = {:?}", k, v);
                }
            }
        }
        Commands::MakeSpec { output } => {
            let spec = package::create_interactive()?;
            // Produce a minimal TOML for interactive-created specs (omit defaults)
            let toml_string = package::spec_to_minimal_toml(&spec)?;

            let output_path = output.unwrap_or_else(|| PathBuf::from(format!("{}.toml", spec.package.name)));

            if output_path.exists() {
                println!(
                    "Warning: File {} already exists. Overwrite? [y/N]",
                    output_path.display()
                );
                let mut input = String::new();
                std::io::stdin().read_line(&mut input)?;
                if !input.trim().eq_ignore_ascii_case("y") {
                    anyhow::bail!("Aborted");
                }
            }

            fs::write(&output_path, toml_string)?;
            println!("Package specification saved to {}", output_path.display());
        }
    }

    Ok(())
}
