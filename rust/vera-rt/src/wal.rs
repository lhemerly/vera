// Canonical VERA intent WAL. This source is embedded by the Rust generator and
// mirrored into rust/vera-rt/src/wal.rs by tools/build_cargo.py.
// Requires Rust 1.89 for advisory File::try_lock().
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

const WAL_MAGIC: &[u8; 4] = b"VERA";
const WAL_VERSION: u16 = 2;
const WAL_HEADER: usize = 32;
const WAL_ALIGNMENT: usize = 64;

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message)
}

/// CRC32 used by canonical WAL frames and portable snapshot envelopes.
pub fn checksum(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for byte in bytes {
        crc ^= *byte as u32;
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320u32 & (0u32.wrapping_sub(crc & 1)));
        }
    }
    !crc
}

/// Arguments are canonical bytes, never a truncating cast to a machine word.
/// The word-array implementation accepts historical callers without losing data.
pub trait IntoIntentArgs {
    fn into_intent_args(self) -> Vec<u8>;
}
impl IntoIntentArgs for Vec<u8> {
    fn into_intent_args(self) -> Vec<u8> {
        self
    }
}
impl IntoIntentArgs for &Vec<u8> {
    fn into_intent_args(self) -> Vec<u8> {
        self.clone()
    }
}
impl IntoIntentArgs for &[u8] {
    fn into_intent_args(self) -> Vec<u8> {
        self.to_vec()
    }
}
impl<const N: usize> IntoIntentArgs for [u8; N] {
    fn into_intent_args(self) -> Vec<u8> {
        self.to_vec()
    }
}
impl<const N: usize> IntoIntentArgs for &[u8; N] {
    fn into_intent_args(self) -> Vec<u8> {
        self.to_vec()
    }
}
impl<const N: usize> IntoIntentArgs for [u64; N] {
    fn into_intent_args(self) -> Vec<u8> {
        self.iter().flat_map(|v| v.to_le_bytes()).collect()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IntentRecord {
    pub intent_id: u64,
    pub tx_seq: u64,
    pub transition: u32,
    pub args: Vec<u8>,
}

impl IntentRecord {
    /// Frames are little endian and padded to a multiple of 64 bytes. The CRC
    /// covers the entire frame, including identity, length, arguments and padding.
    pub fn encode(&self) -> io::Result<Vec<u8>> {
        if self.intent_id != self.tx_seq || self.tx_seq == 0 {
            return Err(invalid("intent identity must equal its nonzero sequence"));
        }
        let payload_len = u32::try_from(self.args.len())
            .map_err(|_| invalid("intent payload exceeds the format length domain"))?;
        let unpadded = WAL_HEADER
            .checked_add(self.args.len())
            .and_then(|n| n.checked_add(4))
            .ok_or_else(|| invalid("intent frame length overflow"))?;
        let frame_len = unpadded
            .checked_add(WAL_ALIGNMENT - 1)
            .map(|n| n / WAL_ALIGNMENT * WAL_ALIGNMENT)
            .ok_or_else(|| invalid("intent frame length overflow"))?;
        let encoded_len = u32::try_from(frame_len)
            .map_err(|_| invalid("intent frame exceeds the format length domain"))?;
        let mut out = vec![0u8; frame_len];
        out[..4].copy_from_slice(WAL_MAGIC);
        out[4..6].copy_from_slice(&WAL_VERSION.to_le_bytes());
        out[8..12].copy_from_slice(&encoded_len.to_le_bytes());
        out[12..16].copy_from_slice(&payload_len.to_le_bytes());
        out[16..24].copy_from_slice(&self.tx_seq.to_le_bytes());
        out[24..28].copy_from_slice(&self.transition.to_le_bytes());
        let header_checksum = checksum(&out[..28]);
        out[28..32].copy_from_slice(&header_checksum.to_le_bytes());
        out[WAL_HEADER..WAL_HEADER + self.args.len()].copy_from_slice(&self.args);
        let checksum = checksum(&out[..frame_len - 4]);
        out[frame_len - 4..].copy_from_slice(&checksum.to_le_bytes());
        Ok(out)
    }

    pub fn decode(frame: &[u8]) -> io::Result<Self> {
        if frame.len() < WAL_ALIGNMENT || frame.len() % WAL_ALIGNMENT != 0 {
            return Err(invalid("invalid intent frame length"));
        }
        if &frame[..4] != WAL_MAGIC
            || u16::from_le_bytes(frame[4..6].try_into().unwrap()) != WAL_VERSION
        {
            return Err(invalid("unsupported intent WAL format"));
        }
        if frame[6..8] != [0, 0] {
            return Err(invalid("unknown intent WAL flags"));
        }
        if checksum(&frame[..28]) != u32::from_le_bytes(frame[28..32].try_into().unwrap()) {
            return Err(invalid("intent WAL header checksum mismatch"));
        }
        let declared = u32::from_le_bytes(frame[8..12].try_into().unwrap()) as usize;
        let payload_len = u32::from_le_bytes(frame[12..16].try_into().unwrap()) as usize;
        if declared != frame.len() || payload_len > frame.len() - WAL_HEADER - 4 {
            return Err(invalid("invalid intent payload length"));
        }
        let expected_checksum = u32::from_le_bytes(frame[frame.len() - 4..].try_into().unwrap());
        if checksum(&frame[..frame.len() - 4]) != expected_checksum {
            return Err(invalid("intent WAL checksum mismatch"));
        }
        if frame[WAL_HEADER + payload_len..frame.len() - 4]
            .iter()
            .any(|b| *b != 0)
        {
            return Err(invalid("noncanonical intent WAL padding"));
        }
        Ok(Self {
            tx_seq: u64::from_le_bytes(frame[16..24].try_into().unwrap()),
            intent_id: u64::from_le_bytes(frame[16..24].try_into().unwrap()),
            transition: u32::from_le_bytes(frame[24..28].try_into().unwrap()),
            args: frame[WAL_HEADER..WAL_HEADER + payload_len].to_vec(),
        })
    }
}

fn scan_records(data: &[u8]) -> io::Result<(Vec<IntentRecord>, usize)> {
    let mut records = Vec::new();
    let mut at = 0usize;
    let mut next_seq = 1u64;
    while at < data.len() {
        let remaining = &data[at..];
        // A torn append can leave any proper prefix of its final frame. A
        // complete but invalid frame is corruption and is never silently erased.
        if remaining.len() < WAL_HEADER {
            let prefix = remaining.len().min(WAL_MAGIC.len());
            if remaining[..prefix] != WAL_MAGIC[..prefix] {
                return Err(invalid("invalid trailing intent WAL header"));
            }
            break;
        }
        if &remaining[..4] != WAL_MAGIC
            || u16::from_le_bytes(remaining[4..6].try_into().unwrap()) != WAL_VERSION
        {
            return Err(invalid("unsupported intent WAL format"));
        }
        if remaining[6..8] != [0, 0] {
            return Err(invalid("unknown intent WAL flags"));
        }
        if checksum(&remaining[..28]) != u32::from_le_bytes(remaining[28..32].try_into().unwrap()) {
            return Err(invalid("intent WAL header checksum mismatch"));
        }
        let len = u32::from_le_bytes(remaining[8..12].try_into().unwrap()) as usize;
        let payload_len = u32::from_le_bytes(remaining[12..16].try_into().unwrap()) as usize;
        if len < WAL_ALIGNMENT || len % WAL_ALIGNMENT != 0 || payload_len > len - WAL_HEADER - 4 {
            return Err(invalid("invalid intent WAL frame length"));
        }
        if len > remaining.len() {
            break;
        }
        let record = IntentRecord::decode(&remaining[..len])?;
        if record.tx_seq != next_seq || record.intent_id != next_seq {
            return Err(invalid("intent WAL sequence is not consecutive"));
        }
        next_seq = next_seq
            .checked_add(1)
            .ok_or_else(|| invalid("intent WAL sequence exhausted"))?;
        records.push(record);
        at += len;
    }
    Ok((records, at))
}

/// Positional write retries EINTR and short writes. It never treats a partial
/// write as a successfully appended record.
pub(crate) fn write_all_at(file: &File, mut bytes: &[u8], mut offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::FileExt;
    #[cfg(windows)]
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        #[cfg(unix)]
        let written = file.write_at(bytes, offset);
        #[cfg(windows)]
        let written = file.seek_write(bytes, offset);
        match written {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "zero-length WAL write",
                ))
            }
            Ok(n) => {
                bytes = &bytes[n..];
                offset += n as u64;
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// A single-writer log. append() publishes a complete record in the file;
/// sync() is the durability boundary, and must precede acknowledging a batch.
pub struct WAL {
    file: File,
    path: PathBuf,
    offset: u64,
    pub tx_seq: u64,
    poisoned: bool,
}

impl WAL {
    pub fn open(path: &str) -> io::Result<Self> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => {
                io::Error::new(io::ErrorKind::WouldBlock, "WAL already has a writer")
            }
            std::fs::TryLockError::Error(error) => error,
        })?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        let (records, valid_len) = scan_records(&bytes)?;
        if valid_len != bytes.len() {
            file.set_len(valid_len as u64)?;
            file.sync_all()?;
        }
        let tx_seq = records.last().map(|r| r.tx_seq).unwrap_or(0);
        Ok(Self {
            file,
            path: PathBuf::from(path),
            offset: valid_len as u64,
            tx_seq,
            poisoned: false,
        })
    }

    pub fn uses_io_uring(&self) -> bool {
        false
    }

    pub fn append<A: IntoIntentArgs>(&mut self, transition: u32, args: A) -> io::Result<()> {
        self.append_using(transition, args, write_all_at)
    }

    pub(crate) fn append_using<A: IntoIntentArgs, F>(
        &mut self,
        transition: u32,
        args: A,
        mut write: F,
    ) -> io::Result<()>
    where
        F: FnMut(&File, &[u8], u64) -> io::Result<()>,
    {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "WAL has an unresolved I/O failure; reopen to recover",
            ));
        }
        let next_seq = self
            .tx_seq
            .checked_add(1)
            .ok_or_else(|| invalid("intent WAL sequence exhausted"))?;
        let record = IntentRecord {
            intent_id: next_seq,
            tx_seq: next_seq,
            transition,
            args: args.into_intent_args(),
        };
        let bytes = record.encode()?;
        let next_offset = self
            .offset
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| invalid("WAL offset overflow"))?;
        if let Err(error) = write(&self.file, &bytes, self.offset) {
            // Keep the sequence and append position unchanged. If rollback itself
            // fails, force an explicit reopen rather than permitting more writes.
            if self.file.set_len(self.offset).is_err() || self.file.sync_all().is_err() {
                self.poisoned = true;
            }
            return Err(error);
        }
        self.offset = next_offset;
        self.tx_seq = next_seq;
        Ok(())
    }

    pub fn sync(&mut self) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                "WAL has an unresolved I/O failure",
            ));
        }
        if let Err(error) = self.file.sync_all().and_then(|_| sync_parent(&self.path)) {
            self.poisoned = true;
            return Err(error);
        }
        Ok(())
    }

    pub fn replay(&self) -> io::Result<Vec<IntentRecord>> {
        let mut file = self.file.try_clone()?;
        file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        Ok(scan_records(&bytes)?.0)
    }
}

