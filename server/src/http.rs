// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Http service

use std::{
    collections::HashMap, convert::Infallible, error::Error as StdError, net::IpAddr, sync::Arc,
};

use log::error;
use logger::RuntimeLevel;
use profile::Profiler;
use query_engine::executor::Executor as QueryExecutor;
use router::endpoint::Endpoint;
use serde_derive::Serialize;
use snafu::{Backtrace, OptionExt, ResultExt, Snafu};
use table_engine::{engine::EngineRuntimes, table::FlushRequest};
use tokio::sync::oneshot::{self, Sender};
use warp::{
    header,
    http::StatusCode,
    reject,
    reply::{self, Reply},
    Filter,
};

use crate::{
    consts,
    context::RequestContext,
    error_util,
    handlers::{self, sql::Request},
    instance::InstanceRef,
    metrics,
};

#[derive(Debug, Snafu)]
pub enum Error {
    #[snafu(display("Failed to create request context, err:{}", source))]
    CreateContext { source: crate::context::Error },

    #[snafu(display("Failed to handle request, err:{}", source))]
    HandleRequest {
        source: Box<crate::handlers::error::Error>,
    },

    #[snafu(display("Failed to handle update log level, err:{}", msg))]
    HandleUpdateLogLevel { msg: String },

    #[snafu(display("Missing engine runtimes to build service.\nBacktrace:\n{}", backtrace))]
    MissingEngineRuntimes { backtrace: Backtrace },

    #[snafu(display("Missing log runtime to build service.\nBacktrace:\n{}", backtrace))]
    MissingLogRuntime { backtrace: Backtrace },

    #[snafu(display("Missing instance to build service.\nBacktrace:\n{}", backtrace))]
    MissingInstance { backtrace: Backtrace },

    #[snafu(display(
        "Fail to do heap profiling, err:{}.\nBacktrace:\n{}",
        source,
        backtrace
    ))]
    ProfileHeap {
        source: profile::Error,
        backtrace: Backtrace,
    },

    #[snafu(display("Fail to join async task, err:{}.", source))]
    JoinAsyncTask { source: common_util::runtime::Error },

    #[snafu(display(
        "Failed to parse ip addr, ip:{}, err:{}.\nBacktrace:\n{}",
        ip,
        source,
        backtrace
    ))]
    ParseIpAddr {
        ip: String,
        source: std::net::AddrParseError,
        backtrace: Backtrace,
    },

    #[snafu(display("Internal err:{}.", source))]
    Internal {
        source: Box<dyn StdError + Send + Sync>,
    },
}

define_result!(Error);

impl reject::Reject for Error {}

pub const DEFAULT_MAX_BODY_SIZE: u64 = 64 * 1024;

/// Http service
///
/// Note that the service does not owns the runtime
pub struct Service<Q> {
    engine_runtimes: Arc<EngineRuntimes>,
    log_runtime: Arc<RuntimeLevel>,
    instance: InstanceRef<Q>,
    profiler: Arc<Profiler>,
    tx: Sender<()>,
    config: HttpConfig,
}

impl<Q> Service<Q> {
    // TODO(yingwen): Maybe log error or return error
    pub fn stop(self) {
        let _ = self.tx.send(());
    }
}

