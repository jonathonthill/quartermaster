//! Minimal POSIX (ustar + pax) tar writer and reader.
//!
//! The writer can stream a member whose size is unknown up front: it writes a
//! placeholder header, streams data, then seeks back and patches the size. Sizes
//! of 8 GiB and above use the GNU base-256 encoding, which GNU tar, bsdtar, and
//! Python's tarfile all read.

use std::io::{self, Read, Seek, SeekFrom, Write};

use crate::error::{Error, Result};

pub const BLOCK: u64 = 512;
const OCTAL11_MAX: u64 = 0o77777777777;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
    Other(u8),
}

impl Kind {
    fn flag(self) -> u8 {
        match self {
            Kind::File => b'0',
            Kind::Dir => b'5',
            Kind::Symlink => b'2',
            Kind::Other(f) => f,
        }
    }
}

/// A member whose data is being streamed; finish it with [`TarWriter::end_member`].
#[derive(Debug)]
pub struct OpenMember {
    header_pos: u64,
    header: [u8; 512],
    pub data_pos: u64,
}

pub struct TarWriter<W: Write + Seek> {
    w: W,
    pos: u64,
}

impl<W: Write + Seek> TarWriter<W> {
    /// Append to `w`, whose current position is `pos`.
    pub fn new(w: W, pos: u64) -> Self {
        TarWriter { w, pos }
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    pub fn get_mut(&mut self) -> &mut W {
        &mut self.w
    }

    pub fn into_inner(self) -> W {
        self.w
    }

    /// Write headers for a member; stream its data with `Write`, then call `end_member`.
    pub fn begin_member(&mut self, path: &str, kind: Kind, mode: u32, mtime: i64, link: Option<&str>) -> io::Result<OpenMember> {
        let mut pax = Vec::new();
        let name_field = field_or_pax(path, 100, "path", &mut pax);
        let link_field = link.map(|l| field_or_pax(l, 100, "linkpath", &mut pax)).unwrap_or_default();
        if !pax.is_empty() {
            let mut h = header(b"PaxHeader/entry", b'x', 0o644, pax.len() as u64, mtime, b"");
            set_checksum(&mut h);
            self.raw(&h)?;
            self.raw(&pax)?;
            self.pad()?;
        }
        let header_pos = self.pos;
        let mut h = header(&name_field, kind.flag(), mode, 0, mtime, &link_field);
        set_checksum(&mut h);
        self.raw(&h)?;
        Ok(OpenMember { header_pos, header: h, data_pos: self.pos })
    }

    /// Pad the member's data and patch its size into the header. Returns the size.
    pub fn end_member(&mut self, m: OpenMember) -> io::Result<u64> {
        let size = self.pos - m.data_pos;
        self.pad()?;
        if size != 0 {
            let mut h = m.header;
            put_size(&mut h[124..136], size);
            set_checksum(&mut h);
            let end = self.pos;
            self.w.seek(SeekFrom::Start(m.header_pos))?;
            self.w.write_all(&h)?;
            self.w.seek(SeekFrom::Start(end))?;
        }
        Ok(size)
    }

    /// Append a member with in-memory data. Returns the data offset.
    pub fn append(&mut self, path: &str, mode: u32, mtime: i64, data: &[u8]) -> io::Result<u64> {
        let m = self.begin_member(path, Kind::File, mode, mtime, None)?;
        let pos = m.data_pos;
        self.write_all(data)?;
        self.end_member(m)?;
        Ok(pos)
    }

    pub fn append_dir(&mut self, path: &str, mode: u32, mtime: i64) -> io::Result<()> {
        let p = if path.ends_with('/') { path.to_string() } else { format!("{path}/") };
        let m = self.begin_member(&p, Kind::Dir, mode, mtime, None)?;
        self.end_member(m).map(|_| ())
    }

    pub fn append_symlink(&mut self, path: &str, target: &str, mtime: i64) -> io::Result<()> {
        let m = self.begin_member(path, Kind::Symlink, 0o777, mtime, Some(target))?;
        self.end_member(m).map(|_| ())
    }

    /// Write the end-of-archive marker (two zero blocks).
    pub fn finish(&mut self) -> io::Result<()> {
        self.raw(&[0u8; 1024])
    }

    fn raw(&mut self, b: &[u8]) -> io::Result<()> {
        self.w.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }

    fn pad(&mut self) -> io::Result<()> {
        let rem = (self.pos % BLOCK) as usize;
        if rem != 0 {
            self.raw(&[0u8; 512][..512 - rem])?;
        }
        Ok(())
    }
}

impl<W: Write + Seek> Write for TarWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.w.write(buf)?;
        self.pos += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> io::Result<()> {
        self.w.flush()
    }
}

/// Returns the bytes for a fixed-width header field, adding a pax record when
/// the value is too long or not plain ASCII.
fn field_or_pax(value: &str, width: usize, key: &str, pax: &mut Vec<u8>) -> Vec<u8> {
    if value.is_ascii() && value.len() <= width {
        return value.as_bytes().to_vec();
    }
    pax.extend_from_slice(&pax_record(key, value));
    // Placeholder name for readers that ignore pax: ASCII, truncated.
    let mut placeholder: Vec<u8> = value.bytes().map(|b| if b.is_ascii() && b != 0 { b } else { b'_' }).collect();
    if placeholder.len() > width {
        placeholder = placeholder[placeholder.len() - width..].to_vec();
    }
    placeholder
}

