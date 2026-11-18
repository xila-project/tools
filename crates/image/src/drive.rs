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
/// [`FileDrive::close`] releases the file even when the drive itself must stay
/// alive, as LittleFS requires `'static` devices.
pub struct FileDrive(Mutex<Option<File>>);

impl FileDrive {
    /// Block size of the drive, in bytes.
    pub const BLOCK_SIZE: u32 = 512;

    /// Open an existing file for reading and writing.
    pub fn open(path: &Path) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self(Mutex::new(Some(file))))
    }

    /// Close the file. Later operations fail with [`Error::NotMounted`].
    pub fn close(&self) {
        if let Ok(mut file) = self.0.lock() {
            file.take();
        }
    }

    fn with_file<T>(
        &self,
        operation: impl FnOnce(&mut File) -> io::Result<T>,
    ) -> file_system::Result<T> {
        let mut guard: MutexGuard<'_, Option<File>> =
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
        file_system::ready(|| {
            self.with_file(|file| {
                file.seek(SeekFrom::Start(position))?;
                file.read(buffer)
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
                file.seek(SeekFrom::Start(position))?;
                file.write(buffer)
            })
        })
    }

    fn flush(&self, _: &mut Context<'_>) -> Poll<file_system::Result<()>> {
        file_system::ready(|| self.with_file(|file| file.sync_all()))
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
                let length =
                    self.with_file(|file| file.metadata().map(|metadata| metadata.len()))?;
                *GET_BLOCK_COUNT::cast_output(output)? =
                    (length / Size::from(Self::BLOCK_SIZE)) as _;
                Ok(())
            }
            _ => Err(Error::UnsupportedOperation),
        })
    }
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
    fn opening_a_missing_file_fails_without_creating_it() {
        let path = temporary_file(0);
        std::fs::remove_file(&path).unwrap();
        assert!(FileDrive::open(&path).is_err());
        assert!(!path.exists());
    }
}
