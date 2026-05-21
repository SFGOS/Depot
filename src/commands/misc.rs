use super::*;

pub(crate) mod internal;

pub(super) fn run_info(args: InfoArgs) -> Result<()> {
    let InfoArgs {
        rootfs_args,
        package,
    } = args;
    let rootfs = rootfs_args.rootfs;
    let path = PathBuf::from(&package);
    if path.exists() {
        let config = config::Config::for_rootfs(&rootfs);
        let info_lock = locking::open_lock(&config)?;
        let info_lock_path = locking::lock_path(&config);
        let _info_lock_guard = locking::try_read(&info_lock, &info_lock_path, "info")?;
        let pkg_spec = package::PackageSpec::from_file(&path)?;
        println!("{}", pkg_spec);

        let db_path = config.installed_db_path(&rootfs);
        deps::print_dep_status(&pkg_spec, &db_path)?;
    } else {
        let config = config::Config::for_rootfs(&rootfs);
        let info_lock = locking::open_lock(&config)?;
        let info_lock_path = locking::lock_path(&config);
        let _info_lock_guard = locking::try_read(&info_lock, &info_lock_path, "info")?;
        let db_path = config.installed_db_path(&rootfs);
        db::show_package_info(&db_path, &package)?;
    }

    Ok(())
}

pub(super) fn run_owns(args: OwnsArgs) -> Result<()> {
    let OwnsArgs { rootfs_args, path } = args;
    let rootfs = rootfs_args.rootfs;
    let config = config::Config::for_rootfs(&rootfs);
    let owns_lock = locking::open_lock(&config)?;
    let owns_lock_path = locking::lock_path(&config);
    let _owns_lock_guard = locking::try_read(&owns_lock, &owns_lock_path, "owns")?;
    let db_path = config.installed_db_path(&rootfs);
    match db::owns_path(&db_path, &path)? {
        Some(owner) => ui::info(format!("{} is owned by {}", path.display(), owner)),
        None => ui::warn(format!("No installed package owns {}", path.display())),
    }

    Ok(())
}

pub(super) fn run_list(args: ListArgs) -> Result<()> {
    let ListArgs { rootfs_args } = args;
    let rootfs = rootfs_args.rootfs;
    let config = config::Config::for_rootfs(&rootfs);
    let list_lock = locking::open_lock(&config)?;
    let list_lock_path = locking::lock_path(&config);
    let _list_lock_guard = locking::try_read(&list_lock, &list_lock_path, "list")?;
    let db_path = config.installed_db_path(&rootfs);
    db::list_packages(&db_path)?;
    Ok(())
}

pub(super) fn run_sign(args: SignArgs) -> Result<()> {
    let SignArgs { rootfs_args, files } = args;
    let rootfs = rootfs_args.rootfs;
    let sig_paths = signing::sign_zst_files_detached(&rootfs, &files)?;
    for sig_path in sig_paths {
        ui::success(format!(
            "Created detached signature: {}",
            sig_path.display()
        ));
    }
    Ok(())
}

pub(super) fn run_generate_artifacts(args: crate::cli::GenerateArtifactsArgs) -> Result<()> {
    let out_dir = args.out_dir;
    cli_assets::generate_cli_assets(&out_dir)?;
    ui::success(format!("Generated CLI assets in {}", out_dir.display()));
    Ok(())
}

pub(super) fn run_config(args: ConfigArgs) -> Result<()> {
    let ConfigArgs { rootfs_args } = args;
    let rootfs = rootfs_args.rootfs;
    let config = config::Config::for_rootfs(&rootfs);
    let config_lock = locking::open_lock(&config)?;
    let config_lock_path = locking::lock_path(&config);
    let _config_lock_guard = locking::try_read(&config_lock, &config_lock_path, "config")?;
    println!("Cache Directory: {}", config.cache_dir.display());
    println!(
        "Package Cache Directory: {}",
        config.package_cache_dir.display()
    );
    println!("Build Directory: {}", config.build_dir.display());
    println!("Database Directory: {}", config.db_dir.display());
    println!("Repo Clone Directory: {}", config.repo_clone_dir.display());
    println!("Install Test Deps: {}", config.install_test_deps);
    println!(
        "Configured Repos: {} source, {} binary",
        config.source_repos.len(),
        config.binary_repos.len()
    );
    println!("\nBuild Overrides: {}", config.build_overrides);
    println!("Package Overrides: {}", config.package_overrides);
    if !config.appends.is_empty() {
        println!("\nAppends:");
        for (k, v) in &config.appends {
            println!("  {} = {:?}", k, v);
        }
    }

    Ok(())
}

