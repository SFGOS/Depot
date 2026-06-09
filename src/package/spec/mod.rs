//! Package specification structures and TOML parsing

mod config;
mod loading;
mod model;

pub use loading::PackageSpec;
pub use model::*;

#[cfg(test)]
mod tests;
