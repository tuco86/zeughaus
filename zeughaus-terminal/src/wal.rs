//! The write-ahead log of a terminal's byte stream, one timestamp per chunk.
//!
//! The timestamps sit on the byte stream, not on lines: a chunk is whatever
//! one PTY read returned, so a line's time is derived later from the chunk
//! it ends in.
//!
//! File format: the 8-byte [`MAGIC`], then records of `u64 LE at_micros`
//! (microseconds since the UNIX epoch), `u32 LE len` and `len` bytes. A
//! reader stops at the last complete record, so a torn tail is not an error.

use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// First bytes of every WAL file.
pub const MAGIC: &[u8; 8] = b"ZGHWAL1\n";

/// A shell's WAL rotates at this size into `wal.1` (one generation kept).
pub const SHELL_WAL_CAP: u64 = 64 << 20;

/// Bytes before a record's payload: the time and the length.
const HEADER: usize = 12;

/// Where a terminal writes its WAL.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Wal {
    /// Nowhere.
    Off,
    /// In the terminal's own directory, rotating at [`SHELL_WAL_CAP`]; gone
    /// with the terminal.
    Capped,
    /// At this path, appended to and never rotated.
    At(PathBuf),
}

/// Microseconds since the UNIX epoch.
pub fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_micros() as u64)
}

fn not_a_wal(path: &Path) -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        format!("not a WAL: {}", path.display()),
    )
}

/// Appends records to a WAL file.
pub struct WalWriter {
    path: PathBuf,
    file: File,
    len: u64,
    cap: Option<u64>,
}

impl WalWriter {
    /// Opens `path` for appending, creating it; an empty file gets the
    /// magic. With `cap`, the file rotates into `<path>.1` before a record
    /// would grow it past that size.
    pub fn open(path: &Path, cap: Option<u64>) -> io::Result<WalWriter> {
        let (file, len) = open_file(path)?;
        Ok(WalWriter {
            path: path.to_path_buf(),
            file,
            len,
            cap,
        })
    }

    /// Appends one record with a single write. After an error the writer
    /// is not to be used again: the tail of the file may be torn.
    pub fn append(&mut self, at_micros: u64, bytes: &[u8]) -> io::Result<()> {
        let len = u32::try_from(bytes.len())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "WAL record too large"))?;
        let record = (HEADER + bytes.len()) as u64;
        if let Some(cap) = self.cap
            && self.len > MAGIC.len() as u64
            && self.len + record > cap
        {
            self.rotate()?;
        }
        let mut buf = Vec::with_capacity(HEADER + bytes.len());
        buf.extend_from_slice(&at_micros.to_le_bytes());
        buf.extend_from_slice(&len.to_le_bytes());
        buf.extend_from_slice(bytes);
        self.file.write_all(&buf)?;
        self.len += record;
        Ok(())
    }

    fn rotate(&mut self) -> io::Result<()> {
        let mut older = self.path.clone().into_os_string();
        older.push(".1");
        std::fs::rename(&self.path, PathBuf::from(older))?;
        let (file, len) = open_file(&self.path)?;
        self.file = file;
        self.len = len;
        Ok(())
    }
}

fn open_file(path: &Path) -> io::Result<(File, u64)> {
    let mut file = OpenOptions::new()
        .read(true)
        .append(true)
        .create(true)
        .open(path)?;
    let mut len = file.metadata()?.len();
    if len == 0 {
        file.write_all(MAGIC)?;
        len = MAGIC.len() as u64;
    } else {
        let mut magic = [0u8; 8];
        file.read_exact(&mut magic).map_err(|_| not_a_wal(path))?;
        if &magic != MAGIC {
            return Err(not_a_wal(path));
        }
    }
    Ok((file, len))
}

/// One chunk of a terminal's output and when it was read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub at_micros: u64,
    pub bytes: Vec<u8>,
}

/// Calls `each` with every complete record of `data` (the part after the
/// magic) and returns how many bytes they took.
fn walk(data: &[u8], mut each: impl FnMut(u64, &[u8])) -> usize {
    let mut at = 0;
    while let Some(header) = data.get(at..at + HEADER) {
        let micros = u64::from_le_bytes(header[..8].try_into().expect("8 bytes"));
        let len = u32::from_le_bytes(header[8..].try_into().expect("4 bytes")) as usize;
        let Some(body) = data.get(at + HEADER..at + HEADER + len) else {
            break;
        };
        each(micros, body);
        at += HEADER + len;
    }
    at
}

