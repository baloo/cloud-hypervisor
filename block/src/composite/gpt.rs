/// This creates a GPT partition table in ram according to the config
use std::{
    fmt, fs,
    io::{self, Cursor, ErrorKind, SeekFrom},
};

use gpt::{
    partition_types::{OperatingSystem, Type},
    GptConfig,
};
use uuid::Uuid;

use super::{Config, Sectors};

const LBA: usize = 512;

struct MockDisk {
    header: io::Cursor<[u8; 34 * LBA]>,
    trailer: io::Cursor<[u8; 34 * LBA]>,
    size: usize,
    position: usize,
}

impl MockDisk {
    fn new(size: Sectors) -> Self {
        Self {
            header: io::Cursor::new([0u8; 34 * LBA]),
            trailer: io::Cursor::new([0u8; 34 * LBA]),
            size: size.as_bytes() as usize,
            position: 0,
        }
    }

    fn trailer_start(&self) -> usize {
        self.size - self.trailer.get_ref().len()
    }

    #[inline]
    fn in_range_header(&self, position: u64) -> bool {
        (0..self.header.get_ref().len()).contains(&(position as usize))
    }

    #[inline]
    fn in_range_trailer(&self, position: u64) -> bool {
        (self.trailer_start()..self.size).contains(&(position as usize))
    }

    #[inline]
    fn in_range(&self, position: u64) -> bool {
        self.in_range_header(position) || self.in_range_trailer(position)
    }
}

impl fmt::Debug for MockDisk {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("MockDisk").finish()
    }
}

impl io::Read for MockDisk {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.in_range_header(self.position as u64) {
            self.header.set_position(self.position as u64);

            let len = self.header.read(buf)?;
            self.position += len;
            Ok(len)
        } else if self.in_range_trailer(self.position as u64) {
            self.trailer
                .set_position((self.position - self.trailer_start()) as u64);
            let len = self.trailer.read(buf)?;
            self.position += len;
            Ok(len)
        } else {
            todo!("return illegal read ...");
        }
    }
}

impl io::Seek for MockDisk {
    fn seek(&mut self, style: io::SeekFrom) -> io::Result<u64> {
        let (base_pos, offset) = match style {
            SeekFrom::Start(n) => {
                self.position = n as usize;
                return Ok(n);
            }
            SeekFrom::End(n) => (self.size, n),
            SeekFrom::Current(n) => (self.position, n),
        };

        match base_pos.checked_add_signed(offset as isize) {
            Some(n) => {
                self.position = n;
                Ok(self.position as u64)
            }
            None => Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid seek to a negative or overflowing position",
            )),
        }
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.position as u64)
    }
}

impl io::Write for MockDisk {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if (0..self.header.get_ref().len()).contains(&self.position) {
            self.header.set_position(self.position as u64);
            let len = self.header.write(buf)?;
            self.position += len;
            Ok(len)
        } else if (self.trailer_start()..self.size).contains(&self.position) {
            self.trailer
                .set_position((self.position - self.trailer_start()) as u64);
            let len = self.trailer.write(buf)?;
            self.position += len;
            Ok(len)
        } else {
            todo!("return illegal read ...");
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub struct PartitionTable {
    inner: MockDisk,
}

impl fmt::Debug for PartitionTable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PartitionTable").finish()
    }
}

impl PartitionTable {
    pub fn new(config: &Config) -> Self {
        let mut mem_device = MockDisk::new(config.size);

        let mbr = gpt::mbr::ProtectiveMBR::with_lb_size(
            u32::try_from((config.size.as_bytes() / 512) - 1).unwrap_or(0xFF_FF_FF_FF),
        );
        mbr.overwrite_lba0(&mut mem_device)
            .expect("failed to write MBR");

        let mut disk = GptConfig::new()
            .writable(true)
            .logical_block_size(gpt::disk::DEFAULT_SECTOR_SIZE)
            .create_from_device(mem_device, None)
            .expect("failed to open disk");

        let mut id = 0;
        for p in &config.partitions {
            id += 1;
            let part_id = disk
                .add_partition_at(
                    p.name.as_str(),
                    id,
                    p.start.as_lba(),
                    p.size.as_lba(),
                    Type {
                        guid: Uuid::parse_str(&format!("{}", p.partition_type)).unwrap(),
                        os: OperatingSystem::None,
                    },
                    p.flags.0,
                )
                .unwrap();

            let mut parts = disk.take_partitions();
            parts.get_mut(&part_id).unwrap().part_guid =
                Uuid::parse_str(&format!("{}", p.unique_id)).unwrap();
            disk.update_partitions(parts);
        }

        let inner = disk.write().unwrap();

        Self { inner }
    }

    pub(crate) fn in_range(&self, position: u64) -> bool {
        self.inner.in_range(position)
    }

    pub(crate) fn trailer_start(&self) -> usize {
        self.inner.trailer_start()
    }
}

impl io::Read for PartitionTable {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl io::Write for PartitionTable {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        return Err(io::ErrorKind::Unsupported.into());
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl PartitionTable {
    #[inline]
    pub(super) fn position(&self) -> u64 {
        self.inner.position as u64
    }
}

impl io::Seek for PartitionTable {
    fn seek(&mut self, style: SeekFrom) -> io::Result<u64> {
        self.inner.seek(style)
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        Ok(self.position())
    }
}
