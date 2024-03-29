// Copyright (c) MangoNet Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use clap::Parser;
use move_cli::base::{
    self,
    test::{self, UnitTestResult},
};
use move_package::BuildConfig;
use move_unit_test::{extensions::set_extension_hook, UnitTestingConfig};
use move_vm_runtime::native_extensions::NativeContextExtensions;
use once_cell::sync::Lazy;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use mgo_move_build::decorate_warnings;
use mgo_move_natives::{object_runtime::ObjectRuntime, NativesCostTable};
use mgo_protocol_config::ProtocolConfig;
use mgo_types::{
    base_types::{ObjectID, SequenceNumber},
    error::MgoResult,
    gas_model::tables::initial_cost_schedule_for_unit_tests,
    metrics::LimitsMetrics,
    object::Object,
    storage::ChildObjectResolver,
};

// Move unit tests will halt after executing this many steps. This is a protection to avoid divergence
const MAX_UNIT_TEST_INSTRUCTIONS: u64 = 1_000_000;

#[derive(Parser)]
#[group(id = "mgo-move-test")]
pub struct Test {
    #[clap(flatten)]
    pub test: test::Test,
    /// If `true`, disable linters
    #[clap(long, global = true)]
    pub no_lint: bool,
}

impl Test {
    pub fn execute(
        self,
        path: Option<PathBuf>,
        build_config: BuildConfig,
    ) -> anyhow::Result<UnitTestResult> {
        let compute_coverage = self.test.compute_coverage;
        if !cfg!(debug_assertions) && compute_coverage {
            return Err(anyhow::anyhow!(
                "The --coverage flag is currently supported only in debug builds. Please build the Mgo CLI from source in debug mode."
            ));
        }
        // find manifest file directory from a given path or (if missing) from current dir
        let rerooted_path = base::reroot_path(path)?;
        let unit_test_config = self.test.unit_test_config();
        run_move_unit_tests(
            rerooted_path,
            build_config,
            Some(unit_test_config),
            compute_coverage,
        )
    }
}

struct DummyChildObjectStore {}

impl ChildObjectResolver for DummyChildObjectStore {
    fn read_child_object(
        &self,
        _parent: &ObjectID,
        _child: &ObjectID,
        _child_version_upper_bound: SequenceNumber,
    ) -> MgoResult<Option<Object>> {
        Ok(None)
    }
    fn get_object_received_at_version(
        &self,
        _owner: &ObjectID,
        _receiving_object_id: &ObjectID,
        _receive_object_at_version: SequenceNumber,
        _epoch_id: mgo_types::committee::EpochId,
    ) -> MgoResult<Option<Object>> {
        Ok(None)
    }
}

static TEST_STORE: Lazy<DummyChildObjectStore> = Lazy::new(|| DummyChildObjectStore {});

static SET_EXTENSION_HOOK: Lazy<()> =
    Lazy::new(|| set_extension_hook(Box::new(new_testing_object_and_natives_cost_runtime)));

/// This function returns a result of UnitTestResult. The outer result indicates whether it
/// successfully started running the test, and the inner result indicatests whether all tests pass.
pub fn run_move_unit_tests(
    path: PathBuf,
    build_config: BuildConfig,
    config: Option<UnitTestingConfig>,
    compute_coverage: bool,
) -> anyhow::Result<UnitTestResult> {
    // bind the extension hook if it has not yet been done
    Lazy::force(&SET_EXTENSION_HOOK);

    let config = config
        .unwrap_or_else(|| UnitTestingConfig::default_with_bound(Some(MAX_UNIT_TEST_INSTRUCTIONS)));

    let result = move_cli::base::test::run_move_unit_tests(
        &path,
        build_config,
        UnitTestingConfig {
            report_stacktrace_on_abort: true,
            ..config
        },
        mgo_move_natives::all_natives(/* silent */ false),
        Some(initial_cost_schedule_for_unit_tests()),
        compute_coverage,
        &mut std::io::stdout(),
    );
    result.map(|(test_result, warning_diags)| {
        if test_result == UnitTestResult::Success {
            if let Some(diags) = warning_diags {
                decorate_warnings(diags, None);
            }
        }
        test_result
    })
}

fn new_testing_object_and_natives_cost_runtime(ext: &mut NativeContextExtensions) {
    // Use a throwaway metrics registry for testing.
    let registry = prometheus::Registry::new();
    let metrics = Arc::new(LimitsMetrics::new(&registry));
    let store = Lazy::force(&TEST_STORE);

    ext.add(ObjectRuntime::new(
        store,
        BTreeMap::new(),
        false,
        Box::leak(Box::new(ProtocolConfig::get_for_max_version_UNSAFE())), // leak for testing
        metrics,
        0, // epoch id
    ));
    ext.add(NativesCostTable::from_protocol_config(
        &ProtocolConfig::get_for_max_version_UNSAFE(),
    ));
}