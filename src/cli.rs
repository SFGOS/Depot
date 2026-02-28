use clap::{Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "Depot")]
#[command(about = "Depot - Source-based package manager for Linux", long_about = None)]
#[command(version)]
pub struct Cli {
    /// Custom root filesystem path
    #[arg(long, short = 'r', default_value = "/", global = true)]
    pub rootfs: PathBuf,

    /// Skip dependency checks
    #[arg(long, global = true)]
    pub no_deps: bool,

    /// Do not export CFLAGS/CXXFLAGS/LDFLAGS to build commands
    #[arg(
        long,
        global = true,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_value_t = false,
        default_missing_value = "true"
    )]
    pub no_flags: bool,

    /// Cross-compilation prefix (e.g., x86_64-linux-musl, aarch64-linux-gnu)
    #[arg(long, global = true)]
    pub cross_prefix: Option<String>,

    /// Clean build workspace after successful install/build
    #[arg(long, global = true)]
    pub clean: bool,

    /// Automatically answer yes to prompts and pick the default provider choice
    #[arg(long, short = 'y', global = true)]
    pub yes: bool,

    /// Show what would happen without performing builds/installs
    #[arg(long, global = true)]
    pub dry_run: bool,

    /// Build/install only the lib32 companion package path (skip primary package output)
    #[arg(long, global = true)]
    pub lib32_only: bool,

    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
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

        /// Install package to rootfs after creating package archive(s)
        #[arg(long)]
        install: bool,
    },
    /// Show information about a package
    Info {
        /// Path to package spec or installed package name
        package: String,
    },
    /// Search configured source and binary repos by package name or provides
    Search {
        /// Search query
        query: String,
        /// Search repository file lists (binary repo metadata) by path substring
        #[arg(long)]
        files: bool,
    },
    /// Show which installed package owns a filesystem path
    Owns {
        /// Path to query (absolute or relative to rootfs)
        path: PathBuf,
    },
    /// List installed packages
    List,
    /// Create a detached minisign signature for a .zst file
    Sign {
        /// Path to the .zst file to sign
        file: PathBuf,
    },
    /// Repository management
    Repo {
        #[command(subcommand)]
        command: RepoCommands,
    },
    /// Show current configuration
    Config,
    /// Generate shell completion scripts and a man page into an output directory.
    #[command(hide = true)]
    GenerateArtifacts {
        /// Output directory for generated files
        #[arg(long, value_name = "DIR")]
        out_dir: PathBuf,
    },
    /// Create a new package specification interactively
    MakeSpec {
        /// Output file path (defaults to <name>.toml)
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[derive(Subcommand)]
pub enum RepoCommands {
    /// Create a repository database from a directory of packages
    Create {
        /// Directory containing .depot.pkg.tar.zst files
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Sync git mirrors configured in /etc/depot.d/mirrors.toml into /usr/src/depot
    Sync,
    /// Sync source repos configured in /etc/depot.d/repos.toml into /usr/src/depot
    Update {
        /// Update only one source repo by name
        name: Option<String>,
    },
    /// List configured source and binary repos
    List,
    /// Add or update a repo entry in /etc/depot.d/repos.toml
    Add {
        /// Repo name (e.g. vertex)
        name: String,
        /// Source git URL or binary repo base URL
        url: String,
        /// Repo kind
        #[arg(long, value_enum, default_value_t = RepoKindArg::Source)]
        kind: RepoKindArg,
        /// Optional source repo subdirectory to index (repeatable)
        #[arg(long = "subdir")]
        subdirs: Vec<String>,
        /// Repo priority (lower = higher priority)
        #[arg(long, default_value_t = 0)]
        priority: i32,
        /// Add repo as disabled
        #[arg(long)]
        disabled: bool,
        /// Binary repo architecture table entry to add/update (defaults to this machine's arch)
        #[arg(long)]
        arch: Option<String>,
        /// Binary repo DB filename/path (relative to repo URL)
        #[arg(long = "repo-db", default_value = "repo.db.zst")]
        repo_db: String,
        /// Allow unsigned repo metadata for this binary repo
        #[arg(long)]
        allow_unsigned: bool,
    },
    /// Remove a repo entry from /etc/depot.d/repos.toml
    Remove {
        /// Repo name
        name: String,
        /// Repo kind (auto-detect if unique)
        #[arg(long)]
        kind: Option<RepoKindArg>,
    },
    /// Enable a repo entry in /etc/depot.d/repos.toml
    Enable {
        /// Repo name
        name: String,
        /// Repo kind (auto-detect if unique)
        #[arg(long)]
        kind: Option<RepoKindArg>,
    },
    /// Disable a repo entry in /etc/depot.d/repos.toml
    Disable {
        /// Repo name
        name: String,
        /// Repo kind (auto-detect if unique)
        #[arg(long)]
        kind: Option<RepoKindArg>,
    },
    /// Query binary repo metadata for the package that owns a file path
    Owns {
        /// Path to query (absolute or relative install path)
        path: PathBuf,
    },
    /// Show status of configured git mirrors
    Status,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RepoKindArg {
    Source,
    Binary,
}
