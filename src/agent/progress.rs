//! What an agent says and shows while its turn runs, before the final reply.
//!
//! - **Activity:** a short label for the tool the agent uses now ("Running a
//!   command"), shown on the agent overlay. [`activity_for_tool`] maps a
//!   harness's tool name to it, MCP tools by their server
//!   ([`activity_for_mcp`]).
//! - **Interims:** the agent itself writes a short acknowledgement ("On
//!   it.") before its first tool call and, at the conversational level, an
//!   update at a real milestone, each in its own `<spoken>` tags
//!   ([`feedback_delta`] asks for them). Only a message a tool call followed
//!   can be one: the final message never is. [`InterimTracker`] holds a
//!   message until a tool call proves that, and [`ProgressGate`] admits the
//!   lines the agent's `feedback` level allows, however many the model
//!   writes.
//!
//! Pure, no I/O and no clock: every time is passed in.

use std::time::{Duration, Instant};

use super::spoken::{last_complete_spoken_block, same_reply};

/// The least time between two admitted interims at the conversational
/// level.
pub const PROGRESS_MIN_GAP: Duration = Duration::from_secs(20);

/// How long a queued acknowledgement stays worth saying.
pub const ACK_TTL: Duration = Duration::from_secs(8);

/// How long a queued progress update stays worth saying.
pub const PROGRESS_TTL: Duration = Duration::from_secs(12);

/// The activity label of a tool with no label of its own.
pub const WORKING: &str = "Working";

// ---------------------------------------------------------------------------
// Feedback levels
// ---------------------------------------------------------------------------

/// How much an agent says while it works (`[[agents]] feedback`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
pub enum Feedback {
    /// Only the final reply.
    Summary,
    /// A short acknowledgement before the first tool call, then the reply.
    #[default]
    Brief,
    /// The acknowledgement, updates at real milestones at least
    /// [`PROGRESS_MIN_GAP`] apart, then the reply.
    Conversational,
}

impl Feedback {
    /// Every level, for messages.
    pub const ALL: &'static [Feedback] =
        &[Feedback::Summary, Feedback::Brief, Feedback::Conversational];

    /// Parse a config value (trimmed, case-insensitive). `None` if unknown,
    /// so the config can warn.
    pub fn parse(s: &str) -> Option<Feedback> {
        match s.trim().to_ascii_lowercase().as_str() {
            "summary" => Some(Feedback::Summary),
            "brief" => Some(Feedback::Brief),
            "conversational" => Some(Feedback::Conversational),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Feedback::Summary => "summary",
            Feedback::Brief => "brief",
            Feedback::Conversational => "conversational",
        }
    }
}

impl std::fmt::Display for Feedback {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// The acknowledgement request both talking levels share. A macro, so the
/// level texts are `concat!`-ed at compile time.
macro_rules! ack_delta {
    () => {
        " Before your first tool call, open your first message with a separate \
         acknowledgement in its own <spoken></spoken> tags: two to six words fitted to the \
         request, like \"On it.\", \"Let me check.\" or \"Pulling the results now.\", never a \
         question. Skip it when you answer without using a tool."
    };
}

/// [`feedback_delta`] for [`Feedback::Brief`].
const BRIEF_DELTA: &str = concat!(
    ack_delta!(),
    " Use no other tags until your final message, whose spoken reply comes last."
);

/// [`feedback_delta`] for [`Feedback::Conversational`].
const CONVERSATIONAL_DELTA: &str = concat!(
    ack_delta!(),
    " While you work, at a real milestone the user would want to hear (you found the cause, \
     finished a major step, or are starting something slow), you may give one short update in \
     its own <spoken></spoken> tags, in a message before your next tool call: one sentence \
     under twelve words, at most one every few steps; this is the one exception to not \
     narrating your steps. Updates follow the same spoken rules and never ask a question. Your \
     final message's spoken reply comes last."
);

/// What is appended to the voice instruction (the default or the agent's
/// `reply_instruction`) for a level. Empty for [`Feedback::Summary`]. It
/// starts with a space, so it reads on from the instruction's last
/// sentence. Like the instruction it is fixed when a session starts, so a
/// change applies from the next `--new`; a lower level applies at once,
/// since [`ProgressGate`] enforces the configured one.
pub fn feedback_delta(feedback: Feedback) -> &'static str {
    match feedback {
        Feedback::Summary => "",
        Feedback::Brief => BRIEF_DELTA,
        Feedback::Conversational => CONVERSATIONAL_DELTA,
    }
}