pub(super) fn run_system(args: SystemArgs) -> Result<()> {
    let SystemArgs {
        rootfs_args,
        command,
    } = args;
    let rootfs = rootfs_args.rootfs;
    let config = config::Config::for_rootfs(&rootfs);
    let mut system_lock = locking::open_lock(&config)?;
    let system_lock_path = locking::lock_path(&config);

    match command {
        crate::cli::SystemCommands::Status => {
            let _guard = locking::try_read(&system_lock, &system_lock_path, "system status")?;
            let state = crate::system_state::load(&config)?;
            print_system_state(&state);
        }
        crate::cli::SystemCommands::Stage { stage } => {
            let _guard = locking::try_write(&mut system_lock, &system_lock_path, "system stage")?;
            let state = crate::system_state::set_stage(&config, stage)?;
            ui::success(format!(
                "Moved system status to stage {}",
                state.stage.as_deref().unwrap_or("unknown")
            ));
        }
        crate::cli::SystemCommands::Layer { command } => {
            let _guard = locking::try_write(&mut system_lock, &system_lock_path, "system layer")?;
            match command {
                crate::cli::SystemLayerCommands::Add { layer, packages } => {
                    let package_count = packages.len();
                    crate::system_state::add_packages_to_layer(&config, layer.clone(), &packages)?;
                    ui::success(format!(
                        "Added {} package(s) to layer {}",
                        package_count, layer
                    ));
                }
                crate::cli::SystemLayerCommands::Remove { layer, packages } => {
                    let package_count = packages.len();
                    crate::system_state::remove_packages_from_layer(
                        &config,
                        layer.clone(),
                        &packages,
                    )?;
                    ui::success(format!(
                        "Removed {} package(s) from layer {}",
                        package_count, layer
                    ));
                }
                crate::cli::SystemLayerCommands::List => {
                    drop(_guard);
                    let _guard =
                        locking::try_read(&system_lock, &system_lock_path, "system layer list")?;
                    let state = crate::system_state::load(&config)?;
                    print_system_layers(&state);
                }
            }
        }
        crate::cli::SystemCommands::InitLbi {
            target,
            arch,
            force,
        } => {
            let _guard =
                locking::try_write(&mut system_lock, &system_lock_path, "system init-lbi")?;
            let state = crate::system_state::init_lbi_layout(
                &rootfs,
                &config,
                &target,
                arch.as_deref(),
                force,
            )?;
            ui::success(format!(
                "Initialized Linux by Intent layout for {} ({})",
                state.target.as_deref().unwrap_or("unknown"),
                state.arch.as_deref().unwrap_or("unknown")
            ));
        }
    }

    Ok(())
}

fn print_system_state(state: &crate::system_state::SystemState) {
    println!(
        "Stage: {}",
        state.stage.as_deref().unwrap_or("uninitialized")
    );
    if let Some(target) = &state.target {
        println!("Target: {target}");
    }
    if let Some(arch) = &state.arch {
        println!("Arch: {arch}");
    }
    print_system_layers(state);
}

fn print_system_layers(state: &crate::system_state::SystemState) {
    if state.layers.is_empty() {
        println!("Layers: none");
        return;
    }

    println!("Layers:");
    for (layer, packages) in &state.layers {
        if packages.is_empty() {
            println!("  {layer}:");
        } else {
            println!("  {}: {}", layer, packages.join(", "));
        }
    }
}

pub(super) fn run_make_spec(args: crate::cli::MakeSpecArgs) -> Result<()> {
    let output = args.output;
    let spec = package::create_interactive()?;
    let toml_string = package::spec_to_minimal_toml(&spec)?;
    let output_path =
        output.unwrap_or_else(|| PathBuf::from(format!("{}.toml", spec.package.name)));

    if output_path.exists() {
        ui::warn(format!("File {} already exists.", output_path.display()));
        if !ui::prompt_yes_no("Overwrite it?", false)? {
            anyhow::bail!("Aborted");
        }
    }

    fs::write(&output_path, toml_string)?;
    ui::success(format!(
        "Package specification saved to {}",
        output_path.display()
    ));
    Ok(())
}

pub(super) fn run_convert(args: ConvertArgs) -> Result<()> {
    let ConvertArgs { input, output } = args;
    let converted = package::convert_starbuild_file(&input, output.as_deref())?;
    let mut outputs = vec![converted.output_path.clone()];
    if let Some(build_script_path) = &converted.build_script_path {
        outputs.push(build_script_path.clone());
    }

    let existing: Vec<_> = outputs
        .iter()
        .filter(|path| path.exists())
        .map(|path| path.display().to_string())
        .collect();
    if !existing.is_empty() {
        ui::warn(format!(
            "Generated files already exist: {}",
            existing.join(", ")
        ));
        if !ui::prompt_yes_no("Overwrite them?", false)? {
            anyhow::bail!("Aborted");
        }
    }

    fs::write(&converted.output_path, converted.toml)
        .with_context(|| format!("Failed to write {}", converted.output_path.display()))?;
    if let (Some(build_script), Some(build_script_path)) =
        (converted.build_script, converted.build_script_path)
    {
        fs::write(&build_script_path, build_script)
            .with_context(|| format!("Failed to write {}", build_script_path.display()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&build_script_path)
                .with_context(|| format!("Failed to stat {}", build_script_path.display()))?
                .permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&build_script_path, perms)
                .with_context(|| format!("Failed to chmod {}", build_script_path.display()))?;
        }
        ui::success(format!(
            "Converted STARBUILD into {} and {}",
            converted.output_path.display(),
            build_script_path.display()
        ));
    } else {
        ui::success(format!(
            "Converted STARBUILD into {}",
            converted.output_path.display()
        ));
    }

    Ok(())
}

pub(super) fn run_internal(args: crate::cli::InternalArgs) -> Result<()> {
    internal::run_internal_command(args.command)
}
