// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Forward for grpc services
use std::{
    collections::HashMap,
    net::Ipv4Addr,
    sync::{Arc, RwLock},
    time::Duration,
};

use async_trait::async_trait;
use ceresdbproto::storage::{storage_service_client::StorageServiceClient, RouteRequest};
use log::{debug, error, warn};
use router::{endpoint::Endpoint, RouterRef};
use serde_derive::Deserialize;
use snafu::{ensure, Backtrace, ResultExt, Snafu};
use tonic::{
    metadata::errors::InvalidMetadataValue,
    transport::{self, Channel},
};

use crate::consts::TENANT_HEADER;

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display(
        "Invalid endpoint, endpoint:{}, err:{}.\nBacktrace:\n{}",
        endpoint,
        source,
        backtrace
    ))]
    InvalidEndpoint {
        endpoint: String,
        source: tonic::transport::Error,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Local ip addr should not be loopback, addr:{}.\nBacktrace:\n{}",
        ip_addr,
        backtrace
    ))]
    LoopbackLocalIpAddr {
        ip_addr: String,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Invalid schema, schema:{}, err:{}.\nBacktrace:\n{}",
        schema,
        source,
        backtrace
    ))]
    InvalidSchema {
        schema: String,
        source: InvalidMetadataValue,
        backtrace: Backtrace,
    },

    #[snafu(display(
        "Failed to connect endpoint, endpoint:{}, err:{}.\nBacktrace:\n{}",
        endpoint,
        source,
        backtrace
    ))]
    Connect {
        endpoint: String,
        source: tonic::transport::Error,
        backtrace: Backtrace,
    },
}

define_result!(Error);

pub type ForwarderRef = Arc<Forwarder<DefaultClientBuilder>>;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub enable: bool,
    /// Thread num for grpc polling
    pub thread_num: usize,
    /// -1 means unlimited
    pub max_send_msg_len: i32,
    /// -1 means unlimited
    pub max_recv_msg_len: i32,
    /// Sets an interval for HTTP2 Ping frames should be sent to keep a
    /// connection alive.
    pub keep_alive_interval: Duration,
    /// A timeout for receiving an acknowledgement of the keep-alive ping
    /// If the ping is not acknowledged within the timeout, the connection will
    /// be closed
    pub keep_alive_timeout: Duration,
    /// default keep http2 connections alive while idle
    pub keep_alive_while_idle: bool,
    pub connect_timeout: Duration,
    pub forward_timeout: Duration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            enable: false,
            thread_num: 4,
            // 20MB
            max_send_msg_len: 20 * (1 << 20),
            // 1GB
            max_recv_msg_len: 1 << 30,
            keep_alive_interval: Duration::from_secs(60 * 10),
            keep_alive_timeout: Duration::from_secs(3),
            keep_alive_while_idle: true,
            connect_timeout: Duration::from_secs(3),
            forward_timeout: Duration::from_secs(60),
        }
    }
}

#[async_trait]
pub trait ClientBuilder {
    async fn connect(&self, endpoint: &Endpoint) -> Result<StorageServiceClient<Channel>>;
}

pub struct DefaultClientBuilder {
    config: Config,
}

impl DefaultClientBuilder {
    #[inline]
    fn make_endpoint_with_scheme(endpoint: &Endpoint) -> String {
        format!("http://{}:{}", endpoint.addr, endpoint.port)
    }
}

