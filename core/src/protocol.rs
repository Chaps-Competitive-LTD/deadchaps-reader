//! What the reader program and the Companion app say to each other: one JSON
//! object a line, the app writing to the reader's stdin and reading its
//! stdout. The reader's log goes to stderr.
//!
//! The reader never uses the network. The one thing it needs from outside,
//! whether Valve's CDN has a replay's metadata file, it asks the app
//! ([`FromReader::AskValve`]), and the app answers ([`ToReader::Valve`]).

use crate::view::{EndedMatch, MemoryView};
use serde::{Deserialize, Serialize};

/// Bumped when a message changes shape; the app refuses a reader that
/// speaks another.
pub const PROTOCOL: u32 = 1;

/// From the app to the reader.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToReader {
    /// The player's own on/off setting for the reader.
    SetEnabled(bool),
    /// The DeadChaps site's kill switch, which applies to every install.
    SetAllowed(bool),
    /// The player's Steam account id, when the app knows it.
    Account(Option<String>),
    /// A match that ended without an end line in memory, identified instead
    /// by the Deadlock API's live match lookup.
    AddEnded(EndedMatch),
    /// The answer to [`FromReader::AskValve`].
    Valve { id: u64, has: bool },
    /// Close the game and exit. The reader exits when stdin closes too.
    Stop,
}

/// From the reader to the app.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FromReader {
    /// The first line: what this reader is.
    Hello { version: String, protocol: u32 },
    /// The view, whole, each time it changes.
    View(Box<MemoryView>),
    /// A replay address found in the game's memory, sent once per address.
    Salts(String),
    /// Whether Valve's CDN has the metadata file at `url`, answered with
    /// [`ToReader::Valve`] and the same id.
    AskValve { id: u64, url: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_one_short_json_object_each() {
        assert_eq!(serde_json::to_string(&ToReader::SetEnabled(true)).unwrap(), r#"{"set_enabled":true}"#);
        assert_eq!(serde_json::to_string(&ToReader::Stop).unwrap(), r#""stop""#);
        assert_eq!(
            serde_json::to_string(&ToReader::Valve { id: 3, has: false }).unwrap(),
            r#"{"valve":{"id":3,"has":false}}"#
        );
        assert_eq!(
            serde_json::to_string(&FromReader::Salts("http://replay1.valve.net/1422450/1_2.meta.bz2".into())).unwrap(),
            r#"{"salts":"http://replay1.valve.net/1422450/1_2.meta.bz2"}"#
        );
    }

    #[test]
    fn a_view_goes_there_and_back_whole() {
        let view = MemoryView { enabled: true, allowed: true, attached: true, steam_id: Some("1161088555".into()), ..MemoryView::default() };
        let line = serde_json::to_string(&FromReader::View(Box::new(view.clone()))).unwrap();
        assert_eq!(serde_json::from_str::<FromReader>(&line).unwrap(), FromReader::View(Box::new(view)));
    }
}
