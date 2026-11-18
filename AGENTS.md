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
