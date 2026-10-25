//! Follow a Claude Code session's transcript while a turn runs, for what the
//! hooks do not tell: which tool the agent uses now
//! ([`TurnUpdate::Activity`]), and the text of a message a tool call followed
//! ([`TurnUpdate::Interim`], maybe an acknowledgement or an update).
//!
//! Not hooks: they have no assistant-text event, a `PreToolUse` hook would
//! spawn a process per tool call, and they are fixed when a session starts
//! (a resume passes no flag). The transcript has both, and one reader also
//! fits the event streams of the phase 2 harnesses.
//!
//! The transcript is JSONL, one line per content block, written when the
//! block completes (measured on 2.1.301): an assistant message's `thinking`,
//! `text` and `tool_use` blocks are separate lines sharing `message.id`, each
//! with an RFC 3339 `timestamp`. Subagent lines carry `isSidechain: true`.
//! Tool results come back as `user` lines.
//!
//! Only our own session's file is ever opened: its path from the `Stop`
//! hook, else `<config>/projects/*/<session id>.jsonl` (the folder is the
//! session's cwd with every `/` made a `-`; found by the id, not rebuilt).

use std::path::PathBuf;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde_json::Value;
use tracing::debug;

use super::claude::find_transcript;
use super::progress::{activity_for_tool, InterimTracker};
use super::{SessionRef, TurnUpdate, TurnUpdates};

/// How often the transcript is read while a turn runs.
pub const TAIL_POLL: Duration = Duration::from_millis(750);

/// What a transcript line says about the turn, in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TailEvent {
    /// An assistant text block, raw.
    Text(String),
    /// The assistant asked to use a tool (`Bash`, `mcp__<server>__<tool>`).
    ToolUse { name: String },
}

/// The events of one transcript line: an assistant line's text and tool use
/// blocks. Nothing for a subagent's (sidechain) line, thinking, a user line
/// (prompts and tool results), other entry types and anything unparsable.
pub fn parse_line(line: &str) -> Vec<TailEvent> {
    parse_entry(line)
        .map(|(_, events)| events)
        .unwrap_or_default()
}

/// [`parse_line`], with the line's timestamp. `None` for a line with no
/// events.
fn parse_entry(line: &str) -> Option<(Option<DateTime<Utc>>, Vec<TailEvent>)> {
    let entry: Value = serde_json::from_str(line.trim()).ok()?;
    if entry.get("type").and_then(Value::as_str) != Some("assistant")
        || entry.get("isSidechain").and_then(Value::as_bool) == Some(true)
    {
        return None;
    }
    let content = entry.pointer("/message/content")?.as_array()?;
    let events: Vec<TailEvent> = content
        .iter()
        .filter_map(|block| match block.get("type").and_then(Value::as_str) {
            Some("text") => block
                .get("text")
                .and_then(Value::as_str)
                .filter(|t| !t.trim().is_empty())
                .map(|t| TailEvent::Text(t.to_string())),
            Some("tool_use") => {
                block
                    .get("name")
                    .and_then(Value::as_str)
                    .map(|name| TailEvent::ToolUse {
                        name: name.to_string(),
                    })
            }
            _ => None,
        })
        .collect();
    if events.is_empty() {
        return None;
    }
    let at = entry
        .get("timestamp")
        .and_then(Value::as_str)
        .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&Utc));
    Some((at, events))
}

/// What a tail reports.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TailMode {
    /// A turn this daemon launched: activities and interims.
    Live,
    /// A turn watched again after a restart: activities only. The turn's
    /// earlier messages were the old daemon's to speak, and the first read
    /// takes them all at once.
    Rewatch,
}

/// A task aborted when this is dropped.
#[derive(Debug)]
pub struct AbortOnDrop(tokio::task::JoinHandle<()>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// Follows a session's transcript and reports to the turn's updates.
pub struct TranscriptTail;

impl TranscriptTail {
    /// Follow `session`'s transcript from `since` (the turn's start) until
    /// the guard is dropped: read every [`TAIL_POLL`], the whole file at
    /// first (only lines at or after `since` count), then what was appended,
    /// a partial last line left for the next read. The path comes from the
    /// newest ids the turn reported ([`TurnUpdates::latest`]), else
    /// `session`, and is looked for by the session id until it exists.
    pub fn spawn(
        session: SessionRef,
        since: DateTime<Utc>,
        updates: TurnUpdates,
        mode: TailMode,
    ) -> AbortOnDrop {
        Self::spawn_every(session, since, updates, mode, TAIL_POLL)
    }

    /// [`TranscriptTail::spawn`] with the poll interval injected, for tests.
    fn spawn_every(
        session: SessionRef,
        since: DateTime<Utc>,
        updates: TurnUpdates,
        mode: TailMode,
        poll: Duration,
    ) -> AbortOnDrop {
        AbortOnDrop(tokio::spawn(async move {
            let mut tail = Tail::new(since, mode);
            let mut path: Option<PathBuf> = None;
            let mut tick = tokio::time::interval(poll);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                tick.tick().await;
                if path.is_none() {
                    path = locate(&session, &updates).await;
                }
                let Some(file) = path.clone() else {
                    continue;
                };
                let offset = tail.offset;
                let read = tokio::task::spawn_blocking(move || read_from(&file, offset)).await;
                match read {
                    Ok(Ok(read)) => tail.feed(read, &updates),
                    Ok(Err(e)) => debug!("transcript tail: {e}"),
                    Err(e) => debug!("transcript tail: the read failed: {e}"),
                }
            }
        }))
    }
}

