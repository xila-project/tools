//! A block device backed by a host file, for building images on the host.

use file_system::{
    ControlCommand, ControlCommandIdentifier, DirectBaseOperations, DirectBlockDevice, IoErrorKind,
    MountOperations, Size,
    block_device::{ERASE, GET_BLOCK_COUNT, GET_BLOCK_SIZE},
};
use shared::AnyByLayout;
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Mutex, MutexGuard},
    task::{Context, Poll},
};

/// Block device over an existing host file, with 512-byte blocks unless
/// opened with [`FileDrive::open_with_block_size`].
///
/// The file is never created or resized: create it with its final size first.
/// Reads and writes outside the file fail with [`IoErrorKind::InvalidArgument`]
/// instead of being shortened or extending it, and they transfer the whole
/// buffer or fail. [`FileDrive::close`] releases the file even when the drive
/// itself must stay alive, as LittleFS requires `'static` devices.
///
/// The drive rewrites in place and does not support erasing, unless
/// [`FileDrive::with_flash_erase`] makes it erase like NOR flash.
pub struct FileDrive {
    file: Mutex<Option<OpenFile>>,
    block_size: u32,
    flash_erase: bool,
}

/// The open file and its length, fixed while the drive is open.
struct OpenFile {
    file: File,
    length: u64,
}

impl OpenFile {
    /// Seek to `position`, checking that `length` bytes from there stay
    /// within the file.
    fn seek_within(&mut self, position: Size, length: Size) -> io::Result<()> {
        if position
            .checked_add(length)
            .is_none_or(|end| end > self.length)
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "access outside the drive",
            ));
        }
        self.file.seek(SeekFrom::Start(position))?;
        Ok(())
    }
}

impl FileDrive {
    /// Default block size of the drive, in bytes: the sector size of MBR disks.
    pub const BLOCK_SIZE: u32 = 512;

    /// Value of erased NOR flash bytes, written by [`FileDrive::with_flash_erase`].
    pub const ERASED_FLASH_VALUE: u8 = 0xFF;

    /// Size of the stack buffer that [`DirectBaseOperations::write_pattern`]
    /// fills and writes repeatedly.
    const PATTERN_BUFFER_SIZE: usize = 4096;