fn pax_record(key: &str, value: &str) -> Vec<u8> {
    // "LEN key=value\n" where LEN counts the whole record including its own digits.
    let body = format!(" {key}={value}\n");
    let mut len = body.len() + 1;
    loop {
        let candidate = len.to_string().len() + body.len();
        if candidate == len {
            break;
        }
        len = candidate;
    }
    format!("{len}{body}").into_bytes()
}

fn header(name: &[u8], flag: u8, mode: u32, size: u64, mtime: i64, link: &[u8]) -> [u8; 512] {
    let mut h = [0u8; 512];
    h[..name.len()].copy_from_slice(name);
    put_octal(&mut h[100..108], (mode & 0o7777) as u64);
    put_octal(&mut h[108..116], 0);
    put_octal(&mut h[116..124], 0);
    put_size(&mut h[124..136], size);
    put_octal(&mut h[136..148], mtime.clamp(0, OCTAL11_MAX as i64) as u64);
    h[156] = flag;
    h[157..157 + link.len()].copy_from_slice(link);
    h[257..263].copy_from_slice(b"ustar\0");
    h[263..265].copy_from_slice(b"00");
    h
}

fn put_octal(field: &mut [u8], v: u64) {
    let digits = field.len() - 1;
    let s = format!("{v:0digits$o}");
    field[..digits].copy_from_slice(s.as_bytes());
    field[digits] = 0;
}

fn put_size(field: &mut [u8], size: u64) {
    if size <= OCTAL11_MAX {
        put_octal(field, size);
    } else {
        field.fill(0);
        field[0] = 0x80;
        let be = size.to_be_bytes();
        field[12 - 8..].copy_from_slice(&be);
    }
}

fn set_checksum(h: &mut [u8; 512]) {
    h[148..156].fill(b' ');
    let sum: u32 = h.iter().map(|&b| b as u32).sum();
    let s = format!("{sum:06o}");
    h[148..154].copy_from_slice(s.as_bytes());
    h[154] = 0;
    h[155] = b' ';
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: Kind,
    pub size: u64,
    pub header_off: u64,
    pub data_off: u64,
    pub mode: u32,
    pub mtime: i64,
    pub link: Option<String>,
}

/// List members of a tar stored in `r` at `[start, end)`. Stops at the
/// end-of-archive marker, or cleanly at `end`/EOF for unsealed archives.
pub fn list<R: Read + Seek>(r: &mut R, start: u64, end: Option<u64>) -> Result<Vec<Entry>> {
    let end = match end {
        Some(e) => e,
        None => r.seek(SeekFrom::End(0))?,
    };
    let mut out = Vec::new();
    let mut pos = start;
    let mut pax_path: Option<String> = None;
    let mut pax_link: Option<String> = None;
    let mut gnu_name: Option<String> = None;
    let mut gnu_link: Option<String> = None;
    let mut h = [0u8; 512];
    while pos + BLOCK <= end {
        r.seek(SeekFrom::Start(pos))?;
        r.read_exact(&mut h)?;
        if h.iter().all(|&b| b == 0) {
            break;
        }
        verify_checksum(&h, pos)?;
        let size = parse_num(&h[124..136]).ok_or_else(|| Error::Corrupt(format!("bad size in tar header at {pos}")))?;
        let data_off = pos + BLOCK;
        let next = data_off + size.div_ceil(BLOCK) * BLOCK;
        if data_off + size > end {
            return Err(Error::Corrupt(format!("tar member at {pos} runs past end")));
        }
        let flag = h[156];
        match flag {
            b'x' => {
                let mut data = vec![0u8; size as usize];
                r.read_exact(&mut data)?;
                for (k, v) in parse_pax(&data) {
                    match k.as_str() {
                        "path" => pax_path = Some(v),
                        "linkpath" => pax_link = Some(v),
                        _ => {}
                    }
                }
            }
            b'L' | b'K' => {
                let mut data = vec![0u8; size as usize];
                r.read_exact(&mut data)?;
                let s = cstr(&data);
                if flag == b'L' { gnu_name = Some(s) } else { gnu_link = Some(s) }
            }
            b'g' => {}
            _ => {
                let path = pax_path.take().or(gnu_name.take()).unwrap_or_else(|| {
                    let name = cstr(&h[0..100]);
                    let prefix = if &h[257..262] == b"ustar" { cstr(&h[345..500]) } else { String::new() };
                    if prefix.is_empty() { name } else { format!("{prefix}/{name}") }
                });
                let link_field = cstr(&h[157..257]);
                let link = pax_link.take().or(gnu_link.take()).or((!link_field.is_empty()).then_some(link_field));
                let kind = match flag {
                    b'0' | 0 | b'7' => Kind::File,
                    b'5' => Kind::Dir,
                    b'2' => Kind::Symlink,
                    f => Kind::Other(f),
                };
                out.push(Entry {
                    path,
                    kind,
                    size,
                    header_off: pos,
                    data_off,
                    mode: parse_num(&h[100..108]).unwrap_or(0o644) as u32,
                    mtime: parse_num(&h[136..148]).unwrap_or(0) as i64,
                    link: if kind == Kind::Symlink { link } else { None },
                });
            }
        }
        pos = next;
    }
    Ok(out)
}

