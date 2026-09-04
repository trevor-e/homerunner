//! Small size-bounded writer for the installed service's operational log.

use anyhow::{Context, Result};
use std::io::Write;
use std::path::PathBuf;

pub struct RotatingLog {
    path: PathBuf,
    max_bytes: u64,
    backups: u32,
}

impl RotatingLog {
    pub fn new(path: PathBuf, max_bytes: u64, backups: u32) -> Self {
        Self {
            path,
            max_bytes,
            backups,
        }
    }

    fn backup_path(&self, index: u32) -> PathBuf {
        PathBuf::from(format!("{}.{}", self.path.display(), index))
    }

    fn rotate(&self) -> Result<()> {
        if self.backups == 0 {
            if self.path.exists() {
                std::fs::remove_file(&self.path)?;
            }
            return Ok(());
        }
        let oldest = self.backup_path(self.backups);
        if oldest.exists() {
            std::fs::remove_file(&oldest)?;
        }
        for index in (1..self.backups).rev() {
            let source = self.backup_path(index);
            if source.exists() {
                std::fs::rename(source, self.backup_path(index + 1))?;
            }
        }
        if self.path.exists() {
            std::fs::rename(&self.path, self.backup_path(1))?;
        }
        Ok(())
    }

    pub fn write_line(&self, line: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let incoming = line.len() as u64 + 1;
        let current = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
        if self.max_bytes == 0 || current.saturating_add(incoming) > self.max_bytes {
            self.rotate()?;
        }
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("open service log {}", self.path.display()))?;
        writeln!(file, "{line}")?;
        Ok(())
    }
}

pub fn from_environment() -> Option<RotatingLog> {
    let path = std::env::var_os("HOMERUNNER_LOG_FILE").map(PathBuf::from)?;
    let max_bytes = std::env::var("HOMERUNNER_LOG_MAX_BYTES")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(10 * 1024 * 1024);
    let backups = std::env::var("HOMERUNNER_LOG_BACKUPS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(3);
    Some(RotatingLog::new(path, max_bytes, backups))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TempDir;

    #[test]
    fn rotates_at_size_and_keeps_bounded_backups() {
        let dir = TempDir::new("rotating-log");
        let path = dir.path().join("service.log");
        let log = RotatingLog::new(path.clone(), 10, 2);

        log.write_line("12345").unwrap();
        log.write_line("67890").unwrap();
        log.write_line("abcde").unwrap();
        log.write_line("fghij").unwrap();

        assert_eq!(std::fs::read_to_string(&path).unwrap(), "fghij\n");
        assert_eq!(
            std::fs::read_to_string(log.backup_path(1)).unwrap(),
            "abcde\n"
        );
        assert_eq!(
            std::fs::read_to_string(log.backup_path(2)).unwrap(),
            "67890\n"
        );
    }
}
