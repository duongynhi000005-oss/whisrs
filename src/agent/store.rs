//! `$XDG_DATA_HOME/whisrs/agents.json`: each agent's session between turns
//! and across daemon restarts.
//!
//! Written (0600, atomically) on every change, and only by a daemon that has
//! `[[agents]]` configured: with none, the file is never created.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::SessionRef;

/// The file format version.
const VERSION: u32 = 1;

/// One agent's persisted state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentRecord {
    /// The harness the session belongs to. A changed harness starts a fresh
    /// session.
    pub harness: String,
    /// The directory the session works in. A changed dir starts a fresh
    /// session.
    pub dir: PathBuf,
    #[serde(default)]
    pub session_id: Option<String>,
    #[serde(default)]
    pub bg_id: Option<String>,
    #[serde(default)]
    pub transcript_path: Option<PathBuf>,
    /// The session id is not confirmed yet ([`SessionRef::pending`]).
    #[serde(default)]
    pub session_pending: bool,
    /// A turn was running when this was written: the daemon watches it again
    /// at startup, so its reply is still spoken.
    #[serde(default)]
    pub turn_running: bool,
    /// When the running turn started.
    #[serde(default)]
    pub turn_started_at: Option<DateTime<Utc>>,
    pub updated_at: DateTime<Utc>,
}

impl AgentRecord {
    /// A record with no session yet.
    pub fn new(harness: &str, dir: &Path) -> Self {
        Self {
            harness: harness.to_string(),
            dir: dir.to_path_buf(),
            session_id: None,
            bg_id: None,
            transcript_path: None,
            session_pending: false,
            turn_running: false,
            turn_started_at: None,
            updated_at: Utc::now(),
        }
    }

    /// The session ids as the driver takes them.
    pub fn session(&self) -> SessionRef {
        SessionRef {
            session_id: self.session_id.clone(),
            bg_id: self.bg_id.clone(),
            transcript_path: self.transcript_path.clone(),
            pending: self.session_pending,
        }
    }

    /// Store the ids a driver reported.
    pub fn set_session(&mut self, session: &SessionRef) {
        self.session_id = session.session_id.clone();
        self.bg_id = session.bg_id.clone();
        self.transcript_path = session.transcript_path.clone();
        self.session_pending = session.pending;
    }

    /// Forget the session (and any turn).
    pub fn clear_session(&mut self) {
        self.session_id = None;
        self.bg_id = None;
        self.transcript_path = None;
        self.session_pending = false;
        self.turn_running = false;
        self.turn_started_at = None;
    }

    /// Whether this record belongs to an agent with this harness and dir.
    pub fn matches(&self, harness: &str, dir: &Path) -> bool {
        self.harness == harness && self.dir == dir
    }
}

/// The whole file: records by agent name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentStore {
    pub version: u32,
    /// Records of agents no longer configured are kept as they are.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentRecord>,
}

impl Default for AgentStore {
    fn default() -> Self {
        Self {
            version: VERSION,
            agents: BTreeMap::new(),
        }
    }
}

/// `$XDG_DATA_HOME/whisrs/agents.json`.
pub fn store_path() -> PathBuf {
    dirs::data_dir()
        .unwrap_or_else(|| PathBuf::from("~/.local/share"))
        .join("whisrs")
        .join("agents.json")
}

/// Read the store. A missing file is an empty store; an unreadable or
/// corrupt one too, with the error returned beside it for the log (the next
/// save replaces it).
pub fn load(path: &Path) -> (AgentStore, Option<String>) {
    let contents = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (AgentStore::default(), None),
        Err(e) => {
            return (
                AgentStore::default(),
                Some(format!("cannot read {}: {e}", path.display())),
            )
        }
    };
    match serde_json::from_str::<AgentStore>(&contents) {
        Ok(store) => (store, None),
        Err(e) => (
            AgentStore::default(),
            Some(format!(
                "{} is not valid, starting without saved sessions: {e}",
                path.display()
            )),
        ),
    }
}

