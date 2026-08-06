//! Hex, hand-rolled.
//!
//! Twenty lines, so that the trust root's dependency list stays at three
//! entries an auditor can read in one sitting. Not a rejection of the `hex`
//! crate — a rejection of spending a supply-chain entry on something this
//! small, in the one crate where the dependency list is part of the pitch.

const DIGITS: &[u8; 16] = b"0123456789abcdef";

pub(crate) fn encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[usize::from(b >> 4)] as char);
        s.push(DIGITS[usize::from(b & 0x0f)] as char);
    }
    s
}

pub(crate) fn decode_into(s: &str, out: &mut [u8]) -> Result<(), ()> {
    let src = s.as_bytes();
    if src.len() != out.len() * 2 {
        return Err(());
    }
    for (i, slot) in out.iter_mut().enumerate() {
        *slot = (nibble(src[i * 2])? << 4) | nibble(src[i * 2 + 1])?;
    }
    Ok(())
}

fn nibble(c: u8) -> Result<u8, ()> {
    match c {
        b'0'..=b'9' => Ok(c - b'0'),
        // Lowercase only on the way out, but accept either on the way in — an
        // auditor pasting a hash from a report should not be tripped by case.
        b'a'..=b'f' => Ok(c - b'a' + 10),
        b'A'..=b'F' => Ok(c - b'A' + 10),
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_every_byte_value() {
        let all: Vec<u8> = (0..=255).collect();
        let mut back = vec![0u8; all.len()];
        decode_into(&encode(&all), &mut back).unwrap();
        assert_eq!(all, back);
    }

    #[test]
    fn encodes_lowercase() {
        assert_eq!(encode(&[0xde, 0xad, 0xbe, 0xef]), "deadbeef");
    }

    #[test]
    fn accepts_either_case_on_input() {
        let mut a = [0u8; 4];
        let mut b = [0u8; 4];
        decode_into("DEADBEEF", &mut a).unwrap();
        decode_into("deadbeef", &mut b).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn rejects_wrong_length_and_non_hex() {
        let mut out = [0u8; 4];
        assert!(decode_into("deadbe", &mut out).is_err());
        assert!(decode_into("deadbeefaa", &mut out).is_err());
        assert!(decode_into("deadbeeg", &mut out).is_err());
    }
}
