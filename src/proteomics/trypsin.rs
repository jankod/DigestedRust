use rayon::prelude::*;
use std::ops::Range;

pub const MAX_MISSED_CLEAVAGES: u8 = 2;

pub struct TrypsinDigester {
    cleavage_sites: Vec<usize>,
    ranges: Vec<Range<usize>>,
}

impl TrypsinDigester {
    pub fn new() -> Self {
        Self {
            cleavage_sites: Vec::with_capacity(128),
            ranges: Vec::with_capacity(256),
        }
    }

    #[inline]
    pub fn digest_ranges(
        &mut self,
        sequence: &[u8],
        missed_cleavages: u8,
        min_len: usize,
        max_len: usize,
    ) -> &[Range<usize>] {
        digest_trypsin_ranges_into(
            sequence,
            missed_cleavages,
            min_len,
            max_len,
            &mut self.cleavage_sites,
            &mut self.ranges,
        );
        &self.ranges
    }
}

impl Default for TrypsinDigester {
    fn default() -> Self {
        Self::new()
    }
}

#[inline(always)]
fn collect_cleavage_sites_scalar_branchless(sequence: &[u8], cleavage_sites: &mut Vec<usize>) {
    let len = sequence.len();
    cleavage_sites.push(0);

    if len == 0 {
        return;
    }

    for i in 0..(len - 1) {
        let current = sequence[i];
        let is_kr = (current == b'K') | (current == b'R');
        let next_is_p = sequence[i + 1] == b'P';
        let cut = is_kr & !next_is_p;

        if cut {
            cleavage_sites.push(i + 1);
        }
    }

    cleavage_sites.push(len);
}

#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn collect_cleavage_sites_avx2(sequence: &[u8], cleavage_sites: &mut Vec<usize>) {
    #[cfg(target_arch = "x86")]
    use std::arch::x86::*;
    #[cfg(target_arch = "x86_64")]
    use std::arch::x86_64::*;

    let len = sequence.len();
    cleavage_sites.push(0);

    if len == 0 {
        return;
    }

    let mut i = 0usize;
    let v_k = _mm256_set1_epi8(b'K' as i8);
    let v_r = _mm256_set1_epi8(b'R' as i8);
    let v_p = _mm256_set1_epi8(b'P' as i8);

    while i + 33 <= len {
        let (cur, next) = unsafe {
            (
                _mm256_loadu_si256(sequence.as_ptr().add(i) as *const __m256i),
                _mm256_loadu_si256(sequence.as_ptr().add(i + 1) as *const __m256i),
            )
        };

        let eq_k = _mm256_cmpeq_epi8(cur, v_k);
        let eq_r = _mm256_cmpeq_epi8(cur, v_r);
        let is_kr = _mm256_or_si256(eq_k, eq_r);

        let is_next_p = _mm256_cmpeq_epi8(next, v_p);
        let cut_mask_v = _mm256_andnot_si256(is_next_p, is_kr);

        let mut mask = _mm256_movemask_epi8(cut_mask_v) as u32;

        while mask != 0 {
            let lane = mask.trailing_zeros() as usize;
            cleavage_sites.push(i + lane + 1);
            mask &= mask - 1;
        }

        i += 32;
    }

    for j in i..(len - 1) {
        let current = sequence[j];
        let is_kr = (current == b'K') | (current == b'R');
        let next_is_p = sequence[j + 1] == b'P';
        let cut = is_kr & !next_is_p;
        if cut {
            cleavage_sites.push(j + 1);
        }
    }

    cleavage_sites.push(len);
}

#[inline(always)]
fn collect_cleavage_sites(sequence: &[u8], cleavage_sites: &mut Vec<usize>) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    {
        if std::arch::is_x86_feature_detected!("avx2") {
            // SAFETY: Runtime feature detection guarantees AVX2 availability.
            unsafe {
                collect_cleavage_sites_avx2(sequence, cleavage_sites);
            }
            return;
        }
    }

    collect_cleavage_sites_scalar_branchless(sequence, cleavage_sites);
}

/// Builds trypsin peptide ranges for a protein sequence.
///
/// Trypsin rule:
/// - cut after `K` or `R`
/// - except when the next residue is `P`
///
/// `missed_cleavages` is clamped to `0..=2`.
pub fn digest_trypsin_ranges_into(
    sequence: &[u8],
    missed_cleavages: u8,
    min_len: usize,
    max_len: usize,
    cleavage_sites: &mut Vec<usize>,
    out_ranges: &mut Vec<Range<usize>>,
) {
    let max_missed = missed_cleavages.min(MAX_MISSED_CLEAVAGES) as usize;

    cleavage_sites.clear();
    out_ranges.clear();

    collect_cleavage_sites(sequence, cleavage_sites);

    let segments = cleavage_sites.len().saturating_sub(1);
    out_ranges.reserve(segments.saturating_mul(max_missed + 1));

    for start_seg in 0..segments {
        let max_end_seg = (start_seg + max_missed + 1).min(segments);
        let start_idx = cleavage_sites[start_seg];
        for end_seg in (start_seg + 1)..=max_end_seg {
            let end_idx = cleavage_sites[end_seg];
            let len = end_idx - start_idx;
            
            if len > max_len {
                break;
            }
            if len >= min_len {
                out_ranges.push(start_idx..end_idx);
            }
        }
    }
}