impl<Q: QueryExecutor + 'static> Service<Q> {
    fn routes(&self) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        self.home()
            .or(self.metrics())
            .or(self.sql())
            .or(self.heap_profile())
            .or(self.admin_block())
            .or(self.flush_memtable())
            .or(self.update_log_level())
    }

    fn home(&self) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        warp::path::end().and(warp::get()).map(|| {
            let mut resp = HashMap::new();
            resp.insert("status", "ok");
            reply::json(&resp)
        })
    }

    // TODO(yingwen): Avoid boilterplate code if there are more handlers
    fn sql(&self) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        // accept json or plain text
        let extract_request = warp::body::json()
            .or(warp::body::bytes().map(Request::from))
            .unify();

        warp::path!("sql")
            .and(warp::post())
            .and(warp::body::content_length_limit(self.config.max_body_size))
            .and(extract_request)
            .and(self.with_context())
            .and(self.with_instance())
            .and_then(|req, ctx, instance| async move {
                let result = handlers::sql::handle_sql(ctx, instance, req)
                    .await
                    .map_err(|e| {
                        // TODO(yingwen): Maybe truncate and print the sql
                        error!("Http service Failed to handle sql, err:{}", e);
                        Box::new(e)
                    })
                    .context(HandleRequest);
                match result {
                    Ok(res) => Ok(reply::json(&res)),
                    Err(e) => Err(reject::custom(e)),
                }
            })
    }

    fn flush_memtable(
        &self,
    ) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        warp::path!("flush_memtable")
            .and(warp::post())
            .and(self.with_instance())
            .and_then(|instance: InstanceRef<Q>| async move {
                let get_all_tables = || {
                    let mut tables = Vec::new();
                    for catalog in instance
                        .catalog_manager
                        .all_catalogs()
                        .map_err(|e| Box::new(e) as _)
                        .context(Internal)?
                    {
                        for schema in catalog
                            .all_schemas()
                            .map_err(|e| Box::new(e) as _)
                            .context(Internal)?
                        {
                            for table in schema
                                .all_tables()
                                .map_err(|e| Box::new(e) as _)
                                .context(Internal)?
                            {
                                tables.push(table);
                            }
                        }
                    }
                    Result::Ok(tables)
                };
                match get_all_tables() {
                    Ok(tables) => {
                        let mut failed_tables = Vec::new();
                        let mut success_tables = Vec::new();

                        for table in tables {
                            let table_name = table.name().to_string();
                            if let Err(e) = table.flush(FlushRequest::default()).await {
                                error!("flush {} failed, err:{}", &table_name, e);
                                failed_tables.push(table_name);
                            } else {
                                success_tables.push(table_name);
                            }
                        }
                        let mut result = HashMap::new();
                        result.insert("success", success_tables);
                        result.insert("failed", failed_tables);
                        Ok(reply::json(&result))
                    }
                    Err(e) => Err(reject::custom(e)),
                }
            })
    }

    fn metrics(&self) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        warp::path!("metrics").and(warp::get()).map(metrics::dump)
    }

    fn heap_profile(
        &self,
    ) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        warp::path!("debug" / "heap_profile" / ..)
            .and(warp::path::param::<u64>())
            .and(warp::get())
            .and(self.with_context())
            .and(self.with_profiler())
            .and_then(
                |duration_sec: u64, ctx: RequestContext, profiler: Arc<Profiler>| async move {
                    let handle = ctx.runtime.spawn_blocking(move || {
                        profiler.dump_mem_prof(duration_sec).context(ProfileHeap)
                    });
                    let result = handle.await.context(JoinAsyncTask);
                    match result {
                        Ok(Ok(prof_data)) => Ok(prof_data.into_response()),
                        Ok(Err(e)) => Err(reject::custom(e)),
                        Err(e) => Err(reject::custom(e)),
                    }
                },
            )
    }

    fn update_log_level(
        &self,
    ) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        warp::path!("log_level" / String)
            .and(warp::put())
            .and(self.with_log_runtime())
            .and_then(
                |log_level: String, log_runtime: Arc<RuntimeLevel>| async move {
                    let result = log_runtime
                        .set_level_by_str(log_level.as_str())
                        .map_err(|e| Error::HandleUpdateLogLevel { msg: e });
                    match result {
                        Ok(()) => Ok(reply::json(&log_level)),
                        Err(e) => Err(reject::custom(e)),
                    }
                },
            )
    }

    fn with_context(
        &self,
    ) -> impl Filter<Extract = (RequestContext,), Error = warp::Rejection> + Clone {
        let default_catalog = self
            .instance
            .catalog_manager
            .default_catalog_name()
            .to_string();
        let default_schema = self
            .instance
            .catalog_manager
            .default_schema_name()
            .to_string();
        //TODO(boyan) use read/write runtime by sql type.
        let runtime = self.engine_runtimes.bg_runtime.clone();

        header::optional::<String>(consts::CATALOG_HEADER)
            .and(header::optional::<String>(consts::TENANT_HEADER))
            .and_then(move |catalog: Option<_>, tenant: Option<_>| {
                // Clone the captured variables
                let default_catalog = default_catalog.clone();
                let default_schema = default_schema.clone();
                let runtime = runtime.clone();
                async {
                    RequestContext::builder()
                        .catalog(catalog.unwrap_or(default_catalog))
                        .tenant(tenant.unwrap_or(default_schema))
                        .runtime(runtime)
                        .build()
                        .context(CreateContext)
                        .map_err(reject::custom)
                }
            })
    }

    fn with_profiler(&self) -> impl Filter<Extract = (Arc<Profiler>,), Error = Infallible> + Clone {
        let profiler = self.profiler.clone();
        warp::any().map(move || profiler.clone())
    }

    fn with_instance(
        &self,
    ) -> impl Filter<Extract = (InstanceRef<Q>,), Error = Infallible> + Clone {
        let instance = self.instance.clone();
        warp::any().map(move || instance.clone())
    }

    fn with_log_runtime(
        &self,
    ) -> impl Filter<Extract = (Arc<RuntimeLevel>,), Error = Infallible> + Clone {
        let log_runtime = self.log_runtime.clone();
        warp::any().map(move || log_runtime.clone())
    }

    fn admin_block(
        &self,
    ) -> impl Filter<Extract = impl warp::Reply, Error = warp::Rejection> + Clone {
        warp::path!("block")
            .and(warp::post())
            .and(warp::body::json())
            .and(self.with_context())
            .and(self.with_instance())
            .and_then(|req, ctx, instance| async {
                let result = handlers::admin::handle_block(ctx, instance, req)
                    .await
                    .map_err(|e| {
                        error!("Http service failed to handle admin block, err:{}", e);
                        Box::new(e)
                    })
                    .context(HandleRequest);

                match result {
                    Ok(res) => Ok(reply::json(&res)),
                    Err(e) => Err(reject::custom(e)),
                }
            })
    }
}