// ---------------------------------------------------------------------------
// Activity labels
// ---------------------------------------------------------------------------

/// The activity label for a tool, by its name as the harness reports it
/// (case-insensitive): Claude Code's tools, the phase 2 harnesses' (Codex,
/// OpenCode, Copilot), and `mcp__<server>__<tool>` through
/// [`activity_for_mcp`].
/// [`WORKING`] for anything else.
pub fn activity_for_tool(name: &str) -> String {
    let name = name.trim();
    if let Some(rest) = strip_prefix_ignore_case(name, "mcp__") {
        let (server, tool) = rest.split_once("__").unwrap_or((rest, ""));
        return activity_for_mcp(server, tool);
    }
    let label = match name.to_ascii_lowercase().as_str() {
        "bash" | "shell" | "command_execution" | "read_bash" | "write_bash" | "stop_bash"
        | "list_bash" => "Running a command",
        "read" | "view" => "Reading files",
        "edit" | "multiedit" | "notebookedit" | "write" | "apply_patch" | "file_change"
        | "create" => "Editing files",
        "grep" | "glob" => "Searching the code",
        "webfetch" | "web_fetch" => "Reading a web page",
        "websearch" | "web_search" => "Searching the web",
        "task" | "agent" | "collab_tool_call" | "read_agent" | "list_agents" | "write_agent" => {
            "Working with a helper"
        }
        "todowrite" | "todo_list" => "Planning",
        _ => WORKING,
    };
    label.to_string()
}

/// The activity label for an MCP tool, by its server (case-insensitive
/// keywords): a browser server "Using the browser", mail "Reading your
/// inbox", a calendar "Checking your calendar", whisrs "Reading your
/// recordings", any other "Using <Server Words>" from the server's name
/// (`claude_ai_Google_Drive` reads "Using Google Drive"). The tool is not
/// needed yet: the server says enough.
pub fn activity_for_mcp(server: &str, _tool: &str) -> String {
    let lower = server.to_ascii_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| lower.contains(w));
    let label = if has(&["chrome", "browser", "playwright"]) {
        "Using the browser"
    } else if has(&["mail"]) {
        "Reading your inbox"
    } else if has(&["calendar"]) {
        "Checking your calendar"
    } else if has(&["whisrs"]) {
        "Reading your recordings"
    } else {
        return match server_words(server) {
            Some(words) => format!("Using {words}"),
            None => WORKING.to_string(),
        };
    };
    label.to_string()
}

/// An MCP server's name as words: split at `_`, `-`, `.`, `:` and spaces,
/// without the `claude_ai_` and `plugin_` prefixes Claude Code adds and a
/// word repeated right after itself (`plugin_context7_context7` is
/// "Context7"), each word's first letter uppercased. `None` when no word is
/// left.
fn server_words(server: &str) -> Option<String> {
    let mut rest = server.trim();
    for prefix in ["claude_ai_", "plugin_"] {
        if let Some(r) = strip_prefix_ignore_case(rest, prefix) {
            rest = r;
        }
    }
    let mut words: Vec<String> = Vec::new();
    for word in rest
        .split(|c: char| matches!(c, '_' | '-' | '.' | ':') || c.is_whitespace())
        .filter(|w| !w.is_empty())
    {
        if words
            .last()
            .is_some_and(|last| last.eq_ignore_ascii_case(word))
        {
            continue;
        }
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            words.push(first.to_uppercase().chain(chars).collect());
        }
    }
    (!words.is_empty()).then(|| words.join(" "))
}

/// `s` without `prefix`, matched ASCII-case-insensitively.
fn strip_prefix_ignore_case<'a>(s: &'a str, prefix: &str) -> Option<&'a str> {
    let head = s.get(..prefix.len())?;
    head.eq_ignore_ascii_case(prefix)
        .then(|| &s[prefix.len()..])
}

