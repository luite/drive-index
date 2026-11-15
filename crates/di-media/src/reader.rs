//! `Read + Seek` over a file that prefetches the start and end in two reads and
//! serves parser accesses from memory where possible (PLAN.md §4.10). Other ranges
//! are read on demand, up to a cap, so a malformed file cannot make us read it all.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;

pub struct HeadTailReader {
    file: File,
    size: u64,
    pos: u64,
    head: Vec<u8>,
    tail: Vec<u8>,
    tail_start: u64,
    extra_read: u64,
    max_extra: u64,
    bytes_read: u64,
}

impl HeadTailReader {
    pub fn open(path: &Path, head_len: usize, tail_len: usize, max_extra: u64) -> io::Result<HeadTailReader> {
        let mut file = File::open(path)?;
        #[cfg(target_os = "macos")]
        unsafe {
            // Don't flood the page cache with data we read once.
            use std::os::fd::AsRawFd;
            libc::fcntl(file.as_raw_fd(), libc::F_NOCACHE, 1);
        }
        let size = file.metadata()?.len();
        let head_n = (head_len as u64).min(size) as usize;
        let mut head = vec![0u8; head_n];
        file.read_exact(&mut head)?;
        let mut bytes_read = head_n as u64;
        let (tail, tail_start) = if size > head_n as u64 {
            let start = size.saturating_sub(tail_len as u64).max(head_n as u64);
            let mut tail = vec![0u8; (size - start) as usize];
            file.seek(SeekFrom::Start(start))?;
            file.read_exact(&mut tail)?;
            bytes_read += tail.len() as u64;
            (tail, start)
        } else {
            (Vec::new(), size)
        };
        Ok(HeadTailReader { file, size, pos: 0, head, tail, tail_start, extra_read: 0, max_extra, bytes_read })
    }

    pub fn size(&self) -> u64 {
        self.size
    }
    pub fn head(&self) -> &[u8] {
        &self.head
    }
    pub fn tail(&self) -> &[u8] {
        if self.tail.is_empty() { &self.head } else { &self.tail }
    }
    pub fn bytes_read(&self) -> u64 {
        self.bytes_read
    }
    pub fn rewind_to_start(&mut self) {
        self.pos = 0;
    }
}

impl Read for HeadTailReader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.size || buf.is_empty() {
            return Ok(0);
        }
        let head_end = self.head.len() as u64;
        if self.pos < head_end {
            let n = ((head_end - self.pos) as usize).min(buf.len());
            buf[..n].copy_from_slice(&self.head[self.pos as usize..self.pos as usize + n]);
            self.pos += n as u64;
            return Ok(n);
        }
        if self.pos >= self.tail_start && !self.tail.is_empty() {
            let off = (self.pos - self.tail_start) as usize;
            let n = (self.tail.len() - off).min(buf.len());
            buf[..n].copy_from_slice(&self.tail[off..off + n]);
            self.pos += n as u64;
            return Ok(n);
        }
        // Between head and tail: real read, capped.
        let limit = (self.tail_start - self.pos) as usize;
        let want = buf.len().min(limit).min(1 << 20);
        if self.extra_read + want as u64 > self.max_extra {
            return Err(io::Error::other("read cap exceeded"));
        }
        self.file.seek(SeekFrom::Start(self.pos))?;
        let n = self.file.read(&mut buf[..want])?;
        self.pos += n as u64;
        self.extra_read += n as u64;
        self.bytes_read += n as u64;
        Ok(n)
    }
}

impl Seek for HeadTailReader {
    fn seek(&mut self, to: SeekFrom) -> io::Result<u64> {
        let new = match to {
            SeekFrom::Start(n) => n as i128,
            SeekFrom::End(d) => self.size as i128 + d as i128,
            SeekFrom::Current(d) => self.pos as i128 + d as i128,
        };
        if new < 0 {
            return Err(io::Error::new(io::ErrorKind::InvalidInput, "seek before start"));
        }
        self.pos = new as u64;
        Ok(self.pos)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serves_any_range() {
        let p = std::env::temp_dir().join(format!("di-htr-{}", std::process::id()));
        let data: Vec<u8> = (0..10_000u32).map(|i| (i % 256) as u8).collect();
        std::fs::write(&p, &data).unwrap();
        let mut r = HeadTailReader::open(&p, 1000, 1000, 1 << 20).unwrap();
        assert_eq!(r.bytes_read(), 2000);
        let mut all = Vec::new();
        r.read_to_end(&mut all).unwrap();
        assert_eq!(all, data);
        r.seek(SeekFrom::End(-5)).unwrap();
        let mut last = [0u8; 5];
        r.read_exact(&mut last).unwrap();
        assert_eq!(&last, &data[9995..]);
        // The middle read is capped.
        let mut capped = HeadTailReader::open(&p, 100, 100, 10).unwrap();
        capped.seek(SeekFrom::Start(500)).unwrap();
        let mut buf = [0u8; 50];
        assert!(capped.read(&mut buf).is_err());
        let _ = std::fs::remove_file(&p);
    }
}
