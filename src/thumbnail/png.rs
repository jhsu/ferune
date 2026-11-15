//! Just enough PNG to read and write the thumbnail specification's metadata: text chunks
//! (`Thumb::URI`, `Thumb::MTime`, …) are read and inserted at the chunk level, so a
//! thumbnailer's image data is never decoded or re-encoded here.

const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// The chunks of a PNG: type and data. `None` when the bytes aren't a well-formed PNG.
fn chunks(bytes: &[u8]) -> Option<Vec<([u8; 4], &[u8])>> {
    let mut rest = bytes.strip_prefix(SIGNATURE)?;
    let mut chunks = Vec::new();
    while !rest.is_empty() {
        let length = u32::from_be_bytes(rest.get(..4)?.try_into().ok()?) as usize;
        let kind: [u8; 4] = rest.get(4..8)?.try_into().ok()?;
        let data = rest.get(8..8 + length)?;
        rest = rest.get(12 + length..)?;
        chunks.push((kind, data));
        if &kind == b"IEND" {
            break;
        }
    }
    (chunks.first()?.0 == *b"IHDR").then_some(chunks)
}

/// Width and height from the header.
pub fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let chunks = chunks(bytes)?;
    let header = chunks[0].1;
    Some((
        u32::from_be_bytes(header.get(0..4)?.try_into().ok()?),
        u32::from_be_bytes(header.get(4..8)?.try_into().ok()?),
    ))
}

/// Key/value pairs from `tEXt` (Latin-1) and uncompressed `iTXt` (UTF-8) chunks. `None` when
/// the bytes aren't a PNG.
pub fn text(bytes: &[u8]) -> Option<Vec<(String, String)>> {
    let pairs = chunks(bytes)?
        .into_iter()
        .filter_map(|(kind, data)| match &kind {
            b"tEXt" => {
                let (key, value) = split_nul(data)?;
                Some((latin1(key), latin1(value)))
            }
            b"iTXt" => {
                let (key, rest) = split_nul(data)?;
                // Compression flag and method, then language and translated keyword.
                let (&[0, _], rest) = rest.split_at_checked(2)? else {
                    return None;
                };
                let (_, rest) = split_nul(rest)?;
                let (_, value) = split_nul(rest)?;
                Some((latin1(key), String::from_utf8(value.to_vec()).ok()?))
            }
            _ => None,
        })
        .collect();
    Some(pairs)
}

fn split_nul(data: &[u8]) -> Option<(&[u8], &[u8])> {
    let nul = data.iter().position(|&byte| byte == 0)?;
    Some((&data[..nul], &data[nul + 1..]))
}

fn latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| byte as char).collect()
}

/// The PNG with `pairs` as `tEXt` chunks right after the header, replacing any text chunks
/// with the same keys. Values must be Latin-1 (URIs and numbers are ASCII).
pub fn with_text(bytes: &[u8], pairs: &[(&str, &str)]) -> Option<Vec<u8>> {
    let chunks = chunks(bytes)?;
    let mut out = SIGNATURE.to_vec();
    for (index, (kind, data)) in chunks.iter().enumerate() {
        let replaced = matches!(kind, b"tEXt" | b"iTXt" | b"zTXt")
            && split_nul(data)
                .is_some_and(|(key, _)| pairs.iter().any(|(new_key, _)| new_key.as_bytes() == key));
        if !replaced {
            push_chunk(&mut out, kind, data);
        }
        if index == 0 {
            for (key, value) in pairs {
                push_chunk(&mut out, b"tEXt", &text_data(key, value));
            }
        }
    }
    Some(out)
}

fn text_data(key: &str, value: &str) -> Vec<u8> {
    let latin1 = |text: &str| -> Vec<u8> {
        text.chars()
            .map(|c| u8::try_from(u32::from(c)).unwrap_or(b'?'))
            .collect()
    };
    let mut data = latin1(key);
    data.push(0);
    data.extend(latin1(value));
    data
}

