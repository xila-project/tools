# Project Overview

`tools` holds the Xila developer tools behind one command line: the `xila`
executable, built from the Cargo package `xila-tools`. The package cannot be
named `xila`, because that is Core's facade crate.

Each tool keeps its engine in a library crate under `crates/`, so that other
repositories can reuse the exact same code, and the command line only parses
arguments and calls those libraries.

# Dependency direction

`A ← B` means B depends on A.

```
core, package_manager ← tools ← platforms
```

- `tools` may depend on Core and `package_manager` only. It must never depend on
  Platforms or Applications: Platforms consumes `tools`, so that would create a
  cycle. When a tool needs a host implementation of a Core contract (a
  file-backed drive, a memory manager), write a small one here or use Core's.
- Cross-repository Cargo git dependencies use `branch = "main"`, never `rev` or
  `tag`; `Cargo.lock` holds the exact commit and CI builds with `--locked`.
  Repin with `cargo update -p <crate>`. Use the same repository URLs as the
  other Xila repositories (`https://github.com/xila-project/core`,
  `https://github.com/xila-project/package_manager`) so that Cargo unifies them.
- Packaging, signing, verification, manifest and registry logic belongs to
  `package_manager`. The `xila package` subcommand calls its libraries; never
  copy that code here. Expose what is missing in `package_manager` first.
- Profiles, shortcuts, repositories and other image data stay in Platforms. The
  image library receives them as paths or values.

# Build & Test Commands

- Format Rust: `mise run format` (check: `mise run format:check`)
- Check, lint and test: `mise run check`, `mise run clippy`, `mise run test`
- Coverage: `mise run coverage`
- Documentation: `mise run documentation:check`
- CI tasks (`MISE_ENV=ci`): `ci:packages` (libclang for Core's LittleFS
  bindings), `ci:format-rust`, `ci:format-toml`, `ci:lint-workflows`

All task names use `<namespace>:<command>`.

# Crates

- `crates/image` (`xila-tools-image`): the image engine library. Platforms
  depends on it, so keep its public API (`build`, `build_profile`,
  `build_with`, `build_profile_with`, `Geometry`, `Layout`, `stage`,
  `Profile`, `ProfileSources`, `instantiate_global_allocator!`) stable, and
  keep images byte-for-byte reproducible. `build` and `build_profile` keep the
  MBR layout on 512-byte blocks; raw images must match the geometry and
  LittleFS options of the firmware that mounts them.
- `crates/command_line` (`xila-tools`): the `xila` executable. It parses
  arguments with getargs-derive (as `package_manager` does) and calls the
  libraries; `xila package` embeds `package_manager`'s `xila-package` command
  line.

# Architecture & Implementation Guidelines

- Use full words in every name and text: `file_system`, not `fs`;
  `configuration`, not `config`; `directory`, not `dir`.
- Keep library crates free of argument parsing and printing: return values and
  errors, and let the command line report them.
- Workflow step names follow `"<Phase> | <Description>"`. Keep
  `permissions: contents: read`, `persist-credentials: false` on checkout and a
  `timeout-minutes` on every job. Run commands through `mise run` tasks.
- Reference workspace actions and reusable workflows by full commit SHA from
  workspace `main`.

# Non-Negotiables

- Keep `target/` and `.mise/` out of Git.
- Do not add a dependency on Platforms or Applications.

# Releases

Tools shares the Xila operating system version with Core and Platforms. Before
1.0, a breaking change bumps the minor version and compatible changes bump the
patch. Submit version bumps through review. The workspace release tags the
pinned Tools commit as `vX.Y.Z`. Keep git dependencies on `main` and build
with `--locked`. The `xila-tools` Cargo package is intended for crates.io once
its dependency chain supports publishing.

See [the Xila release policy](https://github.com/xila-project/core/issues/276).
