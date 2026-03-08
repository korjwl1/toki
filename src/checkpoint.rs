use std::io::{BufRead, Read, Seek, SeekFrom};

use xxhash_rust::xxh3::xxh3_64;

use crate::common::types::FileCheckpoint;

const CHUNK_SIZE: u64 = 4096;

/// Compute xxHash3-64 of a line's raw bytes.
#[inline]
pub fn hash_line(line: &[u8]) -> u64 {
    xxh3_64(line)
}

/// Reverse-scan from file end to find the last processed line.
/// Returns the byte offset immediately after that line (after `\n` or EOF),
/// or `None` if the line no longer exists in the file (full reprocess needed).
pub fn find_resume_offset(path: &str, cp: &FileCheckpoint) -> std::io::Result<Option<u64>> {
    let mut file = std::fs::File::open(path)?;
    let file_size = file.metadata()?.len();

    if file_size == 0 {
        return Ok(None);
    }

    // Bytes carried from the beginning of the previously-read (rightward) chunk.
    // These form the tail end of a line whose start is in an earlier chunk.
    let mut fragment: Vec<u8> = Vec::new();
    // If the line containing `fragment` matches, this is the resume offset.
    let mut fragment_resume: u64 = file_size;
    // Whether `fragment` has been combined with a complete line yet.
    let mut fragment_consumed = true;

    let mut cursor = file_size;

    // Check trailing content without \n at end of file.
    // If it matches the checkpoint, resume from EOF.
    {
        let tail_size = std::cmp::min(file_size, CHUNK_SIZE);
        let tail_start = file_size - tail_size;
        let mut buf = vec![0u8; tail_size as usize];
        file.seek(SeekFrom::Start(tail_start))?;
        file.read_exact(&mut buf)?;

        if !buf.ends_with(b"\n") {
            // Find trailing segment after last \n.
            let trailing_start = match buf.iter().rposition(|&b| b == b'\n') {
                Some(pos) => pos + 1,
                None => 0,
            };
            let trailing = &buf[trailing_start..];
            if !trailing.is_empty() && trailing.len() as u64 == cp.last_line_len {
                if hash_line(trailing) == cp.last_line_hash {
                    return Ok(Some(file_size));
                }
            }
        }
    }

    while cursor > 0 {
        let read_start = cursor.saturating_sub(CHUNK_SIZE);
        let read_len = (cursor - read_start) as usize;

        let mut buf = vec![0u8; read_len];
        file.seek(SeekFrom::Start(read_start))?;
        file.read_exact(&mut buf)?;

        // Scan newlines from right to left.
        // `line_end` tracks the boundary: content of the current line is buf[newline_pos+1..line_end].
        // When line_end is at a \n position, resume_offset = read_start + line_end + 1.
        let mut line_end = buf.len();
        let mut found_newline = false;

        for i in (0..buf.len()).rev() {
            if buf[i] != b'\n' {
                continue;
            }
            found_newline = true;

            // Content between this \n and the previous boundary.
            let content_in_buf = &buf[i + 1..line_end];

            if !fragment_consumed {
                // First \n from the right: combine with carried fragment.
                let mut full_line = content_in_buf.to_vec();
                full_line.extend_from_slice(&fragment);
                fragment.clear();
                fragment_consumed = true;

                if !full_line.is_empty() && full_line.len() as u64 == cp.last_line_len {
                    if hash_line(&full_line) == cp.last_line_hash {
                        return Ok(Some(fragment_resume));
                    }
                }
            } else {
                // Complete line entirely within this chunk.
                if !content_in_buf.is_empty()
                    && content_in_buf.len() as u64 == cp.last_line_len
                {
                    if hash_line(content_in_buf) == cp.last_line_hash {
                        // \n terminating this line is at buf[line_end] (file pos read_start + line_end).
                        return Ok(Some(read_start + line_end as u64 + 1));
                    }
                }
            }

            line_end = i;
        }

        // Everything before the leftmost \n is a partial line fragment.
        let leftover = &buf[..line_end];
        if found_newline {
            // We know where the \n for this fragment is: buf[line_end], file pos = read_start + line_end.
            fragment = leftover.to_vec();
            fragment_resume = read_start + line_end as u64 + 1;
            fragment_consumed = false;
        } else {
            // No \n in this entire chunk — prepend to existing fragment.
            let mut combined = leftover.to_vec();
            combined.extend_from_slice(&fragment);
            fragment = combined;
            // fragment_resume stays the same (the \n is in a rightward chunk).
        }

        cursor = read_start;
    }

    // Check the very first line of the file (no preceding \n).
    if !fragment.is_empty() && fragment.len() as u64 == cp.last_line_len {
        if hash_line(&fragment) == cp.last_line_hash {
            return Ok(Some(fragment_resume));
        }
    }

    // Line not found — compacted away entirely.
    Ok(None)
}