pub fn replay_file(path: &str) -> io::Result<Vec<IntentRecord>> {
    let bytes = std::fs::read(path)?;
    Ok(scan_records(&bytes)?.0)
}

fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        File::open(
            path.parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )?
        .sync_all()
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(())
    }
}

/// Write a complete snapshot beside its destination, fsync it, replace the
/// destination atomically and fsync the containing directory.
pub fn atomic_write(path: &str, bytes: &[u8]) -> io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT_TEMP: AtomicU64 = AtomicU64::new(0);
    let target = Path::new(path);
    let parent = target
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let name = target.file_name().ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "snapshot requires a file name")
    })?;
    let mut temporary = None;
    for _ in 0..32 {
        let mut temp_name = name.to_os_string();
        temp_name.push(format!(
            ".vera-tmp-{}-{}",
            std::process::id(),
            NEXT_TEMP.fetch_add(1, Ordering::Relaxed)
        ));
        let temp_path = parent.join(temp_name);
        match OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
        {
            Ok(file) => {
                temporary = Some((temp_path, file));
                break;
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    let (temp_path, mut file) = temporary.ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not reserve snapshot temporary file",
        )
    })?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp_path, target)?;
        sync_parent(target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp_path);
    }
    result
}

#[cfg(test)]
mod wal_tests {
    use super::*;

