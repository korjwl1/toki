use std::io::{BufRead, Read, Seek, SeekFrom};

use md5::{Digest, Md5};

use crate::common::types::FileCheckpoint;

/// Read complete lines from a file starting at the given byte offset.
/// Drops any incomplete trailing line (still being written).
/// Returns (lines, bytes_read).
pub fn read_from_offset(path: &str, offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;

    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    if data.is_empty() {
        return Ok((Vec::new(), 0));
    }

    // Drop incomplete last line
    if !data.ends_with(b"\n") {
        if let Some(pos) = data.iter().rposition(|&b| b == b'\n') {
            data.truncate(pos + 1);
        } else {
            return Ok((Vec::new(), 0));
        }
    }

    let bytes_read = data.len() as u64;
    let text = String::from_utf8_lossy(&data);
    let lines: Vec<String> = text.lines().map(|l| l.to_string()).collect();

    Ok((lines, bytes_read))
}

/// Verify checkpoint by comparing the 128 bytes before the offset.
pub fn verify_checkpoint(path: &str, cp: &FileCheckpoint) -> std::io::Result<bool> {
    if cp.checkpoint_bytes.is_empty() {
        return Ok(true);
    }

    let check_size = cp.checkpoint_bytes.len() as u64;
    let read_start = cp.last_offset.saturating_sub(check_size);
    let read_len = (cp.last_offset - read_start) as usize;

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(read_start))?;

    let mut buf = vec![0u8; read_len];
    file.read_exact(&mut buf)?;

    Ok(buf == cp.checkpoint_bytes)
}

/// Recover reading position by scanning for a line matching the target MD5 hash.
/// Returns the byte offset just after the last matching line.
pub fn recover_by_hash(path: &str, target_hash: &str) -> std::io::Result<Option<u64>> {
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut last_match_end: Option<u64> = None;
    let mut pos: u64 = 0;

    for line in reader.lines() {
        let line = line?;
        let line_bytes = line.len() as u64 + 1; // +1 for newline

        let mut hasher = Md5::new();
        hasher.update(line.as_bytes());
        let hash = format!("{:x}", hasher.finalize());

        if hash == target_hash {
            last_match_end = Some(pos + line_bytes);
        }
        pos += line_bytes;
    }

    Ok(last_match_end)
}

/// Read the 128 bytes just before the given offset (for checkpoint verification).
pub fn read_checkpoint_bytes(path: &str, offset: u64) -> std::io::Result<Vec<u8>> {
    let check_size: u64 = 128;
    let read_start = offset.saturating_sub(check_size);
    let read_len = (offset - read_start) as usize;

    if read_len == 0 {
        return Ok(Vec::new());
    }

    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(read_start))?;

    let mut buf = vec![0u8; read_len];
    file.read_exact(&mut buf)?;

    Ok(buf)
}

