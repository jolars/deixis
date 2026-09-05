use std::{error::Error, fmt, str::FromStr};

use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize,
)]
pub struct Position {
    pub line: u32,
    pub character: u32,
}

impl Position {
    pub const fn new(line: u32, character: u32) -> Self {
        Self { line, character }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Range {
    pub start: Position,
    pub end: Position,
}

impl Range {
    pub const fn new(start: Position, end: Position) -> Self {
        Self { start, end }
    }
}

#[derive(
    Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize,
)]
pub enum PositionEncoding {
    #[serde(rename = "utf-8")]
    Utf8,
    #[default]
    #[serde(rename = "utf-16")]
    Utf16,
    #[serde(rename = "utf-32")]
    Utf32,
}

impl PositionEncoding {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Utf8 => "utf-8",
            Self::Utf16 => "utf-16",
            Self::Utf32 => "utf-32",
        }
    }

    fn code_units(self, character: char) -> usize {
        match self {
            Self::Utf8 => character.len_utf8(),
            Self::Utf16 => character.len_utf16(),
            Self::Utf32 => 1,
        }
    }
}

impl fmt::Display for PositionEncoding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for PositionEncoding {
    type Err = UnsupportedPositionEncoding;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "utf-8" => Ok(Self::Utf8),
            "utf-16" => Ok(Self::Utf16),
            "utf-32" => Ok(Self::Utf32),
            _ => Err(UnsupportedPositionEncoding {
                value: value.to_owned(),
            }),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UnsupportedPositionEncoding {
    value: String,
}

impl UnsupportedPositionEncoding {
    pub fn value(&self) -> &str {
        &self.value
    }
}

impl fmt::Display for UnsupportedPositionEncoding {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "unsupported position encoding `{}`", self.value)
    }
}

impl Error for UnsupportedPositionEncoding {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PositionError {
    LineOutOfBounds {
        line: u32,
        line_count: usize,
    },
    CharacterOutOfBounds {
        position: Position,
        encoding: PositionEncoding,
        line_length: usize,
    },
    InvalidCharacterBoundary {
        position: Position,
        encoding: PositionEncoding,
    },
    ReversedRange {
        range: Range,
    },
    CoordinateOverflow {
        coordinate: &'static str,
        value: usize,
    },
}

impl fmt::Display for PositionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LineOutOfBounds { line, line_count } => write!(
                formatter,
                "line {line} is outside a document with {line_count} lines"
            ),
            Self::CharacterOutOfBounds {
                position,
                encoding,
                line_length,
            } => write!(
                formatter,
                "character {} on line {} exceeds the line length of {line_length} {encoding} code units",
                position.character, position.line
            ),
            Self::InvalidCharacterBoundary { position, encoding } => write!(
                formatter,
                "character {} on line {} splits a character in {encoding}",
                position.character, position.line
            ),
            Self::ReversedRange { range } => write!(
                formatter,
                "range start {}:{} follows range end {}:{}",
                range.start.line,
                range.start.character,
                range.end.line,
                range.end.character
            ),
            Self::CoordinateOverflow { coordinate, value } => write!(
                formatter,
                "document {coordinate} value {value} exceeds the supported range"
            ),
        }
    }
}

impl Error for PositionError {}

#[derive(Debug, Clone, Copy)]
pub struct PositionConverter<'a> {
    text: &'a str,
}

impl<'a> PositionConverter<'a> {
    pub const fn new(text: &'a str) -> Self {
        Self { text }
    }

    pub fn to_lsp_position(
        self,
        position: Position,
        encoding: PositionEncoding,
    ) -> Result<Position, PositionError> {
        self.convert_position(position, PositionEncoding::Utf8, encoding)
    }

    pub fn from_lsp_position(
        self,
        position: Position,
        encoding: PositionEncoding,
    ) -> Result<Position, PositionError> {
        self.convert_position(position, encoding, PositionEncoding::Utf8)
    }

    pub fn to_lsp_range(
        self,
        range: Range,
        encoding: PositionEncoding,
    ) -> Result<Range, PositionError> {
        self.convert_range(range, PositionEncoding::Utf8, encoding)
    }

    pub fn from_lsp_range(
        self,
        range: Range,
        encoding: PositionEncoding,
    ) -> Result<Range, PositionError> {
        self.convert_range(range, encoding, PositionEncoding::Utf8)
    }

