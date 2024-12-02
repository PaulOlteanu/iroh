use std::{
    net::Ipv4Addr,
    sync::Arc,
    time::{Duration, Instant},
};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use iroh_dns_server::store::{PacketSource, ZoneStore};
use pkarr::{
    dns::{rdata::RData, Name, Packet, ResourceRecord},
    Keypair, SignedPacket,
};
use rayon::{
    iter::{IntoParallelIterator, ParallelIterator},
    ThreadPoolBuilder,
};

fn benchmark(c: &mut Criterion) {
    let mut group = c.benchmark_group("inserts");
    for num_threads in [1, 2, 4, 8, 16] {
        group.bench_with_input(
            BenchmarkId::from_parameter(num_threads),
            &num_threads,
            |b, num_threads| {
                let pool = ThreadPoolBuilder::new()
                    .num_threads(*num_threads)
                    .build()
                    .unwrap();

                b.iter_custom(|iters| {
                    let mut duration = Duration::new(0, 1);

                    for _ in 0..iters {
                        let path = format!("/tmp/bench.db");
                        let store = Arc::new(ZoneStore::persistent(&path).unwrap());
                        // let store = Arc::new(ZoneStore::in_memory().unwrap());
                        let packets = (0..128)
                            .map(|i| {
                                let keypair = Keypair::random();

                                let name = format!("{i}idk.com.");
                                let mut packet = Packet::new_reply(i);
                                packet.answers.push(ResourceRecord::new(
                                    Name::new(&name).unwrap(),
                                    simple_dns::CLASS::IN,
                                    30,
                                    RData::A(pkarr::dns::rdata::A {
                                        address: Ipv4Addr::new(1, 1, 1, 1).into(),
                                    }),
                                ));

                                SignedPacket::from_packet(&keypair, &packet).unwrap()
                            })
                            .collect::<Vec<_>>();

                        let start = Instant::now();
                        pool.install(|| {
                            packets
                                .into_par_iter()
                                .for_each_with(store, |store, packet| {
                                    store.insert(packet, PacketSource::PkarrPublish).unwrap();
                                });

                        });
                        duration += start.elapsed();
                        std::fs::remove_file(path).unwrap();
                    }

                    duration
                });
            },
        );
    }
}

criterion_group!(benches, benchmark,);
criterion_main!(benches);
