use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Clone, Args, Default)]
pub struct RootfsArgs {
    /// Custom root filesystem path
    #[arg(long, short = 'r', default_value = "/")]
    pub rootfs: PathBuf,
}

#[derive(Debug, Clone, Args, Default)]
pub struct PromptArgs {
    /// Automatically answer yes to prompts and pick the default provider choice
    #[arg(long, short = 'y')]
    pub yes: bool,
}

#[derive(Debug, Clone, Args, Default)]
pub struct BuildExecArgs {
    /// Skip dependency checks
    #[arg(long)]
    pub no_deps: bool,

    /// Do not export CFLAGS/CXXFLAGS/LDFLAGS to build commands
    #[arg(
        long,
        action = clap::ArgAction::Set,
        num_args = 0..=1,
        default_value_t = false,
        default_missing_value = "true"
    )]
    pub no_flags: bool,

    /// Cross-compilation prefix (e.g., x86_64-linux-musl, aarch64-linux-gnu)
    #[arg(long)]
    pub cross_prefix: Option<String>,

    /// Clean build workspace and source cache after successful install/build
    #[arg(long)]
    pub clean: bool,

    /// Show what would happen without performing builds/installs
    #[arg(long)]
    pub dry_run: bool,

    /// Install test dependencies alongside build/runtime dependencies
    #[arg(long)]
    pub test_deps: bool,
}

#[derive(Debug, Clone, Args, Default)]
pub struct Lib32Args {
    /// Build/install only the lib32 companion package path (skip primary package output)
    #[arg(long)]
    pub lib32_only: bool,
}

#[derive(Parser)]
#[command(name = "Depot")]
#[command(about = "Source-based package manager for Linux", long_about = None)]
#[command(version)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Debug, Clone, Args)]
pub struct InstallArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,

    #[command(flatten)]
    pub prompt_args: PromptArgs,

    #[command(flatten)]
    pub build_exec_args: BuildExecArgs,

    #[command(flatten)]
    pub lib32_args: Lib32Args,

    /// One or more package names, spec paths (.toml), or package archives (.tar.zst)
    #[arg(
        value_name = "SPEC_OR_ARCHIVE",
        num_args = 1..,
        required_unless_present = "spec"
    )]
    pub spec_or_archive: Vec<PathBuf>,

    /// Explicitly specify path to package spec (.toml file)
    #[arg(
        short,
        long = "spec",
        visible_alias = "package",
        alias = "p",
        conflicts_with = "spec_or_archive"
    )]
    pub spec: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RemoveArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,

    #[command(flatten)]
    pub prompt_args: PromptArgs,

    /// Package name to remove
    pub package: String,
}

#[derive(Debug, Clone, Args)]
pub struct BuildArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,

    #[command(flatten)]
    pub prompt_args: PromptArgs,

    #[command(flatten)]
    pub build_exec_args: BuildExecArgs,

    #[command(flatten)]
    pub lib32_args: Lib32Args,

    /// Path to package spec (.toml file)
    #[arg(value_name = "SPEC")]
    pub spec_pos: Option<PathBuf>,

    /// Explicitly specify path to package spec (.toml file)
    #[arg(short, long = "spec", visible_alias = "package", alias = "p")]
    pub spec: Option<PathBuf>,

    /// Install package to rootfs after creating package archive(s)
    #[arg(long)]
    pub install: bool,

    /// Automatically install missing dependencies before building
    #[arg(long)]
    pub install_deps: bool,

    /// Remove dependencies auto-installed for this build after the command finishes
    #[arg(long)]
    pub cleanup_deps: bool,
}

#[derive(Debug, Clone, Args)]
pub struct UpdateArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,

    #[command(flatten)]
    pub prompt_args: PromptArgs,

    #[command(flatten)]
    pub build_exec_args: BuildExecArgs,

    /// Optional package names to update (defaults to all installed packages with upgrades)
    #[arg(value_name = "PACKAGE", num_args = 0..)]
    pub packages: Vec<String>,
}

#[derive(Debug, Clone, Args)]
pub struct CheckArgs {
    /// Directory to scan recursively for package specs
    #[arg(default_value = ".")]
    pub dir: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct InfoArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,

    /// Path to package spec or installed package name
    pub package: String,
}

