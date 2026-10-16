//! Server-sent-events framing.
//!
//! The parser is byte-oriented because multi-byte UTF-8 characters can straddle two network
//! chunks, and decoding each chunk independently would corrupt them. Bytes are buffered until
//! a complete line is available, then decoded.

/// Splits a byte stream into `data:` payloads.
#[derive(Debug, Default)]
pub struct SseParser {
    buf: Vec<u8>,
}

impl SseParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed raw bytes, yielding the payload of every complete `data:` line.
    pub fn push(&mut self, bytes: &[u8]) -> Vec<String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();

        // Only consume up to the last newline. A partial trailing line stays buffered.
        while let Some(pos) = self.buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = self.buf.drain(..=pos).collect();
            if let Some(payload) = data_payload(&line[..line.len() - 1]) {
                out.push(payload);
            }
        }
        out
    }

    /// Flush a trailing line that arrived without a final newline.
    pub fn finish(&mut self) -> Vec<String> {
        if self.buf.is_empty() {
            return Vec::new();
        }
        let line = std::mem::take(&mut self.buf);
        data_payload(&line).into_iter().collect()
    }
}

/// Extract the payload of a `data:` field, ignoring every other SSE field.
///
/// `event:`, `id:`, `retry:` and `:` comments are all dropped: OpenAI-style streams carry
/// the entire message in a single-line `data:` field and use none of them.
fn data_payload(line: &[u8]) -> Option<String> {
    let line = line.strip_suffix(b"\r").unwrap_or(line);
    let text = std::str::from_utf8(line).ok()?;
    let rest = text.strip_prefix("data:")?;
    // Per the SSE spec a single leading space after the colon is not part of the value.
    Some(rest.strip_prefix(' ').unwrap_or(rest).to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extracts_data_lines_and_ignores_other_fields() {
        let mut p = SseParser::new();
        let out = p.push(b": keepalive\nevent: message\ndata: {\"a\":1}\n\n");
        assert_eq!(out, vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn holds_partial_lines_until_terminated() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"partial\":").is_empty());
        let out = p.push(b"true}\n");
        assert_eq!(out, vec!["{\"partial\":true}".to_string()]);
    }

    /// The realistic case: the network splits a frame mid-way, and the split falls inside a
    /// multi-byte character.
    #[test]
    fn does_not_corrupt_utf8_split_across_chunks() {
        let text = "data: {\"t\":\"héllo → wörld\"}\n";
        let split = 14; // inside the two-byte 'é'
        assert!(!text.is_char_boundary(split), "test needs a mid-char split");
        let frame = text.as_bytes();

        let mut p = SseParser::new();
        assert!(p.push(&frame[..split]).is_empty());
        let out = p.push(&frame[split..]);
        assert_eq!(out, vec!["{\"t\":\"héllo → wörld\"}".to_string()]);
    }

    #[test]
    fn detects_done_sentinel() {
        let mut p = SseParser::new();
        assert_eq!(p.push(b"data: [DONE]\n"), vec!["[DONE]".to_string()]);
    }

    #[test]
    fn finish_flushes_unterminated_line() {
        let mut p = SseParser::new();
        assert!(p.push(b"data: {\"a\":1}").is_empty());
        assert_eq!(p.finish(), vec!["{\"a\":1}".to_string()]);
    }

    #[test]
    fn tolerates_crlf_line_endings() {
        let mut p = SseParser::new();
        assert_eq!(
            p.push(b"data: {\"a\":1}\r\n"),
            vec!["{\"a\":1}".to_string()]
        );
    }
}