    /// Open an existing file for reading and writing, with 512-byte blocks.
    ///
    /// The file length need not be a multiple of the block size: the drive
    /// reports whole blocks only.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let length = file.metadata()?.len();
        Ok(Self {
            file: Mutex::new(Some(OpenFile { file, length })),
            block_size: Self::BLOCK_SIZE,
            flash_erase: false,
        })
    }

    /// Open an existing file for reading and writing, with `block_size`-byte
    /// blocks.
    ///
    /// The block size must be a power of two, and the file length a multiple
    /// of it; otherwise this fails with [`io::ErrorKind::InvalidInput`].
    pub fn open_with_block_size(path: &Path, block_size: u32) -> io::Result<Self> {
        if !block_size.is_power_of_two() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "the block size must be a power of two",
            ));
        }
        let mut drive = Self::open(path)?;
        drive.block_size = block_size;
        let file = drive.file.get_mut().ok().and_then(|file| file.as_ref());
        if file.is_none_or(|file| !file.length.is_multiple_of(u64::from(block_size))) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("the drive length must be a multiple of the block size ({block_size})"),
            ));
        }
        Ok(drive)
    }

    /// Support erasing like NOR flash: erasing whole blocks sets their bytes
    /// to [`FileDrive::ERASED_FLASH_VALUE`].
    ///
    /// LittleFS then writes the same bytes as on a flash partition, so an
    /// image written to flash can be appended to there.
    pub fn with_flash_erase(mut self) -> Self {
        self.flash_erase = true;
        self
    }

    /// Block size of the drive, in bytes.
    pub fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Close the file. Later operations fail with [`IoErrorKind::DeviceUnavailable`].
    pub fn close(&self) {
        if let Ok(mut file) = self.file.lock() {
            file.take();
        }
    }

    fn with_file<T>(
        &self,
        operation: impl FnOnce(&mut OpenFile) -> io::Result<T>,
    ) -> file_system::Result<T> {
        let mut guard: MutexGuard<'_, Option<OpenFile>> =
            self.file.lock().map_err(|_| IoErrorKind::ResourceBusy)?;
        let file = guard.as_mut().ok_or(IoErrorKind::DeviceUnavailable)?;
        operation(file).map_err(io_error::map_std_error)
    }

    /// Write `pattern` `count` times from `position`.
    fn fill(&self, pattern: &[u8], count: usize, position: Size) -> file_system::Result<usize> {
        if pattern.is_empty() || count == 0 {
            return Ok(0);
        }
        let length = pattern
            .len()
            .checked_mul(count)
            .ok_or(IoErrorKind::InvalidArgument)?;
        // Whole repetitions of the pattern, so that every chunk starts with
        // its first byte.
        let mut buffer = [0_u8; Self::PATTERN_BUFFER_SIZE];
        let chunk = if pattern.len() <= buffer.len() {
            let repetitions = (buffer.len() / pattern.len()).min(count);
            let chunk = &mut buffer[..repetitions * pattern.len()];
            for slot in chunk.chunks_exact_mut(pattern.len()) {
                slot.copy_from_slice(pattern);
            }
            &buffer[..repetitions * pattern.len()]
        } else {
            pattern
        };
        self.with_file(|file| {
            file.seek_within(position, length as Size)?;
            let mut remaining = length;
            while remaining > 0 {
                let size = remaining.min(chunk.len());
                file.file.write_all(&chunk[..size])?;
                remaining -= size;
            }
            Ok(length)
        })
    }

    /// Erase whole blocks like NOR flash.
    ///
    /// The range is checked against the open file first, even when empty, and
    /// erased in chunks, so that its size is not limited by `usize`.
    fn erase_range(&self, position: Size, size: Size) -> file_system::Result<()> {
        let block_size = Size::from(self.block_size);
        if !position.is_multiple_of(block_size) || !size.is_multiple_of(block_size) {
            return Err(IoErrorKind::InvalidArgument);
        }
        self.with_file(|file| {
            file.seek_within(position, size)?;
            let erased = [Self::ERASED_FLASH_VALUE; Self::PATTERN_BUFFER_SIZE];
            let mut remaining = size;
            while remaining > 0 {
                let chunk = remaining.min(erased.len() as Size);
                file.file.write_all(&erased[..chunk as usize])?;
                remaining -= chunk;
            }
            Ok(())
        })
    }
}

impl DirectBaseOperations for FileDrive {
    fn read(
        &self,
        _: &mut Context<'_>,
        buffer: &mut [u8],
        position: Size,
    ) -> Poll<file_system::Result<usize>> {
        // `read_exact` and `write_all` retry `ErrorKind::Interrupted`.
        file_system::ready(|| {
            self.with_file(|file| {
                file.seek_within(position, buffer.len() as Size)?;
                file.file.read_exact(buffer)?;
                Ok(buffer.len())
            })
        })
    }

    fn write(
        &self,
        _: &mut Context<'_>,
        buffer: &[u8],
        position: Size,
    ) -> Poll<file_system::Result<usize>> {
        file_system::ready(|| {
            self.with_file(|file| {
                file.seek_within(position, buffer.len() as Size)?;
                file.file.write_all(buffer)?;
                Ok(buffer.len())
            })
        })
    }

    fn write_pattern(
        &self,
        _: &mut Context<'_>,
        pattern: &[u8],
        count: usize,
        position: Size,
    ) -> Poll<file_system::Result<usize>> {
        file_system::ready(|| self.fill(pattern, count, position))
    }

