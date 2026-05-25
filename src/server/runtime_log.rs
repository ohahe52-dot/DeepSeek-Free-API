//! Log runtime - triển khai log::Log tùy chỉnh, ba ngõ output + xoay vòng file
//!
//! Ba ngõ: stderr (thấy ở terminal) + ring buffer bộ nhớ (API truy vấn được) + file (lưu bền vững)
//! Xoay vòng file: mỗi file 10MB, giữ 3 file lịch sử, tổng tối đa ~40MB

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Write};
use std::sync::Arc;

use chrono::Local;
use serde::Serialize;
use tokio::sync::Mutex;

/// Dung lượng ring buffer
const BUFFER_CAPACITY: usize = 2000;
/// Số byte tối đa mỗi file log (10MB)
const MAX_FILE_SIZE: u64 = 10 * 1024 * 1024;
/// Số file log lịch sử giữ lại
const MAX_HISTORY_FILES: usize = 3;

/// Một log runtime
#[derive(Serialize, Clone, Debug)]
pub struct RuntimeLogEntry {
    pub timestamp: String,
    pub level: String,
    pub target: String,
    pub message: String,
}

/// Logger tùy chỉnh
pub struct DualLogger {
    /// Ring buffer bộ nhớ
    buffer: Mutex<VecDeque<RuntimeLogEntry>>,
    /// File log hiện tại (std::sync::Mutex dùng để ghi không chặn trên đường log)
    file: std::sync::Mutex<File>,
    /// Path file log
    log_path: String,
    /// Cấp log tối đa
    max_level: log::LevelFilter,
    /// Có bật output màu hay không
    use_color: bool,
}

impl std::fmt::Debug for DualLogger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DualLogger")
            .field("log_path", &self.log_path)
            .field("max_level", &self.max_level)
            .finish()
    }
}

impl DualLogger {
    fn new(log_path: &str, max_level: log::LevelFilter) -> std::io::Result<Self> {
        if let Some(parent) = std::path::Path::new(log_path).parent() {
            let _ = fs::create_dir_all(parent);
        }

        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)?;

        Ok(Self {
            buffer: Mutex::new(VecDeque::with_capacity(BUFFER_CAPACITY)),
            file: std::sync::Mutex::new(file),
            log_path: log_path.to_string(),
            max_level,
            use_color: std::io::stderr().is_terminal(),
        })
    }

    fn rotate_if_needed(&self) {
        let size = self
            .file
            .lock()
            .ok()
            .and_then(|f| f.metadata().ok().map(|m| m.len()))
            .unwrap_or(0);
        if size < MAX_FILE_SIZE {
            return;
        }

        for i in (1..=MAX_HISTORY_FILES).rev() {
            let old = format!("{}.{}", self.log_path, i);
            if i == MAX_HISTORY_FILES {
                let _ = fs::remove_file(&old);
            } else {
                let new = format!("{}.{}", self.log_path, i + 1);
                let _ = fs::rename(&old, &new);
            }
        }
        let _ = fs::rename(&self.log_path, format!("{}.1", self.log_path));

        if let Ok(new_file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.log_path)
            && let Ok(mut file_guard) = self.file.lock()
        {
            *file_guard = new_file;
        }
    }

    pub async fn query_logs(&self, offset: usize, limit: usize) -> (usize, Vec<RuntimeLogEntry>) {
        self.rotate_if_needed();
        let buffer = self.buffer.lock().await;
        let total = buffer.len();
        let logs: Vec<RuntimeLogEntry> = buffer
            .iter()
            .rev()
            .skip(offset)
            .take(limit)
            .cloned()
            .collect();
        (total, logs)
    }
}

/// Trả về mã màu ANSI theo cấp log (chỉ dùng cho stderr)
fn color_for_level(level: &str) -> &'static str {
    match level {
        "ERROR" => "\x1b[31m",
        "WARN" => "\x1b[33m",
        "INFO" => "\x1b[32m",
        "DEBUG" => "\x1b[34m",
        "TRACE" => "\x1b[35m",
        _ => "\x1b[0m",
    }
}

