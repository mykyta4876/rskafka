use std::{
    collections::BTreeMap,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::Poll,
    time::{Duration, Instant},
};

use criterion::{
    criterion_group, criterion_main, measurement::WallTime, BenchmarkGroup, Criterion, SamplingMode,
};
use futures::{stream::FuturesUnordered, StreamExt};
use pin_project_lite::pin_project;
use rdkafka::{
    producer::{FutureProducer, FutureRecord},
    util::Timeout,
};
use rskafka::{
    client::{
        partition::{Compression, PartitionClient},
        producer::{aggregator::RecordAggregator, BatchProducerBuilder},
        ClientBuilder,
    },
    record::Record,
};
use time::OffsetDateTime;
use tokio::runtime::Runtime;

const PARALLEL_BATCH_SIZE: usize = 1_000_000;
const PARALLEL_LINGER_MS: u64 = 10;

pub fn criterion_benchmark(c: &mut Criterion) {
    let connection = maybe_skip_kafka_integration!();

    let record = Record {
        key: Some(vec![b'k'; 10]),
        value: Some(vec![b'x'; 10_000]),
        headers: BTreeMap::default(),
        timestamp: OffsetDateTime::now_utc(),
    };

    {
        let mut group_sequential = benchark_group(c, "sequential");

        group_sequential.bench_function("rdkafka", |b| {
            b.to_async(runtime()).iter_custom(|iters| {
                let connection = connection.clone();
                let record = record.clone();

                async move {
                    let (client, topic) = setup_rdkafka(connection, false).await;

                    exec_sequential(
                        || async {
                            let f_record = record.to_rdkafka(&topic);
                            client.send(f_record, Timeout::Never).await.unwrap();
                        },
                        iters,
                    )
                    .time_it()
                    .await
                }
            });
        });

        group_sequential.bench_function("rskafka", |b| {
            b.to_async(runtime()).iter_custom(|iters| {
                let connection = connection.clone();
                let record = record.clone();

                async move {
                    let client = setup_rskafka(connection).await;

                    exec_sequential(
                        || async {
                            client
                                .produce(vec![record.clone()], Compression::NoCompression)
                                .await
                                .unwrap();
                        },
                        iters,
                    )
                    .time_it()
                    .await
                }
            });
        });
    }

    {
        let mut group_parallel = benchark_group(c, "parallel");

        group_parallel.bench_function("rdkafka", |b| {
            b.to_async(runtime()).iter_custom(|iters| {
                let connection = connection.clone();
                let record = record.clone();

                async move {
                    let (client, topic) = setup_rdkafka(connection, true).await;

                    exec_parallel(
                        || async {
                            let f_record = record.to_rdkafka(&topic);
                            client.send(f_record, Timeout::Never).await.unwrap();
                        },
                        iters,
                    )
                    .time_it()
                    .await
                }
            });
        });

        group_parallel.bench_function("rskafka", |b| {
            b.to_async(runtime()).iter_custom(|iters| {
                let connection = connection.clone();
                let record = record.clone();

                async move {
                    let client = setup_rskafka(connection).await;
                    let producer = BatchProducerBuilder::new(Arc::new(client))
                        .with_linger(Duration::from_millis(PARALLEL_LINGER_MS))
                        .build(RecordAggregator::new(PARALLEL_BATCH_SIZE));

                    exec_parallel(
                        || async {
                            producer.produce(record.clone()).await.unwrap();
                        },
                        iters,
                    )
                    .time_it()
                    .await
                }
            });
        });
    }
}

async fn exec_sequential<F, Fut>(f: F, iters: u64)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    for _ in 0..iters {
        f().await;
    }
}

async fn exec_parallel<F, Fut>(f: F, iters: u64)
where
    F: Fn() -> Fut,
    Fut: Future<Output = ()>,
{
    let mut tasks: FuturesUnordered<_> = (0..iters).map(|_| f()).collect();
    while tasks.next().await.is_some() {}
}

/// "Time it" extension for futures.
trait FutureTimeItExt {
    type TimeItFut: Future<Output = Duration>;

    /// Measures time it takes to execute given async block once
    fn time_it(self) -> Self::TimeItFut;
}

impl<F> FutureTimeItExt for F
where
    F: Future<Output = ()>,
{
    type TimeItFut = TimeIt<F>;

    fn time_it(self) -> Self::TimeItFut {
        TimeIt {
            t_start: Instant::now(),
            inner: self,
        }
    }
}

pin_project! {
    struct TimeIt<F> {
        t_start: Instant,
        #[pin]
        inner: F,
    }
}