/// Service builder
pub struct Builder<Q> {
    config: HttpConfig,
    engine_runtimes: Option<Arc<EngineRuntimes>>,
    log_runtime: Option<Arc<RuntimeLevel>>,
    instance: Option<InstanceRef<Q>>,
}

impl<Q> Builder<Q> {
    pub fn new(config: HttpConfig) -> Self {
        Self {
            config,
            engine_runtimes: None,
            log_runtime: None,
            instance: None,
        }
    }

    pub fn engine_runtimes(mut self, engine_runtimes: Arc<EngineRuntimes>) -> Self {
        self.engine_runtimes = Some(engine_runtimes);
        self
    }

    pub fn log_runtime(mut self, log_runtime: Arc<RuntimeLevel>) -> Self {
        self.log_runtime = Some(log_runtime);
        self
    }

    pub fn instance(mut self, instance: InstanceRef<Q>) -> Self {
        self.instance = Some(instance);
        self
    }
}

impl<Q: QueryExecutor + 'static> Builder<Q> {
    /// Build and start the service
    pub fn build(self) -> Result<Service<Q>> {
        let engine_runtime = self.engine_runtimes.context(MissingEngineRuntimes)?;
        let log_runtime = self.log_runtime.context(MissingLogRuntime)?;
        let instance = self.instance.context(MissingInstance)?;
        let (tx, rx) = oneshot::channel();

        let service = Service {
            engine_runtimes: engine_runtime.clone(),
            log_runtime,
            instance,
            profiler: Arc::new(Profiler::default()),
            tx,
            config: self.config.clone(),
        };

        let ip_addr: IpAddr = self.config.endpoint.addr.parse().context(ParseIpAddr {
            ip: self.config.endpoint.addr,
        })?;

        // Register filters to warp and rejection handler
        let routes = service.routes().recover(handle_rejection);
        let (_addr, server) = warp::serve(routes).bind_with_graceful_shutdown(
            (ip_addr, self.config.endpoint.port),
            async {
                rx.await.ok();
            },
        );
        // Run the service
        engine_runtime.bg_runtime.spawn(server);

        Ok(service)
    }
}

/// Http service config
#[derive(Debug, Clone)]
pub struct HttpConfig {
    pub endpoint: Endpoint,
    pub max_body_size: u64,
}

#[derive(Debug, Serialize)]
struct ErrorResponse {
    code: u16,
    message: String,
}

fn error_to_status_code(err: &Error) -> StatusCode {
    match err {
        Error::CreateContext { .. } => StatusCode::BAD_REQUEST,
        // TODO(yingwen): Map handle request error to more accurate status code
        Error::HandleRequest { .. }
        | Error::MissingEngineRuntimes { .. }
        | Error::MissingLogRuntime { .. }
        | Error::MissingInstance { .. }
        | Error::ParseIpAddr { .. }
        | Error::ProfileHeap { .. }
        | Error::Internal { .. }
        | Error::JoinAsyncTask { .. }
        | Error::HandleUpdateLogLevel { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

async fn handle_rejection(
    rejection: warp::Rejection,
) -> std::result::Result<impl warp::Reply, Infallible> {
    let code;
    let message;

    if rejection.is_not_found() {
        code = StatusCode::NOT_FOUND;
        message = String::from("NOT_FOUND");
    } else if let Some(err) = rejection.find() {
        code = error_to_status_code(err);
        let err_string = err.to_string();
        message = error_util::first_line_in_error(&err_string).to_string();
    } else {
        error!("handle error: {:?}", rejection);
        code = StatusCode::INTERNAL_SERVER_ERROR;
        message = format!("UNKNOWN_ERROR: {:?}", rejection);
    }

    let json = reply::json(&ErrorResponse {
        code: code.as_u16(),
        message,
    });

    Ok(reply::with_status(json, code))
}