    pub fn end_position(
        self,
        encoding: PositionEncoding,
    ) -> Result<Position, PositionError> {
        let (line, final_line) = final_line(self.text);
        Ok(Position::new(
            to_u32("line", line)?,
            to_u32("character", code_units(final_line, encoding))?,
        ))
    }

    fn convert_range(
        self,
        range: Range,
        source: PositionEncoding,
        target: PositionEncoding,
    ) -> Result<Range, PositionError> {
        if range.start > range.end {
            return Err(PositionError::ReversedRange { range });
        }

        Ok(Range::new(
            self.convert_position(range.start, source, target)?,
            self.convert_position(range.end, source, target)?,
        ))
    }

    fn convert_position(
        self,
        position: Position,
        source: PositionEncoding,
        target: PositionEncoding,
    ) -> Result<Position, PositionError> {
        let line = logical_line(self.text, position.line)?;
        let byte_offset = byte_offset(line, position, source)?;
        let character = code_units(&line[..byte_offset], target);

        Ok(Position::new(
            position.line,
            to_u32("character", character)?,
        ))
    }
}

fn logical_line(text: &str, wanted: u32) -> Result<&str, PositionError> {
    let wanted = usize::try_from(wanted).map_err(|_| {
        PositionError::CoordinateOverflow {
            coordinate: "line",
            value: usize::MAX,
        }
    })?;
    let bytes = text.as_bytes();
    let mut current = 0;
    let mut start = 0;
    let mut cursor = 0;

    while cursor < bytes.len() {
        let newline_length = match bytes[cursor] {
            b'\r' if bytes.get(cursor + 1) == Some(&b'\n') => 2,
            b'\r' | b'\n' => 1,
            _ => {
                cursor += 1;
                continue;
            }
        };

        if current == wanted {
            return Ok(&text[start..cursor]);
        }
        current += 1;
        cursor += newline_length;
        start = cursor;
    }

    if current == wanted {
        Ok(&text[start..])
    } else {
        Err(PositionError::LineOutOfBounds {
            line: u32::try_from(wanted).unwrap_or(u32::MAX),
            line_count: current + 1,
        })
    }
}

fn final_line(text: &str) -> (usize, &str) {
    let bytes = text.as_bytes();
    let mut line = 0;
    let mut start = 0;
    let mut cursor = 0;

    while cursor < bytes.len() {
        match bytes[cursor] {
            b'\r' => {
                cursor += usize::from(bytes.get(cursor + 1) == Some(&b'\n'));
                line += 1;
                start = cursor + 1;
            }
            b'\n' => {
                line += 1;
                start = cursor + 1;
            }
            _ => {}
        }
        cursor += 1;
    }

    (line, &text[start..])
}

fn byte_offset(
    line: &str,
    position: Position,
    encoding: PositionEncoding,
) -> Result<usize, PositionError> {
    let wanted = usize::try_from(position.character).map_err(|_| {
        PositionError::CoordinateOverflow {
            coordinate: "character",
            value: usize::MAX,
        }
    })?;
    let mut units = 0;

    for (byte_offset, character) in line.char_indices() {
        if units == wanted {
            return Ok(byte_offset);
        }
        units += encoding.code_units(character);
        if wanted < units {
            return Err(PositionError::InvalidCharacterBoundary {
                position,
                encoding,
            });
        }
    }

    if units == wanted {
        Ok(line.len())
    } else {
        Err(PositionError::CharacterOutOfBounds {
            position,
            encoding,
            line_length: units,
        })
    }
}

fn code_units(text: &str, encoding: PositionEncoding) -> usize {
    text.chars()
        .map(|character| encoding.code_units(character))
        .sum()
}

fn to_u32(
    coordinate: &'static str,
    value: usize,
) -> Result<u32, PositionError> {
    u32::try_from(value)
        .map_err(|_| PositionError::CoordinateOverflow { coordinate, value })
}

#[cfg(test)]
mod tests {
    use super::{
        Position, PositionConverter, PositionEncoding, PositionError, Range,
    };

    const UNICODE: &str = "plain\ne\u{301}\u{1f980}z";

