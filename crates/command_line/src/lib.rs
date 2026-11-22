//! The `xila` command line: Xila developer tools behind one executable.
//!
//! - `xila image build`: build verified file system images with the
//!   [`image`] library, the same one Platforms links into its image builder:
//!   MBR disks, or raw LittleFS for flash partitions (`--layout raw`).
//! - `xila package`: the `xila-package` command line of `package_manager`,
//!   embedded from its library so that both run exactly the same code.
//!
//! [`run`] parses and runs raw arguments; [`execute`] runs a parsed
//! [`CommandLine`]. The executable that links this library must install the
//! Xila memory manager with [`image::instantiate_global_allocator!`].

use getargs_derive::{GetArgs, Subcommand, ValueEnum};
use image::{Geometry, Profile, ProfileSources};
use std::{io, path::Path};

/// Result of a command: any failure is reported as a boxed error.
pub type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Xila developer tools.
#[derive(Debug, GetArgs)]
#[command(name = "xila", version)]
pub struct CommandLine<'a> {
    #[command(subcommand)]
    command: Command<'a>,
}

/// Subcommands of [`CommandLine`].
#[derive(Debug, Subcommand)]
pub enum Command<'a> {
    /// Build Xila file system images.
    Image(ImageCommandLine<'a>),
    /// Build, sign and verify Xila packages and package registries.
    Package(package_builder::CommandLine<'a>),
}

/// Arguments of `xila image`.
#[derive(Debug, GetArgs)]
pub struct ImageCommandLine<'a> {
    #[command(subcommand)]
    command: ImageCommand<'a>,
}

/// Subcommands of `xila image`.
#[derive(Debug, Subcommand)]
pub enum ImageCommand<'a> {
    /// Create a verified image: LittleFS on a Xila MBR partition, or raw.
    Build(BuildArguments<'a>),
}

/// Where LittleFS lies in an image built by `xila image build`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum Layout {
    /// A Xila MBR and one Xila partition, on 512-byte blocks.
    Mbr,
    /// LittleFS on the whole image, with no MBR, for a raw flash partition.
    Raw,
}

impl Layout {
    /// Block size used when `--block-size` is absent: the MBR sector size, or
    /// the usual flash erase size for raw images.
    pub const fn default_block_size(self) -> u32 {
        match self {
            Self::Mbr => 512,
            Self::Raw => 4096,
        }
    }
}

/// Arguments of `xila image build`.
///
/// Without `--root` or `--profile`, the image is empty. With `--profile`, the
/// image is seeded with Xila's directory layout, the listed application
/// bundles, shortcuts and package repositories. With `--layout raw`, the image
/// holds LittleFS only, to be written to a flash partition: its size must be
/// the partition size and its block size the flash erase size.
#[derive(Debug, GetArgs)]
pub struct BuildArguments<'a> {
    /// New image file; an existing file is never replaced.
    #[arg(value_name = "OUTPUT")]
    output: &'a str,
    /// Image size in bytes: a multiple of the block size; for MBR images, from 1049600 to 2199023255040.
    #[arg(value_name = "SIZE", default_value_t = 16777216)]
    size: u64,
    /// Image layout: LittleFS on a Xila MBR partition, or on the whole image.
    #[arg(long, value_name = "LAYOUT", value_enum, default_value_t = Layout::Mbr)]
    layout: Layout,
    /// Block size in bytes: 512 for MBR images; for raw images, a power of two from 512 to 65536 (default 4096).
    #[arg(long, value_name = "BYTES")]
    block_size: Option<u32>,
    /// Import this directory; host ownership and timestamps are ignored.
    #[arg(long, value_name = "DIRECTORY", conflicts_with = "profile")]
    root: Option<&'a str>,
    /// Seed the guest applications listed in this file, one per line.
    #[arg(long, value_name = "FILE", requires = "bundles_directory")]
    profile: Option<&'a str>,
    /// Directory of verified application bundles, named by package identifier.
    #[arg(long, value_name = "DIRECTORY", requires = "profile")]
    bundles_directory: Option<&'a str>,
    /// Seed the shortcuts of the native applications listed in this file.
    #[arg(
        long,
        value_name = "FILE",
        requires = "profile",
        requires = "shortcuts_directory"
    )]
    shortcuts: Option<&'a str>,
    /// Directory holding `<name>.json` for each listed shortcut.
    #[arg(long, value_name = "DIRECTORY", requires = "shortcuts")]
    shortcuts_directory: Option<&'a str>,
    /// Seed every `*.json` file of this directory as a package repository.
    #[arg(long, value_name = "DIRECTORY", requires = "profile")]
    repositories_directory: Option<&'a str>,
}

/// Parse `arguments` (without the program name) and run the command.
///
/// Help and version requests are printed to the standard output.
pub fn run<'a>(arguments: impl Iterator<Item = &'a str>) -> Result<()> {
    let command_line = match CommandLine::from_args(arguments) {
        Ok(command_line) => command_line,
        Err(error) if error.is_help() || error.is_version() => {
            println!("{error}");
            return Ok(());
        }
        Err(error) => {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, error.to_string()).into());
        }
    };
    execute(command_line)
}

/// Run an already parsed command line.
pub fn execute(command_line: CommandLine<'_>) -> Result<()> {
    match command_line.command {
        Command::Image(ImageCommandLine {
            command: ImageCommand::Build(arguments),
        }) => build_image(&arguments),
        Command::Package(command_line) => package_builder::execute(command_line),
    }
}

