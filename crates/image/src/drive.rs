//! A block device backed by a host file, for building images on the host.

use file_system::{
    ControlCommand, ControlCommandIdentifier, DirectBaseOperations, DirectBlockDevice, Error,
    MountOperations, Size,
    block_device::{GET_BLOCK_COUNT, GET_BLOCK_SIZE},
};
use shared::AnyByLayout;
use std::{
    fs::{File, OpenOptions},
    io::{self, Read, Seek, SeekFrom, Write},
    path::Path,
    sync::{Mutex, MutexGuard},
    task::{Context, Poll},
};

/// Block device over an existing host file, with 512-byte blocks.
///
/// The file is never created or resized: create it with its final size first.
/// Reads and writes outside the file fail with [`Error::InvalidParameter`]
/// instead of being shortened or extending it, and they transfer the whole
/// buffer or fail. [`FileDrive::close`] releases the file even when the drive
/// itself must stay alive, as LittleFS requires `'static` devices.
pub struct FileDrive(Mutex<Option<OpenFile>>);

/// The open file and its length, fixed while the drive is open.
struct OpenFile {
    file: File,
    length: u64,
}

impl OpenFile {
    /// Seek to `position`, checking that `length` bytes from there stay
    /// within the file.
    fn seek_within(&mut self, position: Size, length: usize) -> io::Result<()> {
        let end = u64::try_from(length)
            .ok()
            .and_then(|length| position.checked_add(length));
        if end.is_none_or(|end| end > self.length) {
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
    /// Block size of the drive, in bytes.
    pub const BLOCK_SIZE: u32 = 512;

    /// Size of the stack buffer that [`DirectBaseOperations::write_pattern`]
    /// fills and writes repeatedly.
    const PATTERN_BUFFER_SIZE: usize = 4096;

    /// Open an existing file for reading and writing.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        let length = file.metadata()?.len();
        Ok(Self(Mutex::new(Some(OpenFile { file, length }))))
    }

    /// Close the file. Later operations fail with [`Error::NotMounted`].
    pub fn close(&self) {
        if let Ok(mut file) = self.0.lock() {
            file.take();
        }
    }

    fn with_file<T>(
        &self,
        operation: impl FnOnce(&mut OpenFile) -> io::Result<T>,
    ) -> file_system::Result<T> {
        let mut guard: MutexGuard<'_, Option<OpenFile>> =
            self.0.lock().map_err(|_| Error::RessourceBusy)?;
        let file = guard.as_mut().ok_or(Error::NotMounted)?;
        operation(file).map_err(map_error)
    }
}

fn map_error(error: io::Error) -> Error {
    match error.kind() {
        io::ErrorKind::PermissionDenied => Error::PermissionDenied,
        io::ErrorKind::NotFound => Error::NotFound,
        io::ErrorKind::AlreadyExists => Error::AlreadyExists,
        io::ErrorKind::InvalidInput => Error::InvalidParameter,
        io::ErrorKind::InvalidData => Error::InvalidFile,
        _ => Error::Unknown,
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
                file.seek_within(position, buffer.len())?;
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
                file.seek_within(position, buffer.len())?;
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
        file_system::ready(|| {
            if pattern.is_empty() || count == 0 {
                return Ok(0);
            }
            let length = pattern
                .len()
                .checked_mul(count)
                .ok_or(Error::InvalidParameter)?;
            // Whole repetitions of the pattern, so that every chunk starts
            // with its first byte.
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
                file.seek_within(position, length)?;
                let mut remaining = length;
                while remaining > 0 {
                    let size = remaining.min(chunk.len());
                    file.file.write_all(&chunk[..size])?;
                    remaining -= size;
                }
                Ok(length)
            })
        })
    }

    fn flush(&self, _: &mut Context<'_>) -> Poll<file_system::Result<()>> {
        file_system::ready(|| self.with_file(|file| file.file.sync_all()))
    }

    fn control(
        &self,
        _: &mut Context<'_>,
        command: ControlCommandIdentifier,
        _: &AnyByLayout,
        output: &mut AnyByLayout,
    ) -> Poll<file_system::Result<()>> {
        file_system::ready(|| match command {
            GET_BLOCK_SIZE::IDENTIFIER => {
                *GET_BLOCK_SIZE::cast_output(output)? = Self::BLOCK_SIZE;
                Ok(())
            }
            GET_BLOCK_COUNT::IDENTIFIER => {
                let length = self.with_file(|file| Ok(file.length))?;
                *GET_BLOCK_COUNT::cast_output(output)? = block_count(length)?;
                Ok(())
            }
            _ => Err(Error::UnsupportedOperation),
        })
    }
}

/// Number of whole blocks in `length` bytes, which must fit the `u32` that
/// Core's block devices report.
fn block_count(length: u64) -> file_system::Result<u32> {
    u32::try_from(length / Size::from(FileDrive::BLOCK_SIZE)).map_err(|_| Error::InvalidParameter)
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
            Err(Error::UnsupportedOperation)
        );
        drive.unmount().unwrap();
        assert_eq!(drive.read_sync(&mut buffer, 0), Err(Error::NotMounted));
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn accesses_never_extend_the_file_or_return_short_counts() {
        let path = temporary_file(4 * 512);
        let drive = FileDrive::open(&path).unwrap();
        let mut buffer = [0; 8];
        assert_eq!(
            drive.read_sync(&mut buffer, 2044),
            Err(Error::InvalidParameter)
        );
        assert_eq!(
            drive.read_sync(&mut buffer, u64::MAX),
            Err(Error::InvalidParameter)
        );
        assert_eq!(
            drive.write_sync(&buffer, 2041),
            Err(Error::InvalidParameter)
        );
        assert_eq!(
            drive.write_pattern_sync(&[0xFF], 513, 3 * 512),
            Err(Error::InvalidParameter)
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
        let block = u64::from(FileDrive::BLOCK_SIZE);
        assert_eq!(block_count(u64::from(u32::MAX) * block), Ok(u32::MAX));
        assert_eq!(
            block_count((u64::from(u32::MAX) + 1) * block - 1),
            Ok(u32::MAX)
        );
        assert_eq!(
            block_count((u64::from(u32::MAX) + 1) * block),
            Err(Error::InvalidParameter)
        );
    }

    #[test]
    fn opening_a_missing_file_fails_without_creating_it() {
        let path = temporary_file(0);
        std::fs::remove_file(&path).unwrap();
        assert!(FileDrive::open(&path).is_err());
        assert!(!path.exists());
    }
}
