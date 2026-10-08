use crate::status::{StatusText, set_ntp_status};
use chrono::{DateTime, Utc};
use oximedia_timesync::NtpClient;
use oximedia_timesync::ntp::client::NtpClientConfig;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

const INITIAL_RETRY_DELAY: Duration = Duration::from_secs(2);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5 * 60);
const SUCCESS_SYNC_INTERVAL: Duration = Duration::from_secs(15 * 60);
const SYNC_TIMEOUT: Duration = Duration::from_secs(15);

enum WorkerCommand {
    RetryNow,
    Shutdown,
}

pub(crate) struct NtpSyncWorkerGuard {
    command_sender: SyncSender<WorkerCommand>,
    shutdown_requested: Arc<AtomicBool>,
}

impl NtpSyncWorkerGuard {
    pub(crate) fn retry_now(&self) {
        // A full queue means a retry is already pending, so coalesce clicks.
        let _ = self.command_sender.try_send(WorkerCommand::RetryNow);
    }
}

impl Drop for NtpSyncWorkerGuard {
    fn drop(&mut self) {
        self.shutdown_requested.store(true, Ordering::Relaxed);
        let _ = self.command_sender.try_send(WorkerCommand::Shutdown);
    }
}

pub(crate) fn start_ntp_sync(
    offset_ms: Arc<AtomicI64>,
    status_text: Arc<Mutex<StatusText>>,
    base_instant: Instant,
) -> NtpSyncWorkerGuard {
    let (command_sender, command_receiver) = mpsc::sync_channel(1);
    let shutdown_requested = Arc::new(AtomicBool::new(false));
    let worker_shutdown_requested = shutdown_requested.clone();
    std::thread::spawn(move || {
        let mut schedule = SyncSchedule::default();
        loop {
            if worker_shutdown_requested.load(Ordering::Relaxed) {
                break;
            }

            let synced = match tokio::runtime::Runtime::new() {
                Ok(runtime) => synchronize_once(&runtime, &offset_ms, &status_text, base_instant),
                Err(error) => {
                    use_system_time_fallback(&offset_ms, &status_text, base_instant, error);
                    false
                }
            };

            if worker_shutdown_requested.load(Ordering::Relaxed) {
                break;
            }

            let delay = schedule.delay_after_sync(synced);
            match command_receiver.recv_timeout(delay) {
                Ok(WorkerCommand::RetryNow) | Err(RecvTimeoutError::Timeout) => {}
                Ok(WorkerCommand::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
            }
        }
    });

    NtpSyncWorkerGuard {
        command_sender,
        shutdown_requested,
    }
}

#[derive(Default)]
struct SyncSchedule {
    retry_delay: Option<Duration>,
}

impl SyncSchedule {
    fn delay_after_sync(&mut self, synced: bool) -> Duration {
        if synced {
            self.retry_delay = None;
            SUCCESS_SYNC_INTERVAL
        } else {
            let delay = self.retry_delay.unwrap_or(INITIAL_RETRY_DELAY);
            self.retry_delay = Some(delay.saturating_mul(2).min(MAX_RETRY_DELAY));
            delay
        }
    }
}

fn synchronize_once(
    runtime: &tokio::runtime::Runtime,
    offset_ms: &AtomicI64,
    status_text: &Mutex<StatusText>,
    base_instant: Instant,
) -> bool {
    let result = runtime.block_on(async {
        tokio::time::timeout(SYNC_TIMEOUT, async {
            let mut config = NtpClientConfig::default();
            config.timeout = Duration::from_secs(3);
            config.max_retries = 1;
            let mut ntp = NtpClient::with_config(config);
            let mut dns_errors = Vec::new();
            let mut resolved_server_count = 0;

            for server in [
                "time.google.com:123",
                "time.cloudflare.com:123",
                "pool.ntp.org:123",
            ] {
                match tokio::net::lookup_host(server).await {
                    Ok(addresses) => {
                        let addresses: Vec<_> = addresses.collect();
                        if addresses.is_empty() {
                            dns_errors.push(format!("{server}: no addresses returned"));
                        }
                        for address in addresses {
                            ntp.add_server(address);
                            resolved_server_count += 1;
                        }
                    }
                    Err(error) => {
                        dns_errors.push(format!("{server}: {error}"));
                    }
                }
            }

            if resolved_server_count == 0 {
                return Err(format!(
                    "Could not resolve any NTP servers ({})",
                    dns_errors.join("; ")
                ));
            }

            ntp.synchronize().await.map_err(|error| {
                if dns_errors.is_empty() {
                    error.to_string()
                } else {
                    format!("{error}; DNS lookup failures: {}", dns_errors.join("; "))
                }
            })
        })
        .await
        .map_err(|_| format!("NTP synchronization timed out after {SYNC_TIMEOUT:?}"))?
    });

    match result {
        Ok(sync) => {
            let ntp_sys = adjusted_system_time(sync.offset, SystemTime::now());

            if let Some(time) = ntp_sys.map(DateTime::<Utc>::from) {
                store_time_at_base(offset_ms, time, base_instant);
                set_ntp_status(status_text, "NTP: Locked");
                true
            } else {
                use_system_time_fallback(
                    offset_ms,
                    status_text,
                    base_instant,
                    "invalid NTP time adjustment",
                );
                false
            }
        }
        Err(error) => {
            use_system_time_fallback(offset_ms, status_text, base_instant, error);
            false
        }
    }
}

fn adjusted_system_time(offset: f64, now: SystemTime) -> Option<SystemTime> {
    if !offset.is_finite() {
        return None;
    }

    let adjustment = Duration::try_from_secs_f64(offset.abs()).ok()?;
    if offset < 0.0 {
        now.checked_sub(adjustment)
    } else {
        now.checked_add(adjustment)
    }
}

fn use_system_time_fallback(
    offset_ms: &AtomicI64,
    status_text: &Mutex<StatusText>,
    base_instant: Instant,
    error: impl std::fmt::Display,
) {
    store_time_at_base(offset_ms, Utc::now(), base_instant);
    let status = format!("NTP Sync Error: {error}; falling back to system time");

    set_ntp_status(status_text, &status);
}

pub(crate) fn store_time_at_base(
    offset_ms: &AtomicI64,
    time: DateTime<Utc>,
    base_instant: Instant,
) {
    let elapsed_ms = i64::try_from(base_instant.elapsed().as_millis())
        .expect("elapsed time exceeds timestamp range");
    let timestamp_at_base = time
        .timestamp_millis()
        .checked_sub(elapsed_ms)
        .expect("timestamp at monotonic base is out of range");
    offset_ms.store(timestamp_at_base, Ordering::Relaxed);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failed_syncs_use_capped_exponential_backoff() {
        let mut schedule = SyncSchedule::default();

        assert_eq!(schedule.delay_after_sync(false), Duration::from_secs(2));
        assert_eq!(schedule.delay_after_sync(false), Duration::from_secs(4));
        assert_eq!(schedule.delay_after_sync(false), Duration::from_secs(8));

        for _ in 0..10 {
            schedule.delay_after_sync(false);
        }
        assert_eq!(schedule.delay_after_sync(false), MAX_RETRY_DELAY);
    }

    #[test]
    fn successful_sync_uses_recurring_interval_and_resets_backoff() {
        let mut schedule = SyncSchedule::default();

        assert_eq!(schedule.delay_after_sync(false), INITIAL_RETRY_DELAY);
        assert_eq!(schedule.delay_after_sync(true), SUCCESS_SYNC_INTERVAL);
        assert_eq!(schedule.delay_after_sync(false), INITIAL_RETRY_DELAY);
    }

    #[test]
    fn adjusted_system_time_applies_ntp_offset() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10);

        assert_eq!(
            adjusted_system_time(1.5, now),
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(11_500))
        );
    }
}
