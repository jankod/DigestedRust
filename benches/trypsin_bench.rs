use criterion::{
    black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput,
};
use rand::RngExt;

use digested_rust::proteomics::trypsin::{
    digest_trypsin_many_parallel, digest_trypsin_ranges, TrypsinDigester,
};

// ── Helpers ──────────────────────────────────────────────────────────

const AMINO_ACIDS: &[u8] = b"ACDEFGHIKLMNPQRSTVWY";

/// Generate a random protein-like sequence of `len` amino acids.
fn random_sequence(len: usize) -> Vec<u8> {
    let mut rng = rand::rng();
    (0..len)
        .map(|_| AMINO_ACIDS[rng.random_range(0..AMINO_ACIDS.len())])
        .collect()
}

/// Generate `count` random sequences each of length `len`.
fn random_sequences(count: usize, len: usize) -> Vec<Vec<u8>> {
    (0..count).map(|_| random_sequence(len)).collect()
}

// ── Benchmarks ───────────────────────────────────────────────────────

/// Single-protein digestion, varying protein length.
fn bench_single_digest(c: &mut Criterion) {
    let mut group = c.benchmark_group("single_digest");

    for &seq_len in &[100, 500, 2_000, 10_000] {
        let seq = random_sequence(seq_len);
        group.throughput(Throughput::Bytes(seq_len as u64));

        group.bench_with_input(
            BenchmarkId::new("no_filter", seq_len),
            &seq,
            |b, seq| {
                let mut digester = TrypsinDigester::new();
                b.iter(|| {
                    black_box(digester.digest_ranges(
                        black_box(seq),
                        2,
                        0,
                        usize::MAX,
                    ));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("filter_6_50", seq_len),
            &seq,
            |b, seq| {
                let mut digester = TrypsinDigester::new();
                b.iter(|| {
                    black_box(digester.digest_ranges(
                        black_box(seq),
                        2,
                        6,
                        50,
                    ));
                });
            },
        );
    }

    group.finish();
}

/// Batch parallel digestion — measures throughput over many proteins.
fn bench_parallel_digest(c: &mut Criterion) {
    let mut group = c.benchmark_group("parallel_digest");
    group.sample_size(30);

    for &count in &[1_000, 10_000, 50_000] {
        let sequences = random_sequences(count, 500);
        let total_bytes: u64 = sequences.iter().map(|s| s.len() as u64).sum();
        group.throughput(Throughput::Bytes(total_bytes));

        group.bench_with_input(
            BenchmarkId::new("no_filter", count),
            &sequences,
            |b, seqs| {
                b.iter(|| {
                    black_box(digest_trypsin_many_parallel(
                        black_box(seqs),
                        2,
                        0,
                        usize::MAX,
                    ));
                });
            },
        );

        group.bench_with_input(
            BenchmarkId::new("filter_6_50", count),
            &sequences,
            |b, seqs| {
                b.iter(|| {
                    black_box(digest_trypsin_many_parallel(
                        black_box(seqs),
                        2,
                        6,
                        50,
                    ));
                });
            },
        );
    }

    group.finish();
}

/// Missed cleavages comparison (0, 1, 2) on a fixed large protein.
fn bench_missed_cleavages(c: &mut Criterion) {
    let mut group = c.benchmark_group("missed_cleavages");

    let seq = random_sequence(5_000);
    group.throughput(Throughput::Bytes(seq.len() as u64));

    for mc in 0u8..=2 {
        group.bench_with_input(
            BenchmarkId::new("mc", mc),
            &seq,
            |b, seq| {
                let mut digester = TrypsinDigester::new();
                b.iter(|| {
                    black_box(digester.digest_ranges(
                        black_box(seq),
                        mc,
                        6,
                        50,
                    ));
                });
            },
        );
    }

    group.finish();
}

/// Cleavage-site detection in isolation (via digest_trypsin_ranges with mc=0).
fn bench_cleavage_site_detection(c: &mut Criterion) {
    let mut group = c.benchmark_group("cleavage_site_detection");

    for &seq_len in &[256, 1_024, 8_192, 65_536] {
        let seq = random_sequence(seq_len);
        group.throughput(Throughput::Bytes(seq_len as u64));

        group.bench_with_input(
            BenchmarkId::new("len", seq_len),
            &seq,
            |b, seq| {
                b.iter(|| {
                    black_box(digest_trypsin_ranges(
                        black_box(seq),
                        0,
                        0,
                        usize::MAX,
                    ));
                });
            },
        );
    }

    group.finish();
}

criterion_group!(
    benches,
    bench_single_digest,
    bench_parallel_digest,
    bench_missed_cleavages,
    bench_cleavage_site_detection,
);
criterion_main!(benches);
