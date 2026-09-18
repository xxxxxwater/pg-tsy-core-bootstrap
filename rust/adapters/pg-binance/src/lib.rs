//! Binance adapter boundary.
//!
//! Venue-specific request/response types must stay inside this crate. The
//! production Portfolio Margin adapter is not implemented or wired up yet.
//! These pure helpers neither submit orders nor enable live trading.

pub mod market_protocol;
pub mod order_protocol;

use pg_types::OrderIntent;

pub struct BinancePmAdapter;

/// Binance PM UM `newClientOrderId` allows at most 32 characters. The shared
/// client identity (`pg` + 32 UUID hex digits) is 34 characters and cannot be
/// sent unchanged. Encode all 128 intent UUID bits as unpadded RFC 4648 Base32
/// with a `pg` prefix, yielding 28 characters without changing other venues.
pub fn binance_client_order_id(intent: &OrderIntent) -> String {
    encode_intent_bytes(intent.intent_id.as_bytes())
}

/// Deterministic, full-UUID encoding with no additional crate dependencies.
pub fn encode_intent_bytes(bytes: &[u8; 16]) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(28);
    out.push_str("pg");

    let mut buffer = 0_u32;
    let mut bits = 0_u8;
    for &byte in bytes {
        buffer = (buffer << 8) | u32::from(byte);
        bits += 8;
        while bits >= 5 {
            bits -= 5;
            out.push(ALPHABET[((buffer >> bits) & 31) as usize] as char);
        }
        buffer &= (1_u32 << bits) - 1;
    }
    if bits != 0 {
        out.push(ALPHABET[((buffer << (5 - bits)) & 31) as usize] as char);
    }

    debug_assert_eq!(out.len(), 28);
    out
}

/// Recover all 128 intent UUID bits; reject malformed or noncanonical IDs.
/// The caller must also verify the durable intent and venue order ownership.
pub fn decode_intent_bytes(client_order_id: &str) -> Option<[u8; 16]> {
    let encoded = client_order_id.strip_prefix("pg")?;
    if encoded.len() != 26 {
        return None;
    }

    let mut bytes = [0_u8; 16];
    let mut written = 0_usize;
    let mut buffer = 0_u32;
    let mut bits = 0_u8;

    for symbol in encoded.bytes() {
        let digit = match symbol {
            b'A'..=b'Z' => u32::from(symbol - b'A'),
            b'2'..=b'7' => u32::from(symbol - b'2' + 26),
            _ => return None,
        };
        buffer = (buffer << 5) | digit;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            if written == bytes.len() {
                return None;
            }
            bytes[written] = (buffer >> bits) as u8;
            written += 1;
        }
        buffer &= (1_u32 << bits) - 1;
    }

    if written != bytes.len() || buffer != 0 {
        return None;
    }
    Some(bytes)
}

#[cfg(test)]
mod tests {
    use super::{decode_intent_bytes, encode_intent_bytes};

    #[test]
    fn venue_id_is_stable_short_and_reversible() {
        let bytes = [0x42_u8; 16];
        let venue_id = encode_intent_bytes(&bytes);
        assert_eq!(venue_id.len(), 28);
        assert!(venue_id.starts_with("pg"));
        assert!(venue_id.bytes().all(|ch| ch.is_ascii_alphanumeric()));
        assert_eq!(venue_id, encode_intent_bytes(&bytes));
        assert_eq!(decode_intent_bytes(&venue_id), Some(bytes));
    }

    #[test]
    fn full_uuid_bits_are_preserved() {
        let zero = [0_u8; 16];
        let mut last_bit = zero;
        last_bit[15] = 1;
        assert_eq!(encode_intent_bytes(&zero), format!("pg{}", "A".repeat(26)));
        assert_ne!(encode_intent_bytes(&zero), encode_intent_bytes(&last_bit));
        assert_eq!(
            decode_intent_bytes(&encode_intent_bytes(&last_bit)),
            Some(last_bit)
        );
    }

    #[test]
    fn invalid_and_noncanonical_ids_fail_closed() {
        assert_eq!(decode_intent_bytes("pg"), None);
        assert_eq!(decode_intent_bytes(&format!("xx{}", "A".repeat(26))), None);
        assert_eq!(decode_intent_bytes(&format!("pg{}!", "A".repeat(25))), None);
        assert_eq!(decode_intent_bytes(&format!("pg{}", "a".repeat(26))), None);
        // Twenty-six Base32 symbols encode 130 bits: the final two must be zero.
        assert_eq!(decode_intent_bytes(&format!("pg{}B", "A".repeat(25))), None);
    }
}