#[derive(Debug, Clone, Args)]
pub struct SearchArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,

    /// Search query
    pub query: String,

    /// Search repository file lists (binary repo metadata) by path substring
    #[arg(long)]
    pub files: bool,
}

#[derive(Debug, Clone, Args)]
pub struct OwnsArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,

    /// Path to query (absolute or relative to rootfs)
    pub path: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct ListArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,
}

#[derive(Debug, Clone, Args)]
pub struct SignArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,

    /// One or more .zst files to sign
    #[arg(value_name = "FILE", required = true, num_args = 1..)]
    pub files: Vec<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct RepoArgs {
    #[command(subcommand)]
    pub command: RepoCommands,
}

#[derive(Debug, Clone, Args)]
pub struct ConfigArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,
}

#[derive(Debug, Clone, Args)]
pub struct GenerateArtifactsArgs {
    /// Output directory for generated files
    #[arg(long, value_name = "DIR")]
    pub out_dir: PathBuf,
}

#[derive(Debug, Clone, Args)]
pub struct MakeSpecArgs {
    /// Output file path (defaults to <name>.toml)
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct ConvertArgs {
    /// Path to the legacy STARBUILD file
    #[arg(default_value = "STARBUILD")]
    pub input: PathBuf,

    /// Output TOML file path (defaults to <mainpkgname>.toml beside the STARBUILD)
    #[arg(short, long)]
    pub output: Option<PathBuf>,
}

#[derive(Debug, Clone, Args)]
pub struct InternalArgs {
    #[command(subcommand)]
    pub command: InternalCommands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Build and install a package from a spec file
    Install(InstallArgs),
    /// Remove an installed package
    Remove(RemoveArgs),
    /// Build a package without installing
    Build(BuildArgs),
    /// Update installed packages from configured repositories
    Update(UpdateArgs),
    /// Scan package specs for upstream version updates
    Check(CheckArgs),
    /// Show information about a package
    Info(InfoArgs),
    /// Search configured source and binary repos by package name or provides
    Search(SearchArgs),
    /// Show which installed package owns a filesystem path
    Owns(OwnsArgs),
    /// List installed packages
    List(ListArgs),
    /// Create a detached minisign signature for a .zst file
    Sign(SignArgs),
    /// Repository management
    Repo(RepoArgs),
    /// Show current configuration
    Config(ConfigArgs),
    /// Generate shell completion scripts and a man page into an output directory.
    #[command(hide = true)]
    GenerateArtifacts(GenerateArtifactsArgs),
    /// Create a new package specification interactively
    MakeSpec(MakeSpecArgs),
    /// Convert a legacy STARBUILD into a Depot package spec
    Convert(ConvertArgs),
    #[command(hide = true)]
    Internal(InternalArgs),
}

#[derive(Subcommand, Debug, Clone)]
pub enum InternalCommands {
    #[command(hide = true)]
    PythonBuild {
        #[arg(long, default_value = ".")]
        src_dir: PathBuf,
        #[arg(long, default_value = "dist")]
        dist_dir: PathBuf,
        #[arg(long = "config-setting")]
        config_settings: Vec<String>,
    },
    #[command(hide = true)]
    PythonInstall {
        #[arg(long, default_value = "dist")]
        dist_dir: PathBuf,
        #[arg(long = "wheel", value_name = "FILE")]
        wheels: Vec<PathBuf>,
        #[arg(long, default_value = "/usr")]
        prefix: String,
    },
    #[command(hide = true)]
    Clone { repo: String, dest: Option<PathBuf> },
    #[command(hide = true)]
    AutotoolsConfigure {
        #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    AutotoolsInstall {
        #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    CmakeConfigure {
        #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    CmakeInstall {
        #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    MesonConfigure {
        #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    MesonInstall {
        #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    PerlConfigure {
        #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
    #[command(hide = true)]
    PerlInstall {
        #[arg(value_name = "ARG", num_args = 0.., allow_hyphen_values = true)]
        args: Vec<String>,
    },
}

#[derive(Debug, Clone, Args)]
pub struct RepoCommonArgs {
    #[command(flatten)]
    pub rootfs_args: RootfsArgs,
}

#[derive(Subcommand, Debug, Clone)]
pub enum RepoCommands {
    /// Create a repository database from a directory of packages
    Create {
        #[command(flatten)]
        args: RepoCommonArgs,
        /// Directory containing .depot.pkg.tar.zst files
        #[arg(default_value = ".")]
        dir: PathBuf,
    },
    /// Sync git mirrors configured in /etc/depot.d/mirrors.toml into /usr/src/depot
    Sync {
        #[command(flatten)]
        args: RepoCommonArgs,
    },
    /// Sync source repos configured in /etc/depot.d/repos.toml into /usr/src/depot
    Update {
        #[command(flatten)]
        args: RepoCommonArgs,
        /// Update only one source repo by name
        name: Option<String>,
    },
    /// Create/update a source index at the root of a source repo
    Index {
        #[command(flatten)]
        args: RepoCommonArgs,
        /// Source repository root directory
        #[arg(default_value = ".")]
        dir: PathBuf,
        /// Optional subdirectory to scan (repeatable, e.g. --subdir core --subdir extra)
        #[arg(long = "subdir")]
        subdirs: Vec<String>,
    },
    /// List configured source and binary repos
    List {
        #[command(flatten)]
        args: RepoCommonArgs,
    },
    /// Add or update a repo entry in /etc/depot.d/repos.toml
    Add {
        #[command(flatten)]
        args: RepoCommonArgs,
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
        #[command(flatten)]
        args: RepoCommonArgs,
        /// Repo name
        name: String,
        /// Repo kind (auto-detect if unique)
        #[arg(long)]
        kind: Option<RepoKindArg>,
    },
    /// Enable a repo entry in /etc/depot.d/repos.toml
    Enable {
        #[command(flatten)]
        args: RepoCommonArgs,
        /// Repo name
        name: String,
        /// Repo kind (auto-detect if unique)
        #[arg(long)]
        kind: Option<RepoKindArg>,
    },
    /// Disable a repo entry in /etc/depot.d/repos.toml
    Disable {
        #[command(flatten)]
        args: RepoCommonArgs,
        /// Repo name
        name: String,
        /// Repo kind (auto-detect if unique)
        #[arg(long)]
        kind: Option<RepoKindArg>,
    },
    /// Query binary repo metadata for the package that owns a file path
    Owns {
        #[command(flatten)]
        args: RepoCommonArgs,
        /// Path to query (absolute or relative install path)
        path: PathBuf,
    },
    /// Show status of configured git mirrors
    Status {
        #[command(flatten)]
        args: RepoCommonArgs,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
pub enum RepoKindArg {
    Source,
    Binary,
}

#[cfg(test)]
mod tests {
    use super::{
        BuildArgs, Cli, Commands, ConvertArgs, InstallArgs, RepoArgs, RepoCommands, SearchArgs,
        UpdateArgs,
    };
    use clap::{CommandFactory, Parser};
    use std::path::PathBuf;

    #[test]
    fn install_accepts_multiple_positional_targets() {
        let cli = Cli::try_parse_from(["depot", "install", "base", "linux"]).unwrap();
        match cli.command {
            Commands::Install(InstallArgs {
                spec_or_archive,
                spec,
                ..
            }) => {
                assert!(spec.is_none());
                assert_eq!(
                    spec_or_archive,
                    vec![PathBuf::from("base"), PathBuf::from("linux")]
                );
            }
            _ => panic!("expected install command"),
        }
    }

    #[test]
    fn install_accepts_spec_flag_without_positional_target() {
        let cli = Cli::try_parse_from(["depot", "install", "--spec", "pkg.toml"]).unwrap();
        match cli.command {
            Commands::Install(InstallArgs {
                spec_or_archive,
                spec,
                ..
            }) => {
                assert!(spec_or_archive.is_empty());
                assert_eq!(spec, Some(PathBuf::from("pkg.toml")));
            }
            _ => panic!("expected install command"),
        }
    }

    #[test]
    fn update_accepts_no_package_names() {
        let cli = Cli::try_parse_from(["depot", "update"]).unwrap();
        match cli.command {
            Commands::Update(UpdateArgs { packages, .. }) => assert!(packages.is_empty()),
            _ => panic!("expected update command"),
        }
    }

    #[test]
    fn update_accepts_multiple_package_names() {
        let cli = Cli::try_parse_from(["depot", "update", "linux", "openssl"]).unwrap();
        match cli.command {
            Commands::Update(UpdateArgs { packages, .. }) => {
                assert_eq!(packages, vec!["linux".to_string(), "openssl".to_string()])
            }
            _ => panic!("expected update command"),
        }
    }

    #[test]
    fn check_accepts_custom_directory() {
        let cli = Cli::try_parse_from(["depot", "check", "packages"]).unwrap();
        match cli.command {
            Commands::Check(args) => assert_eq!(args.dir, PathBuf::from("packages")),
            _ => panic!("expected check command"),
        }
    }

    #[test]
    fn install_test_deps_flag_is_parsed_for_install() {
        let cli = Cli::try_parse_from(["depot", "install", "--test-deps", "foo"]).unwrap();
        match cli.command {
            Commands::Install(args) => assert!(args.build_exec_args.test_deps),
            _ => panic!("expected install command"),
        }
    }

    #[test]
    fn build_install_deps_flag_is_parsed() {
        let cli = Cli::try_parse_from(["depot", "build", "--install-deps", "pkg.toml"]).unwrap();
        match cli.command {
            Commands::Build(BuildArgs { install_deps, .. }) => assert!(install_deps),
            _ => panic!("expected build command"),
        }
    }

    #[test]
    fn convert_accepts_default_input() {
        let cli = Cli::try_parse_from(["depot", "convert"]).unwrap();
        match cli.command {
            Commands::Convert(ConvertArgs { input, output }) => {
                assert_eq!(input, PathBuf::from("STARBUILD"));
                assert!(output.is_none());
            }
            _ => panic!("expected convert command"),
        }
    }

    #[test]
    fn convert_accepts_custom_output() {
        let cli = Cli::try_parse_from(["depot", "convert", "legacy/STARBUILD", "-o", "pkg.toml"])
            .unwrap();
        match cli.command {
            Commands::Convert(ConvertArgs { input, output }) => {
                assert_eq!(input, PathBuf::from("legacy/STARBUILD"));
                assert_eq!(output, Some(PathBuf::from("pkg.toml")));
            }
            _ => panic!("expected convert command"),
        }
    }

    #[test]
    fn build_cleanup_deps_flag_is_parsed() {
        let cli = Cli::try_parse_from(["depot", "build", "--cleanup-deps", "pkg.toml"]).unwrap();
        match cli.command {
            Commands::Build(BuildArgs { cleanup_deps, .. }) => assert!(cleanup_deps),
            _ => panic!("expected build command"),
        }
    }

    #[test]
    fn repo_help_does_not_show_build_only_flags() {
        let mut cmd = Cli::command();
        let help = cmd
            .find_subcommand_mut("repo")
            .expect("repo subcommand")
            .render_help()
            .to_string();

        assert!(!help.contains("--no-deps"));
        assert!(!help.contains("--no-flags"));
        assert!(!help.contains("--clean"));
        assert!(!help.contains("--test-deps"));
        assert!(!help.contains("--lib32-only"));
    }

    #[test]
    fn repo_create_help_only_shows_repo_options() {
        let mut cmd = Cli::command();
        let repo = cmd.find_subcommand_mut("repo").expect("repo subcommand");
        let help = repo
            .find_subcommand_mut("create")
            .expect("repo create subcommand")
            .render_help()
            .to_string();

        assert!(help.contains("--rootfs"));
        assert!(!help.contains("--no-deps"));
        assert!(!help.contains("--test-deps"));
    }

    #[test]
    fn search_help_shows_only_search_options() {
        let mut cmd = Cli::command();
        let help = cmd
            .find_subcommand_mut("search")
            .expect("search subcommand")
            .render_help()
            .to_string();

        assert!(help.contains("--rootfs"));
        assert!(help.contains("--files"));
        assert!(!help.contains("--no-deps"));
    }

    #[test]
    fn repo_command_parses_nested_subcommand() {
        let cli = Cli::try_parse_from(["depot", "repo", "status"]).unwrap();
        match cli.command {
            Commands::Repo(RepoArgs {
                command: RepoCommands::Status { .. },
            }) => {}
            _ => panic!("expected repo status command"),
        }
    }

    #[test]
    fn search_parses_rootfs_locally() {
        let cli = Cli::try_parse_from(["depot", "search", "-r", "/tmp/root", "llvm"]).unwrap();
        match cli.command {
            Commands::Search(SearchArgs {
                rootfs_args, query, ..
            }) => {
                assert_eq!(rootfs_args.rootfs, PathBuf::from("/tmp/root"));
                assert_eq!(query, "llvm");
            }
            _ => panic!("expected search command"),
        }
    }
}
