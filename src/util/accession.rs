/// Converts a UniProt accession string into a 64-bit integer using base-36 decoding.
#[inline]
pub fn to_accession_long36(accession: &str) -> Result<u64, std::num::ParseIntError> {
    u64::from_str_radix(accession, 36)
}

const BASE36_CHARS: &[u8; 36] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZ";

/// Converts a 64-bit integer back into a UniProt accession string (base-36, uppercase).
#[inline]
pub fn from_accession_long36(accession: u64) -> String {
    if accession == 0 {
        return "0".to_string();
    }

    // u64::MAX in base-36 has at most 13 characters ("3W5E11264SGSF")
    let mut buf = [0u8; 13];
    let mut pos = buf.len();
    let mut n = accession;

    while n > 0 {
        pos -= 1;
        buf[pos] = BASE36_CHARS[(n % 36) as usize];
        n /= 36;
    }

    // SAFETY: buf[pos..] contains only ASCII characters (0-9, A-Z)
    unsafe { String::from_utf8_unchecked(buf[pos..].to_vec()) }
}

/// Basic UniProt accession format check from UniProt/Swiss-Prot manual:
/// `[OPQ][0-9][A-Z0-9]{3}[0-9] | [A-NR-Z][0-9]([A-Z][A-Z0-9]{2}[0-9]){1,2}`
pub fn is_valid_uniprot_accession(accession: &str) -> bool {
    let bytes = accession.as_bytes();

    match bytes.len() {
        6 => {
            let b0 = bytes[0];
            let b1 = bytes[1];
            let b2 = bytes[2];
            let b3 = bytes[3];
            let b4 = bytes[4];
            let b5 = bytes[5];

            matches!(b0, b'O' | b'P' | b'Q')
                && b1.is_ascii_digit()
                && b2.is_ascii_alphanumeric()
                && b3.is_ascii_alphanumeric()
                && b4.is_ascii_alphanumeric()
                && b5.is_ascii_digit()
        }
        10 => {
            let b0 = bytes[0];
            let b1 = bytes[1];
            let b2 = bytes[2];
            let b3 = bytes[3];
            let b4 = bytes[4];
            let b5 = bytes[5];
            let b6 = bytes[6];
            let b7 = bytes[7];
            let b8 = bytes[8];
            let b9 = bytes[9];

            is_a_to_n_or_r_to_z(b0)
                && b1.is_ascii_digit()
                && b2.is_ascii_uppercase()
                && b3.is_ascii_alphanumeric()
                && b4.is_ascii_alphanumeric()
                && b5.is_ascii_digit()
                && b6.is_ascii_uppercase()
                && b7.is_ascii_alphanumeric()
                && b8.is_ascii_alphanumeric()
                && b9.is_ascii_digit()
        }
        _ => false,
    }
}

#[inline]
fn is_a_to_n_or_r_to_z(b: u8) -> bool {
    b.is_ascii_uppercase() && b != b'O' && b != b'P' && b != b'Q'
}

#[cfg(test)]
mod tests {
    use super::*;


    #[test]
    fn validates_known_good_and_bad_uniprot_accessions() {
        assert!(is_valid_uniprot_accession("P12345"));
        assert!(is_valid_uniprot_accession("Q1AAA9"));
        assert!(is_valid_uniprot_accession("A0A022YWF9"));

        assert!(!is_valid_uniprot_accession("A0A022YWF")); // wrong length
        assert!(!is_valid_uniprot_accession("Z12345")); // invalid 6-char prefix
        assert!(!is_valid_uniprot_accession("A0a022YWF9")); // lowercase not allowed
    }


    #[test]
    fn test_to_accession_long36() {
        let val = to_accession_long36("P12345").unwrap();
        assert_eq!(to_accession_long36("p12345").unwrap(), val);

        assert_eq!(to_accession_long36("A0A022Y1F5").unwrap(), 1016383334204849);
    }

    #[test]
    fn test_from_accession_long36() {
        assert_eq!(from_accession_long36(1016383334204849), "A0A022Y1F5");
        assert_eq!(from_accession_long36(0), "0");
    }

    #[test]
    fn test_roundtrip() {
        let accessions = vec![
            "A2BC19",
            "P12345",
            "A0A023GPI8",
            "Q6LLZ0",
            "A0A0B4HM53",
            "A0A0D3MJQ5",
        ];
        for acc in accessions {
            let encoded = to_accession_long36(acc).unwrap();
            let decoded = from_accession_long36(encoded);
            assert_eq!(decoded, acc, "Roundtrip failed for accession: {}", acc);
        }
    }
}
