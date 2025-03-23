use std::{
    collections::VecDeque,
    fmt,
    fs::File,
    io::{self, Read, Seek, SeekFrom},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};

use serde::{Deserialize, Serialize};
use thiserror::Error;
use uguid::Guid;
use vmm_sys_util::eventfd::EventFd;

use crate::{
    async_io::{DiskFile, DiskFileError, DiskFileResult},
    AsyncAdaptor, AsyncIo, AsyncIoError, AsyncIoResult, BlockBackend, DiskTopology,
};

mod gpt;
mod vfat;

use self::{gpt::PartitionTable, vfat::Vfat};

const MAGIC: &[u8] = b"# want a cookie? have a composite disk";

/// Size of an element in sectors
#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq, PartialOrd)]
pub struct Sectors(pub u64);

impl fmt::Display for Sectors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.4k", self.0)
    }
}

impl Sectors {
    pub const GPT_SECTOR_SIZE: u64 = 0x01 << 12;

    /// Size of an element in bytes
    pub fn as_bytes(&self) -> u64 {
        Self::GPT_SECTOR_SIZE * self.0
    }

    /// LBA (logical based address)
    pub fn as_lba(&self) -> u64 {
        const RATIO: u64 = Sectors::GPT_SECTOR_SIZE / crate::SECTOR_SIZE;
        self.0 * RATIO
    }
}

/// Determine image type through file parsing.
pub fn is_composite(f: &mut File) -> io::Result<bool> {
    f.seek(SeekFrom::Start(0))?;
    let mut f = helpers::FileReset(f);

    let mut buf = [0u8; MAGIC.len()];

    if f.as_mut().read_exact(&mut buf[..]).is_err() {
        return Ok(false);
    }

    if &buf[..] == MAGIC {
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Configuration for a composite drive
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Config {
    /// Overall size of the drive
    size: Sectors,
    partitions: Vec<Partition>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PartitionName(String);

impl PartitionName {
    pub fn as_str(&self) -> &str {
        self.0.as_str()
    }
}

#[derive(Copy, Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct PartitionFlags(u64);

/// Configuration for a partition
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Partition {
    /// Start offset of a partition
    start: Sectors,
    /// Size of the partition
    size: Sectors,

    /// Type of the partition
    /// ESP is C12A7328-F81F-11D2-BA4B-00A0C93EC93B
    partition_type: Guid,
    /// Unique ID of the partition
    unique_id: Guid,
    flags: PartitionFlags,
    name: PartitionName,
    readonly: bool,

    inner: PartitionBackend,
}

/// Backend of the partition
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum PartitionBackend {
    Vfat { path: PathBuf },
    Raw { path: PathBuf },
}

/// Composite disk
#[derive(Debug)]
pub struct Composite {
    config: Config,

    position: u64,
    gpt_header: PartitionTable,
    parts: Vec<Part>,
}

impl Composite {
    pub fn new(mut file: File) -> Result<Self, CompositeError> {
        let mut content = String::new();
        file.read_to_string(&mut content)
            .map_err(CompositeError::ConfigRead)?;

        let config: Config = toml::from_str(&content)?;

        Self::new_from_config(config)
    }

    fn new_from_config(config: Config) -> Result<Self, CompositeError> {
        let gpt_header = PartitionTable::new(&config);

        let mut parts = vec![];
        let mut last_position = Sectors(0);

        for p in &config.partitions {
            if p.start <= last_position {
                return Err(CompositeError::OutOfOrderPartition {
                    partition: p.clone(),
                    last_position,
                });
            }

            let underlying = match &p.inner {
                PartitionBackend::Vfat { path } => {
                    Box::new(Vfat::new(&path, p.size.as_bytes() as usize).unwrap())
                }
                _ => todo!(),
            };

            parts.push(Part {
                offset: p.start,
                size: p.size,
                readonly: p.readonly,
                underlying,
            })
        }

        let mut out = Self {
            config,
            position: 0,
            gpt_header,
            parts,
        };

        //debug!("Copying ...");
        //let mut output = File::create("/tmp/foo").unwrap();
        //io::copy(&mut out, &mut output).unwrap();
        //debug!("Copied");

        Ok(out)
    }
}

impl BlockBackend for Composite {
    fn size(&self) -> std::result::Result<u64, crate::Error> {
        Ok(self.config.size.as_bytes())
    }
}

impl io::Read for Composite {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        debug!(
            "Composite::read position={position} len={len}",
            position = self.position,
            len = buf.len()
        );
        for part in &mut self.parts {
            if part.in_range(self.position) {
                part.seek(SeekFrom::Start(self.position));

                let out = part.read(buf)?;
                self.position += out as u64;
                return Ok(out);
            }
        }

        if self.gpt_header.in_range(self.position) {
            self.gpt_header.seek(SeekFrom::Start(self.position))?;
            let out = self.gpt_header.read(buf)?;
            self.position += out as u64;
            return Ok(out);
        }

        if self.position >= self.config.size.as_bytes() {
            return Ok(0);
        }

        let mut end: Option<u64> = None;
        for part in &self.parts {
            if part.offset.as_bytes() > self.position {
                end = Some(part.offset.as_bytes());
                break;
            }
        }
        let end = end.unwrap_or(self.gpt_header.trailer_start() as u64);
        let len = end - self.position;

        let to_write = buf.len().min(len as usize);
        buf[..to_write].fill(0);
        self.position += to_write as u64;

        Ok(to_write)
    }
}

impl io::Write for Composite {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Writes are not supported
        Ok(0)
    }

    fn flush(&mut self) -> io::Result<()> {
        for part in &mut self.parts {
            part.flush()?
        }
        Ok(())
    }
}

impl io::Seek for Composite {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        debug!("Composite::seek {style:?}",);

        let (base_pos, offset) = match style {
            SeekFrom::Start(n) => {
                self.position = n;
                return Ok(n);
            }
            SeekFrom::End(n) => (self.config.size.as_bytes(), n),
            SeekFrom::Current(n) => (self.position, n),
        };

        match base_pos.checked_add_signed(offset) {
            Some(n) => {
                self.position = n;
                Ok(self.position)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            )),
        }
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.position)
    }
}

