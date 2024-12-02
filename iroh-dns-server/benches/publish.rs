use std::{net::Ipv4Addr, sync::Arc, time::{Duration, Instant}};

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion};
use iroh_dns_server::store::{PacketSource, ZoneStore};
use pkarr::{dns::{rdata::RData, Name, Packet, ResourceRecord}, Keypair, SignedPacket};
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
                b.iter_custom(|iters| {
                    let store = Arc::new(ZoneStore::persistent("/tmp/jkl.db").unwrap());
                    let pool = ThreadPoolBuilder::new()
                        .num_threads(*num_threads)
                        .build()
                        .unwrap();

                    let mut duration = Duration::new(0, 1);

                    for _ in 0..iters {
                        let store = store.clone();
                        let res = pool.install(|| {
                            let res: Duration = (0..128)
                                .into_par_iter()
                                .map_with(store, |store, _val| {
                                    let keypair = Keypair::random();

                                    let mut packet = Packet::new_reply(0);
                                    packet.answers.push(ResourceRecord::new(
                                        Name::new("_derp_region.iroh.").unwrap(),
                                        simple_dns::CLASS::IN,
                                        30,
                                        RData::A(pkarr::dns::rdata::A {
                                            address: Ipv4Addr::new(1, 1, 1, 1).into(),
                                        }),
                                    ));

                                    let signed_packet = SignedPacket::from_packet(&keypair, &packet).unwrap();

                                    let start = Instant::now();
                                    store.insert(signed_packet, PacketSource::PkarrPublish).unwrap();
                                    start.elapsed()
                                })
                                .sum();

                            res
                        });

                        duration += res;
                    }

                    duration
                });
            },
        );
    }
}

criterion_group!(benches, benchmark,);
criterion_main!(benches);