    #[test]
    fn converts_ascii_positions_and_ranges_without_changing_them() {
        let converter = PositionConverter::new("abc\ndef");
        let range = Range::new(Position::new(0, 1), Position::new(1, 3));

        for encoding in [
            PositionEncoding::Utf8,
            PositionEncoding::Utf16,
            PositionEncoding::Utf32,
        ] {
            assert_eq!(converter.to_lsp_range(range, encoding).unwrap(), range);
            assert_eq!(
                converter.from_lsp_range(range, encoding).unwrap(),
                range
            );
        }
    }

    #[test]
    fn converts_combining_marks_and_non_bmp_characters_by_code_unit() {
        let converter = PositionConverter::new(UNICODE);
        let utf8_boundaries = [0, 1, 3, 7, 8];
        let utf16_boundaries = [0, 1, 2, 4, 5];
        let utf32_boundaries = [0, 1, 2, 3, 4];

        for (encoding, expected) in [
            (PositionEncoding::Utf8, utf8_boundaries),
            (PositionEncoding::Utf16, utf16_boundaries),
            (PositionEncoding::Utf32, utf32_boundaries),
        ] {
            for (utf8, encoded) in utf8_boundaries.into_iter().zip(expected) {
                let mcp = Position::new(1, utf8);
                let lsp = Position::new(1, encoded);
                assert_eq!(
                    converter.to_lsp_position(mcp, encoding).unwrap(),
                    lsp
                );
                assert_eq!(
                    converter.from_lsp_position(lsp, encoding).unwrap(),
                    mcp
                );
            }
        }
    }

    #[test]
    fn recognizes_lf_crlf_and_cr_as_logical_line_endings() {
        let converter = PositionConverter::new("a\r\n\rbee\n");

        for position in [
            Position::new(0, 1),
            Position::new(1, 0),
            Position::new(2, 3),
            Position::new(3, 0),
        ] {
            assert_eq!(
                converter
                    .to_lsp_position(position, PositionEncoding::Utf16)
                    .unwrap(),
                position
            );
        }
        assert_eq!(
            converter.end_position(PositionEncoding::Utf16).unwrap(),
            Position::new(3, 0)
        );
    }

    #[test]
    fn rejects_offsets_that_split_encoded_characters() {
        let converter = PositionConverter::new(UNICODE);

        for character in [2, 4, 5, 6] {
            assert!(matches!(
                converter.to_lsp_position(
                    Position::new(1, character),
                    PositionEncoding::Utf16,
                ),
                Err(PositionError::InvalidCharacterBoundary { .. })
            ));
        }
        assert!(matches!(
            converter.from_lsp_position(
                Position::new(1, 3),
                PositionEncoding::Utf16,
            ),
            Err(PositionError::InvalidCharacterBoundary { .. })
        ));
        assert!(matches!(
            converter.from_lsp_position(
                Position::new(1, 2),
                PositionEncoding::Utf8,
            ),
            Err(PositionError::InvalidCharacterBoundary { .. })
        ));
    }

    #[test]
    fn rejects_positions_outside_lines_and_inside_line_endings() {
        let converter = PositionConverter::new("a\r\n\rbee\n");

        assert!(matches!(
            converter
                .to_lsp_position(Position::new(4, 0), PositionEncoding::Utf8,),
            Err(PositionError::LineOutOfBounds { .. })
        ));
        for position in [Position::new(0, 2), Position::new(1, 1)] {
            assert!(matches!(
                converter.to_lsp_position(position, PositionEncoding::Utf8),
                Err(PositionError::CharacterOutOfBounds { .. })
            ));
        }
    }

    #[test]
    fn rejects_reversed_ranges() {
        let converter = PositionConverter::new("first\nsecond");
        let range = Range::new(Position::new(1, 0), Position::new(0, 5));

        assert!(matches!(
            converter.to_lsp_range(range, PositionEncoding::Utf16),
            Err(PositionError::ReversedRange { .. })
        ));
        assert!(matches!(
            converter.from_lsp_range(range, PositionEncoding::Utf32),
            Err(PositionError::ReversedRange { .. })
        ));
    }

    #[test]
    fn serializes_lsp_compatible_values() {
        assert_eq!(
            serde_json::to_value(PositionEncoding::Utf32).unwrap(),
            serde_json::json!("utf-32")
        );
        assert_eq!(
            serde_json::to_value(Range::new(
                Position::new(2, 3),
                Position::new(4, 5),
            ))
            .unwrap(),
            serde_json::json!({
                "start": { "line": 2, "character": 3 },
                "end": { "line": 4, "character": 5 },
            })
        );
    }
}
