//! Build and verify Xila file system images.
//!
//! An image is a raw disk with the Xila MBR signature, a single Xila partition
//! and LittleFS. [`build`] creates its output exclusively, imports an optional
//! source directory, remounts the result and verifies the imported contents
//! and metadata. Existing output files are never replaced, and an incomplete
//! output is removed on failure.
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
    DirectBaseOperations, XILA_DISK_SIGNATURE,
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

/// Block size of the image, in bytes.
pub const BLOCK_SIZE: u64 = 512;

/// LittleFS cache size used to format and mount images.
const LITTLE_FS_CACHE_SIZE: usize = 256;

/// Smallest accepted image size: the MBR and a minimal LittleFS partition.
pub const MINIMUM_SIZE: u64 = (Mbr::MINIMUM_START_BLOCK + 2) * BLOCK_SIZE;

/// Largest accepted image size: Core's block devices count blocks in a `u32`.
pub const MAXIMUM_SIZE: u64 = u32::MAX as u64 * BLOCK_SIZE;

/// Build a verified image at `output`, `size` bytes long, importing the
/// directory `root` when given.
///
/// Returns the absolute path of the image. The output must not exist yet.
pub fn build(output: &Path, size: u64, root: Option<&Path>) -> Result<PathBuf> {
    let output = canonicalize_parent(output)?;
    if !size.is_multiple_of(BLOCK_SIZE) || size < MINIMUM_SIZE {
        return Err(
            "disk size must be a multiple of 512 and accommodate the MBR and LittleFS".into(),
        );
    }
    if size > MAXIMUM_SIZE {
        return Err(
            format!("disk size must not exceed {MAXIMUM_SIZE} bytes (2^32 - 1 blocks)").into(),
        );
    }
    let source = root.map(source::Directory::open).transpose()?;
    if let Some(root) = source.as_ref() {
        populate::validate(root)?;
    }
    create_output_file(&output, size)?;
    let result = format_populate_and_verify(&output, source.as_ref());
    if result.is_err() {
        let _ = std::fs::remove_file(&output);
    }
    result.map(|()| output)
}

/// Stage `profile` from the bundles in `bundles_directory`, then [`build`] an
/// image from it. The staging directory is removed afterwards.
pub fn build_profile(
    output: &Path,
    size: u64,
    profile: &Profile,
    bundles_directory: &Path,
) -> Result<PathBuf> {
    let staging = stage(profile, bundles_directory)?;
    build(output, size, Some(staging.path()))
}

fn format_populate_and_verify(output: &Path, source: Option<&source::Directory>) -> Result<()> {
    {
        let drive: &'static FileDrive = Box::leak(Box::new(FileDrive::open(output)?));
        let result = (|| -> Result<()> {
            let partition = Mbr::format_disk_with_signature_and_partition(
                drive,
                XILA_DISK_SIGNATURE,
                PartitionKind::Xila,
            )?;
            let file_system = little_fs::FileSystem::new_format(
                Box::leak(Box::new(partition)),
                little_fs::Options::new(LITTLE_FS_CACHE_SIZE),
            )?;
            populate::populate(&file_system, source)?;
            drop(file_system);
            drive.flush_sync()?;
            Ok(())
        })();
        drive.close();
        result?;
    }
    {
        let drive: &'static FileDrive = Box::leak(Box::new(FileDrive::open(output)?));
        let result = (|| -> Result<()> {
            let mbr = Mbr::read_from_device(drive)?;
            mbr.validate()?;
            if mbr.get_disk_signature() != XILA_DISK_SIGNATURE {
                return Err("invalid Xila signature".into());
            }
            let entry = mbr
                .partitions
                .iter()
                .find(|partition| partition.is_valid() && partition.kind == PartitionKind::Xila)
                .ok_or("missing Xila partition")?;
            let partition = create_partition_device(drive, entry)?;
            let file_system = little_fs::FileSystem::new(
                Box::leak(Box::new(partition)),
                little_fs::Options::new(LITTLE_FS_CACHE_SIZE),
            )?;
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
}