/// Check if bytes form a complete JSON object by bracket-depth counting.
/// Handles strings and escape sequences correctly. O(n) with minimal branching.
#[inline]
fn is_complete_json_object(bytes: &[u8]) -> bool {
    if bytes.first() != Some(&b'{') {
        return false;
    }
    let mut depth: i32 = 0;
    let mut in_string = false;
    let mut escape = false;
    for &b in bytes {
        if escape {
            escape = false;
            continue;
        }
        if in_string {
            if b == b'\\' {
                escape = true;
            } else if b == b'"' {
                in_string = false;
            }
            continue;
        }
        match b {
            b'"' => in_string = true,
            b'{' => depth += 1,
            b'}' => depth -= 1,
            _ => {}
        }
    }
    depth == 0
}

/// Read complete lines from a file starting at the given byte offset.
/// Lines ending with `\n` are always included.
/// A trailing segment without `\n` is included only if it forms a complete JSON object
/// (balanced `{}`), otherwise it is dropped as an incomplete write.
/// Returns (lines, bytes_consumed).
pub fn read_lines_from(path: &str, offset: u64) -> std::io::Result<(Vec<String>, u64)> {
    let mut file = std::fs::File::open(path)?;
    file.seek(SeekFrom::Start(offset))?;

    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    if data.is_empty() {
        return Ok((Vec::new(), 0));
    }

    let mut bytes_consumed = data.len() as u64;

    // Handle trailing segment without \n.
    if !data.ends_with(b"\n") {
        // Find the trailing segment (after last \n, or entire data if no \n).
        let last_newline = data.iter().rposition(|&b| b == b'\n');
        let trailing_start = match last_newline {
            Some(pos) => pos + 1,
            None => 0,
        };
        let trailing = &data[trailing_start..];

        if is_complete_json_object(trailing) {
            // Complete JSON without trailing \n — include it.
            // bytes_consumed covers everything including this segment.
        } else {
            // Incomplete write — drop the trailing segment.
            if let Some(pos) = last_newline {
                data.truncate(pos + 1);
                bytes_consumed = data.len() as u64;
            } else {
                return Ok((Vec::new(), 0));
            }
        }
    }

    let reader = std::io::BufReader::new(&data[..]);
    let lines: Vec<String> = reader
        .lines()
        .filter_map(|l| l.ok())
        .collect();

    Ok((lines, bytes_consumed))
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

    fn make_checkpoint(line: &str, file_path: &str) -> FileCheckpoint {
        FileCheckpoint {
            file_path: file_path.to_string(),
            last_line_len: line.len() as u64,
            last_line_hash: hash_line(line.as_bytes()),
        }
    }

    // -- hash_line tests --

    #[test]
    fn test_hash_line_deterministic() {
        let h1 = hash_line(b"hello world");
        let h2 = hash_line(b"hello world");
        assert_eq!(h1, h2);
    }

    #[test]
    fn test_hash_line_different_inputs() {
        let h1 = hash_line(b"hello");
        let h2 = hash_line(b"world");
        assert_ne!(h1, h2);
    }

    // -- read_lines_from tests --

    #[test]
    fn test_read_lines_from_start() {
        let (_f, path) = create_test_file("line1\nline2\nline3\n");
        let (lines, bytes) = read_lines_from(&path, 0).unwrap();
        assert_eq!(lines, vec!["line1", "line2", "line3"]);
        assert_eq!(bytes, 18);
    }

    #[test]
    fn test_read_lines_from_middle() {
        let (_f, path) = create_test_file("line1\nline2\nline3\n");
        let (lines, bytes) = read_lines_from(&path, 6).unwrap();
        assert_eq!(lines, vec!["line2", "line3"]);
        assert_eq!(bytes, 12);
    }

    #[test]
    fn test_read_lines_drops_incomplete() {
        let (_f, path) = create_test_file("line1\nline2\nincomple");
        let (lines, bytes) = read_lines_from(&path, 0).unwrap();
        assert_eq!(lines, vec!["line1", "line2"]);
        assert_eq!(bytes, 12);
    }

    #[test]
    fn test_read_lines_empty() {
        let (_f, path) = create_test_file("");
        let (lines, bytes) = read_lines_from(&path, 0).unwrap();
        assert!(lines.is_empty());
        assert_eq!(bytes, 0);
    }

    #[test]
    fn test_read_lines_no_complete_line() {
        let (_f, path) = create_test_file("no newline here");
        let (lines, bytes) = read_lines_from(&path, 0).unwrap();
        assert!(lines.is_empty());
        assert_eq!(bytes, 0);
    }

    // -- find_resume_offset tests --

    #[test]
    fn test_find_resume_last_line() {
        let (_f, path) = create_test_file("line1\nline2\nline3\n");
        let cp = make_checkpoint("line3", &path);
        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();
        // "line1\nline2\nline3\n" = 18 bytes, resume after line3 = 18
        assert_eq!(offset, 18);
    }

    #[test]
    fn test_find_resume_middle_line() {
        let (_f, path) = create_test_file("line1\nline2\nline3\n");
        let cp = make_checkpoint("line2", &path);
        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();
        // "line1\nline2\n" = 12 bytes
        assert_eq!(offset, 12);
    }

    #[test]
    fn test_find_resume_first_line() {
        let (_f, path) = create_test_file("line1\nline2\nline3\n");
        let cp = make_checkpoint("line1", &path);
        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();
        assert_eq!(offset, 6);
    }

    #[test]
    fn test_find_resume_not_found() {
        let (_f, path) = create_test_file("line1\nline2\nline3\n");
        let cp = make_checkpoint("nonexistent", &path);
        assert!(find_resume_offset(&path, &cp).unwrap().is_none());
    }

    #[test]
    fn test_find_resume_empty_file() {
        let (_f, path) = create_test_file("");
        let cp = make_checkpoint("anything", &path);
        assert!(find_resume_offset(&path, &cp).unwrap().is_none());
    }

    #[test]
    fn test_find_resume_after_compaction() {
        // Simulate: original file had lines A,B,C,D. Compaction removed A,B.
        // Checkpoint was on C. File now has C,D.
        let (_f, path) = create_test_file("lineC\nlineD\n");
        let cp = make_checkpoint("lineC", &path);
        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();
        assert_eq!(offset, 6); // after "lineC\n"
    }

    #[test]
    fn test_find_resume_compaction_removed_checkpoint_line() {
        // Checkpoint was on line B, but compaction removed everything up to D.
        let (_f, path) = create_test_file("lineD\nlineE\n");
        let cp = make_checkpoint("lineB", &path);
        assert!(find_resume_offset(&path, &cp).unwrap().is_none());
    }

    #[test]
    fn test_find_resume_duplicate_lines() {
        // If duplicate lines exist, should match the LAST occurrence (scanning from end).
        let (_f, path) = create_test_file("dup\nother\ndup\n");
        let cp = make_checkpoint("dup", &path);
        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();
        // Last "dup\n" ends at 4+6+4 = 14
        assert_eq!(offset, 14);
    }

    #[test]
    fn test_find_resume_large_file_across_chunks() {
        // Create a file larger than CHUNK_SIZE to test chunk-boundary handling.
        let mut content = String::new();
        let target_line = "TARGET_LINE_HERE_12345";
        for i in 0..200 {
            content.push_str(&format!("padding line number {} with some extra data to make it longer\n", i));
        }
        content.push_str(target_line);
        content.push('\n');
        for i in 200..210 {
            content.push_str(&format!("trailing line {}\n", i));
        }

        let (_f, path) = create_test_file(&content);
        let cp = make_checkpoint(target_line, &path);

        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();

        // Verify: reading from offset should give us the trailing lines.
        let (lines, _) = read_lines_from(&path, offset).unwrap();
        assert_eq!(lines.len(), 10);
        assert!(lines[0].starts_with("trailing line 200"));
    }

    #[test]
    fn test_find_resume_line_spanning_chunk_boundary() {
        // Create a line that's likely to span a chunk boundary.
        let mut content = String::new();
        // Fill most of a chunk with small lines.
        let mut byte_count: usize = 0;
        let mut line_num = 0;
        while byte_count < (CHUNK_SIZE as usize - 50) {
            let line = format!("line{:04}\n", line_num);
            byte_count += line.len();
            content.push_str(&line);
            line_num += 1;
        }
        // Now add a long line that will definitely span the boundary.
        let long_line = "X".repeat(200);
        content.push_str(&long_line);
        content.push('\n');
        content.push_str("after_long\n");

        let (_f, path) = create_test_file(&content);
        let cp = make_checkpoint(&long_line, &path);

        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();
        let (lines, _) = read_lines_from(&path, offset).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "after_long");
    }

    #[test]
    fn test_find_resume_line_exceeding_chunk_size() {
        // Line bigger than CHUNK_SIZE (4KB) — spans multiple chunks.
        let big_json = format!(r#"{{"type":"assistant","data":"{}"}}"#, "X".repeat(10000));
        let mut content = String::new();
        content.push_str("before1\n");
        content.push_str("before2\n");
        content.push_str(&big_json);
        content.push('\n');
        content.push_str("after1\n");

        let (_f, path) = create_test_file(&content);
        let cp = make_checkpoint(&big_json, &path);

        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();
        let (lines, _) = read_lines_from(&path, offset).unwrap();
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0], "after1");
    }

    #[test]
    fn test_find_resume_very_large_line_multi_chunk() {
        // 66KB line — realistic file-history-snapshot size, spans ~16 chunks.
        let huge_line = "Y".repeat(66000);
        let mut content = String::new();
        content.push_str("small\n");
        content.push_str(&huge_line);
        content.push('\n');
        content.push_str("end\n");

        let (_f, path) = create_test_file(&content);
        let cp = make_checkpoint(&huge_line, &path);

        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();
        let (lines, _) = read_lines_from(&path, offset).unwrap();
        assert_eq!(lines, vec!["end"]);
    }

    // -- is_complete_json_object tests --

    #[test]
    fn test_complete_json_simple() {
        assert!(is_complete_json_object(b"{}"));
        assert!(is_complete_json_object(br#"{"key":"value"}"#));
    }

    #[test]
    fn test_complete_json_nested() {
        assert!(is_complete_json_object(br#"{"a":{"b":{"c":1}}}"#));
    }

    #[test]
    fn test_complete_json_braces_in_string() {
        assert!(is_complete_json_object(br#"{"msg":"hello {world}"}"#));
        assert!(is_complete_json_object(br#"{"msg":"}{}{}"}"#));
    }

    #[test]
    fn test_complete_json_escaped_quotes() {
        assert!(is_complete_json_object(br#"{"msg":"say \"hi\""}"#));
    }

    #[test]
    fn test_incomplete_json() {
        assert!(!is_complete_json_object(br#"{"key":"val"#));
        assert!(!is_complete_json_object(br#"{"key":"#));
        assert!(!is_complete_json_object(b"{"));
    }

    #[test]
    fn test_not_json_object() {
        assert!(!is_complete_json_object(b"not json"));
        assert!(!is_complete_json_object(b"[1,2,3]"));
        assert!(!is_complete_json_object(b""));
    }

    // -- read_lines_from: trailing line without \n --

    #[test]
    fn test_read_lines_complete_json_no_trailing_newline() {
        let (_f, path) = create_test_file(r#"{"a":1}
{"b":2}"#);
        let (lines, bytes) = read_lines_from(&path, 0).unwrap();
        assert_eq!(lines, vec![r#"{"a":1}"#, r#"{"b":2}"#]);
        assert_eq!(bytes, 15); // entire file
    }

    #[test]
    fn test_read_lines_incomplete_json_no_trailing_newline() {
        let (_f, path) = create_test_file("{\"a\":1}\n{\"b\":2,\"incompl");
        let (lines, bytes) = read_lines_from(&path, 0).unwrap();
        assert_eq!(lines, vec![r#"{"a":1}"#]);
        assert_eq!(bytes, 8); // only up to first \n
    }

    #[test]
    fn test_read_lines_only_incomplete_no_newline() {
        let (_f, path) = create_test_file("{\"incomplete");
        let (lines, bytes) = read_lines_from(&path, 0).unwrap();
        assert!(lines.is_empty());
        assert_eq!(bytes, 0);
    }

    #[test]
    fn test_read_lines_only_complete_no_newline() {
        let (_f, path) = create_test_file(r#"{"only":true}"#);
        let (lines, bytes) = read_lines_from(&path, 0).unwrap();
        assert_eq!(lines, vec![r#"{"only":true}"#]);
        assert_eq!(bytes, 13);
    }

    // -- find_resume_offset: no trailing \n --

    #[test]
    fn test_find_resume_no_trailing_newline() {
        // File doesn't end with \n but last line is complete.
        let (_f, path) = create_test_file("line1\nline2");
        let cp = make_checkpoint("line2", &path);
        let offset = find_resume_offset(&path, &cp).unwrap().unwrap();
        assert_eq!(offset, 11); // EOF
    }

    // -- Integration: find_resume + read_lines --

    #[test]
    fn test_incremental_read_after_append() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let path_str = path.to_str().unwrap();

        // Initial write.
        std::fs::write(&path, "line1\nline2\n").unwrap();
        let (lines, _) = read_lines_from(path_str, 0).unwrap();
        assert_eq!(lines, vec!["line1", "line2"]);

        let cp = make_checkpoint("line2", path_str);

        // Append new data.
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"line3\nline4\n").unwrap();

        // Resume from checkpoint.
        let offset = find_resume_offset(path_str, &cp).unwrap().unwrap();
        let (new_lines, _) = read_lines_from(path_str, offset).unwrap();
        assert_eq!(new_lines, vec!["line3", "line4"]);
    }

    #[test]
    fn test_incremental_read_after_compaction() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("test.jsonl");
        let path_str = path.to_str().unwrap();

        // Original: 4 lines.
        std::fs::write(&path, "line1\nline2\nline3\nline4\n").unwrap();
        let cp = make_checkpoint("line3", path_str);

        // Compaction: remove first 2 lines, add a new one.
        std::fs::write(&path, "line3\nline4\nline5\n").unwrap();

        let offset = find_resume_offset(path_str, &cp).unwrap().unwrap();
        let (new_lines, _) = read_lines_from(path_str, offset).unwrap();
        assert_eq!(new_lines, vec!["line4", "line5"]);
    }
}