/// The transcript's path: reported, else found by the session id. Off the
/// async runtime (a directory scan).
async fn locate(session: &SessionRef, updates: &TurnUpdates) -> Option<PathBuf> {
    let latest = updates.latest().unwrap_or_default();
    if let Some(path) = latest.transcript_path.or(session.transcript_path.clone()) {
        return Some(path);
    }
    let id = latest.session_id.or(session.session_id.clone())?;
    tokio::task::spawn_blocking(move || find_transcript(&id))
        .await
        .ok()
        .flatten()
}

/// The bytes appended since an offset.
struct Read {
    /// Where they start: the offset asked for, or 0 when the file shrank
    /// (rewritten, so read again from the start).
    start: u64,
    bytes: Vec<u8>,
}

/// Read `path` from `offset` to its end. Blocking.
fn read_from(path: &std::path::Path, offset: u64) -> std::io::Result<Read> {
    use std::io::{Read as _, Seek, SeekFrom};
    let mut file = std::fs::File::open(path)?;
    let len = file.metadata()?.len();
    let start = if len < offset { 0 } else { offset };
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(Read { start, bytes })
}

/// A tail's reading state.
struct Tail {
    since: DateTime<Utc>,
    mode: TailMode,
    /// Where the next read starts: after the last complete line.
    offset: u64,
    tracker: InterimTracker,
    /// The last activity sent, so a run of the same tool sends it once.
    activity: Option<String>,
}

impl Tail {
    fn new(since: DateTime<Utc>, mode: TailMode) -> Self {
        Self {
            since,
            mode,
            offset: 0,
            tracker: InterimTracker::new(),
            activity: None,
        }
    }

    /// Handle the complete lines of a read; a partial last line is read
    /// again next time.
    fn feed(&mut self, read: Read, updates: &TurnUpdates) {
        if read.start < self.offset {
            // Rewritten from the start: what was held belongs to the old
            // contents.
            self.tracker.end();
        }
        let Some(end) = read.bytes.iter().rposition(|&b| b == b'\n') else {
            self.offset = read.start;
            return;
        };
        self.offset = read.start + end as u64 + 1;
        for line in read.bytes[..end].split(|&b| b == b'\n') {
            let line = String::from_utf8_lossy(line);
            let Some((at, events)) = parse_entry(&line) else {
                continue;
            };
            if at.is_none_or(|at| at < self.since) {
                continue;
            }
            for event in events {
                self.event(event, updates);
            }
        }
    }

