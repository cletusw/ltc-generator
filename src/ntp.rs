use crate::status::{StatusText, set_ntp_status};
use chrono::{DateTime, Utc};
use oximedia_timesync::NtpClient;
use oximedia_timesync::ntp::client::NtpClientConfig;
use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

pub(crate) fn start_ntp_sync(
    offset_ms: Arc<AtomicI64>,
    status_text: Arc<Mutex<StatusText>>,
    base_instant: Instant,
) {
    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new();
        match rt {
            Ok(runtime) => {
                let result = runtime.block_on(async {
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
                });

                match result {
                    Ok(sync) => {
                        let ntp_sys = adjusted_system_time(sync.offset, SystemTime::now());

                        if let Some(time) = ntp_sys.map(DateTime::<Utc>::from) {
                            store_time_at_base(&offset_ms, time, base_instant);
                            set_ntp_status(&status_text, "NTP Locked (pool.ntp.org)");
                        } else {
                            use_system_time_fallback(
                                &offset_ms,
                                &status_text,
                                base_instant,
                                "invalid NTP time adjustment",
                            );
                        }
                    }
                    Err(error) => {
                        use_system_time_fallback(&offset_ms, &status_text, base_instant, &error);
                    }
                }
            }
            Err(error) => {
                use_system_time_fallback(&offset_ms, &status_text, base_instant, &error);
            }
        }
    });
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
    fn adjusted_system_time_applies_ntp_offset() {
        let now = SystemTime::UNIX_EPOCH + Duration::from_secs(10);

        assert_eq!(
            adjusted_system_time(1.5, now),
            Some(SystemTime::UNIX_EPOCH + Duration::from_millis(11_500))
        );
    }
}