    #[test]
    fn variable_payload_roundtrip() {
        for count in [0, 1, 24, 25, 32, 255, 4096] {
            let record = IntentRecord {
                intent_id: 1,
                tx_seq: 1,
                transition: 70000,
                args: (0..count).map(|i| i as u8).collect(),
            };
            let bytes = record.encode().unwrap();
            assert_eq!(bytes.len() % 64, 0);
            assert_eq!(IntentRecord::decode(&bytes).unwrap(), record);
        }
    }

    #[test]
    fn every_torn_final_frame_recovers_the_same_prefix() {
        let first = IntentRecord {
            intent_id: 1,
            tx_seq: 1,
            transition: 1,
            args: vec![1, 2, 3],
        }
        .encode()
        .unwrap();
        let second = IntentRecord {
            intent_id: 2,
            tx_seq: 2,
            transition: 70000,
            args: vec![9; 255],
        }
        .encode()
        .unwrap();
        for cut in 0..second.len() {
            let mut torn = first.clone();
            torn.extend_from_slice(&second[..cut]);
            let (records, valid) = scan_records(&torn).unwrap();
            assert_eq!(records.len(), 1);
            assert_eq!(valid, first.len());
        }
        let mut complete = first;
        complete.extend_from_slice(&second);
        assert_eq!(scan_records(&complete).unwrap().0.len(), 2);
    }

