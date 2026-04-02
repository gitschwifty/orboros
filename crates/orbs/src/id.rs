use std::collections::HashSet;
use std::fmt;

use serde::{Deserialize, Serialize};

/// Content-addressed ID for an Orb.
///
/// Generated from SHA-256 of `title|description|creator|created_at_nanos|nonce`,
/// encoded as base36, adaptive length 3-8 characters.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct OrbId(String);

impl OrbId {
    /// Creates an `OrbId` from a raw string (for deserialization or testing).
    pub fn from_raw(s: impl Into<String>) -> Self {
        Self(s.into())
    }

    /// Returns the string representation.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// Generates a content-addressed ID, checking for collisions against `existing`.
    ///
    /// The ID is derived from SHA-256 of the seed fields, encoded as base36.
    /// Starts at 3 characters. On collision: increment nonce (0-9), then extend length.
    pub fn generate(
        title: &str,
        description: &str,
        creator: &str,
        created_at_nanos: u128,
        existing: &HashSet<String>,
    ) -> Self {
        for length in 3..=8 {
            for nonce in 0..10 {
                let hash = compute_hash(title, description, creator, created_at_nanos, nonce);
                let candidate = encode_base36(&hash, length);
                let full_id = format!("orb-{candidate}");
                if !existing.contains(&full_id) {
                    return Self(full_id);
                }
            }
        }
        // Fallback: use full hash (extremely unlikely)
        let hash = compute_hash(title, description, creator, created_at_nanos, 0);
        let full = encode_base36(&hash, 16);
        Self(format!("orb-{full}"))
    }

    /// Creates a child ID from this parent ID with a sequential index.
    ///
    /// Example: `orb-abc` + index 1 → `orb-abc.1`
    #[must_use]
    pub fn child(&self, index: u32) -> Self {
        Self(format!("{}.{index}", self.0))
    }

    /// Returns true if this is a child ID (contains a `.` separator).
    pub fn is_child(&self) -> bool {
        self.0.contains('.')
    }

    /// Returns the parent portion of a child ID, or None if this is a root ID.
    pub fn parent_id(&self) -> Option<Self> {
        self.0
            .rsplit_once('.')
            .map(|(parent, _)| Self(parent.to_string()))
    }
}

impl fmt::Display for OrbId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Computes SHA-256 of the seed fields.
fn compute_hash(
    title: &str,
    description: &str,
    creator: &str,
    created_at_nanos: u128,
    nonce: u8,
) -> [u8; 32] {
    sha256_of(format!("{title}|{description}|{creator}|{created_at_nanos}|{nonce}").as_bytes())
}

