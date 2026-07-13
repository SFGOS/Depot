//! Repository management and SQLite database generation

use crate::metadata_time;
use anyhow::{Context, Result};
use rusqlite::{Connection, params};
use sha2::{Digest, Sha256, Sha512};
use std::collections::{BTreeSet, HashMap};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicUsize, Ordering},
    mpsc,
};
use zstd::stream::write::Encoder;

fn parse_license_text(metadata: &toml::Value) -> Option<String> {
    if let Some(s) = metadata.get("license").and_then(|v| v.as_str()) {
        return Some(s.to_string());
    }
    if let Some(arr) = metadata.get("license").and_then(|v| v.as_array()) {
        let licenses: Vec<String> = arr
            .iter()
            .filter_map(|v| v.as_str())
            .map(String::from)
            .collect();
        if !licenses.is_empty() {
            return Some(licenses.join(", "));
        }
    }
    None
}

fn parse_string_array_metadata(metadata: &toml::Value, key: &str) -> Vec<String> {
    metadata
        .get(key)
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|v| v.as_str())
                .map(String::from)
                .collect()
        })
        .unwrap_or_default()
}

fn format_built_against(packages: &[String]) -> String {
    packages.join("\n")
}

fn parse_built_against(raw: &str) -> Vec<String> {
    raw.lines()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(String::from)
        .collect()
}

pub struct RepoManager {
    pub repo_dir: PathBuf,
}

struct HashingReader<R> {
    inner: R,
    sha256: Sha256,
    sha512: Sha512,
}

impl<R> HashingReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            sha256: Sha256::new(),
            sha512: Sha512::new(),
        }
    }

    fn finalize_hex(self) -> (String, String) {
        (
            crate::hex::encode_lower(self.sha256.finalize()),
            crate::hex::encode_lower(self.sha512.finalize()),
        )
    }
}

impl<R: Read> Read for HashingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        if n > 0 {
            self.sha256.update(&buf[..n]);
            self.sha512.update(&buf[..n]);
        }
        Ok(n)
    }
}

struct IndexedPackage {
    name: String,
    real_name: Option<String>,
    version: String,
    revision: u32,
    abi_breaking: bool,
    built_against: Vec<String>,
    completed_at: Option<i64>,
    description: Option<String>,
    homepage: Option<String>,
    license: Option<String>,
    filename: String,
    size: u64,
    sha256: String,
    sha512: String,
    provides: Vec<String>,
    conflicts: Vec<String>,
    replaces: Vec<String>,
    runtime_dependencies: Vec<String>,
    optional_dependencies: Vec<String>,
    groups: Vec<String>,
    archive_files: Vec<String>,
}

/// Search hit returned from a cached binary repository database.
#[derive(Debug, Clone)]
pub struct BinaryRepoSearchHit {
    pub repo_name: String,
    pub name: String,
    pub version: String,
    pub revision: u32,
    pub description: Option<String>,
    pub filename: String,
    pub size: u64,
    pub provides: Vec<String>,
}

/// Exact package record from a binary repository database, including checksums
/// used to verify the downloaded package archive.
#[derive(Debug, Clone)]
pub struct BinaryRepoPackageRecord {
    pub repo_name: String,
    pub name: String,
    pub real_name: Option<String>,
    pub version: String,
    pub revision: u32,
    pub abi_breaking: bool,
    pub built_against: Vec<String>,
    pub completed_at: Option<i64>,
    pub filename: String,
    pub size: u64,
    pub sha512: String,
    pub description: Option<String>,
    pub homepage: Option<String>,
    pub license: Option<String>,
    pub provides: Vec<String>,
    pub conflicts: Vec<String>,
    pub replaces: Vec<String>,
    pub runtime_dependencies: Vec<String>,
    pub optional_dependencies: Vec<String>,
    pub groups: Vec<String>,
}

impl BinaryRepoPackageRecord {
    /// Return the stable package stream name, defaulting to the package name.
    pub fn effective_real_name(&self) -> &str {
        self.real_name.as_deref().unwrap_or(&self.name)
    }
}

/// Local cache paths for a binary package archive and its detached signature.
#[derive(Debug, Clone)]
pub struct BinaryRepoCachedArchive {
    pub package_path: PathBuf,
    pub signature_path: PathBuf,
}

/// File search hit returned from a cached binary repo database.
#[derive(Debug, Clone)]
pub struct BinaryRepoFileSearchHit {
    pub repo_name: String,
    pub package_name: String,
    pub version: String,
    pub revision: u32,
    pub path: String,
    pub size: u64,
}

mod archive;
mod fetch;
mod manager;
mod mirrors;
mod query;

pub use archive::*;
pub use fetch::*;
pub use mirrors::*;
pub use query::*;

fn num_cpus() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

#[cfg(test)]
mod tests;
