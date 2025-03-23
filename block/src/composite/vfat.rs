use std::{
    fmt,
    fs::File,
    io::{self, Cursor, SeekFrom},
    path::Path,
};

use fatfs::{
    format_volume, Date, DateTime, FileSystem, FormatVolumeOptions, FsOptions, Time, TimeProvider,
};
use relative_path::PathExt;
use walkdir::WalkDir;

use crate::BlockBackend;

/// a DOS compatible epoch (1980-05-01 00:00:00)
///
/// This will return a static datetime when queried
#[derive(Debug)]
pub struct Epoch;

impl TimeProvider for Epoch {
    fn get_current_date(&self) -> Date {
        Date {
            year: 1980,
            month: 5,
            day: 1,
        }
    }
    fn get_current_date_time(&self) -> DateTime {
        DateTime {
            date: self.get_current_date(),
            time: Time {
                hour: 0,
                min: 0,
                sec: 0,
                millis: 0,
            },
        }
    }
}

static EPOCH: Epoch = Epoch;

pub struct Vfat {
    inner: Cursor<Vec<u8>>,
}

impl fmt::Debug for Vfat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Vfat").finish()
    }
}

impl Vfat {
    pub fn new(root: &Path, size: usize) -> Result<Self, ()> {
        info!("Creating vfat size: {size}");
        let dev = vec![0; size];
        let mut dev = Cursor::new(dev);

        format_volume(&mut dev, FormatVolumeOptions::new()).unwrap();

        let options = FsOptions::new()
            .update_accessed_date(false)
            .time_provider(&EPOCH);

        {
            let fs = FileSystem::new(&mut dev, options).unwrap();

            let root_dir = fs.root_dir();

            for entry in WalkDir::new(root).into_iter().filter_map(|e| e.ok()) {
                let rel = entry.path().relative_to(root).unwrap();
                if rel.as_str().is_empty() {
                    continue;
                }

                let metadata = entry.metadata().unwrap();
                if metadata.is_dir() {
                    root_dir.create_dir(rel.as_str()).unwrap();
                } else if metadata.is_file() {
                    let mut source = File::open(entry.path()).unwrap();
                    let mut f = root_dir.create_file(rel.as_str()).unwrap();
                    io::copy(&mut source, &mut f).unwrap();
                }
            }
        }

        Ok(Self { inner: dev })
    }
}

impl io::Read for Vfat {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl io::Write for Vfat {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        todo!();
        return Err(io::ErrorKind::Unsupported.into());
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl Vfat {
    #[inline]
    pub(super) fn position(&self) -> u64 {
        self.inner.position()
    }
}

impl io::Seek for Vfat {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        let (base_pos, offset) = match style {
            SeekFrom::Start(n) => {
                self.inner.set_position(n);
                return Ok(n);
            }
            SeekFrom::End(n) => (self.inner.get_ref().len() as u64, n),
            SeekFrom::Current(n) => (self.position(), n),
        };

        match base_pos.checked_add_signed(offset) {
            Some(n) => {
                self.inner.set_position(n);
                Ok(self.position())
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            )),
        }
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.position())
    }
}

impl BlockBackend for Vfat {
    fn size(&self) -> std::result::Result<u64, crate::Error> {
        Ok(self.inner.get_ref().len() as u64)
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    #[test]
    fn test_vfat() {
        let fs = Vfat::new(
            PathBuf::from(
                "/home/arthur_gautier/work/dev/anans-host/packages/abr-bootloader/target/run/disk",
            )
            .as_path(),
            52428800,
        )
        .unwrap();
    }
}