#[async_trait]
impl ClientBuilder for DefaultClientBuilder {
    async fn connect(&self, endpoint: &Endpoint) -> Result<StorageServiceClient<Channel>> {
        let endpoint_with_scheme = Self::make_endpoint_with_scheme(endpoint);
        let configured_endpoint = transport::Endpoint::from_shared(endpoint_with_scheme.clone())
            .context(InvalidEndpoint {
                endpoint: &endpoint_with_scheme,
            })?;

        let configured_endpoint = match self.config.keep_alive_while_idle {
            true => configured_endpoint
                .connect_timeout(self.config.connect_timeout)
                .keep_alive_timeout(self.config.keep_alive_timeout)
                .keep_alive_while_idle(true)
                .http2_keep_alive_interval(self.config.keep_alive_interval),
            false => configured_endpoint
                .connect_timeout(self.config.connect_timeout)
                .keep_alive_while_idle(false),
        };
        let channel = configured_endpoint.connect().await.context(Connect {
            endpoint: &endpoint_with_scheme,
        })?;

        Ok(StorageServiceClient::new(channel))
    }
}

/// Forwarder does request forwarding.
///
/// No forward happens if the router tells the target endpoint is the same as
/// the local endpoint.
///
/// Assuming client wants to access some table which are located on server1 (the
/// router can tell the location information). Then here is the diagram
/// describing what the forwarder does:
///  peer-to-peer procedure: client --> server1
///  forwarding procedure:   client --> server0 (forwarding server) --> server1
pub struct Forwarder<B> {
    config: Config,
    router: RouterRef,
    local_endpoint: Endpoint,
    client_builder: B,
    clients: RwLock<HashMap<Endpoint, StorageServiceClient<Channel>>>,
}

/// The result of forwarding.
///
/// If no forwarding happens, [`Original`] can be used.
pub enum ForwardResult<Resp, Err> {
    Original,
    Forwarded(std::result::Result<Resp, Err>),
}

#[derive(Debug)]
pub struct ForwardRequest<Req> {
    pub schema: String,
    pub metric: String,
    pub req: tonic::Request<Req>,
}

impl Forwarder<DefaultClientBuilder> {
    pub fn try_new(config: Config, router: RouterRef, local_endpoint: Endpoint) -> Result<Self> {
        let client_builder = DefaultClientBuilder {
            config: config.clone(),
        };

        Self::try_new_with_client_builder(config, router, local_endpoint, client_builder)
    }
}

impl<B> Forwarder<B> {
    #[inline]
    fn is_loopback_ip(ip_addr: &str) -> bool {
        ip_addr
            .parse::<Ipv4Addr>()
            .map(|ip| ip.is_loopback())
            .unwrap_or(false)
    }

    /// Check whether the target endpoint is the same as the local endpoint.
    fn is_local_endpoint(&self, target: &Endpoint) -> bool {
        if &self.local_endpoint == target {
            return true;
        }

        if self.local_endpoint.port != target.port {
            return false;
        }

        // Only need to check the remote is loopback addr.
        Self::is_loopback_ip(&target.addr)
    }

    /// Release the client for the given endpoint.
    fn release_client(&self, endpoint: &Endpoint) -> Option<StorageServiceClient<Channel>> {
        let mut clients = self.clients.write().unwrap();
        clients.remove(endpoint)
    }
}

impl<B: ClientBuilder> Forwarder<B> {
    pub fn try_new_with_client_builder(
        config: Config,
        router: RouterRef,
        local_endpoint: Endpoint,
        client_builder: B,
    ) -> Result<Self> {
        let loopback_local_endpoint = Self::is_loopback_ip(&local_endpoint.addr);
        ensure!(
            !loopback_local_endpoint,
            LoopbackLocalIpAddr {
                ip_addr: &local_endpoint.addr,
            }
        );

        Ok(Self {
            config,
            local_endpoint,
            router,
            clients: RwLock::new(HashMap::new()),
            client_builder,
        })
    }

