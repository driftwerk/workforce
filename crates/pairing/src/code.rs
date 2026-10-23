use rand::Rng;

/// Characters used for pairing codes. Excludes ambiguous characters
/// (0/O, 1/I/L) so codes survive being read aloud or hand-copied.
const CHARSET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";

/// Length of a generated pairing code. 12 chars of the 30-char alphabet
/// is ~59 bits of entropy — paired with the rate limit on the polling
/// endpoint, brute force is computationally infeasible.
const CODE_LEN: usize = 12;

pub fn generate_pairing_code() -> String {
    let mut rng = rand::thread_rng();
    (0..CODE_LEN)
        .map(|_| CHARSET[rng.gen_range(0..CHARSET.len())] as char)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn code_has_expected_length() {
        assert_eq!(generate_pairing_code().len(), CODE_LEN);
    }

    #[test]
    fn code_uses_valid_charset() {
        let valid: HashSet<char> = std::str::from_utf8(CHARSET).unwrap().chars().collect();
        for _ in 0..100 {
            for ch in generate_pairing_code().chars() {
                assert!(valid.contains(&ch), "Invalid character: {ch}");
            }
        }
    }

    #[test]
    fn codes_are_unique() {
        let codes: HashSet<String> = (0..1000).map(|_| generate_pairing_code()).collect();
        assert_eq!(codes.len(), 1000);
    }
}