    fn flush(&self, _: &mut Context<'_>) -> Poll<file_system::Result<()>> {
        file_system::ready(|| self.with_file(|file| file.file.sync_all()))
    }

    fn control(
        &self,
        _: &mut Context<'_>,
        command: ControlCommandIdentifier,
        input: &AnyByLayout,
        output: &mut AnyByLayout,
    ) -> Poll<file_system::Result<()>> {
        file_system::ready(|| match command {
            GET_BLOCK_SIZE::IDENTIFIER => {
                *GET_BLOCK_SIZE::cast_output(output)? = self.block_size;
                Ok(())
            }
            GET_BLOCK_COUNT::IDENTIFIER => {
                let length = self.with_file(|file| Ok(file.length))?;
                *GET_BLOCK_COUNT::cast_output(output)? = block_count(length, self.block_size)?;
                Ok(())
            }
            ERASE::IDENTIFIER if self.flash_erase => {
                let range = ERASE::cast_input(input)?;
                self.erase_range(range.position, range.size)
            }
            _ => Err(IoErrorKind::Unsupported),
        })
    }
}

/// Number of whole `block_size`-byte blocks in `length` bytes, which must fit
/// the `u32` that Core's block devices report.
fn block_count(length: u64, block_size: u32) -> file_system::Result<u32> {
    u32::try_from(length / Size::from(block_size)).map_err(|_| IoErrorKind::InvalidArgument)
}

impl MountOperations for FileDrive {
    fn unmount(&self) -> file_system::Result<()> {
        self.close();
        Ok(())
    }
}

