use tracing::{debug, info};

pub fn log_data_sample(direction: &str, data: &[u8], size: usize, log_level: &str) {
    match log_level {
        "verbose" => {
            let preview_size = std::cmp::min(200, size);
            let hex_dump = data[..preview_size]
                .iter()
                .map(|b| format!("{:02x}", b))
                .collect::<Vec<_>>()
                .join(" ");

            let ascii_dump: String = data[..preview_size]
                .iter()
                .map(|&b| if b >= 32 && b <= 126 { b as char } else { '.' })
                .collect();

            info!("{} data[{}]: Hex: {}", direction, size, hex_dump);
            info!("{} data[{}]: ASCII: {}", direction, size, ascii_dump);
        }
        "minimal" => {
            if size > 0 {
                let preview_size = std::cmp::min(50, size);
                debug!(
                    "{} data preview[{}]: {:?}",
                    direction,
                    size,
                    &data[..preview_size]
                );
            }
        }
        _ => {
            if size > 10000 {
                debug!("{} data size: {} bytes", direction, size);
            }
        }
    }
}

pub fn log_transfer_summary(session_id: &str, direction: &str, bytes: u64) {
    let size_str = if bytes < 1024 {
        format!("{} B", bytes)
    } else if bytes < 1024 * 1024 {
        format!("{:.2} KB", bytes as f64 / 1024.0)
    } else if bytes < 1024 * 1024 * 1024 {
        format!("{:.2} MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{:.2} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    };

    info!(
        "Session {}: {} transferred {}",
        session_id, direction, size_str
    );
}
