# AGENTS.md

## Purpose

This document defines mandatory development rules for contributors and automated agents working on the Depot package manager.

All code changes must comply with the policies below.

---

## 1. No Stubs

Stubs are not allowed.

Do **not**:

- Add placeholder functions
- Add `todo!()` or `unimplemented!()`
- Leave partially implemented logic
- Add empty modules “for later”
- Return dummy values to satisfy the compiler
- Bypass logic with `#[allow(unused)]` or similar attributes

All submitted code must:

- Be fully implemented
- Compile cleanly
- Function as described
- Pass relevant tests

If a feature is not ready, do not merge it.

---

## 2. Dependency Management

All Rust dependencies must be added using:

```bash
cargo add <crate>
```

Do **not**:

- Manually edit `Cargo.toml` to add dependencies
- Hardcode versions unless strictly required
- Add unused dependencies

When adding a crate:

- Prefer stable, well-maintained crates
- Avoid unnecessary heavy dependencies
- Justify large or complex dependencies in commit messages

---

## 3. No Dead Code

Do not introduce:

- Unused structs
- Unused enums
- Unused functions
- Unused modules
- Commented-out code

Remove obsolete code instead of commenting it out.

---

## 4. Error Handling

All errors must:

- Use `anyhow::Result` at boundaries
- Provide meaningful context with `.with_context(...)`
- Avoid silent failure

Do not ignore `Result`s.

---

## 5. Deterministic Behavior

Depot is a package manager. Reproducibility matters.

All logic must:

- Avoid non-deterministic behavior
- Avoid reliance on environment state unless explicitly defined
- Avoid hidden global state

---

## 6. Formatting and Linting

Before committing:

```bash
cargo fmt
cargo clippy -- -D warnings
```

Warnings are treated as errors.

---

## 7. Documentation

Public APIs must use Rustdoc comments.

Examples must compile when possible.

Do not leave undocumented public interfaces.

---

## 8. Extraction and Fetching Safety

All archive extraction must:

- Prevent path traversal
- Reject unsafe paths
- Validate checksums before use

Security is not optional.

---

## 9. Architecture Discipline

Keep responsibilities separated:

- Spec parsing: `package/`
- Fetching: `source/`
- Extraction: `source/`
- Build execution: `builder/`
- Installation: `install/`

Do not mix layers.

---

## 10. Testing

New features must include:

- Parsing tests (if spec-related)
- Behavior tests (if build/install-related)

Avoid regression-prone changes without tests.

---

## Summary

Depot is infrastructure.

Code must be:

- Complete
- Deterministic
- Secure
- Minimal
- Explicit

No shortcuts.
