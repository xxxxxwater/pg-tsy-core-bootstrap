//! Binance adapter boundary.
//!
//! Venue-specific request/response types must remain inside this crate. The
//! production Binance Portfolio Margin implementation is not wired up yet.
//! This module only provides a deterministic venue-safe client order identity;
//! it does not submit orders or enable live trading.

use pg_types::OrderIntent;
use uuid::Uuid;

pub struct BinancePmAdapter;

/// Binance PM UM `newClientOrderId` accepts at most 32 characters. The shared
/// core identity (`pg` followed by 32 UUID hex digits) is 34 characters, so it
/// must NOT be submitted unchanged to this venue.
///
/// Prefix the full 128-bit intent UUID with `pg` and encode it using RFC 4648
/// Base32 without padding: 2 + ceil(128 / 5) = 28 ASCII characters. Retaining
/// all 128 bits allows exact mapping back to the persisted intent on recovery.
/// This mapping does not change the core or other venues' client identities.
pub fn binance_client_order_id(intent: &OrderIntent) -> String {
    encode_intent_uuid(intent.intent_id)
}

/// Generate the stable Binance identifier from the persisted intent UUID.
pub fn encode_intent_uuid(intent_id: Uuid) -> String {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = String::with_capacity(28);
    out.push_str("pg");

    let mut buffer = 0_u32;
    let mut bits = 0_u8;
    for &byte in intent_id.as_bytes() {
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

/// Recover the *exact* intent UUID from an ID emitted by `encode_intent_uuid`.
/// Reject invalid lengths, alphabet and nonzero trailing padding bits instead
/// of attempting to guess an order's ownership from its symbol or side.
pub fn decode_intent_uuid(client_order_id: &str) -> Option<Uuid> {
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
    Some(Uuid::from_bytes(bytes))
}

#[cfg(test)]
mod tests {
    use super::{decode_intent_uuid, encode_intent_uuid};
    use uuid::Uuid;

    #[test]
    fn venue_id_is_stable_short_and_reversible() {
        let id = Uuid::parse_str("018f7f2e-6f5c-7cc4-98e8-2cd9b5c67d0f").unwrap();
        let venue_id = encode_intent_uuid(id);
        assert_eq!(venue_id.len(), 28);
        assert!(venue_id.starts_with("pg"));
        assert!(venue_id.bytes().all(|ch| ch.is_ascii_alphanumeric()));
        assert_eq!(venue_id, encode_intent_uuid(id));
        assert_eq!(decode_intent_uuid(&venue_id), Some(id));
    }

    #[test]
    fn full_uuid_bits_are_preserved() {
        let zero = Uuid::from_bytes([0_u8; 16]);
        let last_bit = Uuid::from_bytes({
            let mut bytes = [0_u8; 16];
            bytes[15] = 1;
            bytes
        });
        assert_eq!(encode_intent_uuid(zero), format!("pg{}", "A".repeat(26)));
        assert_ne!(encode_intent_uuid(zero), encode_intent_uuid(last_bit));
        assert_eq!(decode_intent_uuid(&encode_intent_uuid(last_bit)), Some(last_bit));
    }

    #[test]
    fn invalid_and_noncanonical_ids_fail_closed() {
        assert_eq!(decode_intent_uuid("pg"), None);
        assert_eq!(decode_intent_uuid(&format!("xx{}", "A".repeat(26))), None);
        assert_eq!(decode_intent_uuid(&format!("pg{}!", "A".repeat(25))), None);
        assert_eq!(decode_intent_uuid(&format!("pg{}", "a".repeat(26))), None);
        // The UUID contains 128 bits, while 26 Base32 symbols carry 130 bits;
        // the two trailing padding bits must be zero to be canonical.
        assert_eq!(decode_intent_uuid(&format!("pg{}B", "A".repeat(25))), None);
    }
}
