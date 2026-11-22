//! Tactus appliance daemon (`tactusd`) — M0 compilable skeleton.

use std::process::ExitCode;

use tactus_link_sys::AblLink;
use tactus_protocol::TactusConfig;
use tactus_sync_core::SyncEngine;

fn main() -> ExitCode {
    let config = TactusConfig::default();
    if let Err(err) = config.validate() {
        eprintln!("configuration validation failed: {err}");
        return ExitCode::FAILURE;
    }

    let engine = SyncEngine::new(config.sync.clone());
    let link = AblLink::new(engine.snapshot().bpm_link);
    let clock_us = link.clock_micros();

    println!(
        "tactusd M0 skeleton ready: direction={:?}, bpm={}, clock_micros={}",
        engine.snapshot().direction,
        engine.snapshot().bpm_link,
        clock_us
    );

    ExitCode::SUCCESS
}
