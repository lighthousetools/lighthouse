//! Sources of connected data: which connector tools fill the mail, calendar,
//! issue and pull request caches, and how their results map to fields.
//!
//! See `docs/connected.md` for the format and the bundled presets.

use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// One source, such as the mailbox of a mail connector.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceConfig {
    /// Connector whose tools are called.
    pub connector: String,
    /// `mail`, `calendar`, `issues` or `code`. A preset sets it.
    #[serde(default)]
    pub domain: Option<String>,
    /// Bundled preset to start from, such as `github` or `linear`.
    #[serde(default)]
    pub preset: Option<String>,
    /// The service behind the source, such as `gmail`, `linear` or `github`, so
    /// apps can show its logo. Defaults to the preset, then the domain.
    #[serde(default)]
    pub service: Option<String>,
    /// The owner's account in this service. Defaults to `owner.accounts.<connector>`.
    #[serde(default)]
    pub account: Option<String>,
    /// Refresh interval such as `15m`. Defaults to 15 minutes.
    #[serde(default)]
    pub interval: Option<String>,
    /// Tool calls that fill the cache, by name. They replace preset entries of the same name.
    #[serde(default)]
    pub collections: BTreeMap<String, CollectionConfig>,
    /// Tool calls the owner can ask for, by name, such as `reply` or `comment`.
    #[serde(default)]
    pub actions: BTreeMap<String, ActionConfig>,
}

/// One tool call that returns items.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CollectionConfig {
    /// Tool name without the connector.
    pub tool: String,
    /// Arguments. Strings may hold placeholders such as `{{since}}` or `{{parent.external_id}}`.
    #[serde(default)]
    pub input: Option<Value>,
    /// Dotted path to the list in the parsed result. Empty for a top-level list.
    #[serde(default)]
    pub items: String,
    /// Item kind, such as `thread`, `event`, `issue`, `pull_request` or `check`.
    pub kind: String,
    /// Field mapping: target field to a path in the item, or a rule.
    #[serde(default)]
    pub fields: BTreeMap<String, FieldMap>,
    /// Flags every item of this collection gets, such as `review_requested`.
    #[serde(default)]
    pub flags: Vec<String>,
    /// Kind of parent items. The tool is called once per recently changed parent.
    #[serde(default)]
    pub parent: Option<String>,
    /// Most parents per refresh. Defaults to 20.
    #[serde(default)]
    pub parents_limit: Option<usize>,
}

/// Where a field comes from.
#[derive(Clone, Debug, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum FieldMap {
    /// A dotted path such as `user.login` or `labels.0.name`.
    Path(String),
    /// A path with a rule.
    Rule(FieldRule),
}

/// A path with a rule that turns its value into the field.
#[derive(Clone, Debug, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FieldRule {
    /// Dotted path.
    pub path: String,
    /// True if the value, or one entry of a list, contains this text, ignoring case.
    #[serde(default)]
    pub contains: Option<String>,
    /// True if the value equals this text, ignoring case.
    #[serde(default)]
    pub equals: Option<String>,
    /// Keeps the text after the first occurrence of this marker.
    #[serde(default)]
    pub after: Option<String>,
    /// Keeps the text after the last occurrence of this marker.
    #[serde(default)]
    pub after_last: Option<String>,
    /// Keeps the text before the first occurrence of this marker, after `after`.
    #[serde(default)]
    pub before: Option<String>,
}

/// A tool call the owner can ask for. Calls of `write` tools wait for approval.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionConfig {
    /// Tool name without the connector.
    pub tool: String,
    /// Arguments. Strings may hold `{{item.<field>}}` and `{{input.<field>}}` placeholders.
    #[serde(default)]
    pub input: Option<Value>,
    /// Kind of item the action needs, if any, such as `thread` for a reply.
    #[serde(default)]
    pub kind: Option<String>,
}

/// The tool that publishes posts. It must be a `write` tool, so every post
/// waits for the owner's approval.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PublishingConfig {
    /// Connector of the channel.
    pub connector: String,
    /// Tool name without the connector.
    pub tool: String,
    /// Arguments, with `{{post.<field>}}` placeholders such as `{{post.body}}`.
    #[serde(default)]
    pub input: Option<Value>,
    /// The network posts go to, such as `linkedin`, so apps can show its logo.
    #[serde(default)]
    pub service: Option<String>,
}

/// The conversations of the apps.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TalkConfig {
    /// Agent that answers. Without one, the Second Brain answers.
    #[serde(default)]
    pub agent: Option<String>,
}