impl<F> Future for TimeIt<F>
where
    F: Future<Output = ()>,
{
    type Output = Duration;

    fn poll(self: Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.project();
        match this.inner.poll(cx) {
            Poll::Ready(_) => Poll::Ready(this.t_start.elapsed()),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Extension to convert rdkafka to rskafka records.
trait RecordExt {
    fn to_rdkafka<'a>(&'a self, topic: &'a str) -> FutureRecord<'a, Vec<u8>, Vec<u8>>;
}

impl RecordExt for Record {
    fn to_rdkafka<'a>(&'a self, topic: &'a str) -> FutureRecord<'a, Vec<u8>, Vec<u8>> {
        let mut record = FutureRecord::to(topic);
        if let Some(key) = self.key.as_ref() {
            record = record.key(key);
        }
        if let Some(value) = self.value.as_ref() {
            record = record.payload(value);
        }
        record
    }
}

/// Get the testing Kafka connection string or return current scope.
///
/// If `TEST_INTEGRATION` and `KAFKA_CONNECT` are set, return the Kafka connection URL to the
/// caller.
///
/// If `TEST_INTEGRATION` is set but `KAFKA_CONNECT` is not set, fail the tests and provide
/// guidance for setting `KAFKA_CONNECTION`.
///
/// If `TEST_INTEGRATION` is not set, skip the calling test by returning early.
#[macro_export]
macro_rules! maybe_skip_kafka_integration {
    () => {{
        use std::env;
        dotenv::dotenv().ok();

        match (
            env::var("TEST_INTEGRATION").is_ok(),
            env::var("KAFKA_CONNECT").ok(),
        ) {
            (true, Some(kafka_connection)) => {
                let kafka_connection: Vec<String> =
                    kafka_connection.split(",").map(|s| s.to_owned()).collect();
                kafka_connection
            }
            (true, None) => {
                panic!(
                    "TEST_INTEGRATION is set which requires running integration tests, but \
                    KAFKA_CONNECT is not set. Please run Kafka or Redpanda then \
                    set KAFKA_CONNECT as directed in README.md."
                )
            }
            (false, Some(_)) => {
                eprintln!("skipping Kafka integration tests - set TEST_INTEGRATION to run");
                return;
            }
            (false, None) => {
                eprintln!(
                    "skipping Kafka integration tests - set TEST_INTEGRATION and KAFKA_CONNECT to \
                    run"
                );
                return;
            }
        }
    }};
}

fn benchark_group<'a>(c: &'a mut Criterion, name: &str) -> BenchmarkGroup<'a, WallTime> {
    let mut group = c.benchmark_group(name);
    group.measurement_time(Duration::from_secs(60));
    group.sample_size(15);
    group.sampling_mode(SamplingMode::Linear);
    group
}

fn runtime() -> Runtime {
    tokio::runtime::Builder::new_multi_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap()
}

/// Generated random topic name for testing.
fn random_topic_name() -> String {
    format!("test_topic_{}", uuid::Uuid::new_v4())
}

async fn setup_rdkafka(connection: Vec<String>, buffering: bool) -> (FutureProducer, String) {
    use rdkafka::{
        admin::{AdminClient, AdminOptions, NewTopic, TopicReplication},
        ClientConfig,
    };

    let topic_name = random_topic_name();

    // configure clients
    let mut cfg = ClientConfig::new();
    cfg.set("bootstrap.servers", connection.join(","));
    cfg.set("message.timeout.ms", "5000");
    if buffering {
        cfg.set("batch.num.messages", PARALLEL_BATCH_SIZE.to_string()); // = loads
        cfg.set("batch.size", 1_000_000.to_string());
        cfg.set("queue.buffering.max.ms", PARALLEL_LINGER_MS.to_string());
    } else {
        cfg.set("batch.num.messages", "1");
        cfg.set("queue.buffering.max.ms", "0");
    }

    // create topic
    let admin_client: AdminClient<_> = cfg.create().unwrap();
    let topic = NewTopic::new(&topic_name, 1, TopicReplication::Fixed(1));
    let opts = AdminOptions::default();
    let mut results = admin_client.create_topics([&topic], &opts).await.unwrap();
    assert_eq!(results.len(), 1, "created exactly one topic");
    let result = results.pop().expect("just checked the vector length");
    result.unwrap();

    let producer_client: FutureProducer = cfg.create().unwrap();

    // warm up connection
    let key = vec![b'k'; 1];
    let payload = vec![b'x'; 10];
    let f_record = FutureRecord::to(&topic_name).key(&key).payload(&payload);
    producer_client
        .send(f_record, Timeout::Never)
        .await
        .unwrap();

    (producer_client, topic_name)
}

async fn setup_rskafka(connection: Vec<String>) -> PartitionClient {
    let topic_name = random_topic_name();

    let client = ClientBuilder::new(connection).build().await.unwrap();
    client
        .controller_client()
        .unwrap()
        .create_topic(topic_name.clone(), 1, 1, 5_000)
        .await
        .unwrap();

    client.partition_client(topic_name, 0).unwrap()
}

criterion_group!(benches, criterion_benchmark);
criterion_main!(benches);