    /// Forward the request according to the configured router.
    ///
    /// Error will be thrown if it happens in the forwarding procedure, that is
    /// to say, some errors like the output from the `do_rpc` will be
    /// wrapped in the [`ForwardResult::Forwarded`].
    pub async fn forward<Req, Resp, Err, F>(
        &self,
        forward_req: ForwardRequest<Req>,
        do_rpc: F,
    ) -> Result<ForwardResult<Resp, Err>>
    where
        F: FnOnce(
            StorageServiceClient<Channel>,
            tonic::Request<Req>,
            &Endpoint,
        ) -> Box<
            dyn std::future::Future<Output = std::result::Result<Resp, Err>> + Send + Unpin,
        >,
        Req: std::fmt::Debug + Clone,
    {
        if !self.config.enable {
            return Ok(ForwardResult::Original);
        }

        let ForwardRequest {
            schema,
            metric,
            mut req,
        } = forward_req;

        let route_req = RouteRequest {
            metrics: vec![metric],
        };

        let endpoint = match self.router.route(&schema, route_req).await {
            Ok(mut routes) => {
                if routes.len() != 1 || routes[0].endpoint.is_none() {
                    warn!(
                        "Fail to forward request for multiple route results, routes result:{:?}, req:{:?}",
                        routes, req
                    );
                    return Ok(ForwardResult::Original);
                }

                Endpoint::from(routes.remove(0).endpoint.unwrap())
            }
            Err(e) => {
                error!("Fail to route request, req:{:?}, err:{}", req, e);
                return Ok(ForwardResult::Original);
            }
        };

        if self.is_local_endpoint(&endpoint) {
            return Ok(ForwardResult::Original);
        }

        // Update the request.
        {
            // TODO: we should use the timeout from the original request.
            req.set_timeout(self.config.forward_timeout);
            let metadata = req.metadata_mut();
            metadata.insert(
                TENANT_HEADER,
                schema.parse().context(InvalidSchema { schema })?,
            );
        }

        // TODO: add metrics to record the forwarding.
        debug!(
            "Try to forward request to {:?}, request:{:?}",
            endpoint, req,
        );
        let client = self.get_or_create_client(&endpoint).await?;
        match do_rpc(client, req, &endpoint).await {
            Err(e) => {
                // Release the grpc client for the error doesn't belong to the normal error.
                self.release_client(&endpoint);
                Ok(ForwardResult::Forwarded(Err(e)))
            }
            Ok(resp) => Ok(ForwardResult::Forwarded(Ok(resp))),
        }
    }

    async fn get_or_create_client(
        &self,
        endpoint: &Endpoint,
    ) -> Result<StorageServiceClient<Channel>> {
        {
            let clients = self.clients.read().unwrap();
            if let Some(v) = clients.get(endpoint) {
                return Ok(v.clone());
            }
        }

        let new_client = self.client_builder.connect(endpoint).await?;
        {
            let mut clients = self.clients.write().unwrap();
            if let Some(v) = clients.get(endpoint) {
                return Ok(v.clone());
            }
            clients.insert(endpoint.clone(), new_client.clone());
        }

        Ok(new_client)
    }
}

#[cfg(test)]
mod tests {
    use ceresdbproto::storage::{QueryRequest, QueryResponse, Route};
    use futures::FutureExt;
    use router::Router;
    use tonic::IntoRequest;

    use super::*;

    #[test]
    fn test_check_loopback_endpoint() {
        let loopback_ips = vec!["127.0.0.1", "127.0.0.2"];
        for loopback_ip in loopback_ips {
            assert!(Forwarder::<DefaultClientBuilder>::is_loopback_ip(
                loopback_ip
            ));
        }

        let normal_ips = vec!["10.100.10.14", "192.168.1.2", "0.0.0.0"];
        for ip in normal_ips {
            assert!(!Forwarder::<DefaultClientBuilder>::is_loopback_ip(ip));
        }

        let invalid_addrs = vec!["hello.world.com", "test", "localhost", ""];
        for ip in invalid_addrs {
            assert!(!Forwarder::<DefaultClientBuilder>::is_loopback_ip(ip));
        }
    }

    struct MockRouter {
        routing_tables: HashMap<String, Endpoint>,
    }

