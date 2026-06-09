use super::loading::PackageSpec;
use super::model::*;
use anyhow::{Context, Result};

impl PackageSpec {
    /// Apply system configuration overrides and appends
    pub fn apply_config(&mut self, config: &crate::config::Config) {
        // Apply build overrides from /etc/depot.d/build.toml
        self.apply_toml_overrides(&config.build_overrides, "build");

        // Apply appends from /etc/depot.d/build.toml (e.g. build.flags.cflags += ["-O3"])
        for (key, values) in &config.appends {
            let key = normalize_append_key(key);
            if let Some(subkey) = key.strip_prefix("build.flags.") {
                self.apply_append(subkey, values);
            } else if let Some(subkey) = key.strip_prefix("build.") {
                self.apply_append(subkey, values);
            }
        }
    }

    fn apply_toml_overrides(&mut self, overrides: &toml::Value, _prefix: &str) {
        // Support both [build.flags] and top-level [build] fields
        if let Some(table) = overrides.as_table() {
            self.apply_flags_table(table);
        }
        if let Some(table) = overrides.get("flags").and_then(|f| f.as_table()) {
            self.apply_flags_table(table);
        }
    }

    fn apply_default_string(target: &mut String, default: &str, value: &toml::Value) {
        if let Some(s) = value.as_str()
            && (target.trim().is_empty() || target == default)
        {
            *target = s.to_string();
        }
    }

    fn apply_default_bool(target: &mut bool, default: bool, value: &toml::Value) {
        if *target == default
            && let Some(value) = toml_value_as_boolish(value)
        {
            *target = value;
        }
    }