impl DiskFile for Composite {
    fn size(&mut self) -> DiskFileResult<u64> {
        Ok(self.config.size.as_bytes())
    }

    fn new_async_io(&self, _ring_depth: u32) -> DiskFileResult<Box<dyn AsyncIo>> {
        let composite = Self::new_from_config(self.config.clone())
            .map(Mutex::new)
            .map(Arc::new)
            .unwrap();

        Ok(Box::new(CompositeDisk::new(composite)) as Box<dyn AsyncIo>)
    }

    fn topology(&mut self) -> DiskTopology {
        warn!("Device topology not implemented. Using default topology");
        DiskTopology::default()
    }
}

/// Composite disk
#[derive(Debug)]
pub struct CompositeDisk {
    composite: Arc<Mutex<Composite>>,

    eventfd: EventFd,
    completion_list: VecDeque<(u64, i32)>,
}

impl CompositeDisk {
    fn new(composite: Arc<Mutex<Composite>>) -> Self {
        Self {
            composite,
            eventfd: EventFd::new(libc::EFD_NONBLOCK).expect("Failed creating EventFd for RawFile"),
            completion_list: VecDeque::new(),
        }
    }
}

impl AsyncAdaptor<Composite> for Arc<Mutex<Composite>> {
    fn file(&mut self) -> MutexGuard<Composite> {
        self.lock().unwrap()
    }
}

impl AsyncIo for CompositeDisk {
    fn notifier(&self) -> &EventFd {
        &self.eventfd
    }

    fn read_vectored(
        &mut self,
        offset: libc::off_t,
        iovecs: &[libc::iovec],
        user_data: u64,
    ) -> AsyncIoResult<()> {
        self.composite.read_vectored_sync(
            offset,
            iovecs,
            user_data,
            &self.eventfd,
            &mut self.completion_list,
        )
    }

    fn write_vectored(
        &mut self,
        offset: libc::off_t,
        iovecs: &[libc::iovec],
        user_data: u64,
    ) -> AsyncIoResult<()> {
        self.composite.write_vectored_sync(
            offset,
            iovecs,
            user_data,
            &self.eventfd,
            &mut self.completion_list,
        )
    }

    fn fsync(&mut self, user_data: Option<u64>) -> AsyncIoResult<()> {
        self.composite
            .fsync_sync(user_data, &self.eventfd, &mut self.completion_list)
    }