impl BuildArguments<'_> {
    /// Image geometry chosen by `--layout` and `--block-size`.
    pub fn geometry(&self) -> Geometry {
        let block_size = self.block_size.unwrap_or(self.layout.default_block_size());
        match self.layout {
            Layout::Mbr => Geometry {
                layout: image::Layout::Mbr,
                block_size,
            },
            Layout::Raw => Geometry::raw(block_size),
        }
    }
}

fn build_image(arguments: &BuildArguments<'_>) -> Result<()> {
    let output = Path::new(arguments.output);
    let geometry = arguments.geometry();
    geometry.validate_size(arguments.size)?;
    let output = match arguments.profile {
        Some(applications) => {
            let profile = Profile::load(&ProfileSources {
                applications: Some(Path::new(applications)),
                shortcuts: arguments.shortcuts.map(Path::new),
                shortcuts_directory: arguments.shortcuts_directory.map(Path::new),
                repositories_directory: arguments.repositories_directory.map(Path::new),
            })?;
            let bundles_directory = arguments
                .bundles_directory
                .ok_or("--profile requires --bundles-directory")?;
            let staging = image::stage(&profile, Path::new(bundles_directory))?;
            println!(
                "Prepared {} bundle: {}",
                profile.name,
                profile.applications.join(",")
            );
            image::build_with(output, arguments.size, Some(staging.path()), geometry)?
        }
        None => image::build_with(
            output,
            arguments.size,
            arguments.root.map(Path::new),
            geometry,
        )?,
    };
    match geometry.layout {
        image::Layout::Mbr => println!(
            "Verified Xila MBR, LittleFS contents and metadata: {} ({} bytes)",
            output.display(),
            arguments.size
        ),
        image::Layout::Raw => println!(
            "Verified raw LittleFS contents and metadata: {} ({} bytes, {} blocks of {} bytes)",
            output.display(),
            arguments.size,
            arguments.size / u64::from(geometry.block_size),
            geometry.block_size
        ),
    }
    Ok(())
}

#[cfg(test)]
image::instantiate_global_allocator!();

#[cfg(test)]
mod tests {
    use super::*;

    fn parse<'a>(arguments: &[&'a str]) -> CommandLine<'a> {
        CommandLine::from_args(arguments.iter().copied()).unwrap()
    }

    #[test]
    fn image_build_defaults_to_sixteen_mebibytes() {
        let command_line = parse(&["image", "build", "seed.img"]);
        let Command::Image(ImageCommandLine {
            command: ImageCommand::Build(arguments),
        }) = command_line.command
        else {
            panic!("expected image build");
        };
        assert_eq!(arguments.output, "seed.img");
        assert_eq!(arguments.size, image::DEFAULT_SIZE);
        assert_eq!(arguments.root, None);
        assert_eq!(arguments.layout, Layout::Mbr);
        assert_eq!(arguments.geometry(), Geometry::MBR);
    }

    #[test]
    fn image_build_chooses_the_layout_and_block_size() {
        for (arguments, expected) in [
            (&["--layout", "raw"][..], Geometry::raw(4096)),
            (
                &["--layout", "raw", "--block-size", "512"],
                Geometry::raw(512),
            ),
            (&["--layout", "mbr", "--block-size", "512"], Geometry::MBR),
            (
                &["--block-size", "4096"],
                Geometry {
                    layout: image::Layout::Mbr,
                    block_size: 4096,
                },
            ),
        ] {
            let mut command_line = vec!["image", "build", "flash.img", "13565952"];
            command_line.extend_from_slice(arguments);
            let Command::Image(ImageCommandLine {
                command: ImageCommand::Build(build),
            }) = parse(&command_line).command
            else {
                panic!("expected image build");
            };
            assert_eq!(build.geometry(), expected, "{arguments:?}");
        }
        for invalid in [
            &["image", "build", "a.img", "--layout", "gpt"][..],
            &["image", "build", "a.img", "--block-size", "large"],
        ] {
            assert!(
                CommandLine::from_args(invalid.iter().copied()).is_err(),
                "{invalid:?}"
            );
        }
        // The geometry is checked before anything is staged or created.
        assert!(run(["image", "build", "a.img", "--block-size", "4096"].into_iter()).is_err());
        assert!(!Path::new("a.img").exists());
    }

    #[test]
    fn image_build_relations_are_enforced() {
        for invalid in [
            &["image", "build", "a.img", "--profile", "native.txt"][..],
            &["image", "build", "a.img", "--bundles-directory", "bundles"],
            &[
                "image",
                "build",
                "a.img",
                "--root",
                "root",
                "--profile",
                "native.txt",
                "--bundles-directory",
                "bundles",
            ],
            &[
                "image",
                "build",
                "a.img",
                "--profile",
                "native.txt",
                "--bundles-directory",
                "bundles",
                "--shortcuts",
                "native-shortcuts.txt",
            ],
            &["image", "build", "a.img", "not-a-size"],
        ] {
            assert!(
                CommandLine::from_args(invalid.iter().copied()).is_err(),
                "{invalid:?}"
            );
        }
    }

    #[test]
    fn package_subcommands_are_embedded() {
        let command_line = parse(&["package", "key", "generate", "seed", "public"]);
        assert!(matches!(command_line.command, Command::Package(_)));
        assert!(run(["package", "--help"].into_iter()).is_ok());
        assert!(run(["package", "registry", "--help"].into_iter()).is_ok());
    }

    #[test]
    fn help_version_and_errors() {
        assert!(run(["--help"].into_iter()).is_ok());
        assert!(run(["--version"].into_iter()).is_ok());
        assert!(run(["image", "build", "--help"].into_iter()).is_ok());
        assert!(run(["unknown"].into_iter()).is_err());
        assert!(run(["image"].into_iter()).is_err());
    }
}
