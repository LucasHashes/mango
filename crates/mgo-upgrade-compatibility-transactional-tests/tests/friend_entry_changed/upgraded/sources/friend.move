// Copyright (c) MangoNet Labs Ltd.
// SPDX-License-Identifier: Apache-2.0

module base::friend_module {
    public fun call_friend(): u64 { base::base_module::friend_fun() }
}
