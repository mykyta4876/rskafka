//! Building blocks for a more advanced producer chain.
//!
//! This module provides you:
//!
//! - **lingering:** Control how long your data should wait until being submitted.
//! - **aggregation:** Control how much data should be accumulated on the client side.
//! - **transformation:** Map your own data types to [`Record`]s after they have been aggregated.
//!
//! # Data Flow
//!
//! ```text
//!                 +--------------+            +-----------------+
//! ---(MyData)---->|              |            |                 |
//! <-(MyStatus)-o  |     impl     |-(Records)->| PartitionClient |
//!              ║  |  Aggregator  |            |                 |
//! ---(MyData)---->|              |            +-----------------+
//! <-(MyStatus)-o  |              |                     |
//!              ║  |              |                     |
//!      ...     ║  |              |                     |
//!              ║  |              |                     |
//! ---(MyData)---->|              |                     |
//! <-(MyStatus)-o  |              |                     |
//!              ║  +--------------+                     |
//!              ║         |                             |
//!              ║         V                             |
//!              ║  +--------------+                     |
//!              ║  |              |                     |
//!              o==|     impl     |<-(Offsets)----------o
//!                 |    Status-   |
//!                 | Deaggregator |
//!                 |              |
//!                 +--------------+
//! ```
//!
//! # Usage
//!
//! ## [`Record`] Batching
//! This example shows you how you can send [`Record`]s in batches:
//!
//! ```no_run
//! # async fn test() {
//! use rskafka::{
//!     client::{
//!         Client,
//!         producer::{
//!             aggregator::RecordAggregator,
//!             BatchProducerBuilder,
//!         },
//!     },
//!     record::Record,
//! };
//! use time::OffsetDateTime;
//! use std::{
//!     collections::BTreeMap,
//!     sync::Arc,
//!     time::Duration,
//! };
//!
//! // get partition client
//! let connection = "localhost:9093".to_owned();
//! let client = Client::new_plain(vec![connection]).await.unwrap();
//! let partition_client = Arc::new(
//!     client.partition_client("my_topic", 0).await.unwrap()
//! );
//!
//! // construct batch producer
//! let producer = BatchProducerBuilder::new(partition_client)
//!     .with_linger(Duration::from_secs(2))
//!     .build(RecordAggregator::new(
//!         1024,  // maximum bytes
//!     ));
//!
//! // produce data
//! let record = Record {
//!     key: b"".to_vec(),
//!     value: b"hello kafka".to_vec(),
//!     headers: BTreeMap::from([
//!         ("foo".to_owned(), b"bar".to_vec()),
//!     ]),
//!     timestamp: OffsetDateTime::now_utc(),
//! };
//! producer.produce(record.clone()).await.unwrap();
//! # }
//! ```
//!
//! ## Custom Data Types
//! This example demonstrates the usage of a custom data type:
//!
//! ```no_run
//! # async fn test() {
//! use rskafka::{
//!     client::{
//!         Client,
//!         producer::{
//!             aggregator::{
//!                 Aggregator,
//!                 Error as AggError,
//!                 StatusDeaggregator,
//!                 TryPush,
//!             },
//!             BatchProducerBuilder,
//!         },
//!     },
//!     record::Record,
//! };
//! use time::OffsetDateTime;
//! use std::{
//!     collections::BTreeMap,
//!     sync::Arc,
//!     time::Duration,
//! };
//!
//! // This is the custom data type that we want to aggregate
//! struct Payload {
//!     inner: Vec<u8>,
//! }
//!
//! // Define an aggregator
//! #[derive(Default)]
//! struct MyAggregator {
//!     data: Vec<u8>,
//! }
//!
//! impl Aggregator for MyAggregator {
//!     type Input = Payload;
//!     type Tag = ();
//!     type StatusDeaggregator = MyStatusDeagg;
//!
//!     fn try_push(
//!         &mut self,
//!         record: Self::Input,
//!     ) -> Result<TryPush<Self::Input, Self::Tag>, AggError> {
//!         // accumulate up to 1Kb of data
//!         if record.inner.len() + self.data.len() > 1024 {
//!             return Ok(TryPush::NoCapacity(record));
//!         }
//!
//!         let mut record = record;
//!         self.data.append(&mut record.inner);
//!
//!         Ok(TryPush::Aggregated(()))
//!     }
//!
//!     fn flush(&mut self) -> (Vec<Record>, Self::StatusDeaggregator) {
//!         let data = std::mem::take(&mut self.data);
//!         let records = vec![
//!             Record {
//!                 key: b"".to_vec(),
//!                 value: data,
//!                 headers: BTreeMap::from([
//!                     ("foo".to_owned(), b"bar".to_vec()),
//!                 ]),
//!                 timestamp: OffsetDateTime::now_utc(),
//!             },
//!         ];
//!         (
//!             records,
//!             MyStatusDeagg {}
//!         )
//!     }
//! }
//!
//! #[derive(Debug)]
//! struct MyStatusDeagg {}
//!
//! impl StatusDeaggregator for MyStatusDeagg {
//!     type Status = ();
//!     type Tag = ();
//!
//!     fn deaggregate(&self, _input: &[i64], _tag: Self::Tag) -> Result<Self::Status, AggError> {
//!         // don't care about the offsets
//!         Ok(())
//!     }
//! }
//!
//! // get partition client
//! let connection = "localhost:9093".to_owned();
//! let client = Client::new_plain(vec![connection]).await.unwrap();
//! let partition_client = Arc::new(
//!     client.partition_client("my_topic", 0).await.unwrap()
//! );
//!
//! // construct batch producer
//! let producer = BatchProducerBuilder::new(partition_client)
//!     .with_linger(Duration::from_secs(2))
//!     .build(
//!         MyAggregator::default(),
//!     );
//!
//! // produce data
//! let payload = Payload {
//!     inner: b"hello kafka".to_vec(),
//! };
//! producer.produce(payload).await.unwrap();
//! # }
//! ```
use std::sync::Arc;
use std::time::Duration;