impl log::Log for DualLogger {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        metadata.level() <= self.max_level
    }

    fn log(&self, record: &log::Record) {
        if !self.enabled(record.metadata()) {
            return;
        }

        let timestamp = Local::now().format("%Y-%m-%dT%H:%M:%S%.3f%:z").to_string();
        let level = record.level().as_str().to_string();
        let target = record.target().to_string();
        let message = format!("{}", record.args());

        // 1. Ghi stderr (output terminal, cấp có màu)
        if self.use_color {
            eprintln!(
                "[\x1b[2m{} \x1b[0m{}{}\x1b[0m\x1b[2m  {}\x1b[0m] {}",
                timestamp,
                color_for_level(&level),
                level,
                target,
                message
            );
        } else {
            eprintln!("[{} {:5}  {}] {}", timestamp, level, target, message);
        }
        // 2. Ghi file
        let file_line = format!("[{} {:5}  {}] {}\n", timestamp, level, target, message);
        if let Ok(mut file_guard) = self.file.lock() {
            let _ = file_guard.write_all(file_line.as_bytes());
            let _ = file_guard.flush();
        }

        // 3. Ghi ring buffer (try_lock để tránh chặn đường log)
        let entry = RuntimeLogEntry {
            timestamp,
            level,
            target,
            message,
        };
        if let Ok(mut buffer) = self.buffer.try_lock() {
            if buffer.len() >= BUFFER_CAPACITY {
                buffer.pop_front();
            }
            buffer.push_back(entry);
        }
    }

    fn flush(&self) {
        if let Ok(mut file_guard) = self.file.lock() {
            let _ = file_guard.flush();
        }
    }
}

/// Tham chiếu Logger toàn cục
static GLOBAL_LOGGER: std::sync::OnceLock<Arc<DualLogger>> = std::sync::OnceLock::new();

/// Khởi tạo Logger tùy chỉnh, thay env_logger
pub fn init(log_path: &str) -> anyhow::Result<()> {
    let max_level = match std::env::var("RUST_LOG") {
        Ok(ref v) if !v.is_empty() => parse_level(v),
        _ => log::LevelFilter::Info,
    };

    let logger = Arc::new(DualLogger::new(log_path, max_level)?);
    GLOBAL_LOGGER
        .set(logger.clone())
        .map_err(|_| anyhow::anyhow!("Logger đã được khởi tạo"))?;

    // Arc::into_inner cần reference count của Arc bằng 1, nhưng GLOBAL_LOGGER giữ một bản
    // nên bọc Arc clone bằng Box::new
    let boxed: Box<dyn log::Log> = Box::new(LoggerWrapper { inner: logger });
    log::set_boxed_logger(boxed).map_err(|e| anyhow::anyhow!("Thiết lập logger thất bại: {}", e))?;
    log::set_max_level(max_level);
    Ok(())
}

/// Bọc Arc<DualLogger> để triển khai Log (vì set_boxed_logger cần Box<dyn Log>)
struct LoggerWrapper {
    inner: Arc<DualLogger>,
}

impl log::Log for LoggerWrapper {
    fn enabled(&self, metadata: &log::Metadata) -> bool {
        self.inner.enabled(metadata)
    }
    fn log(&self, record: &log::Record) {
        self.inner.log(record);
    }
    fn flush(&self) {
        self.inner.flush();
    }
}

fn parse_level(s: &str) -> log::LevelFilter {
    let mut max_level = log::LevelFilter::Info;
    for segment in s.split(',') {
        let level_str = segment.split('=').next_back().unwrap_or(segment).trim();
        let level = match level_str {
            "trace" => log::LevelFilter::Trace,
            "debug" => log::LevelFilter::Debug,
            "warn" => log::LevelFilter::Warn,
            "error" => log::LevelFilter::Error,
            "off" => log::LevelFilter::Off,
            _ => continue,
        };
        if level > max_level {
            max_level = level;
        }
    }
    max_level
}

/// Truy vấn log runtime (phân trang, đảo từ mới nhất về cũ nhất)
pub async fn query_logs(offset: usize, limit: usize) -> (usize, Vec<RuntimeLogEntry>) {
    match GLOBAL_LOGGER.get() {
        Some(logger) => logger.query_logs(offset, limit).await,
        None => (0, Vec::new()),
    }
}