// ---------------------------------------------------------------------------
// Interims
// ---------------------------------------------------------------------------

/// Holds the newest assistant text with a complete `<spoken>` pair until a
/// tool call proves it was not the turn's final message. Fed in transcript
/// order by a driver; one per turn.
///
/// A newer text with a pair replaces the held one; a text without one (the
/// agent's notes between its acknowledgement and the tool call) leaves it
/// held. So text then tool call releases the text, and text, text, end of
/// turn releases nothing: the final message is never spoken as an interim.
#[derive(Debug, Default)]
pub struct InterimTracker {
    pending: Option<String>,
}

impl InterimTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// An assistant text block, raw.
    pub fn on_text(&mut self, text: &str) {
        if last_complete_spoken_block(text).is_some() {
            self.pending = Some(text.to_string());
        }
    }

    /// A tool call: the held text, if any, was not the final message. Its
    /// raw text, for [`super::TurnUpdate::Interim`].
    pub fn on_tool(&mut self) -> Option<String> {
        self.pending.take()
    }

    /// The turn ended: what is held is the final message (or part of it),
    /// never an interim.
    pub fn end(&mut self) {
        self.pending = None;
    }
}

/// What an admitted interim is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterimKind {
    /// The turn's first: the acknowledgement.
    Ack,
    /// A later update (conversational only).
    Progress,
}

impl InterimKind {
    /// How long it stays worth saying while it waits in the queue.
    pub fn ttl(self) -> Duration {
        match self {
            InterimKind::Ack => ACK_TTL,
            InterimKind::Progress => PROGRESS_TTL,
        }
    }
}

/// Admits the interim lines a turn may speak at its agent's level, whatever
/// the model writes: [`Feedback::Summary`] none; [`Feedback::Brief`] only
/// the first, as the acknowledgement; [`Feedback::Conversational`] the
/// acknowledgement, then updates at least [`PROGRESS_MIN_GAP`] after the
/// last admitted line and not the same as any admitted before ([`same_reply`],
/// the reply dedupe). One per turn.
#[derive(Debug)]
pub struct ProgressGate {
    feedback: Feedback,
    /// The lines admitted so far, in order.
    admitted: Vec<String>,
    /// When the last one was admitted.
    last: Option<Instant>,
}

impl ProgressGate {
    pub fn new(feedback: Feedback) -> Self {
        Self {
            feedback,
            admitted: Vec::new(),
            last: None,
        }
    }

