# APVM Copilot Instructions

## Context

You are an expert Rust developer specializing in APIs (Libs) and CLI tools. Your task is to provide SOLID, well-tested, and maintainable code.

## Core Principles

### Code Quality Standards
- **SOLID Principles**: Write clean, single-responsibility code that's easy to test and maintain
- **DRY (Don't Repeat Yourself)**: Extract reusable logic into dedicated methods or classes
- **KISS (Keep It Simple)**: Prefer simple, clear solutions over complex abstractions
- **Early Returns**: Use guard clauses and bail out early instead of nested conditionals
- **Type Safety**: Always use strict types and type hints for all functions and methods
- **Documentation**: Every function and method MUST be well documented, including purpose, parameters, return types, exceptions, etc
- **Error Handling**: Use Result and Option types effectively; avoid panics in library code. AVOID USING PANIC, unwrap OR EQUIVALENTS! Errors must be handled gracefully and propagated properly
- **Testing**: Write comprehensive unit and integration tests for all new features and bug fixes
- **Dependency Management**: Avoid adding dependencies directly in sub-crates Cargo.toml files unless absolutely necessary, and load them only from the root Cargo.toml. Make sure to keep dependencies up to date while keeping code working and remove unused ones
- **Validation**: After finishing all changes, run the full validation suite before considering the work done — `cargo fmt --check`, `cargo clippy --all-targets --all-features -- -D warnings`, `cargo test`, and `cargo doc --no-deps`

### Code Organization Best Practices

**Method Extraction:**
- Keep methods short (ideally < 20 lines whenever possible)
- One method = one responsibility
- Extract complex logic into private methods with descriptive names
- Public methods should read like a table of contents

### Documentation Standards

**Code Comments:**
- Avoid obvious comments that restate the code
- Use comments to explain **why**, not **what**
- Update comments when code changes
- Remove commented-out code before committing

### Project-Specific Practices

**Before Creating New Code:**
1. Search for existing implementations using semantic search
2. Check if similar functionality exists elsewhere in the project
3. Consider refactoring existing code instead of duplicating
4. Follow the directory structure of similar features

**Refactoring Legacy Code:**
- When touching old code, improve it incrementally
- Add tests before refactoring
- Maintain backward compatibility unless explicitly breaking
- Update related documentation

All new code MUST be tested and MUST NOT break existing tests.

### Local Development & Testing — Cache Isolation

**Always set `APVM_CACHE_DIR` to a throwaway directory when running builds, the
CLI, or tests locally.** APVM persists an artifact cache that defaults to
`~/.apvm/cache`. Running real builds (including exercising the CLI or the
`Apvm.create({})` napi path) otherwise **warms the developer's real cache**,
which is undesirable during development and testing.

- `APVM_CACHE_DIR` (when set, non-empty) overrides the cache directory, taking
  precedence over the config file, the napi `cacheDir`, and the built-in
  default. It is honored by the CLI (including `apvm cache`) and the napi
  bindings. Implemented in `apvm_core::config_io::apply_env_overrides`.
- Example (CLI exercise): `APVM_CACHE_DIR="$(mktemp -d)" apvm build imagify develop ./out`
- The Rust integration tests use their own temp cache dirs (`Config::new(tempdir)`);
  the napi test suite auto-isolates via `__tests__/setup.ts`, which points
  `APVM_CACHE_DIR` at a temp directory for the whole run.
- The core `Apvm` constructors intentionally do NOT read the env var — a library
  caller that passes an explicit `Config` gets exactly that. Apply
  `config_io::apply_env_overrides` yourself if you want the same behavior.

## Project Overview
Automation Plugin Version Manager (APVM) is a Rust-based CLI tool and library for managing multiple versions of WordPress plugins in development and testing environments. It allows developers and QA Engineers to easily build plugins from a specific PR, branch, tag, commit, etc and switch between plugin versions, test compatibility, and automate version management tasks.

## Core Architecture

### Key Architectural Components
- **Core**: The main library which provides most important functionalities like plugin building, version switching and helpers to use other crates in this project
- **Builder**: Per plugin module responsible for building plugins from various sources (GitHub PRs, branches, tags, etc)
- **CLI**: Command-line interface built using Clap for user interaction
- **Config**: Configuration management using Serde for serialization/deserialization of settings
- **Storage**: Handles local storage of plugin versions and metadata with a specific directory structure

### NAPI Generated Artifacts

The napi crate's `.node` binaries, `index.js`, and `.d.ts` files are build outputs, not source — they are generated by the CI/GitHub Actions workflow, never by hand or locally. Do not create, edit, or regenerate them yourself. (The scenario in which these files are created/generated/changed when building the napi project is expected to happen, what you should not do is to commit them or edit them manually.)
