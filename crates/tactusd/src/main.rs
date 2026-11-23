//! Tactus appliance daemon (`tactusd`) — headless RT sync engine, RawMIDI hotplug & atomic config.

mod clock;
mod config_store;
mod control;
mod log;
mod midi;
mod rt;
mod rt_os;

use std::env;
use std::path::PathBuf;
use std::process::ExitCode;

use rtrb::RingBuffer;

use crate::config_store::{load_config, resolve_config_path, CONFIG_ENV_VAR, DEFAULT_CONFIG_PATH};
use crate::control::{run_control_plane, ControlPlaneArgs};
use crate::log::{log_error, log_info, log_warn};
use crate::midi::default_midi_backend;
use crate::rt::{create_rt_wake_pair, spawn_rt_thread, RT_PREFERRED_CORE, RT_SCHED_FIFO_PRIO};

const RING_CAPACITY: usize = 64;

enum CliAction {
    Run { config_override: Option<PathBuf> },
    Help,
}

fn parse_cli_args(args: impl Iterator<Item = String>) -> Result<CliAction, String> {
    let mut config_override = None;
    let mut iter = args.skip(1);

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "-h" | "--help" => return Ok(CliAction::Help),
            "--config" => {
                let value = iter
                    .next()
                    .ok_or_else(|| "--config requires a file path".to_string())?;
                if value.is_empty() {
                    return Err("--config path cannot be empty".to_string());
                }
                config_override = Some(PathBuf::from(value));
            }
            other => {
                if let Some(value) = other.strip_prefix("--config=") {
                    if value.is_empty() {
                        return Err("--config= path cannot be empty".to_string());
                    }
                    config_override = Some(PathBuf::from(value));
                } else {
                    return Err(format!("unknown argument `{other}` (use --help for usage)"));
                }
            }
        }
    }

    Ok(CliAction::Run { config_override })
}

fn print_usage() {
    eprintln!(
        "tactusd {}\n\nUSAGE:\n  tactusd [--config <PATH>]\n\nOPTIONS:\n  --config <PATH>   Path to config.toml (overrides {CONFIG_ENV_VAR}; default {DEFAULT_CONFIG_PATH})\n  -h, --help        Show this help message",
        env!("CARGO_PKG_VERSION")
    );
}

fn log_hardening_report(report: rt_os::RtHardeningReport) {
    if report.pinned_core == Some(RT_PREFERRED_CORE)
        && report.sched_fifo_prio == Some(RT_SCHED_FIFO_PRIO)
        && report.mlockall_ok
    {
        log_info!(
            "RT hardening active: core={}, SCHED_FIFO={}, mlockall=locked",
            RT_PREFERRED_CORE,
            RT_SCHED_FIFO_PRIO
        );
        return;
    }

    if let Some(core) = report.pinned_core {
        log_info!("RT CPU affinity pinned to core {core}");
    } else if let Some(errno) = report.affinity_errno {
        log_warn!(
            "RT CPU affinity to core {RT_PREFERRED_CORE} failed: {}",
            std::io::Error::from_raw_os_error(errno)
        );
    }

    if let Some(prio) = report.sched_fifo_prio {
        log_info!("RT scheduler set to SCHED_FIFO priority {prio}");
    } else if let Some(errno) = report.sched_errno {
        log_warn!(
            "RT SCHED_FIFO priority {RT_SCHED_FIFO_PRIO} unavailable ({}); continuing without FIFO",
            std::io::Error::from_raw_os_error(errno)
        );
    }

    if report.mlockall_ok {
        log_info!("RT memory locked via mlockall(MCL_CURRENT | MCL_FUTURE)");
    } else if let Some(errno) = report.mlock_errno {
        log_warn!(
            "RT mlockall failed ({}); continuing without locked pages",
            std::io::Error::from_raw_os_error(errno)
        );
    }
}

fn main() -> ExitCode {
    let config_override = match parse_cli_args(env::args()) {
        Ok(CliAction::Help) => {
            print_usage();
            return ExitCode::SUCCESS;
        }
        Ok(CliAction::Run { config_override }) => config_override,
        Err(msg) => {
            log_error!("{msg}");
            return ExitCode::FAILURE;
        }
    };

    let config_path = resolve_config_path(config_override, env::var_os(CONFIG_ENV_VAR));
    let (config, existed) = match load_config(&config_path) {
        Ok(loaded) => loaded,
        Err(err) => {
            log_error!("configuration error: {err}");
            return ExitCode::FAILURE;
        }
    };

    log_info!(
        "starting tactusd v{} (config=`{}` [{}], direction={:?}, start_stop_sync={}, offset_ms={:.1})",
        env!("CARGO_PKG_VERSION"),
        config_path.display(),
        if existed { "loaded" } else { "defaults" },
        config.sync.direction,
        config.sync.start_stop_sync,
        config.sync.midi_offset_ms
    );

    let (rt_cmd_tx, cmd_rx) = RingBuffer::new(RING_CAPACITY);
    let (tel_tx, tel_rx) = RingBuffer::new(RING_CAPACITY);
    let (fd_ret_tx, fd_ret_rx) = RingBuffer::new(RING_CAPACITY);
    let (wake_tx, wake_rx) = match create_rt_wake_pair() {
        Ok(pair) => pair,
        Err(err) => {
            log_error!("failed to create RT wakeup descriptor: {err}");
            return ExitCode::FAILURE;
        }
    };

    let (rt_handle, hardening) = match spawn_rt_thread(
        config.sync.clone(),
        rt::RtLoopChannels {
            cmd_rx,
            tel_tx,
            fd_ret_tx,
            wake_rx,
        },
    ) {
        Ok(res) => res,
        Err(err) => {
            log_error!("failed to spawn rt-sync thread: {err}");
            return ExitCode::FAILURE;
        }
    };
    log_hardening_report(hardening);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(3)
        .thread_name("tactus-async")
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(err) => {
            log_error!("failed to build tokio runtime: {err}");
            return ExitCode::FAILURE;
        }
    };

    runtime.block_on(run_control_plane(ControlPlaneArgs {
        config,
        config_path,
        backend: default_midi_backend(),
        rt_cmd_tx,
        tel_rx,
        fd_ret_rx,
        wake_tx,
        rt_handle,
    }));

    ExitCode::SUCCESS
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cli_config_and_help_flags() {
        assert!(matches!(
            parse_cli_args(["tactusd", "--help"].into_iter().map(str::to_string)),
            Ok(CliAction::Help)
        ));

        match parse_cli_args(
            ["tactusd", "--config", "/tmp/custom.toml"]
                .into_iter()
                .map(str::to_string),
        ) {
            Ok(CliAction::Run { config_override }) => {
                assert_eq!(config_override, Some(PathBuf::from("/tmp/custom.toml")));
            }
            _ => panic!("expected Run action"),
        }

        match parse_cli_args(
            ["tactusd", "--config=/etc/tactus.toml"]
                .into_iter()
                .map(str::to_string),
        ) {
            Ok(CliAction::Run { config_override }) => {
                assert_eq!(config_override, Some(PathBuf::from("/etc/tactus.toml")));
            }
            _ => panic!("expected Run action"),
        }

        assert!(parse_cli_args(["tactusd", "--bogus"].into_iter().map(str::to_string)).is_err());
    }
}
