// Copyright 2022 CeresDB Project Authors. Licensed under Apache-2.0.

//! Config for [Client]

use std::str::FromStr;

use common_util::config::ReadableDuration;
use serde_derive::Deserialize;

#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    pub connect_timeout: ReadableDuration,
    pub channel_pool_max_size: usize,
    pub channel_keep_alive_while_idle: bool,
    pub channel_keep_alive_timeout: ReadableDuration,
    pub channel_keep_alive_interval: ReadableDuration,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            connect_timeout: ReadableDuration::from_str("3s").unwrap(),
            channel_pool_max_size: 128,
            channel_keep_alive_interval: ReadableDuration::from_str("600s").unwrap(),
            channel_keep_alive_timeout: ReadableDuration::from_str("3s").unwrap(),
            channel_keep_alive_while_idle: true,
        }
    }
}
