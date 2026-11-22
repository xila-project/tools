//! Build and verify Xila file system images.
//!
//! By default, an image is a raw disk with the Xila MBR signature, a single
//! Xila partition and LittleFS on 512-byte blocks. [`build`] creates its
//! output exclusively, imports an optional source directory, remounts the
//! result and verifies the imported contents and metadata. Existing output
//! files are never replaced, and an incomplete output is removed on failure.
//!
//! A [`Geometry`] chooses another layout: [`Layout::Raw`] puts LittleFS on the
//! whole image, with no MBR and the flash erase size as block size, so that
//! the image can be written straight to a flash partition that the firmware
//! mounts with LittleFS. [`build_with`] and [`build_profile_with`] take it.
//!
//! A [`Profile`] describes a seed: the guest application bundles, the native
//! application shortcuts and the package repositories. Its data belongs to the
//! caller (Platforms keeps its profiles); [`stage`] lays it out as a source
//! directory for [`build`], and [`build_profile`] does both.
//!
//! The library uses Core's file system and LittleFS, so an image always matches
//! the Core revision of the executable that links it. That executable must
//! install the Xila memory manager with [`instantiate_global_allocator!`].

mod drive;
mod memory;
mod populate;
mod profile;
mod source;

pub use drive::FileDrive;
pub use memory::HostMemoryManager;
pub use profile::{Profile, ProfileFile, ProfileSources, Staging, package_identifier, stage};

#[doc(hidden)]
pub extern crate memory as memory_crate;

use file_system::{
    DirectBaseOperations, DirectBlockDevice, XILA_DISK_SIGNATURE,
    mbr::{Mbr, PartitionKind, create_partition_device},
};
use std::{
    error::Error,
    fs::OpenOptions,
    io,
    path::{Path, PathBuf},
};

/// Result of the image operations: any failure is reported as a boxed error.
pub type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Default image size: 16 MiB.
pub const DEFAULT_SIZE: u64 = 16 * 1024 * 1024;

/// Block size of MBR images, in bytes.
pub const BLOCK_SIZE: u64 = 512;

/// LittleFS cache size used to format and mount images. The firmware mounts
/// flash partitions with the same cache, and Core uses it as the lookahead
/// size; read and program sizes are Core's (16 bytes).
pub const LITTLE_FS_CACHE_SIZE: usize = 256;

/// LittleFS block cycles of raw flash images, as the firmware mounts its
/// flash partition. They decide when LittleFS moves metadata for wear
/// levelling, and are not recorded on disk.
pub const FLASH_BLOCK_CYCLES: u16 = 500;

/// Smallest accepted MBR image size: the MBR and a minimal LittleFS partition.
pub const MINIMUM_SIZE: u64 = (Mbr::MINIMUM_START_BLOCK + 2) * BLOCK_SIZE;

/// Largest accepted MBR image size: Core's block devices count blocks in a
/// `u32`.
pub const MAXIMUM_SIZE: u64 = u32::MAX as u64 * BLOCK_SIZE;

/// Where LittleFS lies in an image.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum Layout {
    /// A Xila MBR and one Xila partition holding LittleFS, as the native and
    /// browser seeds use.
    #[default]
    Mbr,
    /// LittleFS on the whole image, with no MBR, for a raw flash partition
    /// that the firmware mounts directly.
    Raw,
}

/// Layout and block size of an image.
///
/// LittleFS records the block size and the block count (the image size
/// divided by the block size) in its superblock, and refuses to mount on a
/// device that reports another geometry. A raw image for a flash partition
/// must therefore use the flash erase size as block size and the partition
/// size as image size.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Geometry {
    /// Where LittleFS lies in the image.
    pub layout: Layout,
    /// Block size in bytes: 512 for MBR images; for raw images, the flash
    /// erase size (4096 on Espressif boards), a power of two from
    /// [`Geometry::MINIMUM_RAW_BLOCK_SIZE`] to
    /// [`Geometry::MAXIMUM_RAW_BLOCK_SIZE`].
    pub block_size: u32,
}

