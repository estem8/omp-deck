//! omp's pinned sessions: `<agent dir>/session-pins.json`, a JSON array of session ids. Pins are
//! keyed by session id (they survive `/move`). OmpDeck only **reads** the file; every write goes
//! through omp ([`crate::omp::Omp::set_pinned`]), which locks and replaces it atomically.

use std::collections::HashSet;
use std::io::ErrorKind;
use std::path::Path;

const PINS_FILE: &str = "session-pins.json";
/// A pins file is a few ids; anything larger is not one.
const MAX_BYTES: u64 = 4 * 1024 * 1024;

/// The pinned session ids of `agent_dir`. A missing file, a file that is not a JSON array, an
/// unreadable one and non-string elements all degrade to what omp itself would read (fewer or no
/// pins) with a debug log, never an error: omp does the same for its resume picker.
pub fn read(agent_dir: &Path) -> HashSet<String> {
    let path = agent_dir.join(PINS_FILE);
    let text = match std::fs::metadata(&path) {
        Ok(m) if m.len() > MAX_BYTES => {
            log::debug!("pins: {} is too large, ignored", path.display());
            return HashSet::new();
        }
        Ok(_) => match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) => {
                log::debug!("pins: cannot read {}: {e}", path.display());
                return HashSet::new();
            }
        },
        Err(e) if e.kind() == ErrorKind::NotFound => return HashSet::new(),
        Err(e) => {
            log::debug!("pins: cannot stat {}: {e}", path.display());
            return HashSet::new();
        }
    };
    match serde_json::from_str::<Vec<serde_json::Value>>(&text) {
        Ok(items) => items
            .into_iter()
            .filter_map(|v| match v {
                serde_json::Value::String(s) => Some(s),
                _ => None,
            })
            .collect(),
        Err(e) => {
            log::debug!("pins: {} is not a JSON array: {e}", path.display());
            HashSet::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir_with(content: Option<&str>) -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        if let Some(c) = content {
            std::fs::write(dir.path().join(PINS_FILE), c).unwrap();
        }
        dir
    }

    #[test]
    fn missing_file_is_empty() {
        assert!(read(dir_with(None).path()).is_empty());
    }

    #[test]
    fn reads_string_ids_and_skips_other_elements() {
        // omp writes tab-indented JSON; non-string elements are dropped like omp's filter.
        let dir = dir_with(Some("[\n\t\"a1\",\n\t7,\n\tnull,\n\t\"b2\"\n]"));
        let pins = read(dir.path());
        assert_eq!(pins, HashSet::from(["a1".to_owned(), "b2".to_owned()]));
    }

    #[test]
    fn corrupt_or_non_array_file_is_empty() {
        assert!(read(dir_with(Some("{\"a\":1}")).path()).is_empty());
        assert!(read(dir_with(Some("[\"a\"")).path()).is_empty());
        assert!(read(dir_with(Some("")).path()).is_empty());
    }
}
