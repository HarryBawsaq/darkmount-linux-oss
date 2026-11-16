//! A small `log` backend with `env_logger`-style level control.
//!
//! `RUST_LOG` takes a comma-separated list of `level` and `target=level`
//! directives, for example `debug` or `warn,proton_presence::discord=trace`.
//! Without it the level is INFO. Output goes to stderr; under systemd each
//! line carries a `<N>` priority prefix so the journal records the level.

use std::env;
use std::fs;
use std::io::{self, Write};
use std::os::unix::fs::MetadataExt;

use log::{Level, LevelFilter, Log, Metadata, Record};

pub struct Logger {
    default: LevelFilter,
    /// `(target prefix, level)` overrides.
    directives: Vec<(String, LevelFilter)>,
    journal: bool,
}

impl Logger {
    pub fn from_spec(spec: &str, journal: bool) -> Logger {
        let mut logger = Logger {
            default: LevelFilter::Info,
            directives: Vec::new(),
            journal,
        };
        // env_logger allows a trailing "/regex" message filter; ignore it.
        let spec = spec.split('/').next().unwrap_or_default();
        for directive in spec.split(',').map(str::trim).filter(|d| !d.is_empty()) {
            match directive.split_once('=') {
                Some((target, level)) => {
                    if let Ok(level) = level.trim().parse() {
                        logger.directives.push((target.trim().to_string(), level));
                    }
                }
                None => match directive.parse() {
                    Ok(level) => logger.default = level,
                    // A bare target name enables everything for it.
                    Err(_) => logger
                        .directives
                        .push((directive.to_string(), LevelFilter::Trace)),
                },
            }
        }
        logger
    }

    /// The level for a target: the most specific matching directive wins.
    pub fn level_for(&self, target: &str) -> LevelFilter {
        self.directives
            .iter()
            .filter(|(prefix, _)| {
                target
                    .strip_prefix(prefix.as_str())
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with("::"))
            })
            .max_by_key(|(prefix, _)| prefix.len())
            .map_or(self.default, |(_, level)| *level)
    }

    pub fn max_level(&self) -> LevelFilter {
        self.directives
            .iter()
            .map(|(_, level)| *level)
            .fold(self.default, Ord::max)
    }
}

/// syslog priority for the journal's `<N>` line prefix.
fn priority(level: Level) -> u8 {
    match level {
        Level::Error => 3,
        Level::Warn => 4,
        Level::Info => 6,
        Level::Debug | Level::Trace => 7,
    }
}

impl Log for Logger {
    fn enabled(&self, metadata: &Metadata) -> bool {
        metadata.level() <= self.level_for(metadata.target())
    }

    fn log(&self, record: &Record) {
        if !self.enabled(record.metadata()) {
            return;
        }
        let message = record.args().to_string();
        let mut out = io::stderr().lock();
        for line in message.lines() {
            let _ = if self.journal {
                writeln!(out, "<{}>{line}", priority(record.level()))
            } else {
                writeln!(out, "[{:<5}] {line}", record.level())
            };
        }
    }

    fn flush(&self) {}
}

/// Whether stderr is the journal stream systemd set up for this service.
/// `JOURNAL_STREAM` holds the device and inode of that stream; it is also
/// inherited by unrelated child processes, hence the comparison.
fn stderr_is_journal() -> bool {
    let Ok(stream) = env::var("JOURNAL_STREAM") else {
        return false;
    };
    let Ok(meta) = fs::metadata("/proc/self/fd/2") else {
        return false;
    };
    stream == format!("{}:{}", meta.dev(), meta.ino())
}

pub fn init() {
    let spec = env::var("RUST_LOG").unwrap_or_default();
    let logger = Logger::from_spec(&spec, stderr_is_journal());
    log::set_max_level(logger.max_level());
    // Fails only if a logger is already installed.
    let _ = log::set_boxed_logger(Box::new(logger));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_to_info() {
        let logger = Logger::from_spec("", false);
        assert_eq!(
            logger.level_for("proton_presence::daemon"),
            LevelFilter::Info
        );
        assert_eq!(logger.max_level(), LevelFilter::Info);
    }

    #[test]
    fn parses_global_level() {
        for (spec, level) in [
            ("debug", LevelFilter::Debug),
            ("TRACE", LevelFilter::Trace),
            (" warn ", LevelFilter::Warn),
            ("off", LevelFilter::Off),
        ] {
            assert_eq!(
                Logger::from_spec(spec, false).level_for("x"),
                level,
                "{spec}"
            );
        }
    }

    #[test]
    fn parses_per_target_levels() {
        let logger = Logger::from_spec(
            "warn,proton_presence=info,proton_presence::discord=trace",
            false,
        );
        assert_eq!(logger.level_for("other"), LevelFilter::Warn);
        assert_eq!(logger.level_for("proton_presence"), LevelFilter::Info);
        assert_eq!(
            logger.level_for("proton_presence::daemon"),
            LevelFilter::Info
        );
        assert_eq!(
            logger.level_for("proton_presence::discord"),
            LevelFilter::Trace
        );
        assert_eq!(logger.level_for("proton_presence_extra"), LevelFilter::Warn);
        assert_eq!(logger.max_level(), LevelFilter::Trace);
    }

    #[test]
    fn bare_target_enables_everything_and_bad_levels_are_ignored() {
        let logger = Logger::from_spec("proton_presence,other=loud/regex", false);
        assert_eq!(logger.level_for("proton_presence::x"), LevelFilter::Trace);
        assert_eq!(logger.level_for("other"), LevelFilter::Info);
    }

    #[test]
    fn maps_levels_to_journal_priorities() {
        assert_eq!(priority(Level::Error), 3);
        assert_eq!(priority(Level::Warn), 4);
        assert_eq!(priority(Level::Info), 6);
        assert_eq!(priority(Level::Debug), 7);
    }
}