use futures::future::BoxFuture;
use futures::{pin_mut, FutureExt};
use thiserror::Error;
use tokio::sync::Mutex;
use tracing::{debug, error, trace};

use crate::client::producer::aggregator::TryPush;
use crate::client::{error::Error as ClientError, partition::PartitionClient};
use crate::record::Record;

pub mod aggregator;
mod broadcast;

#[derive(Debug, Error)]
pub enum Error {
    #[error("Aggregator error: {0}")]
    Aggregator(#[from] aggregator::Error),

    #[error("Client error: {0}")]
    Client(#[from] Arc<ClientError>),

    #[error("Input too large for aggregator")]
    TooLarge,
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Builder for [`BatchProducer`].
#[derive(Debug)]
pub struct BatchProducerBuilder {
    client: Arc<dyn ProducerClient>,

    linger: Duration,
}

impl BatchProducerBuilder {
    /// Build a new `BatchProducer`
    pub fn new(client: Arc<PartitionClient>) -> Self {
        Self::new_with_client(client)
    }

    /// Internal API for creating with any `dyn ProducerClient`
    fn new_with_client(client: Arc<dyn ProducerClient>) -> Self {
        Self {
            client,
            linger: Duration::from_millis(5),
        }
    }

    /// Sets the minimum amount of time to wait for new data before flushing the batch
    pub fn with_linger(self, linger: Duration) -> Self {
        Self { linger, ..self }
    }

    pub fn build<A>(self, aggregator: A) -> BatchProducer<A>
    where
        A: aggregator::Aggregator,
    {
        BatchProducer {
            linger: self.linger,
            client: self.client,
            inner: Mutex::new(ProducerInner {
                aggregator,
                result_slot: Default::default(),
            }),
        }
    }
}

// A trait wrapper to allow mocking
trait ProducerClient: std::fmt::Debug + Send + Sync {
    fn produce(&self, records: Vec<Record>) -> BoxFuture<'_, Result<Vec<i64>, ClientError>>;
}

impl ProducerClient for PartitionClient {
    fn produce(&self, records: Vec<Record>) -> BoxFuture<'_, Result<Vec<i64>, ClientError>> {
        Box::pin(self.produce(records))
    }
}

/// [`BatchProducer`] attempts to aggregate multiple produce requests together
/// using the provided [`Aggregator`].
///
/// It will buffer up records until either the linger time expires or [`Aggregator`]
/// cannot accommodate another record.
///
/// At this point it will flush the [`Aggregator`]
///
/// [`Aggregator`]: aggregator::Aggregator
#[derive(Debug)]
pub struct BatchProducer<A>
where
    A: aggregator::Aggregator,
{
    linger: Duration,

    client: Arc<dyn ProducerClient>,

    inner: Mutex<ProducerInner<A>>,
}

#[derive(Debug)]
struct AggregatedStatus<A>
where
    A: aggregator::Aggregator,
{
    aggregated_status: Vec<i64>,
    status_deagg: <A as aggregator::Aggregator>::StatusDeaggregator,
}

#[derive(Debug)]
struct AggregatedResult<A>
where
    A: aggregator::Aggregator,
{
    inner: Result<Arc<AggregatedStatus<A>>, Arc<ClientError>>,
}

impl<A> AggregatedResult<A>
where
    A: aggregator::Aggregator,
{
    fn extract(&self, tag: A::Tag) -> Result<<A as aggregator::AggregatorStatus>::Status, Error> {
        use self::aggregator::StatusDeaggregator;

        match &self.inner {
            Ok(status) => match status
                .status_deagg
                .deaggregate(&status.aggregated_status, tag)
            {
                Ok(status) => Ok(status),
                Err(e) => Err(Error::Aggregator(e)),
            },
            Err(client_error) => Err(Error::Client(Arc::clone(client_error))),
        }
    }
}

impl<A> Clone for AggregatedResult<A>
where
    A: aggregator::Aggregator,
{
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

#[derive(Debug)]
struct ProducerInner<A>
where
    A: aggregator::Aggregator,
{
    result_slot: broadcast::BroadcastOnce<AggregatedResult<A>>,

    aggregator: A,
}

impl<A> BatchProducer<A>
where
    A: aggregator::Aggregator,
{
    /// Write `data` to this [`BatchProducer`]
    ///
    /// Returns when the data has been committed to Kafka or
    /// an unrecoverable error has been encountered
    ///
    /// # Cancellation
    ///
    /// The returned future is not cancellation safe, if it is dropped the record
    /// may or may not be published
    ///
    pub async fn produce(
        &self,
        data: A::Input,
    ) -> Result<<A as aggregator::AggregatorStatus>::Status> {
        let (result_slot, tag) = {
            // Try to add the record to the aggregator
            let mut inner = self.inner.lock().await;

            let tag = match inner.aggregator.try_push(data)? {
                TryPush::Aggregated(tag) => tag,
                TryPush::NoCapacity(data) => {
                    debug!("Insufficient capacity in aggregator - flushing");

                    Self::flush(&mut inner, self.client.as_ref()).await;
                    match inner.aggregator.try_push(data)? {
                        TryPush::Aggregated(tag) => tag,
                        TryPush::NoCapacity(_) => {
                            error!("Record too large for aggregator");
                            return Err(Error::TooLarge);
                        }
                    }
                }
            };

            // Get a future that completes when the record is published
            (inner.result_slot.receive(), tag)
        };

        let linger = tokio::time::sleep(self.linger).fuse();
        pin_mut!(linger);
        pin_mut!(result_slot);

        futures::select! {
            r = result_slot => return r.extract(tag),
            _ = linger => {}
        }

        // Linger expired - reacquire lock
        let mut inner = self.inner.lock().await;

        // Whilst holding lock - check hasn't been flushed already
        //
        // This covers two scenarios:
        // - the linger expired "simultaneously" with the publish
        // - the linger expired but another thread triggered the flush
        if let Some(r) = result_slot.peek() {
            return r.extract(tag);
        }

        debug!("Linger expired - flushing");

        // Flush data
        Self::flush(&mut inner, self.client.as_ref()).await;

        result_slot
            .now_or_never()
            .expect("just flushed")
            .extract(tag)
    }

    /// Flushes out the data from the aggregator, publishes the result to the result slot,
    /// and creates a fresh result slot for future writes to use
    async fn flush(inner: &mut ProducerInner<A>, client: &dyn ProducerClient) {
        trace!("Flushing batch producer");

        let (output, status_deagg) = inner.aggregator.flush();
        if output.is_empty() {
            return;
        }

        let r = client.produce(output).await;

        // Reset result slot
        let slot = std::mem::take(&mut inner.result_slot);

        let inner = match r {
            Ok(status) => {
                let aggregated_status = AggregatedStatus {
                    aggregated_status: status,
                    status_deagg,
                };
                Ok(Arc::new(aggregated_status))
            }
            Err(e) => Err(Arc::new(e)),
        };

        slot.broadcast(AggregatedResult { inner })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        client::producer::aggregator::RecordAggregator, protocol::error::Error as ProtocolError,
    };
    use futures::stream::FuturesUnordered;
    use futures::StreamExt;
    use time::OffsetDateTime;

    #[derive(Debug)]
    struct MockClient {
        error: Option<ProtocolError>,
        delay: Duration,
        batch_sizes: parking_lot::Mutex<Vec<usize>>,
    }

    impl ProducerClient for MockClient {
        fn produce(&self, records: Vec<Record>) -> BoxFuture<'_, Result<Vec<i64>, ClientError>> {
            Box::pin(async move {
                tokio::time::sleep(self.delay).await;

                if let Some(e) = self.error {
                    return Err(ClientError::ServerError(e, "".to_string()));
                }

                let mut batch_sizes = self.batch_sizes.lock();
                let offset_base = batch_sizes.iter().sum::<usize>();
                let offsets = (0..records.len())
                    .map(|x| (x + offset_base) as i64)
                    .collect();
                batch_sizes.push(records.len());
                Ok(offsets)
            })
        }
    }

    fn record() -> Record {
        Record {
            key: vec![0; 4],
            value: vec![0; 6],
            headers: Default::default(),
            timestamp: OffsetDateTime::from_unix_timestamp(320).unwrap(),
        }
    }

    #[tokio::test]
    async fn test_producer() {
        let record = record();
        let linger = Duration::from_millis(100);

        for delay in [Duration::from_secs(0), Duration::from_millis(1)] {
            let client = Arc::new(MockClient {
                error: None,
                delay,
                batch_sizes: Default::default(),
            });

            let aggregator = RecordAggregator::new(record.approximate_size() * 2);
            let producer = BatchProducerBuilder::new_with_client(Arc::<MockClient>::clone(&client))
                .with_linger(linger)
                .build(aggregator);

            let mut futures = FuturesUnordered::new();

            futures.push(producer.produce(record.clone()));
            futures.push(producer.produce(record.clone()));
            futures.push(producer.produce(record.clone()));

            let assert_ok = |a: Result<Option<Result<_, _>>, _>, expected: i64| {
                let offset = a
                    .expect("no timeout")
                    .expect("Some future left")
                    .expect("no producer error");
                assert_eq!(offset, expected);
            };

            // First two publishes should be ok
            assert_ok(
                tokio::time::timeout(Duration::from_millis(10), futures.next()).await,
                0,
            );
            assert_ok(
                tokio::time::timeout(Duration::from_millis(10), futures.next()).await,
                1,
            );

            // Third should linger
            tokio::time::timeout(Duration::from_millis(10), futures.next())
                .await
                .expect_err("timeout");

            assert_eq!(client.batch_sizes.lock().as_slice(), &[2]);

            // Should publish third record after linger expires
            assert_ok(tokio::time::timeout(linger * 2, futures.next()).await, 2);
            assert_eq!(client.batch_sizes.lock().as_slice(), &[2, 1]);
        }
    }

    #[tokio::test]
    async fn test_producer_error() {
        let record = record();
        let linger = Duration::from_millis(5);
        let client = Arc::new(MockClient {
            error: Some(ProtocolError::NetworkException),
            delay: Duration::from_millis(1),
            batch_sizes: Default::default(),
        });

        let aggregator = RecordAggregator::new(record.approximate_size() * 2);
        let producer = BatchProducerBuilder::new_with_client(Arc::<MockClient>::clone(&client))
            .with_linger(linger)
            .build(aggregator);

        let mut futures = FuturesUnordered::new();
        futures.push(producer.produce(record.clone()));
        futures.push(producer.produce(record.clone()));

        futures.next().await.unwrap().unwrap_err();
        futures.next().await.unwrap().unwrap_err();
    }
}