impl Default for Geometry {
    fn default() -> Self {
        Self::MBR
    }
}

impl Geometry {
    /// Today's default: a Xila MBR on 512-byte blocks.
    pub const MBR: Self = Self {
        layout: Layout::Mbr,
        block_size: BLOCK_SIZE as u32,
    };

    /// Smallest block size of raw images, in bytes.
    pub const MINIMUM_RAW_BLOCK_SIZE: u32 = 512;

    /// Largest block size of raw images, in bytes.
    pub const MAXIMUM_RAW_BLOCK_SIZE: u32 = 64 * 1024;

    /// LittleFS on the whole image, with `block_size`-byte blocks.
    pub const fn raw(block_size: u32) -> Self {
        Self {
            layout: Layout::Raw,
            block_size,
        }
    }

    /// Check that LittleFS and the layout support this block size.
    pub fn validate(&self) -> Result<()> {
        match self.layout {
            Layout::Mbr if u64::from(self.block_size) != BLOCK_SIZE => {
                Err(format!("MBR images use {BLOCK_SIZE}-byte blocks").into())
            }
            Layout::Raw
                if !self.block_size.is_power_of_two()
                    || !(Self::MINIMUM_RAW_BLOCK_SIZE..=Self::MAXIMUM_RAW_BLOCK_SIZE)
                        .contains(&self.block_size) =>
            {
                Err(format!(
                    "the block size of raw images must be a power of two from {} to {}",
                    Self::MINIMUM_RAW_BLOCK_SIZE,
                    Self::MAXIMUM_RAW_BLOCK_SIZE
                )
                .into())
            }
            _ => Ok(()),
        }
    }

    /// Smallest image size: two LittleFS blocks, after the MBR area for MBR
    /// images.
    pub fn minimum_size(&self) -> u64 {
        match self.layout {
            Layout::Mbr => MINIMUM_SIZE,
            Layout::Raw => 2 * u64::from(self.block_size),
        }
    }

    /// Largest image size: Core's block devices count blocks in a `u32`.
    pub fn maximum_size(&self) -> u64 {
        u64::from(u32::MAX) * u64::from(self.block_size)
    }

    /// Check this geometry with [`Geometry::validate`], and that an image of
    /// `size` bytes fits it.
    pub fn validate_size(&self, size: u64) -> Result<()> {
        self.validate()?;
        let block_size = self.block_size;
        if !size.is_multiple_of(u64::from(block_size)) || size < self.minimum_size() {
            return Err(match self.layout {
                Layout::Mbr => format!(
                    "disk size must be a multiple of {block_size} and accommodate the MBR and LittleFS"
                ),
                Layout::Raw => format!(
                    "image size must be a multiple of the block size ({block_size}) and hold at least two blocks"
                ),
            }
            .into());
        }
        let maximum = self.maximum_size();
        if size > maximum {
            return Err(
                format!("disk size must not exceed {maximum} bytes (2^32 - 1 blocks)").into(),
            );
        }
        Ok(())
    }

    /// Open the image file as a drive with this geometry.
    fn open(&self, path: &Path) -> io::Result<FileDrive> {
        match self.layout {
            Layout::Mbr => FileDrive::open(path),
            // Erase like the flash partition, so that the image holds the
            // bytes LittleFS would leave there.
            Layout::Raw => FileDrive::open_with_block_size(path, self.block_size)
                .map(FileDrive::with_flash_erase),
        }
    }

    /// LittleFS options matching the device that mounts the image.
    pub fn little_fs_options(&self) -> little_fs::Options {
        let options = little_fs::Options::new(LITTLE_FS_CACHE_SIZE);
        match self.layout {
            Layout::Mbr => options,
            Layout::Raw => options.set_block_cycles(FLASH_BLOCK_CYCLES),
        }
    }
}

