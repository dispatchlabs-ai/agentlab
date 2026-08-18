use std::io::{self, Write};

/// Escape untrusted single-line values such as filesystem paths before they
/// reach a terminal. The stored JSON keeps the original UTF-8 value.
pub fn escape(value: &str) -> String {
    value.chars().flat_map(escape_character).collect()
}

/// Preserve useful line structure in external presenter/reviewer output while
/// neutralizing terminal controls, carriage-return rewriting, and bidirectional
/// display overrides. Receipts retain the original bytes.
pub fn sanitize_external(value: &str) -> String {
    value
        .chars()
        .flat_map(|character| {
            if matches!(character, '\n' | '\t') {
                character.to_string().chars().collect::<Vec<_>>()
            } else {
                escape_character(character)
            }
        })
        .collect()
}

/// Incrementally sanitize an arbitrary byte stream without corrupting UTF-8
/// sequences that happen to span read boundaries. Invalid bytes are rendered
/// visibly instead of being sent to the terminal.
#[derive(Debug, Default)]
pub struct StreamSanitizer {
    pending: Vec<u8>,
}

impl StreamSanitizer {
    pub fn write(&mut self, destination: &mut dyn Write, bytes: &[u8]) -> io::Result<()> {
        self.pending.extend_from_slice(bytes);
        loop {
            match std::str::from_utf8(&self.pending) {
                Ok(text) => {
                    destination.write_all(sanitize_external(text).as_bytes())?;
                    self.pending.clear();
                    return Ok(());
                }
                Err(error) => {
                    let valid_up_to = error.valid_up_to();
                    if valid_up_to > 0 {
                        let text = std::str::from_utf8(&self.pending[..valid_up_to])
                            .expect("UTF-8 validator reported a valid prefix");
                        destination.write_all(sanitize_external(text).as_bytes())?;
                        self.pending.drain(..valid_up_to);
                    }
                    let Some(error_length) = error.error_len() else {
                        return Ok(());
                    };
                    for byte in self.pending.drain(..error_length) {
                        write!(destination, "\\x{byte:02x}")?;
                    }
                }
            }
        }
    }

    pub fn finish(&mut self, destination: &mut dyn Write) -> io::Result<()> {
        for byte in self.pending.drain(..) {
            write!(destination, "\\x{byte:02x}")?;
        }
        destination.flush()
    }
}

fn escape_character(character: char) -> Vec<char> {
    if character.is_control() || is_bidirectional_override(character) {
        character.escape_default().collect()
    } else {
        vec![character]
    }
}

fn is_bidirectional_override(character: char) -> bool {
    matches!(
        character,
        '\u{202a}'
            | '\u{202b}'
            | '\u{202c}'
            | '\u{202d}'
            | '\u{202e}'
            | '\u{2066}'
            | '\u{2067}'
            | '\u{2068}'
            | '\u{2069}'
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn paths_cannot_create_terminal_lines_or_control_sequences() {
        let rendered = escape("safe\n\u{1b}]52;clipboard\u{7}\u{202e}txt");
        assert_eq!(rendered, "safe\\n\\u{1b}]52;clipboard\\u{7}\\u{202e}txt");
    }

    #[test]
    fn external_output_keeps_markdown_lines_but_neutralizes_rewrites() {
        let rendered = sanitize_external("# Result\nvalue\rforged\u{1b}[2J\n");
        assert_eq!(rendered, "# Result\nvalue\\rforged\\u{1b}[2J\n");
    }

    #[test]
    fn stream_sanitizer_handles_split_unicode_and_invalid_bytes() {
        let mut output = Vec::new();
        let mut sanitizer = StreamSanitizer::default();
        let bytes = "okay\u{202e}done\n".as_bytes();
        sanitizer.write(&mut output, &bytes[..6]).unwrap();
        sanitizer.write(&mut output, &bytes[6..]).unwrap();
        sanitizer.write(&mut output, &[0xff]).unwrap();
        sanitizer.finish(&mut output).unwrap();
        assert_eq!(
            String::from_utf8(output).unwrap(),
            "okay\\u{202e}done\n\\xff"
        );
    }
}