    fn apply_flags_table(&mut self, table: &toml::map::Map<String, toml::Value>) {
        let default_flags = BuildFlags::default();
        for (k, v) in table {
            // match case-insensitively for common keys (allow CXX/Cc etc.)
            match k.to_lowercase().as_str() {
                "cflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.cflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cflags = vec![s.to_string()];
                    }
                }
                "replace_cflags" | "replace-cflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_cflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cflags = vec![s.to_string()];
                    }
                }
                "cflags-lib32" | "cflags_lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.cflags_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cflags_lib32 = vec![s.to_string()];
                    }
                }
                "replace_cflags-lib32" | "replace_cflags_lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_cflags_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cflags_lib32 = vec![s.to_string()];
                    }
                }
                "cxxflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.cxxflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cxxflags = vec![s.to_string()];
                    }
                }
                "replace_cxxflags" | "replace-cxxflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_cxxflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cxxflags = vec![s.to_string()];
                    }
                }
                "cxxflags-lib32" | "cxxflags_lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.cxxflags_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cxxflags_lib32 = vec![s.to_string()];
                    }
                }
                "replace_cxxflags-lib32" | "replace_cxxflags_lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_cxxflags_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cxxflags_lib32 = vec![s.to_string()];
                    }
                }
                "ldflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.ldflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.ldflags = vec![s.to_string()];
                    }
                }
                "fuse_ld" | "fuse-ld" => {
                    Self::apply_default_string(
                        &mut self.build.flags.fuse_ld,
                        &default_flags.fuse_ld,
                        v,
                    );
                }
                "tool_dir" | "tool-dir" | "tools_dir" | "tools-dir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.tool_dir,
                        &default_flags.tool_dir,
                        v,
                    );
                }
                "replace_ldflags" | "replace-ldflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_ldflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_ldflags = vec![s.to_string()];
                    }
                }
                "ltoflags" | "lto_flags" | "lto-flags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.ltoflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.ltoflags = vec![s.to_string()];
                    }
                }
                "rustltoflags" | "rust_ltoflags" | "rust-ltoflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.rustltoflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.rustltoflags = vec![s.to_string()];
                    }
                }
                "replace_ltoflags" | "replace_lto-flags" | "replace_lto_flags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_ltoflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_ltoflags = vec![s.to_string()];
                    }
                }
                "rustflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.rustflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.rustflags = vec![s.to_string()];
                    }
                }
                "replace_rustflags" | "replace-rustflags" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.replace_rustflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_rustflags = vec![s.to_string()];
                    }
                }
                "keep" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.keep = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.keep = vec![s.to_string()];
                    }
                }
                "split_docs" | "split-docs" => {
                    if let Some(b) = toml_value_as_boolish(v) {
                        self.build.flags.split_docs = b;
                    }
                }
                "doc_dirs" | "doc-dirs" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.doc_dirs = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.doc_dirs = vec![s.to_string()];
                    }
                }
                "cc" => {
                    Self::apply_default_string(&mut self.build.flags.cc, &default_flags.cc, v);
                }
                "cxx" => {
                    Self::apply_default_string(&mut self.build.flags.cxx, &default_flags.cxx, v);
                }
                "ar" => {
                    Self::apply_default_string(&mut self.build.flags.ar, &default_flags.ar, v);
                }
                "ranlib" => {
                    Self::apply_default_string(
                        &mut self.build.flags.ranlib,
                        &default_flags.ranlib,
                        v,
                    );
                }
                "strip" => {
                    Self::apply_default_string(
                        &mut self.build.flags.strip,
                        &default_flags.strip,
                        v,
                    );
                }
                "ld" => {
                    Self::apply_default_string(&mut self.build.flags.ld, &default_flags.ld, v);
                }
                "nm" => {
                    Self::apply_default_string(&mut self.build.flags.nm, &default_flags.nm, v);
                }
                "objcopy" => {
                    Self::apply_default_string(
                        &mut self.build.flags.objcopy,
                        &default_flags.objcopy,
                        v,
                    );
                }
                "objdump" => {
                    Self::apply_default_string(
                        &mut self.build.flags.objdump,
                        &default_flags.objdump,
                        v,
                    );
                }
                "readelf" => {
                    Self::apply_default_string(
                        &mut self.build.flags.readelf,
                        &default_flags.readelf,
                        v,
                    );
                }
                "cpp" => {
                    Self::apply_default_string(&mut self.build.flags.cpp, &default_flags.cpp, v);
                }
                "prefix" => {
                    Self::apply_default_string(
                        &mut self.build.flags.prefix,
                        &default_flags.prefix,
                        v,
                    );
                }
                "bindir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.bindir,
                        &default_flags.bindir,
                        v,
                    );
                }
                "sbindir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.sbindir,
                        &default_flags.sbindir,
                        v,
                    );
                }
                "libdir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.libdir,
                        &default_flags.libdir,
                        v,
                    );
                }
                "libexecdir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.libexecdir,
                        &default_flags.libexecdir,
                        v,
                    );
                }
                "sysconfdir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.sysconfdir,
                        &default_flags.sysconfdir,
                        v,
                    );
                }
                "localstatedir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.localstatedir,
                        &default_flags.localstatedir,
                        v,
                    );
                }
                "sharedstatedir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.sharedstatedir,
                        &default_flags.sharedstatedir,
                        v,
                    );
                }
                "includedir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.includedir,
                        &default_flags.includedir,
                        v,
                    );
                }
                "datarootdir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.datarootdir,
                        &default_flags.datarootdir,
                        v,
                    );
                }
                "datadir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.datadir,
                        &default_flags.datadir,
                        v,
                    );
                }
                "mandir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.mandir,
                        &default_flags.mandir,
                        v,
                    );
                }
                "infodir" => {
                    Self::apply_default_string(
                        &mut self.build.flags.infodir,
                        &default_flags.infodir,
                        v,
                    );
                }
                "chost" => {
                    Self::apply_default_string(
                        &mut self.build.flags.chost,
                        &default_flags.chost,
                        v,
                    );
                }
                "cbuild" => {
                    Self::apply_default_string(
                        &mut self.build.flags.cbuild,
                        &default_flags.cbuild,
                        v,
                    );
                }
                "carch" => {
                    Self::apply_default_string(
                        &mut self.build.flags.carch,
                        &default_flags.carch,
                        v,
                    );
                }
                "makeflags" | "make_flags" | "make-flags" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.makeflags = s.to_string();
                    } else if let Some(arr) = v.as_array() {
                        self.build.flags.makeflags = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(str::trim)
                            .filter(|x| !x.is_empty())
                            .collect::<Vec<_>>()
                            .join(" ");
                    }
                }
                "make_vars" | "make-vars" | "make_build_vars" | "make-build-vars" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_vars = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_vars =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_exec" | "make-exec" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.make_exec = s.to_string();
                    }
                }
                "make_target" | "make-target" | "make_build_target" | "make-build-target" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.make_target = s.to_string();
                    }
                }
                "make_targets" | "make-targets" | "make_build_targets" | "make-build-targets" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_targets = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_targets =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_dirs" | "make-dirs" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_dirs = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_dirs =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_test_vars" | "make-test-vars" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_test_vars = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_test_vars =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_test_target" | "make-test-target" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.make_test_target = s.to_string();
                    }
                }
                "make_test_targets" | "make-test-targets" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_test_targets = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_test_targets =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_test_dirs" | "make-test-dirs" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_test_dirs = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_test_dirs =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_install_vars" | "make-install-vars" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_install_vars = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_install_vars =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_install_target" | "make-install-target" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.make_install_target = s.to_string();
                    }
                }
                "make_install_targets" | "make-install-targets" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_install_targets = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_install_targets =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "make_install_dirs" | "make-install-dirs" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.make_install_dirs = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.make_install_dirs =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "passthrough_env" | "passthrough-env" | "pass_env" | "pass-env" | "export_env"
                | "export-env" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.passthrough_env = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.passthrough_env =
                            s.split_whitespace().map(String::from).collect();
                    }
                }
                "env_vars" | "env-vars" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.env_vars = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.env_vars = vec![s.to_string()];
                    }
                }
                "no_flags" | "no-flags" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.no_flags = b;
                    }
                }
                "use_lto" | "use-lto" => {
                    Self::apply_default_bool(
                        &mut self.build.flags.use_lto,
                        default_flags.use_lto,
                        v,
                    );
                }
                "no_strip" | "no-strip" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.no_strip = b;
                    }
                }
                "no_delete_static" | "no-delete-static" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.no_delete_static = b;
                    }
                }
                "no_compress_man"
                | "no-compress-man"
                | "no_compress_manpages"
                | "no-compress-manpages" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.no_compress_man = b;
                    }
                }
                "skip_tests" | "skip-tests" => {
                    if let Some(b) = v.as_bool() {
                        self.build.flags.skip_tests = b;
                    }
                }
                "build_32" | "build-32" => {
                    if let Some(b) = toml_value_as_boolish(v) {
                        self.build.flags.build_32 = b;
                    }
                }
                "lib32_only" | "lib32-only" => {
                    if let Some(b) = toml_value_as_boolish(v) {
                        self.build.flags.lib32_only = b;
                    }
                }
                "host_build" | "host-build" => {
                    if let Some(b) = toml_value_as_boolish(v) {
                        self.build.flags.host_build = b;
                    }
                }
                "configure_lib32" | "configure-lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.configure_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.configure_lib32 = vec![s.to_string()];
                    }
                }
                "config_setting" | "config_settings" | "config-setting" | "config-settings" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.config_settings = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.config_settings = vec![s.to_string()];
                    }
                }
                "configure_file" | "configure-file" => {
                    if let Some(s) = v.as_str() {
                        self.build.flags.configure_file = s.to_string();
                    }
                }
                "post_configure" | "post-configure" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_configure = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_configure = vec![s.to_string()];
                    }
                }
                "post_configure_lib32" | "post_configure-lib32" | "post-configure-lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_configure_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_configure_lib32 = vec![s.to_string()];
                    }
                }
                "post_compile" | "post-compile" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_compile = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_compile = vec![s.to_string()];
                    }
                }
                "post_compile_lib32" | "post_compile-lib32" | "post-compile-lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_compile_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_compile_lib32 = vec![s.to_string()];
                    }
                }
                "post_install" | "post-install" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_install = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_install = vec![s.to_string()];
                    }
                }
                "post_install_lib32" | "post_install-lib32" | "post-install-lib32" => {
                    if let Some(arr) = v.as_array() {
                        self.build.flags.post_install_lib32 = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(String::from)
                            .collect();
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_install_lib32 = vec![s.to_string()];
                    }
                }
                // Add more fields as needed
                _ => {}
            }
        }
    }

    pub(super) fn apply_append(&mut self, key: &str, values: &[toml::Value]) {
        let key = normalize_append_key(key);
        match key.as_str() {
            "cflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cflags.push(s.to_string());
                    }
                }
            }
            "replace_cflags" | "replace-cflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_cflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cflags.push(s.to_string());
                    }
                }
            }
            "cflags-lib32" | "cflags_lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cflags_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cflags_lib32.push(s.to_string());
                    }
                }
            }
            "replace_cflags-lib32" | "replace_cflags_lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_cflags_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cflags_lib32.push(s.to_string());
                    }
                }
            }
            "cxxflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cxxflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cxxflags.push(s.to_string());
                    }
                }
            }
            "replace_cxxflags" | "replace-cxxflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_cxxflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cxxflags.push(s.to_string());
                    }
                }
            }
            "cxxflags-lib32" | "cxxflags_lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cxxflags_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cxxflags_lib32.push(s.to_string());
                    }
                }
            }
            "replace_cxxflags-lib32" | "replace_cxxflags_lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_cxxflags_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_cxxflags_lib32.push(s.to_string());
                    }
                }
            }
            "ldflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .ldflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.ldflags.push(s.to_string());
                    }
                }
            }
            "replace_ldflags" | "replace-ldflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_ldflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_ldflags.push(s.to_string());
                    }
                }
            }
            "ltoflags" | "lto_flags" | "lto-flags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .ltoflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.ltoflags.push(s.to_string());
                    }
                }
            }
            "rustltoflags" | "rust_ltoflags" | "rust-ltoflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .rustltoflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.rustltoflags.push(s.to_string());
                    }
                }
            }
            "replace_ltoflags" | "replace_lto-flags" | "replace_lto_flags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_ltoflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_ltoflags.push(s.to_string());
                    }
                }
            }
            "keep" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .keep
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.keep.push(s.to_string());
                    }
                }
            }
            "doc_dirs" | "doc-dirs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .doc_dirs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.doc_dirs.push(s.to_string());
                    }
                }
            }
            "configure" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .configure
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.configure.push(s.to_string());
                    }
                }
            }
            key if let Some(arch) = configure_arch_append_key(key) => {
                let args = self
                    .build
                    .flags
                    .configure_arch
                    .entry(arch.to_string())
                    .or_default();
                append_string_values(args, values);
            }
            "configure_lib32" | "configure-lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .configure_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.configure_lib32.push(s.to_string());
                    }
                }
            }
            "config_setting" | "config_settings" | "config-setting" | "config-settings" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .config_settings
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.config_settings.push(s.to_string());
                    }
                }
            }
            "configure_file" | "configure-file" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.configure_file = s.to_string();
                }
            }
            "post_configure" | "post-configure" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_configure
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_configure.push(s.to_string());
                    }
                }
            }
            "post_configure_lib32" | "post_configure-lib32" | "post-configure-lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_configure_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_configure_lib32.push(s.to_string());
                    }
                }
            }
            "post_compile" | "post-compile" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_compile
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_compile.push(s.to_string());
                    }
                }
            }
            "post_compile_lib32" | "post_compile-lib32" | "post-compile-lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_compile_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_compile_lib32.push(s.to_string());
                    }
                }
            }
            "post_install" | "post-install" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_install
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_install.push(s.to_string());
                    }
                }
            }
            "post_install_lib32" | "post_install-lib32" | "post-install-lib32" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .post_install_lib32
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.post_install_lib32.push(s.to_string());
                    }
                }
            }
            "cargs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .cargs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.cargs.push(s.to_string());
                    }
                }
            }
            "rustflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .rustflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.rustflags.push(s.to_string());
                    }
                }
            }
            "replace_rustflags" | "replace-rustflags" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .replace_rustflags
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.replace_rustflags.push(s.to_string());
                    }
                }
            }
            "cc" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.cc = s.to_string();
                }
            }
            "cxx" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.cxx = s.to_string();
                }
            }
            "ar" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.ar = s.to_string();
                }
            }
            "ranlib" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.ranlib = s.to_string();
                }
            }
            "strip" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.strip = s.to_string();
                }
            }
            "ld" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.ld = s.to_string();
                }
            }
            "nm" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.nm = s.to_string();
                }
            }
            "objcopy" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.objcopy = s.to_string();
                }
            }
            "objdump" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.objdump = s.to_string();
                }
            }
            "readelf" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.readelf = s.to_string();
                }
            }
            "cpp" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.cpp = s.to_string();
                }
            }
            "prefix" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.prefix = s.to_string();
                }
            }
            "bindir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.bindir = s.to_string();
                }
            }
            "sbindir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.sbindir = s.to_string();
                }
            }
            "libdir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.libdir = s.to_string();
                }
            }
            "libexecdir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.libexecdir = s.to_string();
                }
            }
            "sysconfdir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.sysconfdir = s.to_string();
                }
            }
            "localstatedir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.localstatedir = s.to_string();
                }
            }
            "sharedstatedir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.sharedstatedir = s.to_string();
                }
            }
            "includedir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.includedir = s.to_string();
                }
            }
            "datarootdir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.datarootdir = s.to_string();
                }
            }
            "datadir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.datadir = s.to_string();
                }
            }
            "mandir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.mandir = s.to_string();
                }
            }
            "infodir" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.infodir = s.to_string();
                }
            }
            "chost" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.chost = s.to_string();
                }
            }
            "cbuild" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.cbuild = s.to_string();
                }
            }
            "carch" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.carch = s.to_string();
                }
            }
            "makeflags" | "make_flags" | "make-flags" | "MAKEFLAGS" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        let joined = arr
                            .iter()
                            .filter_map(|x| x.as_str())
                            .map(str::trim)
                            .filter(|x| !x.is_empty())
                            .collect::<Vec<_>>()
                            .join(" ");
                        append_whitespace_separated(&mut self.build.flags.makeflags, &joined);
                    } else if let Some(s) = v.as_str() {
                        append_whitespace_separated(&mut self.build.flags.makeflags, s);
                    }
                }
            }
            "make_vars" | "make-vars" | "make_build_vars" | "make-build-vars" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_vars
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_vars
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_exec" | "make-exec" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.make_exec = s.to_string();
                }
            }
            "make_target" | "make-target" | "make_build_target" | "make-build-target" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.make_target = s.to_string();
                }
            }
            "make_targets" | "make-targets" | "make_build_targets" | "make-build-targets" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_targets
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_targets
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_dirs" | "make-dirs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_dirs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_dirs
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_test_vars" | "make-test-vars" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_test_vars
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_test_vars
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_test_target" | "make-test-target" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.make_test_target = s.to_string();
                }
            }
            "make_test_targets" | "make-test-targets" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_test_targets
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_test_targets
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_test_dirs" | "make-test-dirs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_test_dirs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_test_dirs
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_install_vars" | "make-install-vars" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_install_vars
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_install_vars
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_install_target" | "make-install-target" => {
                if let Some(s) = values.last().and_then(|v| v.as_str()) {
                    self.build.flags.make_install_target = s.to_string();
                }
            }
            "make_install_targets" | "make-install-targets" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_install_targets
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_install_targets
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "make_install_dirs" | "make-install-dirs" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .make_install_dirs
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .make_install_dirs
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "passthrough_env" | "passthrough-env" | "pass_env" | "pass-env" | "export_env"
            | "export-env" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .passthrough_env
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build
                            .flags
                            .passthrough_env
                            .extend(s.split_whitespace().map(String::from));
                    }
                }
            }
            "env_vars" | "env-vars" => {
                for v in values {
                    if let Some(arr) = v.as_array() {
                        self.build
                            .flags
                            .env_vars
                            .extend(arr.iter().filter_map(|x| x.as_str()).map(String::from));
                    } else if let Some(s) = v.as_str() {
                        self.build.flags.env_vars.push(s.to_string());
                    }
                }
            }
            "no_flags" | "no-flags" => {
                if let Some(b) = values.last().and_then(|v| v.as_bool()) {
                    self.build.flags.no_flags = b;
                }
            }
            "use_lto" | "use-lto" => {
                if let Some(b) = values.last().and_then(toml_value_as_boolish) {
                    self.build.flags.use_lto = b;
                }
            }
            "no_strip" | "no-strip" => {
                if let Some(b) = values.last().and_then(|v| v.as_bool()) {
                    self.build.flags.no_strip = b;
                }
            }
            "no_delete_static" | "no-delete-static" => {
                if let Some(b) = values.last().and_then(|v| v.as_bool()) {
                    self.build.flags.no_delete_static = b;
                }
            }
            "no_compress_man"
            | "no-compress-man"
            | "no_compress_manpages"
            | "no-compress-manpages" => {
                if let Some(b) = values.last().and_then(|v| v.as_bool()) {
                    self.build.flags.no_compress_man = b;
                }
            }
            "skip_tests" | "skip-tests" => {
                if let Some(b) = values.last().and_then(toml_value_as_boolish) {
                    self.build.flags.skip_tests = b;
                }
            }
            "build_32" | "build-32" => {
                if let Some(b) = values.last().and_then(toml_value_as_boolish) {
                    self.build.flags.build_32 = b;
                }
            }
            "lib32_only" | "lib32-only" => {
                if let Some(b) = values.last().and_then(toml_value_as_boolish) {
                    self.build.flags.lib32_only = b;
                }
            }
            "split_docs" | "split-docs" => {
                if let Some(b) = values.last().and_then(toml_value_as_boolish) {
                    self.build.flags.split_docs = b;
                }
            }
            _ => {}
        }
    }
}

