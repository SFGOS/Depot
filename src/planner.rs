//! Dependency resolver and execution planner.
//!
//! Builds a recursive dependency closure for install/build operations, resolves
//! dependencies by exact package name or provided feature, and returns a
//! topologically sorted execution plan.

use crate::config::Config;
use crate::db;
use crate::deps;
use crate::index::PackageIndex;
use crate::package::PackageSpec;
use crate::ui;
use anyhow::{Context, Result};
use petgraph::algo::toposort;
use petgraph::graph::{DiGraph, NodeIndex};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub(crate) enum PlanOrigin {
    Installed,
    Source {
        path: PathBuf,
        local_sibling: bool,
    },
    Binary {
        repo_name: String,
        record: Box<db::repo::BinaryRepoPackageRecord>,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub(crate) enum PlanAction {
    SkipInstalled,
    BuildAndInstall,
    InstallBinary,
}

#[derive(Debug, Clone)]
pub(crate) struct PlannedStep {
    pub package: String,
    pub action: PlanAction,
    pub origin: PlanOrigin,
    pub requested_by: Vec<String>,
}

impl PlannedStep {
    pub(crate) fn download_size(&self) -> Option<u64> {
        match &self.origin {
            PlanOrigin::Binary { record, .. } => Some(record.size),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PlanSummary {
    pub total_packages: usize,
    pub skipped_installed: usize,
    pub binary_installs: usize,
    pub source_build_installs: usize,
    pub known_download_bytes: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct ExecutionPlan {
    pub steps: Vec<PlannedStep>,
}

impl ExecutionPlan {
    pub(crate) fn summary(&self) -> PlanSummary {
        let mut out = PlanSummary {
            total_packages: self.steps.len(),
            ..PlanSummary::default()
        };
        for step in &self.steps {
            match step.action {
                PlanAction::SkipInstalled => out.skipped_installed += 1,
                PlanAction::InstallBinary => out.binary_installs += 1,
                PlanAction::BuildAndInstall => out.source_build_installs += 1,
            }
            if let Some(n) = step.download_size() {
                out.known_download_bytes = out.known_download_bytes.saturating_add(n);
            }
        }
        out
    }

    pub(crate) fn actionable_steps(&self) -> impl Iterator<Item = &PlannedStep> {
        self.steps
            .iter()
            .filter(|s| !matches!(s.action, PlanAction::SkipInstalled))
    }
}

#[derive(Debug, Clone)]
pub(crate) enum InstallTarget {
    PackageName(String),
    SpecPath(PathBuf),
}

#[derive(Debug, Clone)]
pub(crate) struct PlannerOptions {
    pub assume_yes: bool,
    pub prefer_binary: bool,
    pub local_sibling_root: Option<PathBuf>,
    pub include_test_deps: bool,
    pub lib32_only_requested_specs: bool,
}

#[derive(Debug, Clone)]
enum CandidateKind {
    Source {
        path: PathBuf,
        local_sibling: bool,
    },
    Binary {
        repo_name: String,
        record: Box<db::repo::BinaryRepoPackageRecord>,
    },
}

#[derive(Debug, Clone, Copy, Eq, PartialEq)]
enum MatchKind {
    Replaces,
    Exact,
    Provides,
}

#[derive(Debug, Clone)]
struct Candidate {
    package: String,
    kind: CandidateKind,
    match_kind: MatchKind,
    sort_repo_priority: i32,
    sort_label: String,
}

#[derive(Debug, Clone)]
struct NodeData {
    step: PlannedStep,
}

#[derive(Debug, Clone)]
struct LocalSpecHit {
    spec_name: String,
    path: PathBuf,
    provides: Vec<String>,
    replaces: Vec<String>,
}

struct Resolver<'a> {
    config: &'a Config,
    rootfs: &'a Path,
    db_path: PathBuf,
    opts: PlannerOptions,
    pkg_index: PackageIndex,
    graph: DiGraph<NodeData, ()>,
    by_package: HashMap<String, NodeIndex>,
    spec_cache: HashMap<PathBuf, PackageSpec>,
    local_sibling_cache: BTreeMap<PathBuf, Vec<LocalSpecHit>>,
    stack: Vec<String>,
    emitted_installed_roots: HashSet<String>,
}

impl<'a> Resolver<'a> {
    fn new(config: &'a Config, rootfs: &'a Path, opts: PlannerOptions) -> Self {
        let db_path = config.installed_db_path(rootfs);
        let pkg_index = PackageIndex::build_with_repo_dir(Some(config.repo_clone_dir.clone()));
        Self {
            config,
            rootfs,
            db_path,
            opts,
            pkg_index,
            graph: DiGraph::new(),
            by_package: HashMap::new(),
            spec_cache: HashMap::new(),
            local_sibling_cache: BTreeMap::new(),
            stack: Vec::new(),
            emitted_installed_roots: HashSet::new(),
        }
    }

    fn plan_for_install_target(mut self, target: InstallTarget) -> Result<ExecutionPlan> {
        self.resolve_install_target(&target)?;
        self.finish_plan()
    }

    fn plan_for_install_targets(mut self, targets: &[InstallTarget]) -> Result<ExecutionPlan> {
        for target in targets {
            self.resolve_install_target(target)?;
        }
        self.finish_plan()
    }

    fn resolve_install_target(&mut self, target: &InstallTarget) -> Result<()> {
        match target {
            InstallTarget::PackageName(name) => {
                if deps::is_dep_satisfied_in_db(name, &self.db_path)? {
                    if self.emitted_installed_roots.insert(name.clone()) {
                        self.add_installed_root_step(name.clone(), "requested package".to_string());
                    }
                } else {
                    let root =
                        self.resolve_dep_node(name, None, "requested package".to_string())?;
                    if let Some(root_idx) = root {
                        self.mark_requested_by(root_idx, "requested package".to_string());
                    }
                }
            }
            InstallTarget::SpecPath(path) => {
                let root_idx =
                    self.ensure_source_spec_node(path, true, true, "requested spec".to_string())?;
                self.mark_requested_by(root_idx, "requested spec".to_string());
            }
        }
        Ok(())
    }

    fn plan_for_deps(mut self, deps_to_install: &[String]) -> Result<ExecutionPlan> {
        for dep in deps_to_install {
            if deps::is_dep_satisfied_in_db(dep, &self.db_path)? {
                continue;
            }
            if let Some(idx) = self.resolve_dep_node(dep, None, format!("dependency {}", dep))? {
                self.mark_requested_by(idx, format!("dependency {}", dep));
            }
        }
        self.finish_plan()
    }

    fn finish_plan(self) -> Result<ExecutionPlan> {
        let order = toposort(&self.graph, None)
            .map_err(|_| anyhow::anyhow!("Dependency cycle detected in plan graph"))?;
        let mut steps = Vec::with_capacity(order.len());
        for idx in order {
            let node = self
                .graph
                .node_weight(idx)
                .with_context(|| format!("Missing plan node {idx:?}"))?;
            steps.push(node.step.clone());
        }
        Ok(ExecutionPlan { steps })
    }

    fn add_installed_root_step(&mut self, package: String, requested_by: String) {
        let step = PlannedStep {
            package,
            action: PlanAction::SkipInstalled,
            origin: PlanOrigin::Installed,
            requested_by: vec![requested_by],
        };
        self.graph.add_node(NodeData { step });
    }

    fn mark_requested_by(&mut self, idx: NodeIndex, reason: String) {
        if let Some(node) = self.graph.node_weight_mut(idx)
            && !node.step.requested_by.contains(&reason)
        {
            node.step.requested_by.push(reason);
        }
    }

    fn ensure_source_spec_node(
        &mut self,
        path: &Path,
        allow_local_sibling_fallback: bool,
        local_sibling: bool,
        requested_by: String,
    ) -> Result<NodeIndex> {
        let include_test_deps = self.opts.include_test_deps;
        let lib32_only =
            self.opts.lib32_only_requested_specs && requested_by.starts_with("requested ");
        let (package_name, deps_needed) = {
            let spec = self.load_spec(path)?;
            (
                spec.package.name.clone(),
                source_deps_for_install(spec, include_test_deps, lib32_only),
            )
        };
        if let Some(&idx) = self.by_package.get(&package_name) {
            self.mark_requested_by(idx, requested_by);
            return Ok(idx);
        }

        self.push_stack(&package_name)?;
        let idx = self.graph.add_node(NodeData {
            step: PlannedStep {
                package: package_name.clone(),
                action: PlanAction::BuildAndInstall,
                origin: PlanOrigin::Source {
                    path: path.to_path_buf(),
                    local_sibling,
                },
                requested_by: vec![requested_by],
            },
        });
        self.by_package.insert(package_name.clone(), idx);

        for dep in deps_needed {
            if let Some(dep_idx) = self.resolve_dep_node(
                &dep,
                allow_local_sibling_fallback.then_some(path),
                format!("{} needs {}", package_name, dep),
            )? {
                self.graph.add_edge(dep_idx, idx, ());
            }
        }

        self.pop_stack(&package_name);
        Ok(idx)
    }

    fn resolve_dep_node(
        &mut self,
        dep: &str,
        requester_spec_path: Option<&Path>,
        requested_by: String,
    ) -> Result<Option<NodeIndex>> {
        if deps::is_dep_satisfied_in_db(dep, &self.db_path)? {
            return Ok(None);
        }

        let dep_name = deps::dep_name(dep);
        let candidates = self.collect_candidates(dep_name, requester_spec_path)?;
        if candidates.is_empty() {
            if let Some(spec_path) = requester_spec_path {
                anyhow::bail!(
                    "Could not resolve dependency '{}' (from {}). Checked binary repos, source repos, and local sibling specs",
                    dep,
                    spec_path.display()
                );
            }
            anyhow::bail!(
                "Could not resolve dependency '{}'. Checked binary repos and source repos",
                dep
            );
        }

        let chosen = self.choose_candidate(dep, &candidates)?;
        let idx = match chosen.kind {
            CandidateKind::Source {
                ref path,
                local_sibling,
            } => self.ensure_source_spec_node(path, true, local_sibling, requested_by)?,
            CandidateKind::Binary {
                ref repo_name,
                ref record,
            } => self.ensure_binary_node(repo_name, (**record).clone(), requested_by)?,
        };
        Ok(Some(idx))
    }

    fn ensure_binary_node(
        &mut self,
        repo_name: &str,
        record: db::repo::BinaryRepoPackageRecord,
        requested_by: String,
    ) -> Result<NodeIndex> {
        if let Some(&idx) = self.by_package.get(&record.name) {
            self.mark_requested_by(idx, requested_by);
            return Ok(idx);
        }

        self.push_stack(&record.name)?;

        let idx = self.graph.add_node(NodeData {
            step: PlannedStep {
                package: record.name.clone(),
                action: PlanAction::InstallBinary,
                origin: PlanOrigin::Binary {
                    repo_name: repo_name.to_string(),
                    record: Box::new(record.clone()),
                },
                requested_by: vec![requested_by],
            },
        });
        self.by_package.insert(record.name.clone(), idx);

        for dep in &record.runtime_dependencies {
            if let Some(dep_idx) =
                self.resolve_dep_node(dep, None, format!("{} needs {}", record.name, dep))?
            {
                self.graph.add_edge(dep_idx, idx, ());
            }
        }

        self.pop_stack(&record.name);
        Ok(idx)
    }

    fn choose_candidate(&self, dep: &str, candidates: &[Candidate]) -> Result<Candidate> {
        let mut sorted =
            dedupe_candidate_packages(sort_candidates(candidates, self.opts.prefer_binary));

        if sorted.len() == 1 {
            return Ok(sorted.remove(0));
        }

        if self.opts.assume_yes {
            let chosen = sorted.remove(0);
            ui::info(format!(
                "Multiple providers matched '{}' - using {} ({}) due to --yes",
                dep, chosen.package, chosen.sort_label
            ));
            return Ok(chosen);
        }

        let options: Vec<String> = sorted.iter().map(format_candidate_label).collect();
        let prompt = format!("Multiple packages satisfy '{}'. Choose one", dep);
        let choice = ui::prompt_select_index(&prompt, &options, 0)?;
        Ok(sorted.remove(choice))
    }

    fn collect_candidates(
        &mut self,
        dep_name: &str,
        requester_spec_path: Option<&Path>,
    ) -> Result<Vec<Candidate>> {
        let mut out = Vec::new();
        let mut seen = HashSet::<String>::new();

        // Local sibling fallback (e.g. ../foo/*.toml when building from a local tree).
        // Prefer these candidates before probing configured repos so local development
        // remains deterministic and does not block on external repository I/O.
        let local_sibling_root = requester_spec_path
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .or_else(|| self.opts.local_sibling_root.clone());
        if let Some(root) = local_sibling_root {
            for hit in self.local_sibling_hits(&root)? {
                let exact = hit.spec_name.eq_ignore_ascii_case(dep_name);
                let replaces = hit
                    .replaces
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(dep_name));
                let provides = hit
                    .provides
                    .iter()
                    .any(|p| p.eq_ignore_ascii_case(dep_name));
                if !(exact || provides || replaces) {
                    continue;
                }
                let key = format!("src:{}", hit.path.display());
                if !seen.insert(key) {
                    continue;
                }
                out.push(Candidate {
                    package: hit.spec_name.clone(),
                    kind: CandidateKind::Source {
                        path: hit.path.clone(),
                        local_sibling: true,
                    },
                    match_kind: if replaces {
                        MatchKind::Replaces
                    } else if exact {
                        MatchKind::Exact
                    } else {
                        MatchKind::Provides
                    },
                    sort_repo_priority: -10,
                    sort_label: "source:local-sibling".to_string(),
                });
            }
        }

        // Binary repos
        let host_arch = std::env::consts::ARCH;
        let mut binary_repos: Vec<_> = self
            .config
            .binary_repos
            .iter()
            .filter(|(_, repo)| repo.enabled && repo.supports_arch(host_arch))
            .collect();
        binary_repos.sort_by(|a, b| a.1.priority.cmp(&b.1.priority).then_with(|| a.0.cmp(b.0)));

        for (repo_name, repo_cfg) in binary_repos {
            match db::repo::find_binary_repo_packages(
                repo_name,
                repo_cfg,
                self.rootfs,
                &self.config.package_cache_dir,
                dep_name,
            ) {
                Ok(records) => {
                    for rec in records {
                        let match_kind = if rec.name.eq_ignore_ascii_case(dep_name) {
                            MatchKind::Exact
                        } else if rec
                            .replaces
                            .iter()
                            .any(|replacement| replacement.eq_ignore_ascii_case(dep_name))
                        {
                            MatchKind::Replaces
                        } else {
                            MatchKind::Provides
                        };
                        let key = format!("bin:{}:{}", repo_name, rec.name);
                        if !seen.insert(key) {
                            continue;
                        }
                        out.push(Candidate {
                            package: rec.name.clone(),
                            kind: CandidateKind::Binary {
                                repo_name: repo_name.clone(),
                                record: Box::new(rec.clone()),
                            },
                            match_kind,
                            sort_repo_priority: repo_cfg.priority,
                            sort_label: format!("binary:{}", repo_name),
                        });
                    }
                }
                Err(e) => crate::log_warn!("Binary repo '{}': {}", repo_name, e),
            }
        }

        // Global source index
        if let Some(path) = self.pkg_index.find(dep_name) {
            let spec = self.load_spec(&path)?;
            let match_kind = if spec
                .alternatives
                .replaces
                .iter()
                .any(|replacement| replacement.eq_ignore_ascii_case(dep_name))
            {
                MatchKind::Replaces
            } else if spec.package.name.eq_ignore_ascii_case(dep_name) {
                MatchKind::Exact
            } else {
                MatchKind::Provides
            };
            let local_sibling = false;
            let key = format!("src:{}", path.display());
            if seen.insert(key) {
                out.push(Candidate {
                    package: spec.package.name.clone(),
                    kind: CandidateKind::Source {
                        path: path.clone(),
                        local_sibling,
                    },
                    match_kind,
                    sort_repo_priority: 0,
                    sort_label: format!("source:{}", source_label_for_path(self.config, &path)),
                });
            }
        }

        // Additional source providers from index (for provider prompt)
        for path in self.pkg_index.find_replacements(dep_name) {
            let spec = self.load_spec(&path)?;
            let key = format!("src:{}", path.display());
            if !seen.insert(key) {
                continue;
            }
            out.push(Candidate {
                package: spec.package.name.clone(),
                kind: CandidateKind::Source {
                    path: path.clone(),
                    local_sibling: false,
                },
                match_kind: MatchKind::Replaces,
                sort_repo_priority: 0,
                sort_label: format!("source:{}", source_label_for_path(self.config, &path)),
            });
        }

        // Additional source providers from index (for provider prompt)
        for path in self.pkg_index.find_providers(dep_name) {
            let spec = self.load_spec(&path)?;
            let key = format!("src:{}", path.display());
            if !seen.insert(key) {
                continue;
            }
            out.push(Candidate {
                package: spec.package.name.clone(),
                kind: CandidateKind::Source {
                    path: path.clone(),
                    local_sibling: false,
                },
                match_kind: if spec.package.name.eq_ignore_ascii_case(dep_name) {
                    MatchKind::Exact
                } else {
                    MatchKind::Provides
                },
                sort_repo_priority: 0,
                sort_label: format!("source:{}", source_label_for_path(self.config, &path)),
            });
        }

        Ok(out)
    }

    fn load_spec(&mut self, path: &Path) -> Result<&PackageSpec> {
        if !self.spec_cache.contains_key(path) {
            let mut spec = PackageSpec::from_file(path)
                .with_context(|| format!("Failed to parse spec {}", path.display()))?;
            spec.apply_config(self.config);
            self.spec_cache.insert(path.to_path_buf(), spec);
        }
        self.spec_cache
            .get(path)
            .with_context(|| format!("Spec cache entry missing for {}", path.display()))
    }

    fn local_sibling_hits(&mut self, root: &Path) -> Result<&[LocalSpecHit]> {
        if !self.local_sibling_cache.contains_key(root) {
            let mut hits = Vec::new();
            for entry in walkdir::WalkDir::new(root).min_depth(1).max_depth(4) {
                let entry = match entry {
                    Ok(e) => e,
                    Err(_) => continue,
                };
                let path = entry.path();
                if !entry.file_type().is_file() {
                    continue;
                }
                if path.extension().is_none_or(|ext| ext != "toml") {
                    continue;
                }
                if let Ok(mut spec) = PackageSpec::from_file(path) {
                    spec.apply_config(self.config);
                    hits.push(LocalSpecHit {
                        spec_name: spec.package.name.clone(),
                        path: path.to_path_buf(),
                        provides: spec.alternatives.provides.clone(),
                        replaces: spec.alternatives.replaces.clone(),
                    });
                }
            }
            self.local_sibling_cache.insert(root.to_path_buf(), hits);
        }
        Ok(self
            .local_sibling_cache
            .get(root)
            .map(Vec::as_slice)
            .unwrap_or(&[]))
    }

    fn push_stack(&mut self, pkg: &str) -> Result<()> {
        if self.stack.iter().any(|s| s == pkg) {
            let mut chain = self.stack.join(" -> ");
            if !chain.is_empty() {
                chain.push_str(" -> ");
            }
            chain.push_str(pkg);
            anyhow::bail!("Dependency cycle detected: {}", chain);
        }
        self.stack.push(pkg.to_string());
        Ok(())
    }

    fn pop_stack(&mut self, pkg: &str) {
        if matches!(self.stack.last(), Some(last) if last == pkg) {
            self.stack.pop();
        }
    }
}

fn source_deps_for_install(
    spec: &PackageSpec,
    include_test_deps: bool,
    lib32_only: bool,
) -> Vec<String> {
    let mut deps_all = Vec::new();
    let lib32_only = lib32_only || spec.builds_only_lib32_output();
    let include_lib32 = lib32_only || spec.builds_lib32_output();
    let skip_automatic_tests = spec.should_skip_automatic_tests() || lib32_only;
    let local_provides = spec.local_dependency_provides_for_selection(!lib32_only, include_lib32);
    if !lib32_only && !spec.is_metapackage() {
        for dep in &spec.dependencies.build {
            push_unique(&mut deps_all, dep.clone());
        }
    }
    if include_lib32 && !spec.is_metapackage() {
        for dep in &spec.lib32_dependencies().build {
            push_unique(&mut deps_all, dep.clone());
        }
    }
    if !lib32_only {
        for dep in &spec.dependencies.runtime {
            if !local_provides.contains(deps::dep_name(dep)) {
                push_unique(&mut deps_all, dep.clone());
            }
        }

        for out in spec.outputs() {
            let deps = spec.dependencies_for_output(&out.name);
            for dep in deps.runtime {
                if !local_provides.contains(deps::dep_name(&dep)) {
                    push_unique(&mut deps_all, dep);
                }
            }
        }
    }
    if include_lib32 {
        for dep in &spec.lib32_dependencies().runtime {
            if !local_provides.contains(deps::dep_name(dep)) {
                push_unique(&mut deps_all, dep.clone());
            }
        }
    }
    if include_test_deps && !skip_automatic_tests {
        if !lib32_only {
            for dep in &spec.dependencies.test {
                push_unique(&mut deps_all, dep.clone());
            }
        }
        if include_lib32 {
            for dep in &spec.lib32_dependencies().test {
                push_unique(&mut deps_all, dep.clone());
            }
        }
    }
    deps_all
}

fn push_unique(v: &mut Vec<String>, item: String) {
    if !v.contains(&item) {
        v.push(item);
    }
}

fn source_label_for_path(config: &Config, path: &Path) -> String {
    if let Ok(rel) = path.strip_prefix(&config.repo_clone_dir)
        && let Some(repo) = rel.components().next()
    {
        return repo.as_os_str().to_string_lossy().into_owned();
    }
    "local".to_string()
}

fn format_candidate_label(c: &Candidate) -> String {
    let match_label = match c.match_kind {
        MatchKind::Replaces => "replaces",
        MatchKind::Exact => "exact",
        MatchKind::Provides => "provides",
    };
    match &c.kind {
        CandidateKind::Source {
            path,
            local_sibling,
        } => format!(
            "{} [{}] {} {}",
            c.package,
            if *local_sibling {
                "source:local-sibling"
            } else {
                &c.sort_label
            },
            match_label,
            path.display()
        ),
        CandidateKind::Binary { repo_name, record } => format!(
            "{} [binary:{}] {} {}-{} size={} file={}",
            c.package,
            repo_name,
            match_label,
            record.version,
            record.revision,
            record.size,
            record.filename
        ),
    }
}

fn sort_candidates(candidates: &[Candidate], prefer_binary: bool) -> Vec<Candidate> {
    let mut sorted = candidates.to_vec();
    sorted.sort_by(|a, b| {
        candidate_sort_key(a, prefer_binary).cmp(&candidate_sort_key(b, prefer_binary))
    });
    sorted
}

fn dedupe_candidate_packages(candidates: Vec<Candidate>) -> Vec<Candidate> {
    let mut out = Vec::with_capacity(candidates.len());
    let mut seen = HashSet::new();
    for candidate in candidates {
        if seen.insert(candidate.package.to_ascii_lowercase()) {
            out.push(candidate);
        }
    }
    out
}

fn candidate_sort_key(c: &Candidate, prefer_binary: bool) -> (i32, i32, i32, String, String) {
    let is_binary = matches!(c.kind, CandidateKind::Binary { .. });
    let kind_rank = match (prefer_binary, is_binary) {
        (true, true) => 0,
        (true, false) => 1,
        (false, false) => 0,
        (false, true) => 1,
    };
    let match_rank = match c.match_kind {
        MatchKind::Replaces => 0,
        MatchKind::Exact => 1,
        MatchKind::Provides => 2,
    };
    (
        kind_rank,
        match_rank,
        c.sort_repo_priority,
        c.package.clone(),
        c.sort_label.clone(),
    )
}

pub(crate) fn build_install_plan(
    config: &Config,
    rootfs: &Path,
    target: InstallTarget,
    opts: PlannerOptions,
) -> Result<ExecutionPlan> {
    Resolver::new(config, rootfs, opts).plan_for_install_target(target)
}

pub(crate) fn build_install_plan_for_targets(
    config: &Config,
    rootfs: &Path,
    targets: &[InstallTarget],
    opts: PlannerOptions,
) -> Result<ExecutionPlan> {
    Resolver::new(config, rootfs, opts).plan_for_install_targets(targets)
}

pub(crate) fn build_dependency_install_plan(
    config: &Config,
    rootfs: &Path,
    deps_to_install: &[String],
    opts: PlannerOptions,
) -> Result<ExecutionPlan> {
    Resolver::new(config, rootfs, opts).plan_for_deps(deps_to_install)
}

#[cfg(test)]
mod tests {
    use super::{
        Candidate, CandidateKind, MatchKind, PlannerOptions, build_dependency_install_plan,
        dedupe_candidate_packages, sort_candidates, source_deps_for_install,
    };
    use crate::config::Config;
    use crate::db;
    use crate::package::{
        Alternatives, Build, BuildFlags, BuildType, Dependencies, PackageInfo, PackageSpec, Source,
    };
    use std::collections::BTreeMap;
    use std::path::{Path, PathBuf};

    fn mk_spec() -> PackageSpec {
        PackageSpec {
            package: PackageInfo {
                name: "foo".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            },
            packages: vec![PackageInfo {
                name: "foo-libs".into(),
                real_name: None,
                version: "1.0".into(),
                revision: 1,
                description: "d".into(),
                homepage: "h".into(),
                abi_breaking: false,
                license: vec!["MIT".into()],
            }],
            alternatives: Alternatives::default(),
            manual_sources: Vec::new(),
            source: vec![Source {
                url: "https://example.test/foo.tar.gz".into(),
                sha256: "skip".into(),
                extract_dir: "foo".into(),
                patches: Vec::new(),
                post_extract: Vec::new(),
                cherry_pick: Vec::new(),
            }],
            build: Build {
                build_type: BuildType::Autotools,
                flags: BuildFlags::default(),
            },
            dependencies: Dependencies {
                build: vec!["make".into()],
                runtime: vec!["foo-libs".into(), "zlib".into()],
                test: vec!["bats".into()],
                optional: vec!["docs-viewer".into()],
                lib32: None,
            },
            package_alternatives: BTreeMap::from([(
                "foo-libs".into(),
                Alternatives {
                    provides: vec!["libfoo".into()],
                    conflicts: Vec::new(),
                    replaces: Vec::new(),
                    lib32: None,
                },
            )]),
            package_dependencies: BTreeMap::from([(
                "foo".into(),
                Dependencies {
                    build: Vec::new(),
                    runtime: vec!["foo-libs".into(), "libfoo".into(), "openssl".into()],
                    test: Vec::new(),
                    optional: Vec::new(),
                    lib32: None,
                },
            )]),
            spec_dir: PathBuf::from("."),
        }
    }

    fn mk_installed_spec(name: &str, version: &str) -> PackageSpec {
        let mut spec = mk_spec();
        spec.package.name = name.to_string();
        spec.package.version = version.to_string();
        spec
    }

    fn mk_binary_candidate(name: &str, repo_name: &str, priority: i32) -> Candidate {
        Candidate {
            package: name.to_string(),
            kind: CandidateKind::Binary {
                repo_name: repo_name.to_string(),
                record: Box::new(db::repo::BinaryRepoPackageRecord {
                    repo_name: repo_name.to_string(),
                    name: name.to_string(),
                    real_name: None,
                    version: "1.0.0".to_string(),
                    revision: 1,
                    abi_breaking: false,
                    completed_at: None,
                    filename: format!("{name}-1.0.0-1-x86_64.depot.pkg.tar.zst"),
                    size: 1024,
                    sha256: "sha256".to_string(),
                    sha512: "sha512".to_string(),
                    description: None,
                    homepage: None,
                    license: None,
                    provides: Vec::new(),
                    conflicts: Vec::new(),
                    replaces: Vec::new(),
                    runtime_dependencies: Vec::new(),
                    optional_dependencies: Vec::new(),
                }),
            },
            match_kind: MatchKind::Exact,
            sort_repo_priority: priority,
            sort_label: format!("binary:{repo_name}"),
        }
    }

    fn mk_source_candidate(name: &str, path: &Path, local_sibling: bool) -> Candidate {
        Candidate {
            package: name.to_string(),
            kind: CandidateKind::Source {
                path: path.to_path_buf(),
                local_sibling,
            },
            match_kind: MatchKind::Exact,
            sort_repo_priority: if local_sibling { -10 } else { 0 },
            sort_label: if local_sibling {
                "source:local-sibling".to_string()
            } else {
                "source:local".to_string()
            },
        }
    }

    #[test]
    fn source_deps_for_install_excludes_local_runtime_outputs_and_provides() {
        let spec = mk_spec();
        let deps = source_deps_for_install(&spec, false, false);
        assert!(deps.contains(&"make".to_string()));
        assert!(deps.contains(&"zlib".to_string()));
        assert!(deps.contains(&"openssl".to_string()));
        assert!(!deps.contains(&"foo-libs".to_string()));
        assert!(!deps.contains(&"libfoo".to_string()));
    }

    #[test]
    fn source_deps_for_install_does_not_include_test_deps() {
        let spec = mk_spec();
        let deps = source_deps_for_install(&spec, false, false);
        assert!(!deps.contains(&"bats".to_string()));
    }

    #[test]
    fn source_deps_for_install_includes_test_deps_when_enabled() {
        let spec = mk_spec();
        let deps = source_deps_for_install(&spec, true, false);
        assert!(deps.contains(&"bats".to_string()));
    }

    #[test]
    fn source_deps_for_install_uses_lib32_only_dependencies_when_requested() {
        let mut spec = mk_spec();
        spec.dependencies.lib32 = Some(crate::package::DependencyGroup {
            build: vec!["gcc-multilib".into()],
            runtime: vec!["lib32-zlib".into()],
            test: vec!["lib32-bats".into()],
            optional: Vec::new(),
        });

        let deps = source_deps_for_install(&spec, true, true);
        assert!(deps.contains(&"gcc-multilib".to_string()));
        assert!(deps.contains(&"lib32-zlib".to_string()));
        assert!(!deps.contains(&"lib32-bats".to_string()));
        assert!(!deps.contains(&"make".to_string()));
        assert!(!deps.contains(&"zlib".to_string()));
        assert!(!deps.contains(&"bats".to_string()));
    }

    #[test]
    fn source_deps_for_install_uses_lib32_only_dependencies_from_spec_flag() {
        let mut spec = mk_spec();
        spec.build.flags.lib32_only = true;
        spec.dependencies.lib32 = Some(crate::package::DependencyGroup {
            build: vec!["gcc-multilib".into()],
            runtime: vec!["lib32-zlib".into()],
            test: vec!["lib32-bats".into()],
            optional: Vec::new(),
        });

        let deps = source_deps_for_install(&spec, true, false);
        assert!(deps.contains(&"gcc-multilib".to_string()));
        assert!(deps.contains(&"lib32-zlib".to_string()));
        assert!(!deps.contains(&"lib32-bats".to_string()));
        assert!(!deps.contains(&"make".to_string()));
        assert!(!deps.contains(&"zlib".to_string()));
        assert!(!deps.contains(&"bats".to_string()));
    }

    #[test]
    fn candidate_dedup_keeps_highest_priority_origin_for_same_package() {
        let candidates = vec![
            mk_source_candidate("meson", Path::new("packages/core/meson/meson.toml"), false),
            mk_binary_candidate("meson", "core", 0),
            mk_source_candidate(
                "meson",
                Path::new("../packages/core/meson/meson.toml"),
                true,
            ),
        ];

        let deduped = dedupe_candidate_packages(sort_candidates(&candidates, true));
        assert_eq!(deduped.len(), 1);
        assert!(matches!(deduped[0].kind, CandidateKind::Binary { .. }));
    }

    #[test]
    fn candidate_dedup_uses_local_origin_when_binaries_are_not_preferred() {
        let candidates = vec![
            mk_binary_candidate("ninja", "core", 0),
            mk_source_candidate("ninja", Path::new("packages/core/ninja/ninja.toml"), false),
        ];

        let deduped = dedupe_candidate_packages(sort_candidates(&candidates, false));
        assert_eq!(deduped.len(), 1);
        assert!(matches!(deduped[0].kind, CandidateKind::Source { .. }));
    }

    #[test]
    fn build_dependency_install_plan_skips_installed_dependency() {
        let rootfs = tempfile::tempdir().unwrap();
        let config = Config::for_rootfs(rootfs.path());
        let db_path = config.db_dir.join("packages.db");
        let destdir = rootfs.path().join("dest");
        std::fs::create_dir_all(&destdir).unwrap();
        db::register_package(&db_path, &mk_installed_spec("meson", "1.0.0"), &destdir).unwrap();

        let plan = build_dependency_install_plan(
            &config,
            rootfs.path(),
            &["meson".to_string()],
            PlannerOptions {
                assume_yes: false,
                prefer_binary: true,
                local_sibling_root: None,
                include_test_deps: false,
                lib32_only_requested_specs: false,
            },
        )
        .unwrap();

        assert!(plan.steps.is_empty());
        assert!(plan.actionable_steps().next().is_none());
    }
}