pub fn digest_trypsin_ranges(
    sequence: &[u8],
    missed_cleavages: u8,
    min_len: usize,
    max_len: usize,
) -> Vec<Range<usize>> {
    let mut digester = TrypsinDigester::new();
    digester.digest_ranges(sequence, missed_cleavages, min_len, max_len).to_vec()
}

pub fn digest_trypsin(
    sequence: &str,
    missed_cleavages: u8,
    min_len: usize,
    max_len: usize,
) -> Vec<&str> {
    assert!(
        sequence.is_ascii(),
        "Trypsin digest expects ASCII amino-acid sequence"
    );

    let ranges = digest_trypsin_ranges(sequence.as_bytes(), missed_cleavages, min_len, max_len);
    let mut peptides = Vec::with_capacity(ranges.len());

    for range in ranges {
        peptides.push(&sequence[range]);
    }

    peptides
}

pub fn digest_trypsin_many_parallel<T>(
    sequences: &[T],
    missed_cleavages: u8,
    min_len: usize,
    max_len: usize,
) -> Vec<Vec<Range<usize>>>
where
    T: AsRef<[u8]> + Sync,
{
    sequences
        .par_iter()
        .map_init(
            || TrypsinDigester::new(),
            |digester, seq| digester.digest_ranges(seq.as_ref(), missed_cleavages, min_len, max_len).to_vec(),
        )
        .collect()
}

#[allow(dead_code)]
fn main() {}

#[cfg(test)]
mod tests {
    use super::{digest_trypsin, digest_trypsin_many_parallel, digest_trypsin_ranges};

    #[test]
    fn digest_trypsin_zero_missed_cleavage() {
        let seq = "AKRPQKRPQR";
        let peptides = digest_trypsin(seq, 0, 0, usize::MAX);
        assert_eq!(peptides, vec!["AK", "RPQK", "RPQR"]);
    }

    #[test]
    fn digest_trypsin_one_missed_cleavage() {
        let seq = "AKRPQKRPQR";
        let peptides = digest_trypsin(seq, 1, 0, usize::MAX);
        assert_eq!(peptides, vec!["AK", "AKRPQK", "RPQK", "RPQKRPQR", "RPQR"]);
    }

    #[test]
    fn digest_trypsin_two_missed_cleavage() {
        let seq = "AKRPQKRPQR";
        let peptides = digest_trypsin(seq, 2, 0, usize::MAX);
        assert_eq!(
            peptides,
            vec!["AK", "AKRPQK", "AKRPQKRPQR", "RPQK", "RPQKRPQR", "RPQR"]
        );
    }

    #[test]
    fn digest_trypsin_no_cut_sites() {
        let seq = "MPEPTIDE";
        let peptides = digest_trypsin(seq, 2, 0, usize::MAX);
        assert_eq!(peptides, vec!["MPEPTIDE"]);
    }

    #[test]
    fn digest_trypsin_clamps_missed_cleavages() {
        let seq = "AKR";
        let peptides = digest_trypsin(seq, 9, 0, usize::MAX);
        assert_eq!(peptides, vec!["AK", "AKR", "R"]);
    }

    #[test]
    fn digest_trypsin_parallel_matches_single() {
        let sequences = vec![
            "AKRPQKRPQR".to_string(),
            "MPEPTIDE".to_string(),
            "RPKRRPK".to_string(),
        ];

        let parallel = digest_trypsin_many_parallel(&sequences, 2, 0, usize::MAX);
        for (i, seq) in sequences.iter().enumerate() {
            let single = digest_trypsin_ranges(seq.as_bytes(), 2, 0, usize::MAX);
            assert_eq!(parallel[i], single);
        }
    }

    #[test]
    fn digest_trypsin_size_limits() {
        let seq = "AKRPQKRPQR";
        // Sizes for missed_cleavages=2:
        // AK (2)
        // RPQK (4)
        // RPQR (4)
        // AKRPQK (6)
        // RPQKRPQR (8)
        // AKRPQKRPQR (10)

        // Only exact 4 range
        let peptides = digest_trypsin(seq, 2, 4, 4);
        assert_eq!(peptides, vec!["RPQK", "RPQR"]);

        // Range 5 to 9
        let peptides2 = digest_trypsin(seq, 2, 5, 9);
        assert_eq!(peptides2, vec!["AKRPQK", "RPQKRPQR"]);
    }
}
