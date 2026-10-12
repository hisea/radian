//! A small server-sent events reader: bytes in, event data out.
//!
//! It works on bytes and decodes a line only once the line is complete, so a multi-byte
//! character split across two network chunks is fine. Line ends may be `\n`, `\r\n` or `\r`,
//! including a `\r\n` split across chunks.

use agent_core::ModelError;

/// One line may not be longer than this. A server that never ends a line cannot grow memory.
const MAX_LINE_BYTES: usize = 4 * 1024 * 1024;

#[derive(Debug, Default)]
pub(crate) struct SseParser {
    line: Vec<u8>,
    data: String,
    has_data: bool,
    /// The previous line ended in `\r`; a `\n` right after it belongs to the same line end.
    skip_lf: bool,
}

impl SseParser {
    /// Feeds bytes and returns the `data` of every event completed by them.
    pub(crate) fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, ModelError> {
        let mut events = Vec::new();
        for &byte in bytes {
            if std::mem::take(&mut self.skip_lf) && byte == b'\n' {
                continue;
            }
            match byte {
                b'\n' => self.end_line(&mut events)?,
                b'\r' => {
                    self.skip_lf = true;
                    self.end_line(&mut events)?;
                }
                _ => {
                    if self.line.len() >= MAX_LINE_BYTES {
                        return Err(ModelError::Protocol(format!(
                            "a stream line is longer than {MAX_LINE_BYTES} bytes"
                        )));
                    }
                    self.line.push(byte);
                }
            }
        }
        Ok(events)
    }

    /// The event still pending when the stream ends. The SSE spec drops it, but some servers
    /// end `data: [DONE]` without the final blank line, so it is returned here and the caller
    /// decides whether it is usable.
    pub(crate) fn finish(&mut self) -> Result<Option<String>, ModelError> {
        let mut events = Vec::new();
        if !self.line.is_empty() {
            self.end_line(&mut events)?;
        }
        if self.has_data {
            events.push(self.take_data());
        }
        Ok(events.into_iter().next_back())
    }

    fn end_line(&mut self, events: &mut Vec<String>) -> Result<(), ModelError> {
        let line = std::mem::take(&mut self.line);
        if line.is_empty() {
            if self.has_data {
                events.push(self.take_data());
            }
            return Ok(());
        }
        let line = String::from_utf8(line)
            .map_err(|_| ModelError::Protocol("the stream is not valid UTF-8".into()))?;
        if line.starts_with(':') {
            return Ok(()); // a comment, often a keep-alive
        }
        let (field, value) = line.split_once(':').unwrap_or((&line, ""));
        if field == "data" {
            self.data.push_str(value.strip_prefix(' ').unwrap_or(value));
            self.data.push('\n');
            self.has_data = true;
        }
        // `event`, `id` and `retry` carry nothing the adapter uses.
        Ok(())
    }

    fn take_data(&mut self) -> String {
        self.has_data = false;
        let mut data = std::mem::take(&mut self.data);
        data.pop(); // the newline added after the last data line
        data
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all(chunks: &[&[u8]]) -> Vec<String> {
        let mut parser = SseParser::default();
        let mut out = Vec::new();
        for chunk in chunks {
            out.extend(parser.push(chunk).unwrap());
        }
        out.extend(parser.finish().unwrap());
        out
    }

    #[test]
    fn events_are_split_at_blank_lines() {
        assert_eq!(all(&[b"data: a\n\ndata: b\n\n"]), ["a", "b"]);
    }

    #[test]
    fn a_split_anywhere_gives_the_same_events() {
        let stream = b"data: {\"x\":1}\r\n\r\n: keep-alive\r\ndata: two\r\ndata: lines\r\n\r\ndata: [DONE]\r\n\r\n";
        let expected = all(&[stream]);
        assert_eq!(expected, ["{\"x\":1}", "two\nlines", "[DONE]"]);
        for cut in 0..stream.len() {
            let (a, b) = stream.split_at(cut);
            assert_eq!(all(&[a, b]), expected, "split at byte {cut}");
        }
        // One byte at a time, which also splits every `\r\n`.
        let bytes: Vec<&[u8]> = stream.chunks(1).collect();
        assert_eq!(all(&bytes), expected);
    }

    #[test]
    fn a_multibyte_character_split_across_chunks_survives() {
        let stream = "data: 你好, 世界 😀\n\n".as_bytes();
        for cut in 0..stream.len() {
            let (a, b) = stream.split_at(cut);
            assert_eq!(all(&[a, b]), ["你好, 世界 😀"], "split at byte {cut}");
        }
    }

    #[test]
    fn a_bare_carriage_return_ends_a_line() {
        assert_eq!(all(&[b"data: a\r\rdata: b\r\r"]), ["a", "b"]);
    }

    #[test]
    fn the_final_event_without_a_blank_line_is_still_returned() {
        assert_eq!(all(&[b"data: a\n\ndata: [DONE]"]), ["a", "[DONE]"]);
        assert_eq!(all(&[b"data: a\n\ndata: [DONE]\n"]), ["a", "[DONE]"]);
    }

    #[test]
    fn other_fields_and_comments_are_ignored() {
        assert_eq!(
            all(&[b"event: x\nid: 7\nretry: 5\n: note\ndata: a\n\n"]),
            ["a"]
        );
    }

    #[test]
    fn invalid_utf8_is_a_protocol_error() {
        let mut parser = SseParser::default();
        assert!(matches!(
            parser.push(b"data: \xff\xfe\n\n"),
            Err(ModelError::Protocol(_))
        ));
    }

    #[test]
    fn an_endless_line_is_cut_off() {
        let mut parser = SseParser::default();
        let chunk = vec![b'a'; 1024 * 1024];
        let mut result = Ok(Vec::new());
        for _ in 0..6 {
            result = parser.push(&chunk);
            if result.is_err() {
                break;
            }
        }
        assert!(matches!(result, Err(ModelError::Protocol(_))));
    }
}
