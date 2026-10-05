//! Shared UTF-8 decoding for capped file reads.

/// Decodes `bytes` as UTF-8 after enforcing the `cap`.
///
/// Callers that need to tell "exactly at the cap" apart from "past
/// the cap" read `cap + 1` bytes; when the buffer overran the cap it
/// is cut back to the cap **on a UTF-8 char boundary** (walking back
/// off continuation bytes), so a multi-byte character straddling the
/// cap truncates instead of failing the whole decode.
///
/// Returns `None` when the (possibly cut) bytes are not valid UTF-8;
/// callers decide whether that is an error (`read_file`) or a skip
/// (`search`).
pub(crate) fn decode_capped(mut bytes: Vec<u8>, cap: usize) -> Option<String> {
    if bytes.len() > cap {
        let mut cut = cap;
        // Walk back off UTF-8 continuation bytes (0b10xxxxxx) so the
        // cut lands on a char boundary.
        while cut > 0 && (bytes[cut] & 0xC0) == 0x80 {
            cut -= 1;
        }
        bytes.truncate(cut);
    }
    String::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cuts_on_char_boundary() {
        // 3-byte characters: byte 7 is mid-character.
        let mut bytes = "✀".repeat(4).into_bytes();
        bytes.push(0xff); // past the cap, also invalid on its own
        let text = decode_capped(bytes, 7).expect("decode");
        assert_eq!(text.len(), 6);
    }

    #[test]
    fn under_cap_decodes_whole() {
        let text = decode_capped("héllo".as_bytes().to_vec(), 1024).expect("decode");
        assert_eq!(text, "héllo");
    }

    #[test]
    fn invalid_utf8_is_none() {
        assert!(decode_capped(vec![0xff, 0xfe], 1024).is_none());
    }
}
