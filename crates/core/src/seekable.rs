//! zstd "seekable format": a stream of independent zstd frames followed by a
//! skippable frame holding a seek table. Stock `zstd -d` decodes it (it skips
//! the table), while we can decompress any byte range by touching only the
//! frames that cover it.
//!
//! Spec: zstd/contrib/seekable_format/zstd_seekable_compression_format.md

use std::collections::VecDeque;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::error::{Error, Result};
use crate::hash::{Digest, Hasher};

const SKIPPABLE_MAGIC: u32 = 0x184D_2A5E;
const SEEKABLE_MAGIC: u32 = 0x8F92_EAB1;
const FOOTER_LEN: u64 = 9;

pub const DEFAULT_FRAME_SIZE: usize = 8 << 20;

/// Compression settings for one stream.
#[derive(Clone, Copy, Debug)]
pub struct CompressOptions {
    pub level: i32,
    pub frame_size: usize,
    pub threads: usize,
}

impl Default for CompressOptions {
    fn default() -> Self {
        CompressOptions { level: 9, frame_size: DEFAULT_FRAME_SIZE, threads: thread::available_parallelism().map(|n| n.get()).unwrap_or(2) }
    }
}

/// True if a sample barely compresses, so a fast level is as good as a slow one.
pub fn looks_incompressible(sample: &[u8]) -> bool {
    if sample.len() < 4096 {
        return false;
    }
    match zstd::bulk::compress(sample, 1) {
        Ok(c) => c.len() as f64 > sample.len() as f64 * 0.97,
        Err(_) => false,
    }
}

/// Reads raw bytes from `src` and yields a seekable zstd stream. The SHA-256 of
/// the raw bytes is available from [`CompressingReader::digest`] after EOF.
pub struct CompressingReader<R: Read> {
    src: R,
    opts: CompressOptions,
    comps: Vec<zstd::bulk::Compressor<'static>>,
    hasher: Option<Hasher>,
    raw_len: u64,
    compressed_len: u64,
    out: Vec<u8>,
    out_pos: usize,
    entries: Vec<(u32, u32)>,
    src_done: bool,
    table_written: bool,
    digest: Option<Digest>,
}

impl<R: Read> CompressingReader<R> {
    pub fn new(src: R, opts: CompressOptions) -> io::Result<Self> {
        let threads = opts.threads.max(1);
        let mut comps = Vec::with_capacity(threads);
        for _ in 0..threads {
            let mut c = zstd::bulk::Compressor::new(opts.level)?;
            c.include_checksum(true)?;
            comps.push(c);
        }
        Ok(CompressingReader {
            src,
            opts: CompressOptions { threads, ..opts },
            comps,
            hasher: Some(Hasher::new()),
            raw_len: 0,
            compressed_len: 0,
            out: Vec::new(),
            out_pos: 0,
            entries: Vec::new(),
            src_done: false,
            table_written: false,
            digest: None,
        })
    }

    /// SHA-256 of the raw input; `None` until the stream has been read to EOF.
    pub fn digest(&self) -> Option<Digest> {
        self.digest
    }

    pub fn raw_len(&self) -> u64 {
        self.raw_len
    }

    pub fn compressed_len(&self) -> u64 {
        self.compressed_len
    }

    fn fill_batch(&mut self) -> io::Result<()> {
        let want = self.opts.frame_size * self.opts.threads;
        let mut batch = vec![0u8; want];
        let mut n = 0;
        while n < want {
            let k = self.src.read(&mut batch[n..])?;
            if k == 0 {
                self.src_done = true;
                break;
            }
            n += k;
        }
        batch.truncate(n);
        if let Some(h) = self.hasher.as_mut() {
            h.update(&batch);
        }
        self.raw_len += n as u64;

        let chunks: Vec<&[u8]> = if n == 0 && self.entries.is_empty() && self.src_done {
            // Empty input still gets one (empty) frame so the stream is a normal zstd file.
            vec![&batch[..0]]
        } else {
            batch.chunks(self.opts.frame_size).collect()
        };
        let results: Vec<io::Result<Vec<u8>>> = thread::scope(|s| {
            let handles: Vec<_> = self.comps.iter_mut().zip(chunks.iter()).map(|(c, chunk)| s.spawn(move || c.compress(chunk))).collect();
            handles.into_iter().map(|h| h.join().expect("compress thread panicked")).collect()
        });
        self.out.clear();
        self.out_pos = 0;
        for (res, chunk) in results.into_iter().zip(chunks.iter()) {
            let frame = res?;
            self.entries.push((frame.len() as u32, chunk.len() as u32));
            self.compressed_len += frame.len() as u64;
            self.out.extend_from_slice(&frame);
        }
        Ok(())
    }

