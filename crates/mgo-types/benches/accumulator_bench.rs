// Copyright (c) MangoNet Labs Ltd.
// SPDX-License-Identifier: Apache-2.0

use fastcrypto::hash::MultisetHash;
use mgo_types::accumulator::Accumulator;
use mgo_types::base_types::ObjectDigest;

use criterion::*;

fn accumulator_benchmark(c: &mut Criterion) {
    {
        let digests: Vec<_> = (0..1_000).map(|_| ObjectDigest::random()).collect();
        let mut accumulator = Accumulator::default();

        let mut group = c.benchmark_group("accumulator");
        group.throughput(Throughput::Elements(digests.len() as u64));

        group.bench_function("accumulate_digests", |b| {
            b.iter(|| accumulator.insert_all(&digests))
        });
    }

    {
        let mut group = c.benchmark_group("accumulator");
        group.throughput(Throughput::Elements(1));

        let mut accumulator = Accumulator::default();
        let point = {
            let digest = ObjectDigest::random();
            let mut accumulator = Accumulator::default();
            accumulator.insert(digest);
            accumulator
        };
        group.bench_function("sum_accumulators", |b| b.iter(|| accumulator.union(&point)));
    }
}

criterion_group!(benches, accumulator_benchmark);
criterion_main!(benches);