pub(super) fn preprocess_spec_toml_appends(
    input: &str,
) -> Result<(String, std::collections::HashMap<String, Vec<toml::Value>>)> {
    let mut base_text = String::new();
    let mut appends = std::collections::HashMap::new();
    let mut current_table: Option<String> = None;
    let mut in_array_table = false;

    for line in input.lines() {
        let trimmed = line.trim();

        if trimmed.starts_with("[[") && trimmed.ends_with("]]") && trimmed.len() >= 4 {
            current_table = Some(normalize_append_key(trimmed[2..trimmed.len() - 2].trim()));
            in_array_table = true;
            base_text.push_str(line);
            base_text.push('\n');
            continue;
        }
        if trimmed.starts_with('[') && trimmed.ends_with(']') && trimmed.len() >= 2 {
            current_table = Some(normalize_append_key(trimmed[1..trimmed.len() - 1].trim()));
            in_array_table = false;
            base_text.push_str(line);
            base_text.push('\n');
            continue;
        }

        if trimmed.is_empty() || trimmed.starts_with('#') {
            base_text.push_str(line);
            base_text.push('\n');
            continue;
        }

        if let Some(plus_idx) = trimmed.find("+=") {
            if in_array_table {
                anyhow::bail!(
                    "'+=' is not supported inside array-of-table sections ({})",
                    current_table.as_deref().unwrap_or("")
                );
            }
            let key = normalize_append_key(trimmed[..plus_idx].trim());
            let val_str = trimmed[plus_idx + 2..].trim();
            let val: toml::Value = toml::from_str::<toml::Value>(&format!("v = {}", val_str))
                .context("Failed to parse append value")?
                .get("v")
                .cloned()
                .unwrap();

            let full_key = if key.contains('.') {
                key
            } else if let Some(table) = current_table.as_deref() {
                format!("{}.{}", table, key)
            } else {
                key
            };

            appends.entry(full_key).or_insert_with(Vec::new).push(val);
            // Preserve line numbering for parser diagnostics.
            base_text.push('\n');
            continue;
        }

        base_text.push_str(line);
        base_text.push('\n');
    }

    Ok((base_text, appends))
}

pub(super) fn normalize_append_key(raw: &str) -> String {
    raw.split('.')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .map(|part| {
            let stripped = if (part.starts_with('"') && part.ends_with('"'))
                || (part.starts_with('\'') && part.ends_with('\''))
            {
                &part[1..part.len() - 1]
            } else {
                part
            };
            stripped.trim().to_ascii_lowercase()
        })
        .collect::<Vec<_>>()
        .join(".")
}

fn configure_arch_append_key(key: &str) -> Option<&str> {
    key.strip_prefix("configure_")
        .map(str::trim)
        .filter(|arch| !arch.is_empty() && !matches!(*arch, "file" | "lib32"))
}

fn append_string_values(target: &mut Vec<String>, values: &[toml::Value]) {
    for value in values {
        if let Some(arr) = value.as_array() {
            target.extend(
                arr.iter()
                    .filter_map(|entry| entry.as_str())
                    .map(String::from),
            );
        } else if let Some(s) = value.as_str() {
            target.push(s.to_string());
        }
    }
}