impl DirectBlockDevice for FileDrive {}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_file(size: u64) -> std::path::PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "xila-tools-drive-{}-{}.img",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        File::create(&path).unwrap().set_len(size).unwrap();
        path
    }

    #[test]
    fn reads_back_writes_and_reports_geometry() {
        let path = temporary_file(16 * 512);
        let drive = FileDrive::open(&path).unwrap();
        assert_eq!(drive.write_sync(b"Xila", 1000).unwrap(), 4);
        let mut buffer = [0; 4];
        assert_eq!(drive.read_sync(&mut buffer, 1000).unwrap(), 4);
        assert_eq!(&buffer, b"Xila");
        drive.flush_sync().unwrap();
        assert_eq!(drive.get_block_size().unwrap(), 512);
        assert_eq!(drive.get_block_count().unwrap(), 16);
        let mut output = 0_u32;
        assert_eq!(
            drive.control_sync(
                file_system::block_device::ERASE::IDENTIFIER,
                AnyByLayout::NONE,
                AnyByLayout::from_mutable(&mut output),
            ),
            Err(IoErrorKind::Unsupported)
        );
        drive.unmount().unwrap();
        assert_eq!(
            drive.read_sync(&mut buffer, 0),
            Err(IoErrorKind::DeviceUnavailable)
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn accesses_never_extend_the_file_or_return_short_counts() {
        let path = temporary_file(4 * 512);
        let drive = FileDrive::open(&path).unwrap();
        let mut buffer = [0; 8];
        assert_eq!(
            drive.read_sync(&mut buffer, 2044),
            Err(IoErrorKind::InvalidArgument)
        );
        assert_eq!(
            drive.read_sync(&mut buffer, u64::MAX),
            Err(IoErrorKind::InvalidArgument)
        );
        assert_eq!(
            drive.write_sync(&buffer, 2041),
            Err(IoErrorKind::InvalidArgument)
        );
        assert_eq!(
            drive.write_pattern_sync(&[0xFF], 513, 3 * 512),
            Err(IoErrorKind::InvalidArgument)
        );
        assert_eq!(drive.write_sync(&buffer, 2040).unwrap(), 8);
        assert_eq!(drive.read_sync(&mut buffer, 2040).unwrap(), 8);
        drive.unmount().unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 4 * 512);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn write_pattern_matches_repeated_writes() {
        let path = temporary_file(32 * 512);
        let drive = FileDrive::open(&path).unwrap();
        // One byte, a block, a pattern that does not divide the buffer, and a
        // pattern larger than the buffer.
        let cases: [(Vec<u8>, usize, u64); 4] = [
            (vec![0x00], 512, 512),
            (vec![0xA5; 512], 3, 1024),
            ((1..=7).collect(), 1000, 3000),
            ((0..=255).cycle().take(5000).collect(), 2, 4000),
        ];
        for (pattern, count, position) in cases {
            std::fs::write(&path, vec![0x11; 32 * 512]).unwrap();
            assert_eq!(
                drive.write_pattern_sync(&pattern, count, position).unwrap(),
                pattern.len() * count
            );
            let mut expected = vec![0x11; 32 * 512];
            let start = position as usize;
            expected[start..start + pattern.len() * count].copy_from_slice(&pattern.repeat(count));
            assert_eq!(std::fs::read(&path).unwrap(), expected);
        }
        assert_eq!(drive.write_pattern_sync(&[], 10, 0).unwrap(), 0);
        assert_eq!(drive.write_pattern_sync(&[1], 0, 0).unwrap(), 0);
        drive.unmount().unwrap();
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn block_counts_beyond_u32_are_rejected() {
        for block_size in [FileDrive::BLOCK_SIZE, 4096] {
            let block = u64::from(block_size);
            assert_eq!(
                block_count(u64::from(u32::MAX) * block, block_size),
                Ok(u32::MAX)
            );
            assert_eq!(
                block_count((u64::from(u32::MAX) + 1) * block - 1, block_size),
                Ok(u32::MAX)
            );
            assert_eq!(
                block_count((u64::from(u32::MAX) + 1) * block, block_size),
                Err(IoErrorKind::InvalidArgument)
            );
        }
    }

    #[test]
    fn chosen_block_sizes_are_reported_and_validated() {
        let path = temporary_file(4 * 4096);
        let drive = FileDrive::open_with_block_size(&path, 4096).unwrap();
        assert_eq!(drive.block_size(), 4096);
        assert_eq!(drive.get_block_size().unwrap(), 4096);
        assert_eq!(drive.get_block_count().unwrap(), 4);
        drive.unmount().unwrap();
        for invalid in [0, 3000] {
            let error = FileDrive::open_with_block_size(&path, invalid)
                .err()
                .unwrap();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
        std::fs::remove_file(path).unwrap();

        let path = temporary_file(4 * 4096 + 512);
        let error = FileDrive::open_with_block_size(&path, 4096).err().unwrap();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            FileDrive::open(&path).unwrap().get_block_count().unwrap(),
            33
        );
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn flash_erase_sets_whole_blocks_to_the_erased_value() {
        let path = temporary_file(4 * 4096);
        let drive = FileDrive::open_with_block_size(&path, 4096)
            .unwrap()
            .with_flash_erase();
        drive.erase(4096, 2 * 4096).unwrap();
        drive.erase(4 * 4096, 0).unwrap();
        for (position, size) in [
            (100, 4096),
            (4096, 100),
            (3 * 4096, 2 * 4096),
            (5 * 4096, 0),
            (4096, u64::MAX - 4095),
        ] {
            assert_eq!(
                drive.erase(position, size),
                Err(IoErrorKind::InvalidArgument),
                "{position} {size}"
            );
        }
        drive.unmount().unwrap();
        assert_eq!(drive.erase(0, 0), Err(IoErrorKind::DeviceUnavailable));
        let mut expected = vec![0; 4 * 4096];
        expected[4096..3 * 4096].fill(FileDrive::ERASED_FLASH_VALUE);
        assert_eq!(std::fs::read(&path).unwrap(), expected);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn opening_a_missing_file_fails_without_creating_it() {
        let path = temporary_file(0);
        std::fs::remove_file(&path).unwrap();
        assert!(FileDrive::open(&path).is_err());
        assert!(!path.exists());
    }
}
