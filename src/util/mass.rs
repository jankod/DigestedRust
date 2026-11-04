pub const WATER_MONOISOTOPIC_MASS: f64 = 18.010_564_683_700_003;

const MASS_TABLE: [f64; 256] = build_mass_table();

const fn build_mass_table() -> [f64; 256] {
    let mut table = [f64::NAN; 256];

    // Pyteomics monoisotopic masses
    table[b'A' as usize] = 71.037_113_784_71;
    table[b'R' as usize] = 156.101_111_023_599_97;
    table[b'N' as usize] = 114.042_927_441_140_01;
    table[b'D' as usize] = 115.026_943_023_830_01;
    table[b'C' as usize] = 103.009_184_784_71;
    table[b'E' as usize] = 129.042_593_087_969_98;
    table[b'Q' as usize] = 128.058_577_505_279_97;
    table[b'G' as usize] = 57.021_463_720_57;
    table[b'H' as usize] = 137.058_911_858_450_02;
    table[b'I' as usize] = 113.084_063_977_130_01;
    table[b'L' as usize] = 113.084_063_977_130_01;
    table[b'K' as usize] = 128.094_963_013_999_97;
    table[b'M' as usize] = 131.040_484_912_99;
    table[b'F' as usize] = 147.068_413_912_989_97;
    table[b'P' as usize] = 97.052_763_848_85;
    table[b'S' as usize] = 87.032_028_404_270_01;
    table[b'T' as usize] = 101.047_678_468_41;
    table[b'W' as usize] = 186.079_312_949_859_97;
    table[b'Y' as usize] = 163.063_328_532_549_97;
    table[b'V' as usize] = 99.068_413_912_99;
    
    table[b'J' as usize] = 113.084_063_977_130_01; // J=I=L
    table[b'U' as usize] = 150.953_635_084_71; // Selenocysteine
    table[b'O' as usize] = 237.147_726_862_849_96; // Pyrrolysine

    table
}

/// Monoisotopic peptide mass with terminal water mass included.
///
/// Returns `None` for any non-standard residue (expects ONLY uppercase letters).
#[inline]
pub fn calc_protein_mass(sequence: &str) -> Option<f64> {
    let mut mass = WATER_MONOISOTOPIC_MASS;

    for &aa in sequence.as_bytes() {
        let residue_mass = MASS_TABLE[aa as usize];
        if residue_mass.is_nan() {
            return None;
        }
        mass += residue_mass;
    }

    Some(mass)
}

/// Discretizes a given mass (f64) into a `u32` with 4 decimal places of precision.
/// It multiplies the mass by 10,000 and rounds to the nearest integer.
/// Suitable for masses up to ~429,496.7295 Da.
#[inline]
pub fn discretize_mass(mass: f64) -> u32 {
    // We add 0.5 before truncating to simulate rounding to the nearest integer,
    // which is faster than using `f64::round()`.
    (mass * 10000.0 + 0.5) as u32
}

/// Converts a discretized `u32` mass back to its `f64` representation.
/// Note: The result will have at most 4 decimal places of precision.
#[inline]
pub fn undiscretize_mass(discrete_mass: u32) -> f64 {
    (discrete_mass as f64) / 10000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mass_adds_water() {
        let g = calc_protein_mass("G").unwrap();
        let expected = WATER_MONOISOTOPIC_MASS + 57.021_463_720_57;
        assert!((g - expected).abs() < 1e-9);

        let prot = "MPEQAQQGEQSVKRRRVTRACDECRKKKVKCDGQQPCIHCTVYSYECTYKKPTKRTQNSGNSGVLTLGNVTTGPSSSTVVAAAASNPNKLLSNIKTERAILPGASTIPASNNPSKPRKYKTKSTRLQSKIDRYKQIFDEVFPQLPDIDNLDIPVFLQIFHNFKRDSQSFLDDTVKEYTLIVNDSSSPIQPVLSSNSKNSTPDEFLPNMKSDSNSASSNREQDSVDTYSNIPVGREIKIILPPKAIALQFVKSTWEHCCVLLRFYHRPSFIRQLDELYETDPNNYTSKQMQFLPLCYAAIAVGALFSKSIVSNDSSREKFLQDEGYKYFIAARKLIDITNARDLNSIQAILMLIIFLQCSARLSTCYTYIGVAMRSALRAGFHRKLSPNSGFSPIEIEMRKRLFYTIYKLDVYINAMLGLPRSISPDDFDQTLPLDLSDENITEVAYLPENQHSVLSSTGISNEHTKLFLILNEIISELYPIKKTSNIISHETVTSLELKLRNWLDSLPKELIPNAENIDPEYERANRLLHLSFLHVQIILYRPFIHYLSRNMNAENVDPLCYRRARNSIAVARTVIKLAKEMVSNNLLTGSYWYACYTIFYSVAGLLFYIHEAQLPDKDSAREYYDILKDAETGRSVLIQLKDSSMAASRTYNLLNQIFEKLNSKTIQLTALHSSPSNESAFLVTNNSSALKPHLGDSLQPPVFFSSQDTKNSFSLAKSEESTNDYAMANYLNNTPISENPLNEAQQQDQVSQGTTNMSNERDPNNFLSIDIRLDNNGQSNILDATDDVFIRNDGDIPTNSAFDFSSSKSNASNNSNPDTINNNYNNVSGKNNNNNNITNNSNNNHNNNNNDNNNNNNNNNNNNNNNNNSGNSSNNNNNNNNNKNNNDFGIKIDNNSPSYEGFPQLQIPLSQDNLNIEDKEEMSPNIEIKNEQNMTDSNDILGVFDQLDAQLFGKYLPLNYPSE";
        let mass = calc_protein_mass(prot).unwrap();
        // Expected mass according to pyteomics: 108713.92867831836
        let expected = 108713.92867831836;

        assert!((mass - expected).abs() < 1e-6, "calc mass = {}, expected = {}", mass, expected);
    }

    #[test]
    fn mass_rejects_unknown_residue() {
        assert!(calc_protein_mass("PEPTIDE*").is_none());
    }

    #[test]
    fn test_discretize_mass() {
        assert_eq!(discretize_mass(1234.56789), 12345679);
        assert_eq!(discretize_mass(1234.56781), 12345678);
        assert_eq!(discretize_mass(0.0), 0);
        assert_eq!(discretize_mass(429496.7295), 4294967295);
    }

    #[test]
    fn test_undiscretize_mass() {
        assert_eq!(undiscretize_mass(12345679), 1234.5679);
        assert_eq!(undiscretize_mass(0), 0.0);
    }

    #[test]
    fn test_mass_roundtrip() {
        let masses = vec![0.0, 1.2345, 100.0001, 54321.9876, 429496.7295];
        for mass in masses {
            let discrete = discretize_mass(mass);
            let restored = undiscretize_mass(discrete);
            assert!((restored - mass).abs() < 1e-5, "Roundtrip failed for {}", mass);
        }
    }
}
