//! Reading text a user wrote or redirected: Windows PowerShell 5.1's `>`
//! writes UTF-16 with a BOM, and some editors start UTF-8 with one.

use std::path::Path;

/// `path`'s text: a UTF-8 BOM dropped, UTF-16 LE / BE decoded by its BOM,
/// anything else read as UTF-8.
///
/// # Errors
/// The read's I/O error, or [`std::io::ErrorKind::InvalidData`] for text
/// that is not valid in its encoding.
pub(crate) fn read(path: &Path) -> std::io::Result<String> {
    decode(std::fs::read(path)?)
}

fn decode(bytes: Vec<u8>) -> std::io::Result<String> {
    let invalid =
        |what: &str| std::io::Error::new(std::io::ErrorKind::InvalidData, what.to_owned());
    match bytes.as_slice() {
        [0xEF, 0xBB, 0xBF, rest @ ..] => {
            String::from_utf8(rest.to_vec()).map_err(|_| invalid("not valid UTF-8"))
        }
        [0xFF, 0xFE, rest @ ..] => {
            utf16(rest, u16::from_le_bytes).ok_or_else(|| invalid("not valid UTF-16 LE"))
        }
        [0xFE, 0xFF, rest @ ..] => {
            utf16(rest, u16::from_be_bytes).ok_or_else(|| invalid("not valid UTF-16 BE"))
        }
        _ => String::from_utf8(bytes).map_err(|_| invalid("not valid UTF-8")),
    }
}

fn utf16(bytes: &[u8], unit: fn([u8; 2]) -> u16) -> Option<String> {
    if bytes.len() % 2 != 0 {
        return None;
    }
    let units = bytes.chunks_exact(2).map(|c| unit([c[0], c[1]]));
    char::decode_utf16(units)
        .collect::<Result<String, _>>()
        .ok()
}

#[cfg(test)]
mod tests {
    use super::decode;

    fn utf16(text: &str, bom: [u8; 2], unit: fn(u16) -> [u8; 2]) -> Vec<u8> {
        let mut out = bom.to_vec();
        out.extend(text.encode_utf16().flat_map(unit));
        out
    }

    #[test]
    fn every_bom_reads_as_the_same_text() {
        let text = "[{\"model\": \"é\"}]";
        assert_eq!(decode(text.into()).unwrap(), text);
        let mut bom = vec![0xEF, 0xBB, 0xBF];
        bom.extend(text.bytes());
        assert_eq!(decode(bom).unwrap(), text);
        assert_eq!(
            decode(utf16(text, [0xFF, 0xFE], u16::to_le_bytes)).unwrap(),
            text
        );
        assert_eq!(
            decode(utf16(text, [0xFE, 0xFF], u16::to_be_bytes)).unwrap(),
            text
        );
    }

    #[test]
    fn broken_text_is_invalid_data() {
        let odd = vec![0xFF, 0xFE, 0x41];
        assert_eq!(
            decode(odd).unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
        assert!(decode(vec![0xC3]).is_err());
    }
}