    /// Whether the spoken `line` ([`super::spoken::interim_line`]) is
    /// spoken, and as what. An admitted line counts towards the next
    /// decision; a refused one does not.
    pub fn admit(&mut self, line: &str, now: Instant) -> Option<InterimKind> {
        if line.trim().is_empty() {
            return None;
        }
        let kind = match (self.feedback, self.last) {
            (Feedback::Summary, _) => return None,
            (_, None) => InterimKind::Ack,
            (Feedback::Brief, Some(_)) => return None,
            (Feedback::Conversational, Some(last)) => {
                if now.saturating_duration_since(last) < PROGRESS_MIN_GAP
                    || self.admitted.iter().any(|a| same_reply(a, line))
                {
                    return None;
                }
                InterimKind::Progress
            }
        };
        self.admitted.push(line.to_string());
        self.last = Some(now);
        Some(kind)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::DEFAULT_REPLY_INSTRUCTION;

    #[test]
    fn feedback_parses_case_insensitively_and_unknown_is_none() {
        assert_eq!(Feedback::parse(" Brief "), Some(Feedback::Brief));
        assert_eq!(Feedback::parse("SUMMARY"), Some(Feedback::Summary));
        assert_eq!(
            Feedback::parse("conversational"),
            Some(Feedback::Conversational)
        );
        assert_eq!(Feedback::parse("chatty"), None);
        assert_eq!(Feedback::parse(""), None);
        assert_eq!(Feedback::default(), Feedback::Brief);
        for f in Feedback::ALL {
            assert_eq!(Feedback::parse(f.as_str()), Some(*f));
        }
    }

    #[test]
    fn claude_tools_have_labels() {
        for (tool, label) in [
            ("Bash", "Running a command"),
            ("Read", "Reading files"),
            ("Edit", "Editing files"),
            ("MultiEdit", "Editing files"),
            ("Write", "Editing files"),
            ("Grep", "Searching the code"),
            ("Glob", "Searching the code"),
            ("WebFetch", "Reading a web page"),
            ("WebSearch", "Searching the web"),
            ("Task", "Working with a helper"),
            ("Agent", "Working with a helper"),
            ("TodoWrite", "Planning"),
            ("bash", "Running a command"),
            ("BASH", "Running a command"),
        ] {
            assert_eq!(activity_for_tool(tool), label, "{tool}");
        }
    }

    #[test]
    fn phase_two_tools_have_labels() {
        for (tool, label) in [
            ("shell", "Running a command"),
            ("command_execution", "Running a command"),
            ("read", "Reading files"),
            ("edit", "Editing files"),
            ("apply_patch", "Editing files"),
            ("file_change", "Editing files"),
            ("web_search", "Searching the web"),
            ("todo_list", "Planning"),
            ("collab_tool_call", "Working with a helper"),
            ("webfetch", "Reading a web page"),
            ("todowrite", "Planning"),
            // Copilot's (1.0.103).
            ("view", "Reading files"),
            ("create", "Editing files"),
            ("read_bash", "Running a command"),
            ("write_bash", "Running a command"),
            ("stop_bash", "Running a command"),
            ("list_bash", "Running a command"),
            ("web_fetch", "Reading a web page"),
            ("read_agent", "Working with a helper"),
            ("write_agent", "Working with a helper"),
            ("list_agents", "Working with a helper"),
        ] {
            assert_eq!(activity_for_tool(tool), label, "{tool}");
        }
    }

    #[test]
    fn unknown_tools_are_working() {
        assert_eq!(activity_for_tool("StructuredOutput"), WORKING);
        assert_eq!(activity_for_tool(""), WORKING);
        assert_eq!(activity_for_tool("mcp__"), WORKING);
        assert_eq!(activity_for_mcp("plugin_", "x"), WORKING);
    }

    #[test]
    fn mcp_tools_are_labelled_by_their_server() {
        for (tool, label) in [
            ("mcp__claude-in-chrome__navigate", "Using the browser"),
            ("mcp__claude-in-chrome__computer", "Using the browser"),
            ("mcp__playwright__browser_click", "Using the browser"),
            ("mcp__claude_ai_Gmail__search_threads", "Reading your inbox"),
            ("mcp__gmail__send_message", "Reading your inbox"),
            (
                "mcp__claude_ai_Google_Calendar__list_events",
                "Checking your calendar",
            ),
            ("mcp__whisrs__get_transcript", "Reading your recordings"),
            ("MCP__WHISRS__list_recordings", "Reading your recordings"),
            ("mcp__claude_ai_Google_Drive__search", "Using Google Drive"),
            (
                "mcp__plugin_context7_context7__query-docs",
                "Using Context7",
            ),
            ("mcp__furcule__graph_check", "Using Furcule"),
            ("mcp__github", "Using Github"),
            ("mcp__linear-server__list_issues", "Using Linear Server"),
        ] {
            assert_eq!(activity_for_tool(tool), label, "{tool}");
        }
        assert_eq!(activity_for_mcp("my_db", "query"), "Using My Db");
        assert_eq!(activity_for_mcp("GitHub", "create_pr"), "Using GitHub");
    }

    #[test]
    fn the_tracker_releases_a_text_only_at_a_tool_call() {
        let mut t = InterimTracker::new();
        // Text, then a tool call: an interim.
        t.on_text("<spoken>On it.</spoken> Let me check.");
        assert_eq!(
            t.on_tool().as_deref(),
            Some("<spoken>On it.</spoken> Let me check.")
        );
        // Released once.
        assert_eq!(t.on_tool(), None);
        // Text without a pair holds nothing.
        t.on_text("Let me look at the logs.");
        assert_eq!(t.on_tool(), None);
        // Notes after the acknowledgement keep it held.
        t.on_text("<spoken>On it.</spoken>");
        t.on_text("Reading the config first.");
        assert_eq!(t.on_tool().as_deref(), Some("<spoken>On it.</spoken>"));
        // A newer pair replaces the held one.
        t.on_text("<spoken>On it.</spoken>");
        t.on_text("<spoken>Found the cause.</spoken>");
        assert_eq!(
            t.on_tool().as_deref(),
            Some("<spoken>Found the cause.</spoken>")
        );
        // Text, text, end of turn: nothing.
        t.on_text("<spoken>Fixed it.</spoken>");
        t.on_text("<spoken>Fixed it, tests pass.</spoken>");
        t.end();
        assert_eq!(t.on_tool(), None);
    }

    #[test]
    fn summary_admits_nothing() {
        let now = Instant::now();
        let mut gate = ProgressGate::new(Feedback::Summary);
        assert_eq!(gate.admit("On it.", now), None);
        assert_eq!(gate.admit("Found it.", now + Duration::from_secs(60)), None);
    }

    #[test]
    fn brief_admits_only_the_first_as_the_ack() {
        let now = Instant::now();
        let mut gate = ProgressGate::new(Feedback::Brief);
        assert_eq!(gate.admit("  ", now), None);
        assert_eq!(gate.admit("On it.", now), Some(InterimKind::Ack));
        assert_eq!(gate.admit("Found it.", now + Duration::from_secs(60)), None);
        assert_eq!(gate.admit("Later.", now + Duration::from_secs(600)), None);
    }

    #[test]
    fn conversational_spaces_updates_and_drops_repeats() {
        let t0 = Instant::now();
        let at = |s: u64| t0 + Duration::from_secs(s);
        let mut gate = ProgressGate::new(Feedback::Conversational);
        assert_eq!(gate.admit("On it.", t0), Some(InterimKind::Ack));
        // Too soon after the ack.
        assert_eq!(gate.admit("Found the cause.", at(19)), None);
        // A refused line does not move the gap.
        assert_eq!(
            gate.admit("Found the cause.", at(20)),
            Some(InterimKind::Progress)
        );
        assert_eq!(gate.admit("Now running the tests.", at(30)), None);
        // The same line again, up to whitespace: dropped, however late.
        assert_eq!(gate.admit(" Found  the cause. ", at(100)), None);
        assert_eq!(gate.admit("On it.", at(100)), None);
        assert_eq!(
            gate.admit("Tests are running.", at(100)),
            Some(InterimKind::Progress)
        );
        assert_eq!(gate.admit("Almost there.", at(119)), None);
        assert_eq!(
            gate.admit("Almost there.", at(120)),
            Some(InterimKind::Progress)
        );
    }

    #[test]
    fn interim_ttls() {
        assert_eq!(InterimKind::Ack.ttl(), ACK_TTL);
        assert_eq!(InterimKind::Progress.ttl(), PROGRESS_TTL);
        assert_eq!(ACK_TTL, Duration::from_secs(8));
        assert_eq!(PROGRESS_TTL, Duration::from_secs(12));
        assert_eq!(PROGRESS_MIN_GAP, Duration::from_secs(20));
    }

    #[test]
    fn the_deltas_build_on_the_voice_instruction() {
        assert_eq!(feedback_delta(Feedback::Summary), "");
        let ack = ack_delta!();
        let brief = feedback_delta(Feedback::Brief);
        let conversational = feedback_delta(Feedback::Conversational);
        assert!(brief.starts_with(ack) && conversational.starts_with(ack));
        for delta in [brief, conversational] {
            // Reads on from the instruction's last sentence.
            assert!(delta.starts_with(' '), "{delta}");
            assert!(delta.contains("<spoken></spoken>"));
            assert!(delta.contains("spoken reply comes last"));
            assert!(!delta.contains('\u{2014}'), "{delta}");
            let combined = format!("{DEFAULT_REPLY_INSTRUCTION}{delta}");
            assert!(combined.contains("session. Before your first tool call"));
        }
        // The default forbids narrating steps; only the conversational level
        // makes its updates the exception.
        assert!(DEFAULT_REPLY_INSTRUCTION.contains("Do not narrate your steps"));
        assert!(conversational.contains("the one exception to not narrating your steps"));
        assert!(!brief.contains("narrating"));
    }
}