fn read_checked(path: &Path) -> io::Result<Vec<u8>> {
    let data = std::fs::read(path)?;
    if !data.starts_with(MAGIC) {
        return Err(not_a_wal(path));
    }
    Ok(data)
}

/// Every complete record of the WAL at `path`.
pub fn read(path: &Path) -> io::Result<Vec<Record>> {
    let data = read_checked(path)?;
    let mut records = Vec::new();
    walk(&data[MAGIC.len()..], |at_micros, bytes| {
        records.push(Record {
            at_micros,
            bytes: bytes.to_vec(),
        });
    });
    Ok(records)
}

/// All record bytes of the WAL at `path`, concatenated.
pub fn read_bytes(path: &Path) -> io::Result<Vec<u8>> {
    let data = read_checked(path)?;
    let mut out = Vec::new();
    walk(&data[MAGIC.len()..], |_, bytes| {
        out.extend_from_slice(bytes)
    });
    Ok(out)
}

/// The last `max` bytes of [`read_bytes`], reading only the records that
/// hold them: the others are skipped by seeking.
pub fn tail_bytes(path: &Path, max: usize) -> io::Result<Vec<u8>> {
    let mut file = File::open(path)?;
    let size = file.metadata()?.len();
    let mut magic = [0u8; 8];
    if file.read_exact(&mut magic).is_err() || &magic != MAGIC {
        return Err(not_a_wal(path));
    }
    // (offset of the payload, its length) of every complete record.
    let mut bodies: Vec<(u64, u32)> = Vec::new();
    let mut at = MAGIC.len() as u64;
    let mut header = [0u8; HEADER];
    while at + HEADER as u64 <= size {
        file.read_exact(&mut header)?;
        let len = u32::from_le_bytes(header[8..].try_into().expect("4 bytes"));
        let body = at + HEADER as u64;
        if body + u64::from(len) > size {
            break;
        }
        bodies.push((body, len));
        at = body + u64::from(len);
        file.seek(SeekFrom::Start(at))?;
    }
    let mut first = bodies.len();
    let mut held = 0usize;
    while first > 0 && held < max {
        first -= 1;
        held += bodies[first].1 as usize;
    }
    let mut out = Vec::with_capacity(held);
    for &(body, len) in &bodies[first..] {
        file.seek(SeekFrom::Start(body))?;
        let mark = out.len();
        out.resize(mark + len as usize, 0);
        file.read_exact(&mut out[mark..])?;
    }
    if out.len() > max {
        out.drain(..out.len() - max);
    }
    Ok(out)
}

/// Reads the records appended to a WAL since the last call.
pub struct Follower {
    path: PathBuf,
    file: File,
    offset: u64,
}

impl Follower {
    /// Starts at the first record. The file need not have its magic yet.
    pub fn open(path: &Path) -> io::Result<Follower> {
        Ok(Follower {
            path: path.to_path_buf(),
            file: File::open(path)?,
            offset: 0,
        })
    }

    /// The complete records appended since the last call; a record still
    /// being written is left for the next one.
    pub fn next_records(&mut self) -> io::Result<Vec<Record>> {
        self.file.seek(SeekFrom::Start(self.offset))?;
        let mut data = Vec::new();
        self.file.read_to_end(&mut data)?;
        let mut records = Vec::new();
        let mut body = &data[..];
        if self.offset == 0 {
            if data.len() < MAGIC.len() {
                return Ok(records);
            }
            if !data.starts_with(MAGIC) {
                return Err(not_a_wal(&self.path));
            }
            body = &data[MAGIC.len()..];
            self.offset = MAGIC.len() as u64;
        }
        let used = walk(body, |at_micros, bytes| {
            records.push(Record {
                at_micros,
                bytes: bytes.to_vec(),
            });
        });
        self.offset += used as u64;
        Ok(records)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);