    fn next_completed_request(&mut self) -> Option<(u64, i32)> {
        self.completion_list.pop_front()
    }
}

/// An element of a composite disk
#[derive(Debug)]
struct Part {
    offset: Sectors,
    size: Sectors,
    readonly: bool,
    underlying: Box<dyn BlockBackend>,
}

impl Part {
    /// Is the specified offset within the range of this part.
    #[inline]
    fn in_range(&self, offset: u64) -> bool {
        let range = self.offset.as_bytes()..self.offset.as_bytes() + self.size.as_bytes();
        range.contains(&offset)
    }
}

impl io::Read for Part {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.underlying.read(buf)
    }
}

impl io::Write for Part {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.underlying.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.underlying.flush()
    }
}

impl io::Seek for Part {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        if let SeekFrom::Start(pos) = pos {
            self.underlying
                .seek(SeekFrom::Start(pos - self.offset.as_bytes()))
        } else {
            self.underlying.seek(pos)
        }
    }
}

#[derive(Debug, Error)]
pub enum CompositeError {
    #[error("Reading config: {0}")]
    ConfigRead(io::Error),
    #[error(transparent)]
    ConfigParse(#[from] toml::de::Error),

    #[error("Partition ({partition:?}) was out of order, last part ended at {last_position}")]
    OutOfOrderPartition {
        partition: Partition,
        last_position: Sectors,
    },
}

mod helpers {
    use std::{
        fs::File,
        io::{Seek, SeekFrom},
    };

    /// FileReset is a guard pattern to reset the file to the original location once we exit here.
    pub(super) struct FileReset<'f>(pub &'f mut File);

    impl<'f> AsMut<File> for FileReset<'f> {
        fn as_mut(&mut self) -> &mut File {
            &mut self.0
        }
    }

    impl<'f> Drop for FileReset<'f> {
        fn drop(&mut self) {
            let _ = self.0.seek(SeekFrom::Start(0));
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        io::{self, Read, Seek},
        path::PathBuf,
    };

    use tempfile::tempdir;
    use uguid::guid;

    use super::{
        Composite, Config, Partition, PartitionBackend, PartitionFlags, PartitionName, Sectors,
    };

