//! Bounded, caller-owned Telegram reads.

use crate::commands::messages::parse_since_date;
use crate::output::MessageInfo;
use crate::{Result, TelegramClient, TgError};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use serde::{Deserialize, Serialize};

fn invalid(message: &str) -> TgError {
    TgError::Other(message.into())
}
const MAX_TDLIB_ID: i64 = (1_i64 << 53) - 1;
fn valid_chat(id: i64) -> bool {
    id != 0 && (-MAX_TDLIB_ID..=MAX_TDLIB_ID).contains(&id)
}
fn valid_message(id: i64) -> bool {
    (1..=MAX_TDLIB_ID).contains(&id)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub struct Window {
    pub since: Option<i32>,
    pub until: Option<i32>,
}

impl Window {
    pub fn parse(since: Option<&str>, until: Option<&str>) -> Result<Self> {
        fn bound(value: &str) -> Result<(i32, u32)> {
            Ok((
                parse_since_date(value)?,
                chrono::DateTime::parse_from_rfc3339(value)
                    .map_or(0, |dt| dt.timestamp_subsec_nanos()),
            ))
        }
        fn ceiling(value: (i32, u32)) -> Result<i32> {
            value
                .0
                .checked_add(i32::from(value.1 > 0))
                .ok_or_else(|| invalid("date out of TDLib timestamp range"))
        }
        let since = since.map(bound).transpose()?;
        let until = until.map(bound).transpose()?;
        if matches!((since, until), (Some(s), Some(u)) if s >= u) {
            return Err(invalid("since must be earlier than until"));
        }
        let value = Self {
            since: since.map(ceiling).transpose()?,
            until: until.map(ceiling).transpose()?,
        };
        value.validate()?;
        Ok(value)
    }
    fn validate(&self) -> Result<()> {
        if matches!((self.since, self.until), (Some(s), Some(u)) if s > u) {
            return Err(invalid("since must be earlier than until"));
        }
        Ok(())
    }
    fn contains(&self, timestamp: i32) -> bool {
        self.since.is_none_or(|s| timestamp >= s) && self.until.is_none_or(|u| timestamp < u)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PublicChat {
    pub id: i64,
    pub title: String,
    pub kind: String,
    pub username: String,
}

impl crate::output::PlainText for PublicChat {
    fn to_plain_text(&self) -> String {
        format!(
            "{} {} [{}] {}",
            self.id, self.username, self.kind, self.title
        )
    }
}

pub fn username(value: &str) -> Result<String> {
    let name = value
        .strip_prefix('@')
        .ok_or_else(|| invalid("public source must be an exact @username"))?;
    if name.is_empty()
        || name.len() > 32
        || !name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
    {
        return Err(invalid("invalid public @username"));
    }
    Ok(name.to_ascii_lowercase())
}

// TDLib interprets message IDs and private-link permissions. This check only
// excludes unrelated URLs and oversized inputs before asking TDLib.
pub fn validate_link(link: &str) -> Result<()> {
    if link.len() > 2048 {
        return Err(invalid("message link is too long"));
    }
    let url = reqwest::Url::parse(link).map_err(|_| invalid("invalid Telegram message URL"))?;
    if !matches!(url.scheme(), "http" | "https")
        || !matches!(
            url.host_str(),
            Some("t.me" | "telegram.me" | "telegram.dog")
        )
        || !url.username().is_empty()
        || url.password().is_some()
        || url.port().is_some()
    {
        return Err(invalid("unsupported Telegram message URL"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Source {
    Chat {
        chat_id: i64,
    },
    Thread {
        post_chat_id: i64,
        post_message_id: i64,
        chat_id: i64,
        thread_id: i64,
        root_ids: Vec<i64>,
    },
}

impl Source {
    pub fn chat_id(&self) -> i64 {
        match self {
            Self::Chat { chat_id } | Self::Thread { chat_id, .. } => *chat_id,
        }
    }
    fn validate(&self) -> Result<()> {
        if !valid_chat(self.chat_id()) {
            return Err(invalid("cursor has invalid chat ID"));
        }
        if let Self::Thread {
            post_chat_id,
            post_message_id,
            thread_id,
            root_ids,
            ..
        } = self
            && (!valid_chat(*post_chat_id)
                || !valid_message(*post_message_id)
                || !valid_message(*thread_id)
                || root_ids.is_empty()
                || root_ids.len() > 100
                || root_ids.iter().any(|id| !valid_message(*id)))
        {
            return Err(invalid("cursor has invalid thread identity"));
        }
        Ok(())
    }
    fn is_root(&self, id: i64) -> bool {
        matches!(self, Self::Thread {root_ids, ..} if root_ids.contains(&id))
    }
    fn start_anchor(&self) -> i64 {
        match self {
            Self::Chat { .. } => 1 << 20,
            Self::Thread { root_ids, .. } => *root_ids.iter().min().unwrap(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Discussion {
    pub source: Source,
    pub post: MessageInfo,
    pub roots: Vec<MessageInfo>,
    pub reply_count: Option<i32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    v: u8,
    source: Source,
    window: Window,
    before: i64,
}
impl Cursor {
    fn encode(&self) -> Result<String> {
        Ok(URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).map_err(|_| invalid("cannot encode cursor"))?))
    }
    fn decode(value: &str) -> Result<Self> {
        if value.len() > 8192 {
            return Err(invalid("cursor is too large"));
        }
        let bytes = URL_SAFE_NO_PAD
            .decode(value)
            .map_err(|_| invalid("invalid cursor encoding"))?;
        let cursor: Self =
            serde_json::from_slice(&bytes).map_err(|_| invalid("invalid cursor schema"))?;
        cursor.source.validate()?;
        cursor.window.validate()?;
        if cursor.v != 1 || !(0..=MAX_TDLIB_ID).contains(&cursor.before) {
            return Err(invalid("invalid cursor version or position"));
        }
        Ok(cursor)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct PageRequest {
    pub username: Option<String>,
    pub chat: Option<i64>,
    pub link: Option<String>,
    pub since: Option<String>,
    pub until: Option<String>,
    pub cursor: Option<String>,
    #[serde(default = "page_limit")]
    pub limit: i32,
}
fn page_limit() -> i32 {
    20
}

impl Default for PageRequest {
    fn default() -> Self {
        Self {
            username: None,
            chat: None,
            link: None,
            since: None,
            until: None,
            cursor: None,
            limit: page_limit(),
        }
    }
}

impl From<crate::cli::HistoryArgs> for PageRequest {
    fn from(args: crate::cli::HistoryArgs) -> Self {
        Self {
            username: args.username,
            chat: args.chat,
            link: None,
            since: args.page.since,
            until: args.page.until,
            cursor: args.page.cursor,
            limit: args.page.limit,
        }
    }
}
impl From<crate::cli::ThreadArgs> for PageRequest {
    fn from(args: crate::cli::ThreadArgs) -> Self {
        Self {
            username: None,
            chat: None,
            link: args.link,
            since: args.page.since,
            until: args.page.until,
            cursor: args.page.cursor,
            limit: args.page.limit,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Page {
    pub source: Source,
    pub messages: Vec<MessageInfo>,
    pub has_more: bool,
    pub next_cursor: Option<String>,
    pub incomplete: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub discussion: Option<Discussion>,
}

impl crate::output::PlainText for Page {
    fn to_plain_text(&self) -> String {
        serde_json::to_string_pretty(self).unwrap()
    }
}

#[async_trait::async_trait]
pub trait HistorySource: Send + Sync {
    async fn batch(
        &self,
        source: &Source,
        from: i64,
        offset: i32,
        limit: i32,
    ) -> Result<Vec<MessageInfo>>;
}
#[async_trait::async_trait]
impl<C: TelegramClient> HistorySource for C {
    async fn batch(
        &self,
        source: &Source,
        from: i64,
        offset: i32,
        limit: i32,
    ) -> Result<Vec<MessageInfo>> {
        self.read_history_batch(source, from, offset, limit).await
    }
}

/// Newest first. Short batches never prove exhaustion. A start-of-history
/// probe supplies the oldest ID; reaching that ID does prove exhaustion.
async fn paginate<C: HistorySource>(client: &C, mut cursor: Cursor, limit: i32) -> Result<Page> {
    if !(1..=100).contains(&limit) {
        return Err(invalid("limit must be between 1 and 100"));
    }
    let mut messages = Vec::new();
    let mut done = false;
    let mut stalled = 0;
    let mut floor = None;
    {
        for _ in 0..5 {
            let first = client
                .batch(&cursor.source, cursor.source.start_anchor(), -99, 100)
                .await?;
            if let Some(id) = first
                .iter()
                .filter(|m| m.chat_id == cursor.source.chat_id())
                .map(|m| m.id)
                .filter(|id| *id > 0)
                .min()
            {
                floor = Some(id);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
    }
    for _ in 0..20 {
        let mut batch = client.batch(&cursor.source, cursor.before, 0, 100).await?;
        batch.retain(|m| {
            m.chat_id == cursor.source.chat_id()
                && m.id > 0
                && (cursor.before == 0 || m.id < cursor.before)
        });
        batch.sort_unstable_by_key(|m| std::cmp::Reverse(m.id));
        batch.dedup_by_key(|m| m.id);
        if batch.is_empty() {
            stalled += 1;
            if stalled == 5 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            continue;
        }
        stalled = 0;
        for message in batch {
            // Look ahead without advancing past a message the caller has not seen.
            if !cursor.source.is_root(message.id)
                && cursor.window.contains(message.timestamp)
                && messages.len() == limit as usize
            {
                return page(cursor, messages, false, false);
            }
            cursor.before = message.id;
            let below = cursor.window.since.is_some_and(|s| message.timestamp < s);
            let at_floor = floor.is_some_and(|id| message.id <= id);
            if !cursor.source.is_root(message.id) && cursor.window.contains(message.timestamp) {
                messages.push(message);
            }
            if below || at_floor {
                done = true;
                break;
            }
        }
        if done {
            break;
        }
    }
    page(cursor, messages, done, !done)
}

fn page(cursor: Cursor, messages: Vec<MessageInfo>, done: bool, incomplete: bool) -> Result<Page> {
    Ok(Page {
        source: cursor.source.clone(),
        messages,
        has_more: !done,
        next_cursor: if done { None } else { Some(cursor.encode()?) },
        incomplete,
        discussion: None,
    })
}

pub async fn history<C: TelegramClient>(client: &C, req: PageRequest) -> Result<Page> {
    read_page(client, req, false).await
}
pub async fn thread<C: TelegramClient>(client: &C, req: PageRequest) -> Result<Page> {
    read_page(client, req, true).await
}
async fn read_page<C: TelegramClient>(client: &C, req: PageRequest, thread: bool) -> Result<Page> {
    if !(1..=100).contains(&req.limit) {
        return Err(invalid("limit must be between 1 and 100"));
    }
    if req.username.is_some() && req.chat.is_some()
        || thread && (req.chat.is_some() || req.username.is_some())
        || !thread && req.link.is_some()
    {
        return Err(invalid("conflicting source arguments"));
    }
    let window = Window::parse(req.since.as_deref(), req.until.as_deref())?;
    let mut discussion = None;
    let explicit_source = if thread {
        if let Some(link) = req.link.as_deref() {
            validate_link(link)?;
            let value = client.read_discussion(link).await?;
            let source = value.source.clone();
            discussion = Some(value);
            Some(source)
        } else {
            None
        }
    } else if let Some(name) = req.username.as_deref() {
        let name = username(name)?;
        Some(Source::Chat {
            chat_id: client.find_chat_by_username(&name).await?,
        })
    } else {
        req.chat.map(|chat_id| Source::Chat { chat_id })
    };
    let cursor = if let Some(value) = req.cursor.as_deref() {
        let cursor = Cursor::decode(value)?;
        if matches!(cursor.source, Source::Thread { .. }) != thread
            || explicit_source
                .as_ref()
                .is_some_and(|s| s != &cursor.source)
            || req.since.is_some() && window.since != cursor.window.since
            || req.until.is_some() && window.until != cursor.window.until
        {
            return Err(invalid("cursor conflicts with source or date arguments"));
        }
        cursor
    } else {
        let source = explicit_source.ok_or_else(|| invalid("a source or cursor is required"))?;
        source.validate()?;
        Cursor {
            v: 1,
            source,
            window,
            before: 0,
        }
    };
    // Re-read thread metadata on continuation too. A cursor is untrusted and
    // cannot establish access to a discussion or invent its root identities.
    if thread
        && discussion.is_none()
        && let Source::Thread {
            post_chat_id,
            post_message_id,
            ..
        } = &cursor.source
    {
        let value = client
            .read_discussion_by_id(*post_chat_id, *post_message_id)
            .await?;
        if value.source != cursor.source {
            return Err(invalid("discussion identity changed; restart reading"));
        }
        discussion = Some(value);
    }
    let mut result = paginate(client, cursor, req.limit).await?;
    result.discussion = discussion;
    Ok(result)
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolveRequest {
    pub username: String,
}
pub async fn resolve<C: TelegramClient>(client: &C, req: ResolveRequest) -> Result<PublicChat> {
    client.read_public_chat(&username(&req.username)?).await
}
#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct LinkRequest {
    pub link: String,
}
pub async fn message_link<C: TelegramClient>(client: &C, req: LinkRequest) -> Result<MessageInfo> {
    validate_link(&req.link)?;
    client.read_message_link(&req.link).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn date_window_is_inclusive_then_exclusive() {
        let window = Window::parse(Some("2026-11-12"), Some("2026-11-13T01:00:00+01:00")).unwrap();
        assert!(window.contains(1794441600));
        assert!(window.contains(1794527999));
        assert!(!window.contains(1794528000));
        assert!(Window::parse(Some("2026-11-13"), Some("2026-11-12")).is_err());
        assert!(Window::parse(None, Some("2026-11-12T12:00:00")).is_err());
        let fractional = Window::parse(
            Some("2026-11-12T00:00:00.5Z"),
            Some("2026-11-12T00:00:02.5Z"),
        )
        .unwrap();
        assert!(!fractional.contains(1794441600));
        assert!(fractional.contains(1794441601));
        assert!(fractional.contains(1794441602));
        assert!(!fractional.contains(1794441603));
    }

    struct History {
        messages: Vec<MessageInfo>,
        empty: std::sync::atomic::AtomicUsize,
        width: usize,
        no_floor: bool,
    }
    #[async_trait::async_trait]
    impl HistorySource for History {
        async fn batch(
            &self,
            _source: &Source,
            from: i64,
            offset: i32,
            _limit: i32,
        ) -> Result<Vec<MessageInfo>> {
            if offset < 0 {
                if self.no_floor {
                    return Ok(vec![]);
                }
                return Ok(self
                    .messages
                    .iter()
                    .take(self.width)
                    .rev()
                    .cloned()
                    .collect());
            }
            if self
                .empty
                .fetch_update(
                    std::sync::atomic::Ordering::SeqCst,
                    std::sync::atomic::Ordering::SeqCst,
                    |n| n.checked_sub(1),
                )
                .is_ok()
            {
                return Ok(vec![]);
            }
            Ok(self
                .messages
                .iter()
                .rev()
                .filter(|m| from == 0 || m.id < from)
                .take(self.width)
                .cloned()
                .collect())
        }
    }
    fn fake_history(count: i64, width: usize, empty: usize) -> History {
        let sample = crate::client::mock::MockClient::default().messages[0].clone();
        History {
            messages: (1..=count)
                .map(|id| {
                    let mut m = sample.clone();
                    m.id = id << 20;
                    m.chat_id = 1;
                    m.timestamp = id as i32;
                    m
                })
                .collect(),
            width,
            empty: empty.into(),
            no_floor: false,
        }
    }
    fn cursor() -> Cursor {
        Cursor {
            v: 1,
            source: Source::Chat { chat_id: 1 },
            window: Window::default(),
            before: 0,
        }
    }

    #[tokio::test]
    async fn cold_short_pages_continue_without_duplicates_or_gaps() {
        let source = fake_history(9, 1, 2);
        let mut position = cursor();
        let mut ids = Vec::new();
        loop {
            let page = paginate(&source, position, 2).await.unwrap();
            ids.extend(page.messages.iter().map(|m| m.id >> 20));
            assert!(!page.incomplete);
            if !page.has_more {
                assert!(page.next_cursor.is_none());
                break;
            }
            position = Cursor::decode(&page.next_cursor.unwrap()).unwrap();
        }
        assert_eq!(ids, vec![9, 8, 7, 6, 5, 4, 3, 2, 1]);
    }

    #[tokio::test]
    async fn date_filters_keep_their_position_and_bounds_on_continuation() {
        let source = fake_history(20, 3, 0);
        let mut position = cursor();
        position.window = Window {
            since: Some(5),
            until: Some(10),
        };
        let first = paginate(&source, position, 2).await.unwrap();
        assert_eq!(
            first
                .messages
                .iter()
                .map(|m| m.timestamp)
                .collect::<Vec<_>>(),
            vec![9, 8]
        );
        let second = paginate(
            &source,
            Cursor::decode(first.next_cursor.as_deref().unwrap()).unwrap(),
            10,
        )
        .await
        .unwrap();
        assert_eq!(
            second
                .messages
                .iter()
                .map(|m| m.timestamp)
                .collect::<Vec<_>>(),
            vec![7, 6, 5]
        );
        assert!(!second.has_more);
    }

    #[tokio::test]
    async fn scan_budget_and_empty_cache_do_not_claim_exhaustion() {
        let mut source = fake_history(50, 1, 0);
        let mut position = cursor();
        position.window.until = Some(2);
        let first = paginate(&source, position, 2).await.unwrap();
        assert!(first.messages.is_empty());
        assert!(first.has_more && first.incomplete);
        assert!(
            Cursor::decode(first.next_cursor.as_deref().unwrap())
                .unwrap()
                .before
                > 0
        );
        source.no_floor = true;
        source.empty.store(10, std::sync::atomic::Ordering::SeqCst);
        let empty = paginate(&source, cursor(), 2).await.unwrap();
        assert!(empty.messages.is_empty());
        assert!(empty.has_more && empty.incomplete && empty.next_cursor.is_some());
    }

    #[tokio::test]
    async fn thread_roots_are_separate_from_comments() {
        let source = fake_history(4, 4, 0);
        let mut position = cursor();
        position.source = Source::Thread {
            post_chat_id: -100,
            post_message_id: 999,
            chat_id: 1,
            thread_id: 1 << 20,
            root_ids: vec![1 << 20],
        };
        let page = paginate(&source, position, 20).await.unwrap();
        assert_eq!(
            page.messages.iter().map(|m| m.id >> 20).collect::<Vec<_>>(),
            vec![4, 3, 2]
        );
        assert!(!page.has_more);
    }

    #[test]
    fn cursors_are_untrusted_and_contain_only_read_coordinates() {
        assert!(Cursor::decode("garbage").is_err());
        assert!(Cursor::decode(&"A".repeat(8193)).is_err());
        let mut position = cursor();
        position.v = 2;
        assert!(Cursor::decode(&position.encode().unwrap()).is_err());
        position.v = 1;
        position.before = -1;
        assert!(Cursor::decode(&position.encode().unwrap()).is_err());
        let encoded = cursor().encode().unwrap();
        let data: serde_json::Value =
            serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
        assert_eq!(
            data,
            serde_json::json!({"v":1,"source":{"kind":"chat","chat_id":1},"window":{"since":null,"until":null},"before":0})
        );
    }

    #[tokio::test]
    async fn cursor_rejects_conflicting_sources_and_bounds_before_reading() {
        let client = crate::client::mock::MockClient::default();
        for req in [
            PageRequest {
                chat: Some(2),
                ..Default::default()
            },
            PageRequest {
                since: Some("2026-11-12".into()),
                ..Default::default()
            },
            PageRequest {
                link: Some("https://t.me/channel/1".into()),
                ..Default::default()
            },
        ] {
            let req = PageRequest {
                cursor: Some(cursor().encode().unwrap()),
                limit: 20,
                ..req
            };
            assert!(history(&client, req).await.is_err());
        }
    }

    #[test]
    fn links_and_usernames_reject_unrelated_inputs() {
        for value in ["name", "@", "@name space", "@https://t.me/name"] {
            assert!(username(value).is_err());
        }
        assert_eq!(username("@LLM_under_hood").unwrap(), "llm_under_hood");
        for value in [
            "https://t.me.evil.test/a/1",
            "file:///tmp/a",
            "https://user@t.me/a/1",
            "tg://resolve?domain=a",
            "https://t.me:8443/a/1",
        ] {
            assert!(validate_link(value).is_err());
        }
        assert!(validate_link("https://t.me/c/123/456").is_ok());
    }
}