    impl Dir {
        fn new(name: &str) -> Dir {
            let dir = std::env::temp_dir().join(format!("zh-wal-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the test directory");
            Dir(dir)
        }

        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[test]
    fn records_round_trip_with_their_times() {
        let dir = Dir::new("roundtrip");
        let path = dir.path("wal");
        let mut writer = WalWriter::open(&path, None).expect("open");
        writer.append(10, b"abc").expect("append");
        writer.append(25, b"").expect("append");
        writer.append(40, b"defgh").expect("append");
        drop(writer);
        let records = read(&path).expect("read");
        assert_eq!(
            records,
            vec![
                Record {
                    at_micros: 10,
                    bytes: b"abc".to_vec()
                },
                Record {
                    at_micros: 25,
                    bytes: Vec::new()
                },
                Record {
                    at_micros: 40,
                    bytes: b"defgh".to_vec()
                },
            ]
        );
        assert_eq!(read_bytes(&path).expect("bytes"), b"abcdefgh");
        // Reopening appends after the magic instead of writing it again.
        let mut writer = WalWriter::open(&path, None).expect("reopen");
        writer.append(50, b"i").expect("append");
        assert_eq!(read_bytes(&path).expect("bytes"), b"abcdefghi");
    }

    #[test]
    fn a_file_without_the_magic_is_not_a_wal() {
        let dir = Dir::new("magic");
        let path = dir.path("log");
        std::fs::write(&path, "plain text, no magic").expect("write");
        let message = read(&path).expect_err("not a WAL").to_string();
        assert_eq!(message, format!("not a WAL: {}", path.display()));
        assert!(WalWriter::open(&path, None).is_err());
        assert!(tail_bytes(&path, 4).is_err());
    }

    #[test]
    fn a_torn_tail_reads_the_complete_records_only() {
        let dir = Dir::new("torn");
        let path = dir.path("wal");
        let mut writer = WalWriter::open(&path, None).expect("open");
        writer.append(1, b"abc").expect("append");
        writer.append(2, b"defgh").expect("append");
        drop(writer);
        let full = std::fs::read(&path).expect("read raw");
        // Cut inside the second record's payload, then inside its header.
        for cut in [full.len() - 2, 8 + 12 + 3 + 5] {
            std::fs::write(&path, &full[..cut]).expect("truncate");
            let records = read(&path).expect("read");
            assert_eq!(records.len(), 1, "cut at {cut}");
            assert_eq!(records[0].bytes, b"abc");
            assert_eq!(read_bytes(&path).expect("bytes"), b"abc");
            assert_eq!(tail_bytes(&path, 10).expect("tail"), b"abc");
        }
    }

    #[test]
    fn the_tail_is_the_last_bytes_of_the_stream() {
        let dir = Dir::new("tail");
        let path = dir.path("wal");
        let mut writer = WalWriter::open(&path, None).expect("open");
        writer.append(1, b"abc").expect("append");
        writer.append(2, b"defgh").expect("append");
        drop(writer);
        assert_eq!(tail_bytes(&path, 5).expect("tail"), b"defgh");
        assert_eq!(tail_bytes(&path, 6).expect("tail"), b"cdefgh");
        assert_eq!(tail_bytes(&path, 100).expect("tail"), b"abcdefgh");
        assert_eq!(tail_bytes(&path, 0).expect("tail"), b"");
    }

    #[test]
    fn a_capped_writer_rotates_into_a_second_file() {
        let dir = Dir::new("rotate");
        let path = dir.path("wal");
        let mut writer = WalWriter::open(&path, Some(64)).expect("open");
        // 8 + 32 = 40, and a second 32-byte record would make 72 > 64.
        writer.append(1, &[b'a'; 20]).expect("append");
        writer.append(2, &[b'b'; 20]).expect("append");
        drop(writer);
        let older = dir.path("wal.1");
        assert_eq!(read_bytes(&older).expect("older"), vec![b'a'; 20]);
        assert_eq!(read_bytes(&path).expect("newer"), vec![b'b'; 20]);
        assert!(std::fs::read(&path).expect("raw").starts_with(MAGIC));
    }

    #[test]
    fn a_follower_returns_only_new_complete_records() {
        let dir = Dir::new("follow");
        let path = dir.path("wal");
        let mut writer = WalWriter::open(&path, None).expect("open");
        let mut follower = Follower::open(&path).expect("follow");
        assert!(follower.next_records().expect("empty").is_empty());
        writer.append(1, b"one").expect("append");
        let first = follower.next_records().expect("first");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].bytes, b"one");
        assert!(follower.next_records().expect("quiet").is_empty());
        // A record cut in the middle waits until it is whole.
        let mut raw = OpenOptions::new().append(true).open(&path).expect("raw");
        let mut record = Vec::new();
        record.extend_from_slice(&2u64.to_le_bytes());
        record.extend_from_slice(&4u32.to_le_bytes());
        record.extend_from_slice(b"twoo");
        raw.write_all(&record[..14]).expect("part");
        assert!(follower.next_records().expect("torn").is_empty());
        raw.write_all(&record[14..]).expect("rest");
        let second = follower.next_records().expect("second");
        assert_eq!(second.len(), 1);
        assert_eq!(second[0].bytes, b"twoo");
        assert_eq!(second[0].at_micros, 2);
    }
}
