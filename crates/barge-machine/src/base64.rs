//! Minimal RFC 4648 standard base64 encoding and decoding for binary payloads
//! carried inside JSON protocol documents (git bundles, patches, and logs).

const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode arbitrary bytes as standard RFC 4648 base64 with `=` padding.
pub fn encode(input: &[u8]) -> String {
    if input.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    let (chunks, remainder) = input.as_chunks::<3>();
    for chunk in chunks {
        let n = (u32::from(chunk[0]) << 16) | (u32::from(chunk[1]) << 8) | u32::from(chunk[2]);
        out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
        out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
        out.push(TABLE[(n & 0x3f) as usize] as char);
    }
    match remainder {
        [a] => {
            let n = u32::from(*a) << 16;
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push('=');
            out.push('=');
        }
        [a, b] => {
            let n = (u32::from(*a) << 16) | (u32::from(*b) << 8);
            out.push(TABLE[((n >> 18) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 12) & 0x3f) as usize] as char);
            out.push(TABLE[((n >> 6) & 0x3f) as usize] as char);
            out.push('=');
        }
        _ => {}
    }
    out
}

/// Decode standard RFC 4648 base64. Rejects invalid characters, non-canonical
/// padding and malformed lengths.
pub fn decode(input: &str) -> Option<Vec<u8>> {
    let bytes = input.as_bytes();
    if bytes.is_empty() {
        return Some(Vec::new());
    }
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity((bytes.len() / 4) * 3);
    let (chunks, _) = bytes.as_chunks::<4>();
    let total_chunks = chunks.len();
    for (idx, chunk) in chunks.iter().enumerate() {
        let is_last = idx + 1 == total_chunks;
        let a = decode_char(chunk[0])?;
        let b = decode_char(chunk[1])?;
        match (chunk[2], chunk[3]) {
            (b'=', b'=') if is_last => {
                if (b & 0x0f) != 0 {
                    return None;
                }
                out.push((a << 2) | (b >> 4));
            }
            (c_byte, b'=') if is_last => {
                let c = decode_char(c_byte)?;
                if (c & 0x03) != 0 {
                    return None;
                }
                out.push((a << 2) | (b >> 4));
                out.push((b << 4) | (c >> 2));
            }
            (c_byte, d_byte) => {
                let c = decode_char(c_byte)?;
                let d = decode_char(d_byte)?;
                out.push((a << 2) | (b >> 4));
                out.push((b << 4) | (c >> 2));
                out.push((c << 6) | d);
            }
        }
    }
    Some(out)
}

fn decode_char(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rfc4648_vectors_and_binary_round_trip() {
        for (plain, encoded) in [
            (&b""[..], ""),
            (b"f", "Zg=="),
            (b"fo", "Zm8="),
            (b"foo", "Zm9v"),
            (b"foob", "Zm9vYg=="),
            (b"fooba", "Zm9vYmE="),
            (b"foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain), encoded);
            assert_eq!(decode(encoded).as_deref(), Some(plain));
        }
        let all_bytes: Vec<u8> = (0..=255).collect();
        let encoded = encode(&all_bytes);
        assert_eq!(decode(&encoded).as_deref(), Some(all_bytes.as_slice()));
    }

    #[test]
    fn malformed_base64_is_rejected() {
        for bad in ["a", "ab", "abc", "Zm=v", "Zg==Zg==", "Zh==", "Zm9=", "****"] {
            assert_eq!(decode(bad), None, "{bad}");
        }
    }
}