    fn write_table(&mut self) {
        self.out.clear();
        self.out_pos = 0;
        let n = self.entries.len() as u32;
        let body_len = n * 8 + FOOTER_LEN as u32;
        self.out.extend_from_slice(&SKIPPABLE_MAGIC.to_le_bytes());
        self.out.extend_from_slice(&body_len.to_le_bytes());
        for &(c, d) in &self.entries {
            self.out.extend_from_slice(&c.to_le_bytes());
            self.out.extend_from_slice(&d.to_le_bytes());
        }
        self.out.extend_from_slice(&n.to_le_bytes());
        self.out.push(0); // descriptor: no per-frame checksums in the table
        self.out.extend_from_slice(&SEEKABLE_MAGIC.to_le_bytes());
        self.compressed_len += self.out.len() as u64;
        self.table_written = true;
        self.digest = self.hasher.take().map(|h| h.finish());
    }
}

impl<R: Read> Read for CompressingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        loop {
            if self.out_pos < self.out.len() {
                let k = buf.len().min(self.out.len() - self.out_pos);
                buf[..k].copy_from_slice(&self.out[self.out_pos..self.out_pos + k]);
                self.out_pos += k;
                return Ok(k);
            }
            if !self.src_done {
                self.fill_batch()?;
            } else if !self.table_written {
                self.write_table();
            } else {
                return Ok(0);
            }
        }
    }
}

/// One frame of a seekable stream, with offsets relative to the stream start.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Frame {
    pub c_off: u64,
    pub c_len: u32,
    pub d_off: u64,
    pub d_len: u32,
}

#[derive(Clone, Debug)]
pub struct SeekTable {
    pub frames: Vec<Frame>,
}

impl SeekTable {
    /// Parse the seek table of a stream stored at `[start, start + len)` in `f`.
    pub fn read<F: Read + Seek>(f: &mut F, start: u64, len: u64) -> Result<SeekTable> {
        if len < 8 + FOOTER_LEN {
            return Err(Error::Corrupt("seekable stream too short".into()));
        }
        let mut footer = [0u8; FOOTER_LEN as usize];
        f.seek(SeekFrom::Start(start + len - FOOTER_LEN))?;
        f.read_exact(&mut footer)?;
        let n = u32::from_le_bytes(footer[0..4].try_into().unwrap()) as u64;
        let desc = footer[4];
        let magic = u32::from_le_bytes(footer[5..9].try_into().unwrap());
        if magic != SEEKABLE_MAGIC {
            return Err(Error::Corrupt("missing seek table".into()));
        }
        let entry_len = if desc & 0x80 != 0 { 12 } else { 8 };
        let body_len = n * entry_len + FOOTER_LEN;
        let table_len = 8 + body_len;
        if table_len > len {
            return Err(Error::Corrupt("seek table larger than stream".into()));
        }
        let mut table = vec![0u8; table_len as usize];
        f.seek(SeekFrom::Start(start + len - table_len))?;
        f.read_exact(&mut table)?;
        let skip_magic = u32::from_le_bytes(table[0..4].try_into().unwrap());
        let skip_len = u32::from_le_bytes(table[4..8].try_into().unwrap()) as u64;
        if skip_magic != SKIPPABLE_MAGIC || skip_len != body_len {
            return Err(Error::Corrupt("bad seek table header".into()));
        }
        let mut frames = Vec::with_capacity(n as usize);
        let (mut c_off, mut d_off) = (0u64, 0u64);
        for i in 0..n as usize {
            let e = &table[8 + i * entry_len as usize..];
            let c_len = u32::from_le_bytes(e[0..4].try_into().unwrap());
            let d_len = u32::from_le_bytes(e[4..8].try_into().unwrap());
            frames.push(Frame { c_off, c_len, d_off, d_len });
            c_off += c_len as u64;
            d_off += d_len as u64;
        }
        if c_off + table_len != len {
            return Err(Error::Corrupt("seek table does not match stream length".into()));
        }
        Ok(SeekTable { frames })
    }

    pub fn raw_len(&self) -> u64 {
        self.frames.last().map(|f| f.d_off + f.d_len as u64).unwrap_or(0)
    }

    /// Index of the frame containing raw offset `pos`.
    pub fn frame_at(&self, pos: u64) -> Option<usize> {
        let i = self.frames.partition_point(|f| f.d_off + f.d_len as u64 <= pos);
        (i < self.frames.len()).then_some(i)
    }
}

/// Recently decompressed frames and seek tables, shared across reads so that
/// many small files from one solid block don't each decompress the same frame.
#[derive(Clone)]
pub struct FrameCache(Arc<Mutex<CacheInner>>);

