// Copyright (c) MangoNet Labs Ltd.
// SPDX-License-Identifier: Apache-2.0

/// Create a dependency on a package to test Move.lock generation.
module depends::depends_on_basics {
    use examples::object_basics;
    use mgo::tx_context::TxContext;

    public entry fun delegate(ctx: &mut TxContext) {
        object_basics::share(ctx);
    }
}