fn verify_checksum(h: &[u8; 512], pos: u64) -> Result<()> {
    let stored = parse_num(&h[148..156]).ok_or_else(|| Error::Corrupt(format!("bad tar checksum field at {pos}")))?;
    let mut sum: u64 = 0;
    for (i, &b) in h.iter().enumerate() {
        sum += if (148..156).contains(&i) { b' ' as u64 } else { b as u64 };
    }
    if sum != stored {
        return Err(Error::Corrupt(format!("tar header checksum mismatch at offset {pos}")));
    }
    Ok(())
}

fn parse_num(field: &[u8]) -> Option<u64> {
    if field[0] & 0x80 != 0 {
        let mut v: u64 = (field[0] & 0x7f) as u64;
        for &b in &field[1..] {
            v = v.checked_mul(256)?.checked_add(b as u64)?;
        }
        return Some(v);
    }
    let s: String = field.iter().take_while(|&&b| b != 0).map(|&b| b as char).collect();
    let s = s.trim();
    if s.is_empty() {
        return Some(0);
    }
    u64::from_str_radix(s, 8).ok()
}

fn cstr(b: &[u8]) -> String {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    String::from_utf8_lossy(&b[..end]).into_owned()
}

fn parse_pax(mut data: &[u8]) -> Vec<(String, String)> {
    let mut out = Vec::new();
    while !data.is_empty() {
        let Some(sp) = data.iter().position(|&b| b == b' ') else { break };
        let Ok(len) = std::str::from_utf8(&data[..sp]).unwrap_or("").parse::<usize>() else { break };
        if len == 0 || len > data.len() {
            break;
        }
        let rec = &data[sp + 1..len];
        let rec = rec.strip_suffix(b"\n").unwrap_or(rec);
        if let Some(eq) = rec.iter().position(|&b| b == b'=') {
            out.push((String::from_utf8_lossy(&rec[..eq]).into_owned(), String::from_utf8_lossy(&rec[eq + 1..]).into_owned()));
        }
        data = &data[len..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    #[test]
    fn pax_record_length_is_self_consistent() {
        for n in [1usize, 80, 90, 95, 100, 990, 995, 1000] {
            let v = "x".repeat(n);
            let rec = pax_record("path", &v);
            let len: usize = std::str::from_utf8(&rec).unwrap().split(' ').next().unwrap().parse().unwrap();
            assert_eq!(len, rec.len());
        }
    }

    #[test]
    fn write_then_list() {
        let long = format!("{}/file.txt", "d".repeat(150));
        let uni = "Données/über/résumé.csv";
        let mut w = TarWriter::new(Cursor::new(Vec::new()), 0);
        w.append("a.txt", 0o644, 1_700_000_000, b"hello").unwrap();
        let m = w.begin_member(&long, Kind::File, 0o600, 5, None).unwrap();
        w.write_all(&vec![1u8; 1000]).unwrap();
        assert_eq!(w.end_member(m).unwrap(), 1000);
        w.append(uni, 0o644, 7, b"").unwrap();
        w.append_dir("folder", 0o755, 9).unwrap();
        w.append_symlink("link", &"t".repeat(120), 9).unwrap();
        w.finish().unwrap();
        let buf = w.into_inner().into_inner();
        assert_eq!(buf.len() % 512, 0);

        let entries = list(&mut Cursor::new(&buf), 0, None).unwrap();
        let names: Vec<_> = entries.iter().map(|e| e.path.as_str()).collect();
        assert_eq!(names, vec!["a.txt", long.as_str(), uni, "folder/", "link"]);
        assert_eq!(entries[0].size, 5);
        assert_eq!(&buf[entries[0].data_off as usize..][..5], b"hello");
        assert_eq!(entries[1].size, 1000);
        assert_eq!(entries[1].mode, 0o600);
        assert_eq!(entries[3].kind, Kind::Dir);
        assert_eq!(entries[4].link.as_deref(), Some("t".repeat(120).as_str()));
    }

    #[test]
    fn base256_size_round_trip() {
        let mut f = [0u8; 12];
        put_size(&mut f, 100 << 30);
        assert_eq!(parse_num(&f), Some(100 << 30));
        put_size(&mut f, 12345);
        assert_eq!(parse_num(&f), Some(12345));
    }

    #[test]
    fn detects_damaged_header() {
        let mut w = TarWriter::new(Cursor::new(Vec::new()), 0);
        w.append("a.txt", 0o644, 0, b"hello").unwrap();
        w.finish().unwrap();
        let mut buf = w.into_inner().into_inner();
        buf[3] ^= 1;
        assert!(list(&mut Cursor::new(&buf), 0, None).is_err());
    }
}