/// (stream id, byte offset in the underlying file)
type CacheKey = (u64, u64);

struct CacheInner {
    frames: VecDeque<(CacheKey, Arc<Vec<u8>>)>,
    tables: VecDeque<(CacheKey, Arc<SeekTable>)>,
    bytes: usize,
    cap_bytes: usize,
}

impl FrameCache {
    pub fn new(cap_bytes: usize) -> Self {
        FrameCache(Arc::new(Mutex::new(CacheInner { frames: VecDeque::new(), tables: VecDeque::new(), bytes: 0, cap_bytes })))
    }

    fn frame(&self, key: (u64, u64)) -> Option<Arc<Vec<u8>>> {
        let inner = self.0.lock().ok()?;
        inner.frames.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
    }

    fn put_frame(&self, key: (u64, u64), data: Arc<Vec<u8>>) {
        let Ok(mut inner) = self.0.lock() else { return };
        inner.bytes += data.len();
        inner.frames.push_back((key, data));
        while inner.bytes > inner.cap_bytes && inner.frames.len() > 1 {
            if let Some((_, old)) = inner.frames.pop_front() {
                inner.bytes -= old.len();
            }
        }
    }

    fn table(&self, key: (u64, u64)) -> Option<Arc<SeekTable>> {
        let inner = self.0.lock().ok()?;
        inner.tables.iter().find(|(k, _)| *k == key).map(|(_, v)| v.clone())
    }

    fn put_table(&self, key: (u64, u64), t: Arc<SeekTable>) {
        let Ok(mut inner) = self.0.lock() else { return };
        inner.tables.push_back((key, t));
        if inner.tables.len() > 256 {
            inner.tables.pop_front();
        }
    }
}

fn decode_frame<F: Read + Seek>(f: &mut F, start: u64, fr: &Frame) -> Result<Vec<u8>> {
    let mut cbuf = vec![0u8; fr.c_len as usize];
    f.seek(SeekFrom::Start(start + fr.c_off))?;
    f.read_exact(&mut cbuf)?;
    let d = zstd::bulk::decompress(&cbuf, fr.d_len as usize).map_err(|e| Error::Corrupt(format!("zstd frame at +{}: {e}", fr.c_off)))?;
    if d.len() != fr.d_len as usize {
        return Err(Error::Corrupt(format!("zstd frame at +{} has wrong length", fr.c_off)));
    }
    Ok(d)
}

/// Streams decompressed bytes `[pos, end)` of a seekable stream.
pub struct RangeReader<F: Read + Seek> {
    f: F,
    start: u64,
    table: Arc<SeekTable>,
    pos: u64,
    end: u64,
    cur: Arc<Vec<u8>>,
    cur_off: u64,
    cache: Option<(FrameCache, u64)>,
}

impl<F: Read + Seek> RangeReader<F> {
    /// `start`/`len` locate the compressed stream in `f`; `raw_off`/`raw_len`
    /// select the decompressed range to read.
    pub fn new(f: F, start: u64, len: u64, raw_off: u64, raw_len: u64) -> Result<Self> {
        Self::with_cache(f, start, len, raw_off, raw_len, None)
    }

    /// Like `new`, sharing decompressed frames through `cache`. `stream_id`
    /// must identify the underlying file (e.g. a pack id).
    pub fn with_cache(mut f: F, start: u64, len: u64, raw_off: u64, raw_len: u64, cache: Option<(FrameCache, u64)>) -> Result<Self> {
        let table = match cache.as_ref().and_then(|(c, id)| c.table((*id, start))) {
            Some(t) => t,
            None => {
                let t = Arc::new(SeekTable::read(&mut f, start, len)?);
                if let Some((c, id)) = &cache {
                    c.put_table((*id, start), t.clone());
                }
                t
            }
        };
        if raw_off + raw_len > table.raw_len() {
            return Err(Error::Corrupt("range past end of stream".into()));
        }
        Ok(RangeReader { f, start, table, pos: raw_off, end: raw_off + raw_len, cur: Arc::new(Vec::new()), cur_off: 0, cache })
    }

    /// Whole decompressed stream.
    pub fn whole(mut f: F, start: u64, len: u64) -> Result<Self> {
        let table = Arc::new(SeekTable::read(&mut f, start, len)?);
        let end = table.raw_len();
        Ok(RangeReader { f, start, table, pos: 0, end, cur: Arc::new(Vec::new()), cur_off: 0, cache: None })
    }

