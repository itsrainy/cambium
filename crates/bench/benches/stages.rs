//! Criterion entry points for the in-memory stages.
//!
//! `cambium bench run` is the operator-facing path (machine-fingerprinted JSON,
//! regression gating); this is the developer-facing one, for when you want
//! criterion's statistics while changing an inner loop.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use cambium_bench::stages::StageContext;
use cambium_index::{walk, Csr, IndexBuilder, WalkOptions};
use cambium_synth::{generate, GraphSpec};

fn bench_build_index(c: &mut Criterion) {
    let mut g = c.benchmark_group("build_index");
    for preset in ["small", "medium"] {
        let graph = generate(&GraphSpec::preset(preset).unwrap());
        let updates = graph.package_updates();
        let targets: usize = updates.iter().map(|u| u.targets.len()).sum();
        g.throughput(Throughput::Elements(targets as u64));
        g.bench_with_input(BenchmarkId::from_parameter(preset), &updates, |b, u| {
            b.iter(|| {
                let mut builder = IndexBuilder::new(cambium_index::new_lineage());
                builder.apply_packages(u);
                builder.finish("bench")
            })
        });
    }
    g.finish();
}

fn bench_walk(c: &mut Criterion) {
    let mut g = c.benchmark_group("walk");
    for preset in ["small", "medium"] {
        let graph = generate(&GraphSpec::preset(preset).unwrap());
        let index = graph.build_index("bench");
        let v = index.view();
        let map = v.labels.build_map();
        let direct: Vec<u32> = (0..graph.targets.len() as u32)
            .step_by((graph.targets.len() / 10).max(1))
            .take(10)
            .filter_map(|i| map.get(graph.target_label(i).as_str()).copied())
            .collect();
        let affected = walk::walk(v, &direct, &WalkOptions::default())
            .affected
            .len();
        g.throughput(Throughput::Elements(affected as u64));
        g.bench_with_input(BenchmarkId::from_parameter(preset), &direct, |b, d| {
            b.iter(|| walk::walk(v, d, &WalkOptions::default()))
        });
    }
    g.finish();
}

fn bench_reverse_rebuild(c: &mut Criterion) {
    let mut g = c.benchmark_group("reverse_rebuild");
    for preset in ["small", "medium"] {
        let graph = generate(&GraphSpec::preset(preset).unwrap());
        let index = graph.build_index("bench");
        let v = index.view();
        let n = v.num_nodes() as usize;
        g.throughput(Throughput::Elements(v.num_edges() as u64));
        g.bench_function(BenchmarkId::new("serial", preset), |b| {
            b.iter(|| Csr::reverse_of(v.forward, n))
        });
        g.bench_function(BenchmarkId::new("parallel", preset), |b| {
            b.iter(|| Csr::reverse_of_parallel(v.forward, n))
        });
    }
    g.finish();
}

fn bench_parse_proto(c: &mut Criterion) {
    let mut g = c.benchmark_group("parse_proto");
    for preset in ["small", "medium"] {
        let graph = generate(&GraphSpec::preset(preset).unwrap());
        let bytes = cambium_synth::proto::to_streamed_proto(&graph);
        let opts = cambium_proto::ConvertOptions::default();
        let files = cambium_engine::query::no_files();
        g.throughput(Throughput::Bytes(bytes.len() as u64));
        g.bench_with_input(BenchmarkId::from_parameter(preset), &bytes, |b, bytes| {
            b.iter(|| cambium_engine::query::parse_stream(bytes, &opts, &files).unwrap())
        });
    }
    g.finish();
}

fn bench_splice_and_e2e(c: &mut Criterion) {
    let mut g = c.benchmark_group("pipeline");
    for preset in ["small", "medium"] {
        let ctx = StageContext::new(preset, 1).unwrap();
        g.bench_function(BenchmarkId::new("splice", preset), |b| {
            b.iter(|| cambium_bench::stages::splice(&ctx))
        });
        g.bench_function(BenchmarkId::new("e2e_memory", preset), |b| {
            b.iter(|| cambium_bench::stages::e2e_memory(&ctx))
        });
    }
    g.finish();
}

criterion_group!(
    benches,
    bench_build_index,
    bench_walk,
    bench_reverse_rebuild,
    bench_parse_proto,
    bench_splice_and_e2e
);
criterion_main!(benches);
