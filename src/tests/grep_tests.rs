use crate::agent::tools::GrepTool;

struct SniffOnlyBinaryReader {
    emitted: usize,
}

impl std::io::Read for SniffOnlyBinaryReader {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        assert!(
            self.emitted < 8192,
            "binary detection must not read beyond the sniff window"
        );
        let count = buffer.len().min(8192 - self.emitted);
        buffer[..count].fill(b'a');
        if self.emitted == 0 && count > 0 {
            buffer[0] = 0;
        }
        self.emitted += count;
        Ok(count)
    }
}

#[test]
fn glob_literal_chars() {
    assert_eq!(GrepTool::glob_to_regex("hello"), "hello");
}

#[test]
fn glob_dot() {
    assert_eq!(GrepTool::glob_to_regex("file.txt"), "file\\.txt");
}

#[test]
fn glob_star() {
    assert_eq!(GrepTool::glob_to_regex("*.rs"), "[^/]*\\.rs");
}

#[test]
fn glob_question_mark() {
    assert_eq!(GrepTool::glob_to_regex("file.?"), "file\\.[^/]");
}

#[test]
fn glob_brace_alternation() {
    assert_eq!(GrepTool::glob_to_regex("*.{ts,tsx}"), "[^/]*\\.(?:ts|tsx)");
}

#[test]
fn glob_complex_pattern() {
    assert_eq!(
        GrepTool::glob_to_regex("src/**/test_*.{rs,toml}"),
        "src/(?:.*/)?test_[^/]*\\.(?:rs|toml)"
    );
}

#[test]
fn glob_empty() {
    assert_eq!(GrepTool::glob_to_regex(""), "");
}

#[test]
fn is_binary_null_byte_in_first_8k() {
    let mut data = vec![b'a'; 100];
    data[50] = 0;
    assert!(GrepTool::is_binary(&data));
}

#[test]
fn is_binary_no_null_byte() {
    let data = vec![b'a'; 100];
    assert!(!GrepTool::is_binary(&data));
}

#[test]
fn is_binary_empty() {
    assert!(!GrepTool::is_binary(&[]));
}

#[test]
fn is_binary_null_at_start() {
    let data = vec![0, b'a', b'b'];
    assert!(GrepTool::is_binary(&data));
}

#[test]
fn is_binary_null_at_end() {
    let mut data = vec![b'a'; 8192];
    data[8191] = 0;
    assert!(GrepTool::is_binary(&data));
}

#[test]
fn is_binary_all_text() {
    let data = b"hello world\nline 2\nline 3\n";
    assert!(!GrepTool::is_binary(data));
}

#[test]
fn is_binary_non_utf8_no_null() {
    let data = vec![0xFF, 0xFE, 0xFD];
    assert!(!GrepTool::is_binary(&data));
}

#[test]
fn binary_reader_stops_after_the_sniff_window() {
    let mut reader = SniffOnlyBinaryReader { emitted: 0 };
    let result = GrepTool::read_non_binary(&mut reader, 32 * 1024).unwrap();
    assert!(result.is_none());
    assert_eq!(reader.emitted, 8192);
}

#[test]
fn text_reader_preserves_the_sniffed_prefix_and_remaining_bytes() {
    let expected = vec![b'x'; 10 * 1024];
    let mut reader = std::io::Cursor::new(expected.clone());
    let result = GrepTool::read_non_binary(&mut reader, expected.len())
        .unwrap()
        .expect("text input");
    assert_eq!(result, expected);
}