    fn load(&mut self, fr: Frame) -> Result<Arc<Vec<u8>>> {
        let key = self.cache.as_ref().map(|(_, id)| (*id, self.start + fr.c_off));
        if let (Some((c, _)), Some(k)) = (&self.cache, key) {
            if let Some(d) = c.frame(k) {
                return Ok(d);
            }
        }
        let d = Arc::new(decode_frame(&mut self.f, self.start, &fr)?);
        if let (Some((c, _)), Some(k)) = (&self.cache, key) {
            c.put_frame(k, d.clone());
        }
        Ok(d)
    }
}

impl<F: Read + Seek> Read for RangeReader<F> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if self.pos >= self.end || buf.is_empty() {
            return Ok(0);
        }
        let in_cur = self.pos >= self.cur_off && self.pos < self.cur_off + self.cur.len() as u64;
        if !in_cur {
            let i = self.table.frame_at(self.pos).ok_or_else(|| io::Error::new(io::ErrorKind::UnexpectedEof, "past last frame"))?;
            let fr = self.table.frames[i];
            self.cur = self.load(fr).map_err(io::Error::other)?;
            self.cur_off = fr.d_off;
        }
        let off = (self.pos - self.cur_off) as usize;
        let avail = (self.cur.len() - off).min((self.end - self.pos) as usize);
        let k = buf.len().min(avail);
        buf[..k].copy_from_slice(&self.cur[off..off + k]);
        self.pos += k as u64;
        Ok(k)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn sample(n: usize) -> Vec<u8> {
        // Compressible but not trivial.
        (0..n).map(|i| ((i * 7919) % 251) as u8 ^ ((i / 1000) as u8)).collect()
    }

    fn compress(data: &[u8], frame: usize, threads: usize) -> (Vec<u8>, Digest) {
        let mut r = CompressingReader::new(Cursor::new(data.to_vec()), CompressOptions { level: 3, frame_size: frame, threads }).unwrap();
        let mut out = Vec::new();
        r.read_to_end(&mut out).unwrap();
        assert_eq!(r.compressed_len(), out.len() as u64);
        (out, r.digest().unwrap())
    }

    #[test]
    fn round_trip_and_ranges() {
        let data = sample(1_000_003);
        let (z, digest) = compress(&data, 64 * 1024, 3);
        assert_eq!(digest, Digest::of(&data));
        // Stock zstd decoder must read it (skippable frame ignored, frames concatenated).
        let plain = zstd::stream::decode_all(Cursor::new(&z)).unwrap();
        assert_eq!(plain, data);

        let table = SeekTable::read(&mut Cursor::new(&z), 0, z.len() as u64).unwrap();
        assert_eq!(table.frames.len(), 16);
        assert_eq!(table.raw_len(), data.len() as u64);

        for &(off, len) in &[(0u64, 10u64), (65_530, 20), (999_990, 13), (123_456, 300_000), (0, 1_000_003)] {
            let mut rr = RangeReader::new(Cursor::new(&z), 0, z.len() as u64, off, len).unwrap();
            let mut got = Vec::new();
            rr.read_to_end(&mut got).unwrap();
            assert_eq!(got, &data[off as usize..(off + len) as usize]);
        }
    }

    #[test]
    fn embedded_at_offset() {
        let data = sample(200_000);
        let (z, _) = compress(&data, 50_000, 2);
        let mut container = vec![7u8; 1234];
        container.extend_from_slice(&z);
        container.extend_from_slice(&[9u8; 99]);
        let mut rr = RangeReader::whole(Cursor::new(&container), 1234, z.len() as u64).unwrap();
        let mut got = Vec::new();
        rr.read_to_end(&mut got).unwrap();
        assert_eq!(got, data);
    }

    #[test]
    fn empty_input() {
        let (z, digest) = compress(&[], 1024, 4);
        assert_eq!(digest, Digest::of(&[]));
        assert!(zstd::stream::decode_all(Cursor::new(&z)).unwrap().is_empty());
        let table = SeekTable::read(&mut Cursor::new(&z), 0, z.len() as u64).unwrap();
        assert_eq!(table.frames.len(), 1);
        assert_eq!(table.raw_len(), 0);
    }

    #[test]
    fn detects_corruption() {
        let data = sample(300_000);
        let (mut z, _) = compress(&data, 100_000, 1);
        z[500] ^= 0xFF; // inside the first frame
        let mut rr = RangeReader::whole(Cursor::new(&z), 0, z.len() as u64).unwrap();
        let mut got = Vec::new();
        assert!(rr.read_to_end(&mut got).is_err());
    }

    #[test]
    fn incompressible_detection() {
        let mut x = 0x1234_5678u64;
        let noise: Vec<u8> = (0..1 << 16)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        assert!(looks_incompressible(&noise));
        assert!(!looks_incompressible(&sample(1 << 16)));
    }
}
