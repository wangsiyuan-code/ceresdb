// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Error definitions for storage service.

use ceresdbproto::common::ResponseHeader;
use common_util::define_result;
use http::StatusCode;
use snafu::Snafu;

use crate::error_util;

define_result!(Error);

#[derive(Snafu, Debug)]
#[snafu(visibility(pub))]
pub enum Error {
    #[snafu(display("Rpc error, code:{:?}, message:{}", code, msg))]
    ErrNoCause { code: StatusCode, msg: String },

    #[snafu(display("Rpc error, code:{:?}, message:{}, cause:{}", code, msg, source))]
    ErrWithCause {
        code: StatusCode,
        msg: String,
        source: Box<dyn std::error::Error + Send + Sync>,
    },
}

impl Error {
    pub fn code(&self) -> StatusCode {
        match *self {
            Error::ErrNoCause { code, .. } => code,
            Error::ErrWithCause { code, .. } => code,
        }
    }

    /// Get the error message returned to the user.
    pub fn error_message(&self) -> String {
        match self {
            Error::ErrNoCause { msg, .. } => msg.clone(),

            Error::ErrWithCause { msg, source, .. } => {
                let err_string = source.to_string();
                let first_line = error_util::first_line_in_error(&err_string);
                format!("{}. Caused by: {}", msg, first_line)
            }
        }
    }
}

pub fn build_err_header(err: Error) -> ResponseHeader {
    ResponseHeader {
        code: err.code().as_u16() as u32,
        error: err.error_message(),
    }
}

pub fn build_ok_header() -> ResponseHeader {
    ResponseHeader {
        code: StatusCode::OK.as_u16() as u32,
        ..Default::default()
    }
}

impl From<router::Error> for Error {
    fn from(route_err: router::Error) -> Self {
        match &route_err {
            router::Error::RouteNotFound { .. } | router::Error::ShardNotFound { .. } => {
                Error::ErrNoCause {
                    code: StatusCode::NOT_FOUND,
                    msg: route_err.to_string(),
                }
            }
            router::Error::ParseEndpoint { .. }
            | router::Error::OtherWithCause { .. }
            | router::Error::OtherNoCause { .. } => Error::ErrNoCause {
                code: StatusCode::INTERNAL_SERVER_ERROR,
                msg: route_err.to_string(),
            },
        }
    }
}
