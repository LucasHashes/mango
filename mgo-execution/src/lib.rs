// Copyright (c) MangoNet Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

// DO NOT MODIFY, Generated by ./scripts/execution-layer

use std::path::PathBuf;
use std::sync::Arc;

use mgo_protocol_config::ProtocolConfig;
use mgo_types::{error::MgoResult, metrics::BytecodeVerifierMetrics};

pub use executor::Executor;
pub use verifier::Verifier;

pub mod executor;
pub mod verifier;

mod latest;
mod next_vm;
mod v0;
mod v1;

#[cfg(test)]
mod tests;

pub const NEXT_VM: u64 = u64::MAX;
pub fn executor(
    protocol_config: &ProtocolConfig,
    silent: bool,
    enable_profiler: Option<PathBuf>,
) -> MgoResult<Arc<dyn Executor + Send + Sync>> {
    let version = protocol_config.execution_version_as_option().unwrap_or(0);
    Ok(match version {
        0 => Arc::new(v0::Executor::new(protocol_config, silent, enable_profiler)?),

        1 => Arc::new(v1::Executor::new(protocol_config, silent, enable_profiler)?),

        2 => Arc::new(latest::Executor::new(
            protocol_config,
            silent,
            enable_profiler,
        )?),

        NEXT_VM => Arc::new(next_vm::Executor::new(
            protocol_config,
            silent,
            enable_profiler,
        )?),

        v => panic!("Unsupported execution version {v}"),
    })
}

pub fn verifier<'m>(
    protocol_config: &ProtocolConfig,
    is_metered: bool,
    metrics: &'m Arc<BytecodeVerifierMetrics>,
) -> Box<dyn Verifier + 'm> {
    let version = protocol_config.execution_version_as_option().unwrap_or(0);
    match version {
        0 => Box::new(v0::Verifier::new(protocol_config, is_metered, metrics)),
        1 => Box::new(v1::Verifier::new(protocol_config, is_metered, metrics)),
        2 => Box::new(latest::Verifier::new(protocol_config, is_metered, metrics)),
        NEXT_VM => Box::new(next_vm::Verifier::new(protocol_config, is_metered, metrics)),
        v => panic!("Unsupported execution version {v}"),
    }
}