    #[test]
    fn test_config() {
        let config: Config = toml::from_str(
            r#"
        size = 13107200

        [[partitions]]
        # ESP starts at 1MB and is 50MB
        start = 256
        size = 12800

        partition_type = "C12A7328-F81F-11D2-BA4B-00A0C93EC93B"
        unique_id = "68b8c820-29a7-404b-8952-d548c3963c84"
        flags = 0
        name = "esp0"
        readonly = true

        inner = { "type" = "vfat", "path" = "/path/to/backend" }
        "#,
        )
        .expect("parse config");

        assert_eq!(
            config,
            Config {
                size: Sectors(13107200),
                partitions: vec![Partition {
                    start: Sectors(256),
                    size: Sectors(12800),
                    partition_type: guid!("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
                    unique_id: guid!("68b8c820-29a7-404b-8952-d548c3963c84"),
                    flags: PartitionFlags(0),
                    name: PartitionName("esp0".to_string()),
                    readonly: true,
                    inner: PartitionBackend::Vfat {
                        path: PathBuf::from("/path/to/backend")
                    }
                }]
            }
        );
    }

    #[test]
    fn test_read_composite() {
        let esp = tempdir().expect("create tempdir");
        let mock_file = esp.path().join("dummy");
        fs::write(mock_file, b"dummy").expect("Write sample file");

        let config = Config {
            size: Sectors(50 * 1024 * 1024 / 4096),
            partitions: vec![
                Partition {
                    start: Sectors(256),
                    size: Sectors(512), // 2MB
                    partition_type: guid!("C12A7328-F81F-11D2-BA4B-00A0C93EC93B"),
                    unique_id: guid!("68b8c820-29a7-404b-8952-d548c3963c84"),
                    flags: PartitionFlags(0),
                    name: PartitionName("esp0".to_string()),
                    readonly: true,
                    inner: PartitionBackend::Vfat {
                        path: esp.path().to_path_buf(),
                    },
                },
                Partition {
                    start: Sectors(1024),
                    size: Sectors(512), // 2MB
                    partition_type: guid!("C12A7328-F81F-11D2-BA4B-00A0C93EC94B"),
                    unique_id: guid!("68b8c820-29a7-404b-8952-d548c3963c83"),
                    flags: PartitionFlags(0),
                    name: PartitionName("esp1".to_string()),
                    readonly: true,
                    inner: PartitionBackend::Vfat {
                        path: esp.path().to_path_buf(),
                    },
                },
            ],
        };

        let mut composite = Composite::new_from_config(config).expect("create composite file");

        let len = composite.seek(io::SeekFrom::End(0)).expect("Read length");
        assert_eq!(len, 50 * 1024 * 1024);

        composite
            .seek(io::SeekFrom::Start(0))
            .expect("read back to the start");
        assert_eq!(composite.position, 0);

        let mut buf = [0u8; 512];
        info!("===== Partition table");

        for _ in 0..34 {
            let read = composite.read(&mut buf[..]).expect("read partition header");
            assert_eq!(read, 512);
        }

        assert_eq!(composite.position, 34 * 512);
        assert_eq!(composite.gpt_header.position(), 34 * 512);

        info!("===== (void)");

        // Read the void between the partition table and the first partition
        for _ in 34..(2048/* 1MB */) {
            let read = composite.read(&mut buf[..]).expect("read partition header");
            assert_eq!(read, 512);
            assert_eq!(buf, [0u8; 512]);
        }

        assert_eq!(composite.position, 1024 * 1024);

        info!("===== ESP");

        // Read the partition (2MB long)
        for i in 0..4096 {
            assert_eq!(composite.position, 1024 * 1024 + i * 512);
            let read = composite.read(&mut buf[..]).expect("read partition header");

            assert_eq!(
                composite.parts[0]
                    .stream_position()
                    .expect("grab underlying stream position"),
                (i + 1) * 512,
                "underlying position mismatch"
            );
            assert_eq!(
                read, 512,
                "Read should return the full buffer (composite position: {}, i: {i})",
                composite.position
            );
            assert_eq!(
                composite.position,
                1024 * 1024 + (i + 1) * 512,
                "Position should bump"
            );
        }
        assert_eq!(composite.position, 3 * 1024 * 1024);

        info!("===== (void)");

        for i in (2048 + 4096)..(8192) {
            assert_eq!(composite.position, i * 512, "Position mismatch ({i})");
            let read = composite.read(&mut buf[..]).expect("read partition header");
            assert_eq!(
                read, 512,
                "Read should return the full buffer (composite position: {}, i: {i})",
                composite.position
            );
            assert_eq!(buf, [0u8; 512]);
        }

        // Read the partition (2MB long)
        for i in 8192..12288 {
            assert_eq!(composite.position, i * 512);
            let read = composite.read(&mut buf[..]).expect("read partition header");

            assert_eq!(
                composite.parts[1]
                    .stream_position()
                    .expect("grab underlying stream position"),
                (i - 8192 + 1) * 512,
                "underlying position mismatch"
            );
            assert_eq!(
                read, 512,
                "Read should return the full buffer (composite position: {}, i: {i})",
                composite.position
            );
        }

        info!("===== (void)");
        for i in 12288..((50 * 1024 * 1024 / 512) - 34) {
            assert_eq!(composite.position, i * 512, "Position mismatch ({i})");
            let read = composite.read(&mut buf[..]).expect("read partition header");
            assert_eq!(
                read, 512,
                "Read should return the full buffer (composite position: {}, i: {i})",
                composite.position
            );
            assert_eq!(buf, [0u8; 512]);
        }

        info!("===== backup partition");
        for i in ((50 * 1024 * 1024 / 512) - 34)..(50 * 1024 * 1024 / 512) {
            assert_eq!(composite.position, i * 512, "Position mismatch ({i})");
            let read = composite.read(&mut buf[..]).expect("read partition header");
            assert_eq!(
                read, 512,
                "Read should return the full buffer (composite position: {}, i: {i})",
                composite.position
            );
        }

        info!("===== reached the end");
        let read = composite.read(&mut buf[..]).expect("read partition header");
        assert_eq!(read, 0, "Reached the end of the disk",);
    }
}