    #[async_trait]
    impl Router for MockRouter {
        async fn route(&self, _schema: &str, req: RouteRequest) -> router::Result<Vec<Route>> {
            let endpoint = self.routing_tables.get(&req.metrics[0]);
            match endpoint {
                None => Ok(vec![]),
                Some(v) => Ok(vec![Route {
                    metric: req.metrics[0].clone(),
                    endpoint: Some(v.clone().into()),
                    ext: vec![],
                }]),
            }
        }
    }

    struct MockClientBuilder;

    #[async_trait]
    impl ClientBuilder for MockClientBuilder {
        async fn connect(&self, _: &Endpoint) -> Result<StorageServiceClient<Channel>> {
            let (channel, _) = Channel::balance_channel::<usize>(10);
            Ok(StorageServiceClient::<Channel>::new(channel))
        }
    }

    #[tokio::test]
    async fn test_normal_forward() {
        let config = Config {
            enable: true,
            ..Default::default()
        };

        let mut mock_router = MockRouter {
            routing_tables: HashMap::new(),
        };
        let test_metric0: &str = "test_metric0";
        let test_metric1: &str = "test_metric1";
        let test_metric2: &str = "test_metric2";
        let test_metric3: &str = "test_metric3";
        let test_endpoint0 = Endpoint::new("192.168.1.12".to_string(), 8831);
        let test_endpoint1 = Endpoint::new("192.168.1.2".to_string(), 8831);
        let test_endpoint2 = Endpoint::new("192.168.1.2".to_string(), 8832);
        let test_endpoint3 = Endpoint::new("192.168.1.1".to_string(), 8831);
        mock_router
            .routing_tables
            .insert(test_metric0.to_string(), test_endpoint0.clone());
        mock_router
            .routing_tables
            .insert(test_metric1.to_string(), test_endpoint1.clone());
        mock_router
            .routing_tables
            .insert(test_metric2.to_string(), test_endpoint2.clone());
        mock_router
            .routing_tables
            .insert(test_metric3.to_string(), test_endpoint3.clone());
        let mock_router = Arc::new(mock_router);

        let local_endpoint = test_endpoint3.clone();
        let forwarder = Forwarder::try_new_with_client_builder(
            config,
            mock_router.clone() as _,
            local_endpoint.clone(),
            MockClientBuilder,
        )
        .unwrap();

        let make_forward_req = |metric: &str| {
            let query_request = QueryRequest {
                metrics: vec![metric.to_string()],
                ql: "".to_string(),
            };
            ForwardRequest {
                schema: "public".to_string(),
                metric: metric.to_string(),
                req: query_request.into_request(),
            }
        };

        let do_rpc = |_client, req: tonic::Request<QueryRequest>, endpoint: &Endpoint| {
            let tenant = req.metadata().get(TENANT_HEADER).unwrap().to_str().unwrap();
            assert_eq!(tenant, "public");
            let req = req.into_inner();
            let expect_endpoint = mock_router.routing_tables.get(&req.metrics[0]).unwrap();
            assert_eq!(expect_endpoint, endpoint);

            let resp = QueryResponse::default();
            Box::new(async move { Ok(resp) }.boxed()) as _
        };

        for test_metric in [test_metric0, test_metric1, test_metric2, test_metric3] {
            let endpoint = mock_router.routing_tables.get(test_metric).unwrap();
            let forward_req = make_forward_req(test_metric);
            let res: Result<ForwardResult<QueryResponse, Error>> =
                forwarder.forward(forward_req, do_rpc).await;
            let forward_res = res.expect("should succeed in forwarding");
            if endpoint == &local_endpoint {
                assert!(forwarder.is_local_endpoint(endpoint));
                assert!(
                    matches!(forward_res, ForwardResult::Original),
                    "endpoint is:{:?}",
                    endpoint
                );
            } else {
                assert!(!forwarder.is_local_endpoint(endpoint));
                assert!(
                    matches!(forward_res, ForwardResult::Forwarded(_)),
                    "endpoint is:{:?}",
                    endpoint
                );
            }
        }
    }
}
