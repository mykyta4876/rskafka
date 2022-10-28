use async_trait::async_trait;
use std::ops::ControlFlow;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{error, info};

use crate::{
    backoff::{Backoff, BackoffConfig, ErrorOrThrottle},
    client::{Error, Result},
    connection::{
        BrokerCache, BrokerConnection, BrokerConnector, MessengerTransport, MetadataLookupMode,
    },
    messenger::RequestError,
    protocol::{
        error::Error as ProtocolError,
        messages::{CreateTopicRequest, CreateTopicsRequest},
        primitives::{Int16, Int32, String_},
    },
    throttle::maybe_throttle,
    validation::ExactlyOne,
};

use super::error::RequestContext;

#[derive(Debug)]
pub struct ControllerClient {
    brokers: Arc<BrokerConnector>,

    backoff_config: BackoffConfig,

    /// Current broker connection if any
    current_broker: Mutex<Option<BrokerConnection>>,
}

impl ControllerClient {
    pub(super) fn new(brokers: Arc<BrokerConnector>) -> Self {
        Self {
            brokers,
            backoff_config: Default::default(),
            current_broker: Mutex::new(None),
        }
    }

    /// Create a topic
    pub async fn create_topic(
        &self,
        name: impl Into<String> + Send,
        num_partitions: i32,
        replication_factor: i16,
        timeout_ms: i32,
    ) -> Result<()> {
        let request = &CreateTopicsRequest {
            topics: vec![CreateTopicRequest {
                name: String_(name.into()),
                num_partitions: Int32(num_partitions),
                replication_factor: Int16(replication_factor),
                assignments: vec![],
                configs: vec![],
                tagged_fields: None,
            }],
            timeout_ms: Int32(timeout_ms),
            validate_only: None,
            tagged_fields: None,
        };

        maybe_retry(&self.backoff_config, self, "create_topic", || async move {
            let broker = self.get().await.map_err(ErrorOrThrottle::Error)?;
            let response = broker
                .request(request)
                .await
                .map_err(|e| ErrorOrThrottle::Error(e.into()))?;

            maybe_throttle(response.throttle_time_ms)?;

            let topic = response
                .topics
                .exactly_one()
                .map_err(|e| ErrorOrThrottle::Error(Error::exactly_one_topic(e)))?;

            match topic.error {
                None => Ok(()),
                Some(protocol_error) => Err(ErrorOrThrottle::Error(Error::ServerError {
                    protocol_error,
                    error_message: topic.error_message.and_then(|s| s.0),
                    request: RequestContext::Topic(topic.name.0),
                    response: None,
                    is_virtual: false,
                })),
            }
        })
        .await?;

        // Refresh the cache now there is definitely a new topic to observe.
        let _ = self.brokers.refresh_metadata().await;

        Ok(())
    }

    /// Retrieve the broker ID of the controller
    async fn get_controller_id(&self) -> Result<i32> {
        // Request an uncached, fresh copy of the metadata.
        let (metadata, _gen) = self
            .brokers
            .request_metadata(MetadataLookupMode::ArbitraryBroker, Some(vec![]))
            .await?;

        let controller_id = metadata
            .controller_id
            .ok_or_else(|| Error::InvalidResponse("Leader is NULL".to_owned()))?
            .0;

        Ok(controller_id)
    }
}

/// Caches the cluster controller broker.
#[async_trait]
impl BrokerCache for &ControllerClient {
    type R = MessengerTransport;
    type E = Error;

    async fn get(&self) -> Result<Arc<Self::R>> {
        let mut current_broker = self.current_broker.lock().await;
        if let Some(broker) = &*current_broker {
            return Ok(Arc::clone(broker));
        }

        info!("Creating new controller broker connection",);

        let controller_id = self.get_controller_id().await?;
        let broker = self.brokers.connect(controller_id).await?.ok_or_else(|| {
            Error::InvalidResponse(format!(
                "Controller {} not found in metadata response",
                controller_id
            ))
        })?;

        *current_broker = Some(Arc::clone(&broker));
        Ok(broker)
    }

    async fn invalidate(&self, reason: &'static str) {
        info!(reason, "Invalidating cached controller broker",);
        self.current_broker.lock().await.take();
    }
}

/// Takes a `request_name` and a function yielding a fallible future
/// and handles certain classes of error
async fn maybe_retry<B, R, F, T>(
    backoff_config: &BackoffConfig,
    broker_cache: B,
    request_name: &str,
    f: R,
) -> Result<T>
where
    B: BrokerCache,
    R: (Fn() -> F) + Send + Sync,
    F: std::future::Future<Output = Result<T, ErrorOrThrottle<Error>>> + Send,
{
    let mut backoff = Backoff::new(backoff_config);

    backoff
        .retry_with_backoff(request_name, || async {
            let error = match f().await {
                Ok(v) => {
                    return ControlFlow::Break(Ok(v));
                }
                Err(ErrorOrThrottle::Throttle(t)) => {
                    return ControlFlow::Continue(ErrorOrThrottle::Throttle(t));
                }
                Err(ErrorOrThrottle::Error(e)) => e,
            };

            match error {
                // broken connection
                Error::Request(RequestError::Poisoned(_) | RequestError::IO(_))
                | Error::Connection(_) => {
                    broker_cache
                        .invalidate("controller client: connection broken")
                        .await
                }

                // our broker is actually not the controller
                Error::ServerError {
                    protocol_error: ProtocolError::NotController,
                    ..
                } => {
                    broker_cache
                        .invalidate("controller client: server error: not controller")
                        .await;
                }

                // fatal
                _ => {
                    error!(
                        e=%error,
                        request_name,
                        "request encountered fatal error",
                    );
                    return ControlFlow::Break(Err(error));
                }
            }
            ControlFlow::Continue(ErrorOrThrottle::Error(error))
        })
        .await
        .map_err(Error::RetryFailed)?
}
