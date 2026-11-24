//! 带时间戳与级别的日志。
//!
//! 不用 `env_logger` 之类：只需要四个级别 + RFC3339 前缀，且日志同时要能进 journald
//! （stdout/stderr）与 launchd 的日志文件（需要自带时间戳）。

use std::sync::atomic::{AtomicBool, Ordering};

static VERBOSE: AtomicBool = AtomicBool::new(false);

pub fn set_verbose(v: bool) {
    VERBOSE.store(v, Ordering::Relaxed);
}

pub fn verbose() -> bool {
    VERBOSE.load(Ordering::Relaxed)
}

fn emit(level: &str, msg: &str) {
    let line = format!("{} {level} {msg}", crate::util::now_rfc3339());
    if level == "INFO " || level == "DEBUG" {
        println!("{line}");
    } else {
        eprintln!("{line}");
    }
}

pub fn info(msg: impl AsRef<str>) {
    emit("INFO ", msg.as_ref());
}

pub fn warn(msg: impl AsRef<str>) {
    emit("WARN ", msg.as_ref());
}

pub fn error(msg: impl AsRef<str>) {
    emit("ERROR", msg.as_ref());
}

/// 只在 `--verbose` 下输出的细节（候选地址、计划明细等）。
pub fn debug(msg: impl AsRef<str>) {
    if verbose() {
        emit("DEBUG", msg.as_ref());
    }
}