    fn event(&mut self, event: TailEvent, updates: &TurnUpdates) {
        match event {
            TailEvent::Text(text) => {
                if self.mode == TailMode::Live {
                    self.tracker.on_text(&text);
                }
            }
            TailEvent::ToolUse { name } => {
                let activity = activity_for_tool(&name);
                if self.activity.as_ref() != Some(&activity) {
                    self.activity = Some(activity.clone());
                    updates.send(TurnUpdate::Activity(activity));
                }
                if let Some(raw) = self.tracker.on_tool() {
                    updates.send(TurnUpdate::Interim(raw));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::claude::find_transcript_in;
    use std::io::Write as _;
    use tokio::sync::mpsc;

    /// An assistant line shaped like Claude Code 2.1.301 writes it (trimmed
    /// of usage and request fields), with one content block.
    fn assistant(at: &str, block: Value, sidechain: bool) -> String {
        serde_json::json!({
            "parentUuid": "cfcc9191-f5b7-491f-8bf8-72954274f8f9",
            "isSidechain": sidechain,
            "message": {
                "model": "claude-opus-5-5",
                "id": "msg_011CgMHJygAqd8ceNqa7nmt5",
                "type": "message",
                "role": "assistant",
                "content": [block],
                "stop_reason": null,
            },
            "type": "assistant",
            "uuid": "8e92e2a0-7d56-4a66-b84b-33c91229a133",
            "timestamp": at,
            "userType": "external",
            "entrypoint": "cli",
            "cwd": "/home/u/Projects/whisrs",
            "sessionId": "55c757bc-7513-4173-a715-ddf62b818005",
            "version": "2.1.301",
            "gitBranch": "main",
        })
        .to_string()
    }

    fn text(at: &str, t: &str) -> String {
        assistant(at, serde_json::json!({"type": "text", "text": t}), false)
    }

    fn tool(at: &str, name: &str) -> String {
        assistant(
            at,
            serde_json::json!({
                "type": "tool_use",
                "id": "toolu_014N1avpTYBFNT6zpqwVFNdP",
                "name": name,
                "input": {"command": "cargo test"},
                "caller": {"type": "direct"},
            }),
            false,
        )
    }

    const AT: &str = "2026-10-24T19:41:07.789Z";

    #[test]
    fn text_and_tool_use_lines_are_events() {
        assert_eq!(
            parse_line(&text(AT, "<spoken>On it.</spoken>")),
            [TailEvent::Text("<spoken>On it.</spoken>".into())]
        );
        for name in [
            "Bash",
            "mcp__claude-in-chrome__navigate",
            "mcp__claude_ai_Gmail__search_threads",
        ] {
            assert_eq!(
                parse_line(&tool(AT, name)),
                [TailEvent::ToolUse { name: name.into() }]
            );
        }
        // The timestamp is read with the events.
        let (at, _) = parse_entry(&tool(AT, "Bash")).unwrap();
        assert_eq!(
            at,
            Some(
                DateTime::parse_from_rfc3339(AT)
                    .unwrap()
                    .with_timezone(&Utc)
            )
        );
    }

    #[test]
    fn thinking_sidechains_users_and_others_are_nothing() {
        let thinking = assistant(
            AT,
            serde_json::json!({"type": "thinking", "thinking": "", "signature": "CAQS"}),
            false,
        );
        assert!(parse_line(&thinking).is_empty());
        let sidechain = assistant(
            AT,
            serde_json::json!({"type": "text", "text": "<spoken>subagent</spoken>"}),
            true,
        );
        assert!(parse_line(&sidechain).is_empty());
        let sidechain_tool = assistant(
            AT,
            serde_json::json!({"type": "tool_use", "id": "t", "name": "Bash", "input": {}}),
            true,
        );
        assert!(parse_line(&sidechain_tool).is_empty());
        let tool_result = serde_json::json!({
            "parentUuid": "7ff5c9ae-9393-4f88-aa5f-d484b9d2bc19",
            "isSidechain": false,
            "type": "user",
            "message": {"role": "user", "content": [{
                "tool_use_id": "toolu_014N1avpTYBFNT6zpqwVFNdP",
                "type": "tool_result",
                "content": "ok",
            }]},
            "timestamp": AT,
            "toolUseResult": "ok",
        })
        .to_string();
        assert!(parse_line(&tool_result).is_empty());
        let prompt = serde_json::json!({
            "type": "user",
            "isSidechain": false,
            "message": {"role": "user", "content": "Reply with exactly: pong"},
            "timestamp": AT,
        })
        .to_string();
        assert!(parse_line(&prompt).is_empty());
        assert!(parse_line(r#"{"type":"cost-state","costUSD":0.1}"#).is_empty());
        assert!(parse_line(&text(AT, "  ")).is_empty());
        assert!(parse_line("not json").is_empty());
        assert!(parse_line("").is_empty());
    }

    /// Append `s` to `path` and flush.
    fn append(path: &std::path::Path, s: &str) {
        let mut f = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        f.write_all(s.as_bytes()).unwrap();
        f.flush().unwrap();
    }

    async fn next(rx: &mut mpsc::UnboundedReceiver<TurnUpdate>) -> TurnUpdate {
        tokio::time::timeout(Duration::from_secs(5), rx.recv())
            .await
            .expect("no update in time")
            .expect("updates closed")
    }

    /// Nothing more arrives within a few polls.
    async fn quiet(rx: &mut mpsc::UnboundedReceiver<TurnUpdate>) {
        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(rx.try_recv().ok(), None);
    }

    fn now() -> String {
        Utc::now().to_rfc3339()
    }

    /// Lines appended in steps, a partial one among them: activities in
    /// order, an interim only for a text a tool call followed, and nothing
    /// for text, text, end of turn. Lines from before the turn are skipped.
    #[tokio::test]
    async fn a_live_tail_reports_activities_and_interims_in_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("55c757bc.jsonl");
        // An earlier turn's text and tool call: before `since`.
        let old = "2026-10-24T10:00:00.000Z";
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n",
                text(old, "<spoken>Old ack.</spoken>"),
                tool(old, "Read")
            ),
        )
        .unwrap();
        let since = Utc::now() - chrono::Duration::seconds(1);
        let session = SessionRef {
            session_id: Some("55c757bc".into()),
            bg_id: Some("55c757bc".into()),
            transcript_path: Some(path.clone()),
        };
        let (updates, mut rx) = TurnUpdates::channel();
        let _tail = TranscriptTail::spawn_every(
            session,
            since,
            updates,
            TailMode::Live,
            Duration::from_millis(10),
        );
        quiet(&mut rx).await;

        // The acknowledgement, then a tool call written in two parts.
        append(
            &path,
            &format!("{}\n", text(&now(), "<spoken>On it.</spoken>")),
        );
        let line = tool(&now(), "Bash");
        let (head, rest) = line.split_at(line.len() / 2);
        append(&path, head);
        quiet(&mut rx).await;
        append(&path, &format!("{rest}\n"));
        assert_eq!(
            next(&mut rx).await,
            TurnUpdate::Activity("Running a command".into())
        );
        assert_eq!(
            next(&mut rx).await,
            TurnUpdate::Interim("<spoken>On it.</spoken>".into())
        );
        quiet(&mut rx).await;

        // A tool call with no text before it: the activity only.
        append(
            &path,
            &format!("{}\n", tool(&now(), "mcp__claude-in-chrome__navigate")),
        );
        assert_eq!(
            next(&mut rx).await,
            TurnUpdate::Activity("Using the browser".into())
        );
        quiet(&mut rx).await;

        // Text, text, end of turn: nothing extra.
        append(
            &path,
            &format!(
                "{}\n{}\n",
                text(&now(), "<spoken>Found it.</spoken>"),
                text(&now(), "<spoken>Done, the page loads.</spoken>")
            ),
        );
        quiet(&mut rx).await;
    }

    /// A turn watched again after a restart reports activities only, the
    /// same tool once, and starts reading once the turn reports where its
    /// transcript is.
    #[tokio::test]
    async fn a_rewatch_tail_reports_activities_only() {
        let config = tempfile::tempdir().unwrap();
        let dir = config
            .path()
            .join("projects")
            .join("-home-u-Projects-whisrs");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("abc12345-1111.jsonl");
        std::fs::write(
            &path,
            format!(
                "{}\n{}\n{}\n",
                text(&now(), "<spoken>On it.</spoken>"),
                tool(&now(), "Bash"),
                tool(&now(), "Bash"),
            ),
        )
        .unwrap();
        let since = Utc::now() - chrono::Duration::seconds(5);
        let (updates, mut rx) = TurnUpdates::channel();
        // Only the bg id at first: nothing to read.
        let session = SessionRef {
            session_id: None,
            bg_id: Some("abc12345".into()),
            transcript_path: None,
        };
        let _tail = TranscriptTail::spawn_every(
            session.clone(),
            since,
            updates.clone(),
            TailMode::Rewatch,
            Duration::from_millis(10),
        );
        quiet(&mut rx).await;
        // The file is found by its session id in any project folder.
        // `find_transcript` reads CLAUDE_CONFIG_DIR, which a test cannot set
        // safely, so the tail gets the path from the turn's report.
        let projects = config.path().join("projects");
        assert_eq!(
            find_transcript_in(&projects, "abc12345-1111"),
            Some(path.clone())
        );
        assert_eq!(find_transcript_in(&projects, "ffff0000"), None);
        updates.send(TurnUpdate::Session(SessionRef {
            session_id: Some("abc12345-1111".into()),
            transcript_path: Some(path.clone()),
            ..session
        }));
        assert!(matches!(next(&mut rx).await, TurnUpdate::Session(_)));
        assert_eq!(
            next(&mut rx).await,
            TurnUpdate::Activity("Running a command".into())
        );
        append(
            &path,
            &format!(
                "{}\n{}\n",
                text(&now(), "<spoken>Found it.</spoken>"),
                tool(&now(), "Edit")
            ),
        );
        assert_eq!(
            next(&mut rx).await,
            TurnUpdate::Activity("Editing files".into())
        );
        quiet(&mut rx).await;
    }

    #[test]
    fn a_partial_line_is_read_again() {
        let (updates, mut rx) = TurnUpdates::channel();
        let since = Utc::now() - chrono::Duration::seconds(1);
        let mut tail = Tail::new(since, TailMode::Live);
        let line = format!("{}\n", tool(&now(), "Grep"));
        let (head, _) = line.split_at(10);
        tail.feed(
            Read {
                start: 0,
                bytes: head.as_bytes().to_vec(),
            },
            &updates,
        );
        assert_eq!(tail.offset, 0);
        assert!(rx.try_recv().is_err());
        tail.feed(
            Read {
                start: 0,
                bytes: line.as_bytes().to_vec(),
            },
            &updates,
        );
        assert_eq!(tail.offset, line.len() as u64);
        assert_eq!(
            rx.try_recv().unwrap(),
            TurnUpdate::Activity("Searching the code".into())
        );
    }
}