/// Compute MD5 hash of a line (for checkpoint storage).
pub fn hash_line(line: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(line.as_bytes());
    format!("{:x}", hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn create_test_file(content: &str) -> (tempfile::NamedTempFile, String) {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        let path = f.path().to_str().unwrap().to_string();
        (f, path)
    }

    #[test]
    fn test_read_from_offset_start() {
        let (_f, path) = create_test_file("line1\nline2\nline3\n");
        let (lines, bytes) = read_from_offset(&path, 0).unwrap();

        assert_eq!(lines, vec!["line1", "line2", "line3"]);
        assert_eq!(bytes, 18); // 6+6+6
    }

    #[test]
    fn test_read_from_offset_middle() {
        let (_f, path) = create_test_file("line1\nline2\nline3\n");
        let (lines, bytes) = read_from_offset(&path, 6).unwrap();

        assert_eq!(lines, vec!["line2", "line3"]);
        assert_eq!(bytes, 12);
    }

    #[test]
    fn test_read_from_offset_drops_incomplete() {
        let (_f, path) = create_test_file("line1\nline2\nincomple");
        let (lines, bytes) = read_from_offset(&path, 0).unwrap();

        assert_eq!(lines, vec!["line1", "line2"]);
        assert_eq!(bytes, 12);
    }

    #[test]
    fn test_read_from_offset_empty() {
        let (_f, path) = create_test_file("");
        let (lines, bytes) = read_from_offset(&path, 0).unwrap();

        assert!(lines.is_empty());
        assert_eq!(bytes, 0);
    }

    #[test]
    fn test_read_from_offset_no_complete_line() {
        let (_f, path) = create_test_file("no newline here");
        let (lines, bytes) = read_from_offset(&path, 0).unwrap();

        assert!(lines.is_empty());
        assert_eq!(bytes, 0);
    }

    #[test]
    fn test_verify_checkpoint_valid() {
        let content = "aaaa\nbbbb\ncccc\n";
        let (_f, path) = create_test_file(content);

        let cp = FileCheckpoint {
            file_path: path.clone(),
            last_offset: 10, // after "aaaa\nbbbb\n"
            last_line_hash: String::new(),
            checkpoint_bytes: b"aaaa\nbbbb\n".to_vec(),
        };

        assert!(verify_checkpoint(&path, &cp).unwrap());
    }

    #[test]
    fn test_verify_checkpoint_invalid() {
        let content = "aaaa\nbbbb\ncccc\n";
        let (_f, path) = create_test_file(content);

        let cp = FileCheckpoint {
            file_path: path.clone(),
            last_offset: 10,
            last_line_hash: String::new(),
            checkpoint_bytes: b"xxxx\nyyyy\n".to_vec(),
        };

        assert!(!verify_checkpoint(&path, &cp).unwrap());
    }

    #[test]
    fn test_verify_checkpoint_empty_bytes() {
        let (_f, path) = create_test_file("anything\n");

        let cp = FileCheckpoint {
            file_path: path.clone(),
            last_offset: 9,
            last_line_hash: String::new(),
            checkpoint_bytes: vec![],
        };

        assert!(verify_checkpoint(&path, &cp).unwrap());
    }

    #[test]
    fn test_recover_by_hash() {
        let content = "line1\nline2\nline3\n";
        let (_f, path) = create_test_file(content);

        let target_hash = hash_line("line2");
        let offset = recover_by_hash(&path, &target_hash).unwrap().unwrap();

        // After "line1\nline2\n" = 6 + 6 = 12
        assert_eq!(offset, 12);
    }

    #[test]
    fn test_recover_by_hash_not_found() {
        let content = "line1\nline2\n";
        let (_f, path) = create_test_file(content);

        let target_hash = hash_line("nonexistent");
        assert!(recover_by_hash(&path, &target_hash).unwrap().is_none());
    }

    #[test]
    fn test_recover_by_hash_last_occurrence() {
        let content = "dup\nother\ndup\n";
        let (_f, path) = create_test_file(content);

        let target_hash = hash_line("dup");
        let offset = recover_by_hash(&path, &target_hash).unwrap().unwrap();

        // Last "dup\n" ends at 4 + 6 + 4 = 14
        assert_eq!(offset, 14);
    }

    #[test]
    fn test_read_checkpoint_bytes() {
        let content = "abcdefghij\n";
        let (_f, path) = create_test_file(content);

        let bytes = read_checkpoint_bytes(&path, 10).unwrap();
        assert_eq!(bytes, b"abcdefghij");
    }

    #[test]
    fn test_read_checkpoint_bytes_short_file() {
        let content = "short\n";
        let (_f, path) = create_test_file(content);

        // Offset 6, file is only 6 bytes, so we read all 6
        let bytes = read_checkpoint_bytes(&path, 6).unwrap();
        assert_eq!(bytes, b"short\n");
    }

    #[test]
    fn test_hash_line() {
        let h1 = hash_line("hello");
        let h2 = hash_line("hello");
        let h3 = hash_line("world");

        assert_eq!(h1, h2);
        assert_ne!(h1, h3);
        assert_eq!(h1.len(), 32); // MD5 hex is 32 chars
    }
}