/// Minimal SHA-256 implementation (no external dependency).
#[allow(clippy::too_many_lines, clippy::many_single_char_names)]
fn sha256_of(data: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a_2f98,
        0x7137_4491,
        0xb5c0_fbcf,
        0xe9b5_dba5,
        0x3956_c25b,
        0x59f1_11f1,
        0x923f_82a4,
        0xab1c_5ed5,
        0xd807_aa98,
        0x1283_5b01,
        0x2431_85be,
        0x550c_7dc3,
        0x72be_5d74,
        0x80de_b1fe,
        0x9bdc_06a7,
        0xc19b_f174,
        0xe49b_69c1,
        0xefbe_4786,
        0x0fc1_9dc6,
        0x240c_a1cc,
        0x2de9_2c6f,
        0x4a74_84aa,
        0x5cb0_a9dc,
        0x76f9_88da,
        0x983e_5152,
        0xa831_c66d,
        0xb003_27c8,
        0xbf59_7fc7,
        0xc6e0_0bf3,
        0xd5a7_9147,
        0x06ca_6351,
        0x1429_2967,
        0x27b7_0a85,
        0x2e1b_2138,
        0x4d2c_6dfc,
        0x5338_0d13,
        0x650a_7354,
        0x766a_0abb,
        0x81c2_c92e,
        0x9272_2c85,
        0xa2bf_e8a1,
        0xa81a_664b,
        0xc24b_8b70,
        0xc76c_51a3,
        0xd192_e819,
        0xd699_0624,
        0xf40e_3585,
        0x106a_a070,
        0x19a4_c116,
        0x1e37_6c08,
        0x2748_774c,
        0x34b0_bcb5,
        0x391c_0cb3,
        0x4ed8_aa4a,
        0x5b9c_ca4f,
        0x682e_6ff3,
        0x748f_82ee,
        0x78a5_636f,
        0x84c8_7814,
        0x8cc7_0208,
        0x90be_fffa,
        0xa450_6ceb,
        0xbef9_a3f7,
        0xc671_78f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];

    // Padding
    let bit_len = (data.len() as u64) * 8;
    let mut padded = data.to_vec();
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    // Process blocks
    for chunk in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for i in 0..16 {
            w[i] = u32::from_be_bytes([
                chunk[i * 4],
                chunk[i * 4 + 1],
                chunk[i * 4 + 2],
                chunk[i * 4 + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;

        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut result = [0u8; 32];
    for (i, val) in h.iter().enumerate() {
        result[i * 4..i * 4 + 4].copy_from_slice(&val.to_be_bytes());
    }
    result
}

/// Encodes bytes as base36, truncated to `length` characters.
fn encode_base36(bytes: &[u8], length: usize) -> String {
    const ALPHABET: &[u8] = b"0123456789abcdefghijklmnopqrstuvwxyz";

    // Convert bytes to a big number, then repeatedly mod 36
    let mut digits: Vec<u8> = Vec::with_capacity(length);
    let mut num = bytes.to_vec();

    while digits.len() < length {
        let mut remainder = 0u32;
        let mut next = Vec::new();
        for &byte in &num {
            let acc = remainder * 256 + u32::from(byte);
            let quotient = acc / 36;
            remainder = acc % 36;
            if !next.is_empty() || quotient > 0 {
                #[allow(clippy::cast_possible_truncation)]
                next.push(quotient as u8);
            }
        }
        #[allow(clippy::cast_possible_truncation)]
        digits.push(ALPHABET[remainder as usize]);
        num = next;
        if num.is_empty() && digits.len() < length {
            // Pad with zeros if we run out of bits
            while digits.len() < length {
                digits.push(b'0');
            }
        }
    }

    // digits are in reverse order (LSB first)
    digits.truncate(length);
    digits.reverse();
    String::from_utf8(digits).expect("base36 is valid utf8")
}

/// Computes a content hash of the mutable content fields.
/// Used for change detection (refinement termination, dedup).
pub fn content_hash(
    title: &str,
    description: &str,
    design: Option<&str>,
    acceptance_criteria: Option<&str>,
    orb_type: &str,
    scope: &[String],
    priority: u8,
) -> String {
    let scope_joined = scope.join(",");
    let input = format!(
        "{title}|{description}|{}|{}|{orb_type}|{scope_joined}|{priority}",
        design.unwrap_or(""),
        acceptance_criteria.unwrap_or(""),
    );
    let hash = sha256_of(input.as_bytes());
    encode_base36(&hash, 12)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_generation_is_deterministic() {
        let existing = HashSet::new();
        let id1 = OrbId::generate("title", "desc", "user", 1000, &existing);
        let id2 = OrbId::generate("title", "desc", "user", 1000, &existing);
        assert_eq!(id1, id2);
    }

    #[test]
    fn id_starts_with_orb_prefix() {
        let existing = HashSet::new();
        let id = OrbId::generate("title", "desc", "user", 1000, &existing);
        assert!(id.as_str().starts_with("orb-"));
    }

    #[test]
    fn different_inputs_produce_different_ids() {
        let existing = HashSet::new();
        let id1 = OrbId::generate("title1", "desc", "user", 1000, &existing);
        let id2 = OrbId::generate("title2", "desc", "user", 1000, &existing);
        assert_ne!(id1, id2);
    }

    #[test]
    fn collision_handling_increments_nonce() {
        let existing = HashSet::new();
        let id1 = OrbId::generate("title", "desc", "user", 1000, &existing);

        let mut existing_with_collision = HashSet::new();
        existing_with_collision.insert(id1.as_str().to_string());

        let id2 = OrbId::generate("title", "desc", "user", 1000, &existing_with_collision);
        assert_ne!(id1, id2);
        // Still starts with orb-
        assert!(id2.as_str().starts_with("orb-"));
    }

    #[test]
    fn collision_handling_extends_length() {
        // Fill all 10 nonces at length 3
        let mut existing = HashSet::new();
        for nonce in 0..10 {
            let hash = compute_hash("title", "desc", "user", 1000, nonce);
            let candidate = encode_base36(&hash, 3);
            existing.insert(format!("orb-{candidate}"));
        }

        let id = OrbId::generate("title", "desc", "user", 1000, &existing);
        assert!(id.as_str().starts_with("orb-"));
        // Should be longer than 3 chars after prefix
        assert!(id.as_str().len() > "orb-".len() + 3);
    }

    #[test]
    fn child_id_generation() {
        let parent = OrbId::from_raw("orb-abc");
        let child1 = parent.child(1);
        let child2 = parent.child(2);
        assert_eq!(child1.as_str(), "orb-abc.1");
        assert_eq!(child2.as_str(), "orb-abc.2");
    }

    #[test]
    fn child_id_is_child() {
        let parent = OrbId::from_raw("orb-abc");
        assert!(!parent.is_child());
        let child = parent.child(1);
        assert!(child.is_child());
    }

    #[test]
    fn parent_id_extraction() {
        let child = OrbId::from_raw("orb-abc.1");
        assert_eq!(child.parent_id(), Some(OrbId::from_raw("orb-abc")));

        let root = OrbId::from_raw("orb-abc");
        assert_eq!(root.parent_id(), None);

        // Nested child
        let nested = OrbId::from_raw("orb-abc.1.2");
        assert_eq!(nested.parent_id(), Some(OrbId::from_raw("orb-abc.1")));
    }

    #[test]
    fn content_hash_changes_on_content_edit() {
        let h1 = content_hash("title", "desc", None, None, "task", &[], 3);
        let h2 = content_hash("title", "desc changed", None, None, "task", &[], 3);
        assert_ne!(h1, h2);
    }

    #[test]
    fn content_hash_stable_on_same_input() {
        let h1 = content_hash("title", "desc", None, None, "task", &[], 3);
        let h2 = content_hash("title", "desc", None, None, "task", &[], 3);
        assert_eq!(h1, h2);
    }

    #[test]
    fn content_hash_changes_on_priority() {
        let h1 = content_hash("title", "desc", None, None, "task", &[], 3);
        let h2 = content_hash("title", "desc", None, None, "task", &[], 1);
        assert_ne!(h1, h2);
    }

    #[test]
    fn content_hash_changes_on_type() {
        let h1 = content_hash("title", "desc", None, None, "task", &[], 3);
        let h2 = content_hash("title", "desc", None, None, "bug", &[], 3);
        assert_ne!(h1, h2);
    }

    #[test]
    fn serde_round_trip() {
        let id = OrbId::from_raw("orb-abc");
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "\"orb-abc\"");
        let parsed: OrbId = serde_json::from_str(&json).unwrap();
        assert_eq!(id, parsed);
    }
}
