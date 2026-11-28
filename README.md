# Xila tools

[Documentation](https://www.xila.dev/docs/) ·
[Try Xila](https://platforms.xila.dev/) ·
[Community](https://matrix.to/#/#xila:anneraud.fr)

[![CI](https://github.com/xila-project/tools/actions/workflows/ci.yml/badge.svg)](https://github.com/xila-project/tools/actions/workflows/ci.yml) [![Rust coverage](https://codecov.io/gh/xila-project/tools/graph/badge.svg)](https://codecov.io/gh/xila-project/tools)
<!-- Vulnerabilities badge: to be added by the repository owner once the Nanobom project exists. -->

Developer tools for [Xila](https://www.xila.dev/), behind one command line: the
`xila` executable (Cargo package `xila-tools`; it cannot be named `xila`,
which is Core's facade crate).

## Install

Install from source:

```sh
cargo install --locked --git https://github.com/xila-project/tools xila-tools
```

## Usage

```sh
xila --help
xila image build --help
xila package --help
```

### File system images

`xila image build` creates a raw disk with the Xila MBR signature, a Xila
partition and LittleFS on 512-byte blocks. It exclusively creates its output,
remounts it, and verifies the imported contents and metadata. Existing output
files are never replaced, and an incomplete output is removed on failure.

`--layout raw` builds LittleFS on the whole image instead, with no MBR, to be
written straight to a flash partition that the firmware mounts with LittleFS.
LittleFS records the block size and block count in its superblock and refuses
any other geometry, so the image size must be the partition size and
`--block-size` the flash erase size (4096 by default for raw images; a power
of two from 512 to 65536). A raw image starts erased (every byte `0xFF`) and
uses the firmware's LittleFS options (cache 256, block cycles 500).

```sh
# Empty disk (16 MiB by default).
xila image build empty.img
# Import a directory; host ownership and timestamps are deliberately ignored.
xila image build populated.img 16777216 --root ./root
# Seed a profile from verified application bundle directories (xila.clock/, ...).
xila image build native.img 16777216 \
  --profile profiles/native.txt \
  --bundles-directory bundles \
  --shortcuts profiles/native-shortcuts.txt \
  --shortcuts-directory shortcuts \
  --repositories-directory repositories
# LittleFS for a flash partition of 3312 blocks of 4096 bytes, without an MBR.
xila image build storage.img 13565952 --layout raw --block-size 4096 --root ./root
```

A profile is data, kept by its owner (Platforms keeps its profiles in
`tools/fs_image`):

| Option | Content |
| --- | --- |
| `--profile <FILE>` | Guest applications, one per line; `pixel_painter` is seeded from the bundle `xila.pixel-painter` into `/applications/bundles/`. The file stem names the profile. |
| `--bundles-directory <DIRECTORY>` | Verified bundle directories named by package identifier, each with `.xila/manifest.json`. |
| `--shortcuts <FILE>` | Native applications whose shortcuts are seeded, one per line. |
| `--shortcuts-directory <DIRECTORY>` | `<name>.json` for each listed shortcut, written to `/configuration/shared/applications/shortcuts/`. |
| `--repositories-directory <DIRECTORY>` | `*.json` package repositories, written to `/configuration/system/packages/repositories/`. |

Sizes must be multiples of the block size. MBR images must accommodate the MBR
and LittleFS (at least 1049600 bytes; populated images need more); raw images
must hold at least two blocks. Images hold at most 2^32 - 1 blocks
(2199023255040 bytes for MBR images). Import accepts regular files and directories with
UTF-8 names, rejecting symbolic links and special files, even if the source
tree changes during generation. It uses sorted traversal, zero timestamps, root
ownership, directory mode 0755, executable and library mode 0555, other file
mode 0644; account directories and global application data and configuration
are 0700. Repeating a build with identical inputs and tool
versions produces identical bytes. Host-side population requires Linux or
macOS.

### Packages

`xila package` is the `xila-package` command line of
[`package_manager`](https://github.com/xila-project/package_manager), embedded
from its library: the same subcommands (`manifest`, `bundle`, `key`,
`registry`, `verify`, `pack`) and the same code.

```sh
xila package bundle init ./weather community.weather 1.0.0 wasm32-wasip1 --xila-version 0.1 \
  --entrypoint executables/weather.wasm --display-name Weather
xila package bundle build ./weather ./Weather.app
xila package bundle archive ./Weather.app ./Weather.xpk
```

### Crash reports

Use the ELF file from the exact firmware or native build that produced the
report. The decoder checks the build identifier before it resolves addresses.
Use `--allow-mismatch` with `--elf` to proceed when the identifier is missing.

```sh
xila crash decode report.xcr --elf firmware.elf
xila crash decode report.xcr --elf-directory ./firmware-builds --json
xila crash decode - --elf firmware.elf < report.base64
```

The report can be binary or Base64. Use `--unwind-stack` to use the saved
stack window and ELF `.debug_frame` rules when the report already has frames.

## Workspace

| Crate | Responsibility |
| --- | --- |
| `xila-tools-image` (`crates/image`) | Image engine library: profiles, staging, image geometry (MBR or raw, block size), a host file-backed drive and memory manager, LittleFS population and verification |
| `xila-tools` (`crates/command_line`) | The `xila` executable: argument parsing only, calling the libraries |

## Place in the Xila dependency graph

`A ← B` means B depends on A.

```text
core, package_manager ← tools ← platforms
```

`tools` depends on Core and `package_manager` only, never on Platforms or
Applications. Platforms depends on the `xila-tools-image` library through Git
(`branch = "main"`), so its `Cargo.lock` pins `tools` and unifies Core with the
firmware: an image is always built by the Core revision of the firmware that
mounts it. Platforms keeps its profiles, shortcuts and repositories and a thin
image builder that passes them to the library.

## Development

```sh
mise run format:check
mise run clippy
mise run test
mise run coverage
```

Building Core's LittleFS bindings needs libclang (`mise run ci:packages` with
`MISE_ENV=ci` installs it on Debian and Ubuntu).

## Releases

Tools ships with Core and Platforms under a shared `vX.Y.Z` tag. The
[workspace](https://github.com/xila-project/workspace) selects the revisions
for each Xila release. The `xila-tools` crate is planned for crates.io once its
dependencies can be resolved there.

## License

Licensed under the [Apache License, Version 2.0](LICENSE).

<!-- xila:footer:start -->

---

Part of **[Xila](https://www.xila.dev/)** ·
[Repositories](https://github.com/xila-project) ·
[Community](https://matrix.to/#/#xila:anneraud.fr)

Special thanks to [Martin Legand](https://www.behance.net/martinlegand) for generously designing the Xila logo.

<!-- xila:footer:end -->
