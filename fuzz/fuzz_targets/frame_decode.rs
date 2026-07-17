#![no_main]
#![allow(dead_code)]

use libfuzzer_sys::fuzz_target;
use std::sync::LazyLock;

#[path = "../../src/model.rs"]
mod model;

#[path = "../../src/protocol.rs"]
mod protocol;

static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
});

fuzz_target!(|data: &[u8]| {
    let mut framed = data;
    let _ = RUNTIME.block_on(protocol::read_frame::<_, protocol::Request>(&mut framed));
});