/// Write the store atomically (a 0600 temp file renamed over the old one).
pub fn save(path: &Path, store: &AgentStore) -> anyhow::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;

    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("{} has no parent directory", path.display()))?;
    std::fs::create_dir_all(parent)?;
    let json = serde_json::to_vec_pretty(store)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("agents.json"),
        std::process::id()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&json)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn a_missing_file_is_an_empty_store() {
        let dir = tempfile::tempdir().unwrap();
        let (store, err) = load(&dir.path().join("agents.json"));
        assert_eq!(store, AgentStore::default());
        assert!(err.is_none());
    }

    #[test]
    fn save_then_load_round_trips_with_0600() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("agents.json");
        let mut store = AgentStore::default();
        let mut record = AgentRecord::new("claude", Path::new("/proj"));
        record.set_session(&SessionRef {
            session_id: Some("cc7a37cf-9343-40fc-bd3c-9c8b77339da2".into()),
            bg_id: Some("cc7a37cf".into()),
            transcript_path: Some("/t/cc7a37cf.jsonl".into()),
            pending: false,
        });
        record.turn_running = true;
        record.turn_started_at = Some(Utc::now());
        store.agents.insert("jarvis".into(), record);
        save(&path, &store).unwrap();

        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        let (loaded, err) = load(&path);
        assert!(err.is_none());
        assert_eq!(loaded, store);
        // No temp file left behind.
        let names: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().file_name())
            .collect();
        assert_eq!(names, vec![std::ffi::OsString::from("agents.json")]);
    }

    #[test]
    fn a_corrupt_file_loads_empty_with_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("agents.json");
        std::fs::write(&path, "{not json").unwrap();
        let (store, err) = load(&path);
        assert_eq!(store, AgentStore::default());
        assert!(err.unwrap().contains("is not valid"));
    }

    #[test]
    fn records_track_harness_and_dir() {
        let mut r = AgentRecord::new("claude", Path::new("/proj"));
        assert!(r.matches("claude", Path::new("/proj")));
        assert!(!r.matches("claude", Path::new("/other")));
        assert!(!r.matches("codex", Path::new("/proj")));
        r.session_id = Some("s".into());
        r.session_pending = true;
        r.turn_running = true;
        r.clear_session();
        assert_eq!(r.session(), SessionRef::default());
        assert!(!r.turn_running);
    }

    /// A pending OpenCode id survives a restart, and a file written before
    /// the flag existed reads as confirmed.
    #[test]
    fn a_pending_session_round_trips_and_old_files_are_confirmed() {
        let mut r = AgentRecord::new("opencode", Path::new("/proj"));
        let pending = SessionRef {
            session_id: Some("ses_1".into()),
            bg_id: None,
            transcript_path: None,
            pending: true,
        };
        r.set_session(&pending);
        let json = serde_json::to_string(&r).unwrap();
        let back: AgentRecord = serde_json::from_str(&json).unwrap();
        assert_eq!(back.session(), pending);
        let old: AgentRecord = serde_json::from_str(
            r#"{"harness":"opencode","dir":"/proj","session_id":"ses_1","updated_at":"2026-10-25T10:00:00Z"}"#,
        )
        .unwrap();
        assert!(!old.session().pending);
    }

    #[test]
    fn the_wire_format_names_every_field() {
        let mut r = AgentRecord::new("claude", Path::new("/proj"));
        r.bg_id = Some("cc7a37cf".into());
        let json = serde_json::to_value(&r).unwrap();
        for key in [
            "harness",
            "dir",
            "session_id",
            "bg_id",
            "transcript_path",
            "session_pending",
            "turn_running",
            "turn_started_at",
            "updated_at",
        ] {
            assert!(json.get(key).is_some(), "{key} missing from {json}");
        }
    }
}