/// Build a verified MBR image at `output`, `size` bytes long, importing the
/// directory `root` when given.
///
/// Returns the absolute path of the image. The output must not exist yet.
/// This is [`build_with`] and [`Geometry::MBR`].
pub fn build(output: &Path, size: u64, root: Option<&Path>) -> Result<PathBuf> {
    build_with(output, size, root, Geometry::MBR)
}

/// Build a verified image with `geometry` at `output`, `size` bytes long,
/// importing the directory `root` when given.
///
/// Returns the absolute path of the image. The output must not exist yet. A
/// raw image starts erased, every byte `0xFF` as on erased flash, so that it
/// holds exactly what the flash partition will.
pub fn build_with(
    output: &Path,
    size: u64,
    root: Option<&Path>,
    geometry: Geometry,
) -> Result<PathBuf> {
    let output = canonicalize_parent(output)?;
    geometry.validate_size(size)?;
    let source = root.map(source::Directory::open).transpose()?;
    if let Some(root) = source.as_ref() {
        populate::validate(root)?;
    }
    create_output_file(&output, size)?;
    let result = format_populate_and_verify(&output, size, source.as_ref(), geometry);
    if result.is_err() {
        let _ = std::fs::remove_file(&output);
    }
    result.map(|()| output)
}

/// Stage `profile` from the bundles in `bundles_directory`, then [`build`] an
/// MBR image from it. The staging directory is removed afterwards.
pub fn build_profile(
    output: &Path,
    size: u64,
    profile: &Profile,
    bundles_directory: &Path,
) -> Result<PathBuf> {
    build_profile_with(output, size, profile, bundles_directory, Geometry::MBR)
}

/// Stage `profile` from the bundles in `bundles_directory`, then
/// [`build_with`] an image with `geometry` from it. The staging directory is
/// removed afterwards.
pub fn build_profile_with(
    output: &Path,
    size: u64,
    profile: &Profile,
    bundles_directory: &Path,
    geometry: Geometry,
) -> Result<PathBuf> {
    // Reject an invalid geometry before staging the bundles.
    geometry.validate_size(size)?;
    let staging = stage(profile, bundles_directory)?;
    build_with(output, size, Some(staging.path()), geometry)
}

fn format_populate_and_verify(
    output: &Path,
    size: u64,
    source: Option<&source::Directory>,
    geometry: Geometry,
) -> Result<()> {
    let options = geometry.little_fs_options();
    {
        let drive: &'static FileDrive = Box::leak(Box::new(geometry.open(output)?));
        let result = (|| -> Result<()> {
            let device: &'static dyn DirectBlockDevice = match geometry.layout {
                Layout::Mbr => Box::leak(Box::new(Mbr::format_disk_with_signature_and_partition(
                    drive,
                    XILA_DISK_SIGNATURE,
                    PartitionKind::Xila,
                )?)),
                Layout::Raw => {
                    // Start from an erased partition, as flashing tools do.
                    drive.erase(0, size)?;
                    drive
                }
            };
            let file_system = little_fs::FileSystem::new_format(device, options)?;
            populate::populate(&file_system, source)?;
            drop(file_system);
            drive.flush_sync()?;
            Ok(())
        })();
        drive.close();
        result?;
    }
    {
        let drive: &'static FileDrive = Box::leak(Box::new(geometry.open(output)?));
        let result = (|| -> Result<()> {
            let device: &'static dyn DirectBlockDevice = match geometry.layout {
                Layout::Mbr => {
                    let mbr = Mbr::read_from_device(drive)?;
                    mbr.validate()?;
                    if mbr.get_disk_signature() != XILA_DISK_SIGNATURE {
                        return Err("invalid Xila signature".into());
                    }
                    let entry = mbr
                        .partitions
                        .iter()
                        .find(|partition| {
                            partition.is_valid() && partition.kind == PartitionKind::Xila
                        })
                        .ok_or("missing Xila partition")?;
                    Box::leak(Box::new(create_partition_device(drive, entry)?))
                }
                Layout::Raw => drive,
            };
            let file_system = little_fs::FileSystem::new(device, options)?;
            populate::verify(&file_system, source)?;
            drop(file_system);
            drive.flush_sync()?;
            Ok(())
        })();
        drive.close();
        result?;
    }
    Ok(())
}