/// A 1×1 transparent PNG carrying `pairs`: what the specification's `fail/` folder holds to
/// record that a file couldn't be thumbnailed.
pub fn marker(pairs: &[(&str, &str)]) -> Vec<u8> {
    let mut out = SIGNATURE.to_vec();
    // 1×1, 8-bit RGBA, no interlacing.
    push_chunk(&mut out, b"IHDR", &[0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0, 0, 0]);
    for (key, value) in pairs {
        push_chunk(&mut out, b"tEXt", &text_data(key, value));
    }
    // One scanline (filter byte + one pixel) in a stored, uncompressed deflate block.
    let raw = [0u8; 5];
    let mut zlib = vec![0x78, 0x01, 0x01];
    zlib.extend((raw.len() as u16).to_le_bytes());
    zlib.extend((!(raw.len() as u16)).to_le_bytes());
    zlib.extend(raw);
    zlib.extend(adler32(&raw).to_be_bytes());
    push_chunk(&mut out, b"IDAT", &zlib);
    push_chunk(&mut out, b"IEND", &[]);
    out
}

fn push_chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend((data.len() as u32).to_be_bytes());
    out.extend(kind);
    out.extend(data);
    let mut crc_input = kind.to_vec();
    crc_input.extend(data);
    out.extend(crc32(&crc_input).to_be_bytes());
}

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 1 {
                (crc >> 1) ^ 0xedb8_8320
            } else {
                crc >> 1
            };
        }
    }
    !crc
}

fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for &byte in bytes {
        a = (a + u32::from(byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checksums_match_known_values() {
        assert_eq!(crc32(b"IEND"), 0xae42_6082);
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn markers_are_valid_pngs_with_their_text() {
        let png = marker(&[("Thumb::URI", "file:///a"), ("Thumb::MTime", "7")]);
        assert_eq!(dimensions(&png), Some((1, 1)));
        assert_eq!(
            text(&png).unwrap(),
            vec![
                ("Thumb::URI".into(), "file:///a".into()),
                ("Thumb::MTime".into(), "7".into())
            ]
        );
        // The last chunk is IEND with its well-known CRC.
        assert!(png.ends_with(&[0, 0, 0, 0, b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]));
    }

    #[test]
    fn replaces_existing_thumbnail_keys_and_keeps_others() {
        let original = marker(&[
            ("Thumb::MTime", "1"),
            ("Software", "elsewhere"),
            ("Thumb::URI", "file:///old"),
        ]);
        let updated = with_text(
            &original,
            &[("Thumb::URI", "file:///new"), ("Thumb::MTime", "2")],
        )
        .unwrap();
        assert_eq!(
            text(&updated).unwrap(),
            vec![
                ("Thumb::URI".into(), "file:///new".into()),
                ("Thumb::MTime".into(), "2".into()),
                ("Software".into(), "elsewhere".into()),
            ]
        );
        assert_eq!(dimensions(&updated), Some((1, 1)));
    }

    #[test]
    fn reads_uncompressed_itxt_and_rejects_non_pngs() {
        let mut png = SIGNATURE.to_vec();
        push_chunk(&mut png, b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 3, 8, 6, 0, 0, 0]);
        push_chunk(
            &mut png,
            b"iTXt",
            b"Thumb::URI\0\0\0\0\0file:///caf\xc3\xa9",
        );
        push_chunk(&mut png, b"iTXt", b"Compressed\0\x01\0\0\0xx");
        push_chunk(&mut png, b"IEND", &[]);
        assert_eq!(dimensions(&png), Some((2, 3)));
        assert_eq!(
            text(&png).unwrap(),
            vec![("Thumb::URI".into(), "file:///café".into())]
        );
        assert_eq!(text(b"GIF89a"), None);
        assert_eq!(text(&png[..png.len() - 6]), None, "truncated");
    }
}
