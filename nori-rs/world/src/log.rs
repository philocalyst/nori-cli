//! The harness's own log, for a test that needs to see why.
//!
//! `NORI_WORLD_LOG=<filter>` (an `EnvFilter` directive such as `nori_harness=debug,acp_event_flow=debug`)
//! prints the tracing of everything a world runs (harness, host, fake agents) to the test's
//! output. Unset, nothing is installed and tests stay quiet.

use std::sync::Once;

pub(crate) fn init() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let Ok(filter) = std::env::var("NORI_WORLD_LOG") else {
            return;
        };
        let _ = tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::new(filter))
            .with_test_writer()
            .try_init();
    });
}