/// Create the image file exclusively, with its final size.
fn create_output_file(path: &Path, size: u64) -> io::Result<()> {
    if path.to_str().is_none() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "output image path must be valid UTF-8",
        ));
    }
    // Exclusive creation prevents accidental replacement of an existing image.
    OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)?
        .set_len(size)
}

/// Resolve the parent directory of `path`, which must exist, keeping its file
/// name.
fn canonicalize_parent(path: &Path) -> Result<PathBuf> {
    let parent = path
        .parent()
        .filter(|path| !path.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent.canonicalize()?;
    Ok(parent.join(path.file_name().ok_or("output path must name a file")?))
}

#[cfg(test)]
crate::instantiate_global_allocator!();

#[cfg(test)]
mod tests {
    use super::*;
    use std::{fs, io::Write};

    fn temporary_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "xila-tools-image-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[cfg(unix)]
    #[test]
    fn rejects_non_utf8_output_before_creating_file() {
        use std::os::unix::ffi::OsStringExt;

        let mut name = temporary_path("invalid").into_os_string().into_vec();
        name.push(0xff);
        let path = PathBuf::from(std::ffi::OsString::from_vec(name));

        let result = create_output_file(&path, DEFAULT_SIZE);

        let error = result.unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert!(error.to_string().contains("valid UTF-8"));
        assert!(!path.exists());
    }

    #[test]
    fn refuses_existing_output_without_modifying_it() {
        let path = temporary_path("existing");
        let mut existing = fs::File::create(&path).unwrap();
        existing.write_all(b"keep this installation").unwrap();
        drop(existing);

        let result = create_output_file(&path, DEFAULT_SIZE);

        assert!(result.is_err());
        assert_eq!(fs::read(&path).unwrap(), b"keep this installation");
        assert!(build(&path, DEFAULT_SIZE, None).is_err());
        assert_eq!(fs::read(&path).unwrap(), b"keep this installation");
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn rejects_sizes_that_cannot_hold_the_partition() {
        for size in [
            MINIMUM_SIZE - BLOCK_SIZE,
            DEFAULT_SIZE + 1,
            MAXIMUM_SIZE + BLOCK_SIZE,
            u64::MAX - u64::MAX % BLOCK_SIZE,
        ] {
            let path = temporary_path("size");
            assert!(build(&path, size, None).is_err());
            assert!(!path.exists());
        }
    }

    #[test]
    fn output_must_name_a_file_in_an_existing_directory() {
        assert!(canonicalize_parent(Path::new("/")).is_err());
        assert!(canonicalize_parent(&temporary_path("missing").join("image.img")).is_err());
        let relative = canonicalize_parent(Path::new("image.img")).unwrap();
        assert!(relative.is_absolute());
        assert!(relative.ends_with("image.img"));
    }

    #[test]
    fn empty_images_are_verified_and_reproducible() {
        let first = temporary_path("empty-first");
        let second = temporary_path("empty-second");
        let output = build(&first, MINIMUM_SIZE * 4, None).unwrap();
        assert_eq!(output, first);
        build(&second, MINIMUM_SIZE * 4, None).unwrap();
        assert_eq!(fs::read(&first).unwrap(), fs::read(&second).unwrap());
        assert_eq!(fs::metadata(&first).unwrap().len(), MINIMUM_SIZE * 4);
        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();
    }

    /// Geometry of the WT32-SC01 Plus `storage` partition, which its firmware
    /// mounts with LittleFS: 3312 blocks of 4096 bytes.
    const FLASH_BLOCK_SIZE: u32 = 4096;
    const FLASH_PARTITION_SIZE: u64 = 3312 * FLASH_BLOCK_SIZE as u64;

    /// Mount `path` as the firmware mounts its flash partition: a block device
    /// with `block_size`-byte blocks and the firmware's LittleFS options.
    fn mount_flash(path: &Path, block_size: u32) -> file_system::Result<little_fs::FileSystem> {
        let drive: &'static FileDrive = Box::leak(Box::new(
            FileDrive::open_with_block_size(path, block_size)
                .unwrap()
                .with_flash_erase(),
        ));
        little_fs::FileSystem::new(drive, little_fs::Options::new(256).set_block_cycles(500))
    }

    fn read_file(file_system: &little_fs::FileSystem, path: &str) -> Vec<u8> {
        use file_system::{AccessFlags, BaseOperations, Context, FileSystemOperations};

        let mut context = Context::new_empty();
        file_system
            .lookup_file(&mut context, path.as_ref(), AccessFlags::Read.into())
            .unwrap();
        let mut content = Vec::new();
        let mut buffer = [0; 1024];
        loop {
            let count = file_system
                .read_sync(&mut context, &mut buffer, content.len() as u64)
                .unwrap();
            if count == 0 {
                break;
            }
            content.extend_from_slice(&buffer[..count]);
        }
        file_system.close(&mut context).unwrap();
        content
    }

    #[test]
    fn raw_flash_images_mount_at_the_partition_geometry() {
        let root = temporary_path("raw-root");
        fs::create_dir_all(root.join("configuration/shared")).unwrap();
        let large: Vec<u8> = (0..=255).cycle().take(3 * 4096 + 17).collect();
        fs::write(root.join("configuration/shared/large.bin"), &large).unwrap();
        fs::write(root.join("readme.txt"), b"seeded for flash").unwrap();
        let first = temporary_path("raw-first");
        let second = temporary_path("raw-second");
        let geometry = Geometry::raw(FLASH_BLOCK_SIZE);
        for path in [&first, &second] {
            build_with(path, FLASH_PARTITION_SIZE, Some(&root), geometry).unwrap();
        }
        let image = fs::read(&first).unwrap();
        assert_eq!(image, fs::read(&second).unwrap());
        assert_eq!(image.len() as u64, FLASH_PARTITION_SIZE);

        // LittleFS starts at the first byte: its superblock names itself in
        // block 0 or 1, and no MBR boot signature precedes it.
        let block = FLASH_BLOCK_SIZE as usize;
        assert!(image[8..16] == *b"littlefs" || image[block + 8..block + 16] == *b"littlefs");
        assert_ne!(image[510..512], [0x55, 0xAA]);
        // Unused blocks stay erased, as on the flash partition.
        let erased = image
            .chunks_exact(block)
            .filter(|block| block.iter().all(|&byte| byte == 0xFF))
            .count();
        assert!(erased > 3000, "{erased} erased blocks");

        let file_system = mount_flash(&first, FLASH_BLOCK_SIZE).unwrap();
        assert_eq!(read_file(&file_system, "/readme.txt"), b"seeded for flash");
        assert_eq!(
            read_file(&file_system, "/configuration/shared/large.bin"),
            large
        );
        let source = source::Directory::open(&root).unwrap();
        populate::verify(&file_system, Some(&source)).unwrap();
        drop(file_system);

        // LittleFS refuses another block size or block count.
        assert!(mount_flash(&first, 512).is_err());
        let mut larger = image.clone();
        larger.extend_from_slice(&[0xFF; FLASH_BLOCK_SIZE as usize]);
        fs::write(&second, &larger).unwrap();
        assert!(mount_flash(&second, FLASH_BLOCK_SIZE).is_err());
        // Mounting must not have changed the image.
        assert_eq!(fs::read(&first).unwrap(), image);

        for path in [first, second] {
            fs::remove_file(path).unwrap();
        }
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn profiles_are_built_with_the_chosen_geometry() {
        let bundles = temporary_path("profile-bundles");
        fs::create_dir_all(&bundles).unwrap();
        let profile = Profile {
            name: "flash".into(),
            applications: Vec::new(),
            shortcuts: vec![ProfileFile {
                name: "terminal.json".into(),
                content: b"terminal shortcut".to_vec(),
            }],
            repositories: vec![ProfileFile {
                name: "xila.json".into(),
                content: b"xila repository".to_vec(),
            }],
        };
        let flash = temporary_path("profile-flash");
        let geometry = Geometry::raw(FLASH_BLOCK_SIZE);
        build_profile_with(&flash, FLASH_PARTITION_SIZE, &profile, &bundles, geometry).unwrap();
        let file_system = mount_flash(&flash, FLASH_BLOCK_SIZE).unwrap();
        assert_eq!(
            read_file(
                &file_system,
                "/configuration/shared/applications/shortcuts/terminal.json"
            ),
            b"terminal shortcut"
        );
        assert_eq!(
            read_file(
                &file_system,
                "/configuration/system/packages/repositories/xila.json"
            ),
            b"xila repository"
        );
        drop(file_system);

        let disk = temporary_path("profile-disk");
        build_profile(&disk, MINIMUM_SIZE * 4, &profile, &bundles).unwrap();
        assert_eq!(fs::read(&disk).unwrap()[510..512], [0x55, 0xAA]);

        let invalid = temporary_path("profile-invalid");
        assert!(
            build_profile_with(
                &invalid,
                FLASH_PARTITION_SIZE + 512,
                &profile,
                &bundles,
                geometry
            )
            .is_err()
        );
        assert!(!invalid.exists());
        for path in [flash, disk] {
            fs::remove_file(path).unwrap();
        }
        fs::remove_dir_all(bundles).unwrap();
    }

    #[test]
    fn raw_images_accept_the_smallest_geometry() {
        for block_size in [Geometry::MINIMUM_RAW_BLOCK_SIZE, FLASH_BLOCK_SIZE] {
            let path = temporary_path("raw-minimum");
            let geometry = Geometry::raw(block_size);
            build_with(&path, geometry.minimum_size() * 8, None, geometry).unwrap();
            assert!(mount_flash(&path, block_size).is_ok());
            fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn invalid_geometries_are_rejected_before_creating_the_image() {
        let raw = Geometry::raw(FLASH_BLOCK_SIZE);
        for (size, geometry) in [
            (DEFAULT_SIZE, Geometry::raw(0)),
            (DEFAULT_SIZE, Geometry::raw(3000)),
            (DEFAULT_SIZE, Geometry::raw(256)),
            (DEFAULT_SIZE, Geometry::raw(128 * 1024)),
            (
                DEFAULT_SIZE,
                Geometry {
                    layout: Layout::Mbr,
                    block_size: 4096,
                },
            ),
            (FLASH_PARTITION_SIZE + 512, raw),
            (raw.minimum_size() - u64::from(FLASH_BLOCK_SIZE), raw),
            (raw.maximum_size() + u64::from(FLASH_BLOCK_SIZE), raw),
        ] {
            let path = temporary_path("geometry");
            assert!(
                build_with(&path, size, None, geometry).is_err(),
                "{size} {geometry:?}"
            );
            assert!(!path.exists());
        }
        assert_eq!(Geometry::default(), Geometry::MBR);
        assert_eq!(Geometry::MBR.minimum_size(), MINIMUM_SIZE);
        assert_eq!(Geometry::MBR.maximum_size(), MAXIMUM_SIZE);
    }

    #[test]
    fn the_default_layout_is_unchanged() {
        let first = temporary_path("default-build");
        let second = temporary_path("default-build-with");
        build(&first, MINIMUM_SIZE * 4, None).unwrap();
        build_with(&second, MINIMUM_SIZE * 4, None, Geometry::MBR).unwrap();
        let image = fs::read(&first).unwrap();
        assert_eq!(image, fs::read(&second).unwrap());
        assert_eq!(image[510..512], [0x55, 0xAA]);
        fs::remove_file(first).unwrap();
        fs::remove_file(second).unwrap();
    }
}