    #[test]
    fn snapshot_replacement_is_complete_and_cleans_up_on_failure() {
        let directory = std::env::temp_dir().join(format!("vera-snapshot-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir(&directory).unwrap();
        let path = directory.join("state.snap");
        atomic_write(path.to_str().unwrap(), b"old").unwrap();
        atomic_write(path.to_str().unwrap(), b"replacement").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"replacement");
        assert!(atomic_write(directory.to_str().unwrap(), b"cannot replace a directory").is_err());
        assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 1);
        std::fs::remove_dir_all(directory).unwrap();
    }

    #[test]
    fn partial_append_failure_does_not_publish_a_sequence() {
        let path = std::env::temp_dir().join(format!("vera-wal-failure-{}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut wal = WAL::open(path.to_str().unwrap()).unwrap();
        let error = wal
            .append_using(9, b"first", |file, bytes, at| {
                write_all_at(file, &bytes[..9], at)?;
                Err(io::Error::new(
                    io::ErrorKind::Other,
                    "injected write failure",
                ))
            })
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Other);
        assert_eq!(wal.tx_seq, 0);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
        wal.append(10, b"retry").unwrap();
        wal.sync().unwrap();
        let records = wal.replay().unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].tx_seq, 1);
        assert_eq!(records[0].args, b"retry");
        drop(wal);
        std::fs::remove_file(path).unwrap();
    }
}
