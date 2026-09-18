//! The focused text field, read over AT-SPI.
//!
//! Qt, GTK, Firefox, Chromium and Electron expose their widgets on the
//! session's accessibility bus, but only once `org.a11y.Status.IsEnabled`
//! is true, which is what a screen reader sets. [`A11y::connect`] can set
//! it and [`A11y::shutdown`] puts it back the way it was found.
//!
//! One background task follows focus: every `object:state-changed:focused`
//! (and `object:text-caret-moved`) event names an object, and the last one
//! that implements the Text interface is kept. [`A11y::focused_text`] checks
//! that object is still focused and reads the text around its caret. Every
//! call to an application is bounded by [`CALL_TIMEOUT`], so a hung
//! application costs a dictation its context, not the dictation.
//!
//! Nothing here writes to an application. Text is only ever read.

use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};

use atspi_common::events::object::{StateChangedEvent, TextCaretMovedEvent};
use atspi_common::events::{Event, ObjectEvents};
use atspi_common::{Interface, ObjectRefOwned, Role, State, StateSet};
use atspi_connection::AccessibilityConnection;
use atspi_proxies::accessible::{AccessibleProxy, ObjectRefExt};
use atspi_proxies::bus::{BusProxy, StatusProxy};
use atspi_proxies::text::TextProxy;
use futures_lite::StreamExt as _;
use tokio::sync::RwLock;
use zbus::proxy::CacheProperties;

/// Longest any one call to an application may take.
pub const CALL_TIMEOUT: Duration = Duration::from_millis(300);
/// Characters read before the caret.
pub const BEFORE_CHARS: i32 = 600;
/// Characters read after the caret.
pub const AFTER_CHARS: i32 = 200;
/// Most objects the startup walk visits before giving up.
const WALK_LIMIT: usize = 400;
/// Longest the startup walk may take.
const WALK_BUDGET: Duration = Duration::from_millis(1500);

/// The text field that has keyboard focus, as read at one moment.
#[derive(Debug, Clone)]
pub struct FocusedText {
    /// The toolkit's application name ("konsole", "Firefox").
    pub app: String,
    /// The AT-SPI role name ("text", "terminal", "entry", "password text").
    pub role: String,
    /// A password field. `before` and `after` are empty for one.
    pub password: bool,
    /// Caret offset in characters.
    pub caret: i32,
    /// Length of the field in characters.
    pub length: i32,
    /// Up to [`BEFORE_CHARS`] characters before the caret.
    pub before: String,
    /// Up to [`AFTER_CHARS`] characters after the caret.
    pub after: String,
    /// The field itself, for reading it again later.
    pub handle: TextHandle,
}

/// A reference to one text object, for re-reading the same field later
/// (the caret may have moved and the text may have changed in between).
/// Cheap to clone; every method is one bounded D-Bus call.
#[derive(Debug, Clone)]
pub struct TextHandle {
    conn: zbus::Connection,
    object: ObjectRefOwned,
}

impl TextHandle {
    async fn text(&self) -> anyhow::Result<TextProxy<'_>> {
        let name = self
            .object
            .name()
            .ok_or_else(|| anyhow::anyhow!("text object without a bus name"))?
            .clone();
        let proxy = TextProxy::builder(&self.conn)
            .destination(name)?
            .path(self.object.path().clone())?
            .cache_properties(CacheProperties::No)
            .build()
            .await?;
        Ok(proxy)
    }

    /// The characters between `from` and `to` (offsets in characters,
    /// `to` excluded). Offsets are clamped to the field.
    pub async fn read(&self, from: i32, to: i32) -> anyhow::Result<String> {
        let text = self.text().await?;
        let length = bounded("character count", text.character_count()).await?;
        let from = from.clamp(0, length.max(0));
        let to = to.clamp(from, length.max(0));
        if from == to {
            return Ok(String::new());
        }
        bounded("text", text.get_text(from, to)).await
    }

    /// Where the caret is now, in characters.
    pub async fn caret(&self) -> anyhow::Result<i32> {
        let text = self.text().await?;
        bounded("caret offset", text.caret_offset()).await
    }

}

/// What the follower remembers about the focused object.
#[derive(Debug, Clone)]
struct Tracked {
    object: ObjectRefOwned,
    app: String,
    role: Role,
}

/// A connection to the accessibility bus that follows keyboard focus.
pub struct A11y {
    conn: AccessibilityConnection,
    tracked: Arc<RwLock<Option<Tracked>>>,
    /// parla set `IsEnabled`; `shutdown` clears it again.
    enabled_by_us: bool,
    follower: tokio::task::JoinHandle<()>,
}

impl A11y {
    /// Open the accessibility bus and start following focus. With
    /// `enable`, `org.a11y.Status.IsEnabled` is set when it is not already,
    /// so toolkits load their AT-SPI bridge; the state found is logged and
    /// [`A11y::shutdown`] restores it.
    pub async fn connect(enable: bool) -> anyhow::Result<Self> {
        let session = crate::bus::session().await?;
        let status = StatusProxy::new(&session).await?;
        let was_enabled = bounded("IsEnabled", status.is_enabled()).await?;
        let mut enabled_by_us = false;
        if enable && !was_enabled {
            bounded("set IsEnabled", status.set_is_enabled(true)).await?;
            enabled_by_us = true;
        }
        let connected = Self::open(&session, was_enabled, enabled_by_us).await;
        if connected.is_err() && enabled_by_us {
            // Nothing will call `shutdown` for a connection that never
            // came to be; do not leave the flag on for it.
            match bounded("clear IsEnabled", status.set_is_enabled(false)).await {
                Ok(()) => tracing::info!("a11y: IsEnabled restored to false"),
                Err(e) => tracing::warn!("a11y: could not restore IsEnabled: {e:#}"),
            }
        }
        connected
    }

    /// The part of [`A11y::connect`] after `IsEnabled` is settled.
    async fn open(
        session: &zbus::Connection,
        was_enabled: bool,
        enabled_by_us: bool,
    ) -> anyhow::Result<Self> {
        let address = bounded(
            "a11y bus address",
            BusProxy::new(session).await?.get_address(),
        )
        .await?;
        let conn = tokio::time::timeout(
            Duration::from_secs(2),
            AccessibilityConnection::from_address(address.parse()?),
        )
        .await
        .map_err(|_| anyhow::anyhow!("a11y bus: connect timed out"))??;
        tracing::info!(
            "a11y bus reachable; IsEnabled was {was_enabled}{}",
            if enabled_by_us { ", set to true" } else { "" }
        );
        conn.register_event::<StateChangedEvent>().await?;
        conn.register_event::<TextCaretMovedEvent>().await?;

        let tracked = Arc::new(RwLock::new(None));
        let follower = tokio::spawn(follow(conn.clone(), Arc::clone(&tracked)));
        let a11y = Self {
            conn,
            tracked,
            enabled_by_us,
            follower,
        };
        // Whatever was focused before parla connected never emitted an
        // event; look for it once.
        if let Some(t) = a11y.find_focused().await {
            tracing::debug!("focused text object at startup: {} ({})", t.app, t.role);
            *a11y.tracked.write().await = Some(t);
        }
        Ok(a11y)
    }

    /// Whether the a11y bus can be reached, and whether `IsEnabled` is set.
    /// For `--check`; connects and disconnects.
    pub async fn status() -> anyhow::Result<bool> {
        let session = crate::bus::session().await?;
        let enabled = bounded("IsEnabled", StatusProxy::new(&session).await?.is_enabled()).await?;
        let address = bounded(
            "a11y bus address",
            BusProxy::new(&session).await?.get_address(),
        )
        .await?;
        tokio::time::timeout(
            Duration::from_secs(2),
            AccessibilityConnection::from_address(address.parse()?),
        )
        .await
        .map_err(|_| anyhow::anyhow!("a11y bus: connect timed out"))??;
        Ok(enabled)
    }

    /// Whether parla turned `IsEnabled` on.
    pub fn enabled_by_us(&self) -> bool {
        self.enabled_by_us
    }

    /// Stop following focus and, if parla set `IsEnabled`, clear it.
    pub async fn shutdown(&self) {
        self.follower.abort();
        if !self.enabled_by_us {
            return;
        }
        let restore = async {
            let session = crate::bus::session().await?;
            let status = StatusProxy::new(&session).await?;
            bounded("clear IsEnabled", status.set_is_enabled(false)).await
        };
        match restore.await {
            Ok(()) => tracing::info!("a11y: IsEnabled restored to false"),
            Err(e) => tracing::warn!("a11y: could not restore IsEnabled: {e:#}"),
        }
    }

    /// The focused text field and the text around its caret, or None when
    /// nothing with a Text interface has focus. A password field comes
    /// back with `password` set and no text.
    pub async fn focused_text(&self) -> anyhow::Result<Option<FocusedText>> {
        let cached = self.tracked.read().await.clone();
        let tracked = match cached {
            Some(t) if self.still_focused(&t).await => Some(t),
            _ => {
                // Stale or empty: the event may have been missed, or the
                // object was focused before parla connected.
                let found = self.find_focused().await;
                if found.is_some() {
                    *self.tracked.write().await = found.clone();
                }
                found
            }
        };
        let Some(t) = tracked else {
            return Ok(None);
        };
        let handle = TextHandle {
            conn: self.conn.connection().clone(),
            object: t.object.clone(),
        };
        let password = t.role == Role::PasswordText;
        let text = handle.text().await?;
        let caret = bounded("caret offset", text.caret_offset()).await?;
        let length = bounded("character count", text.character_count()).await?;
        let (before, after) = if password {
            (String::new(), String::new())
        } else {
            let caret = caret.clamp(0, length.max(0));
            let from = (caret - BEFORE_CHARS).max(0);
            let to = (caret + AFTER_CHARS).min(length.max(0));
            let before = if from < caret {
                bounded("text before caret", text.get_text(from, caret)).await?
            } else {
                String::new()
            };
            let after = if caret < to {
                bounded("text after caret", text.get_text(caret, to)).await?
            } else {
                String::new()
            };
            (before, after)
        };
        Ok(Some(FocusedText {
            app: t.app,
            role: t.role.name().to_string(),
            password,
            caret,
            length,
            before,
            after,
            handle,
        }))
    }

    async fn still_focused(&self, t: &Tracked) -> bool {
        let Ok(proxy) = t.object.as_accessible_proxy(self.conn.connection()).await else {
            return false;
        };
        match bounded("state", proxy.get_state()).await {
            Ok(state) => has_focus(state),
            Err(e) => {
                tracing::debug!("focused object unreadable: {e:#}");
                false
            }
        }
    }

    /// Walk the active windows for the object that has focus and a Text
    /// interface. Bounded in objects and time; None when nothing is found
    /// within the budget.
    async fn find_focused(&self) -> Option<Tracked> {
        let started = Instant::now();
        let conn = self.conn.connection();
        let root = self.conn.root_accessible_on_registry().await.ok()?;
        let apps = bounded("registry children", root.get_children())
            .await
            .ok()?;
        let mut visited = 0usize;
        for app in apps {
            if started.elapsed() > WALK_BUDGET {
                break;
            }
            let Ok(app_proxy) = app.as_accessible_proxy(conn).await else {
                continue;
            };
            let Ok(windows) = bounded("children", app_proxy.get_children()).await else {
                continue;
            };
            for window in windows {
                let Ok(w) = window.as_accessible_proxy(conn).await else {
                    continue;
                };
                let Ok(state) = bounded("state", w.get_state()).await else {
                    continue;
                };
                if !state.contains(State::Active) {
                    continue;
                }
                let mut stack = vec![window];
                while let Some(object) = stack.pop() {
                    visited += 1;
                    if visited > WALK_LIMIT || started.elapsed() > WALK_BUDGET {
                        tracing::debug!("focus walk gave up after {visited} objects");
                        return None;
                    }
                    let Ok(proxy) = object.as_accessible_proxy(conn).await else {
                        continue;
                    };
                    let Ok(state) = bounded("state", proxy.get_state()).await else {
                        continue;
                    };
                    // A hidden subtree (a closed menu, an inactive tab)
                    // cannot hold the focus, however its objects report
                    // themselves; Qt marks hidden page tabs as focused.
                    if !showing(state) {
                        continue;
                    }
                    if state.contains(State::Focused) {
                        if let Some(t) = describe(conn, &object, false).await {
                            return Some(t);
                        }
                        continue;
                    }
                    if let Ok(children) = bounded("children", proxy.get_children()).await {
                        // Push in reverse so the first child is visited first.
                        stack.extend(children.into_iter().rev());
                    }
                }
            }
        }
        None
    }
}

/// On screen: toolkits clear both for a closed menu or an inactive tab.
fn showing(state: StateSet) -> bool {
    state.contains(State::Showing) || state.contains(State::Visible)
}

/// Focused, and on screen to be.
fn has_focus(state: StateSet) -> bool {
    state.contains(State::Focused) && showing(state)
}

/// Read what the follower keeps about an object, or None when it has no
/// Text interface (or, with `require_focused`, is not focused).
async fn describe(
    conn: &zbus::Connection,
    object: &ObjectRefOwned,
    require_focused: bool,
) -> Option<Tracked> {
    let proxy = object.as_accessible_proxy(conn).await.ok()?;
    let interfaces = bounded("interfaces", proxy.get_interfaces()).await.ok()?;
    if !interfaces.contains(Interface::Text) {
        return None;
    }
    if require_focused {
        let state = bounded("state", proxy.get_state()).await.ok()?;
        if !has_focus(state) {
            return None;
        }
    }
    let role = bounded("role", proxy.get_role()).await.ok()?;
    let app = app_name(conn, &proxy).await.unwrap_or_default();
    Some(Tracked {
        object: object.clone(),
        app,
        role,
    })
}

async fn app_name(conn: &zbus::Connection, proxy: &AccessibleProxy<'_>) -> anyhow::Result<String> {
    let app = bounded("application", proxy.get_application()).await?;
    let app = app.as_accessible_proxy(conn).await?;
    bounded("application name", app.name()).await
}

/// The background task: keep the most recently focused text object.
async fn follow(conn: AccessibilityConnection, tracked: Arc<RwLock<Option<Tracked>>>) {
    let mut events = std::pin::pin!(conn.event_stream());
    while let Some(event) = events.next().await {
        let (object, from_caret) = match event {
            Ok(Event::Object(ObjectEvents::StateChanged(e)))
                if e.state == State::Focused && e.enabled =>
            {
                (e.item, false)
            }
            Ok(Event::Object(ObjectEvents::TextCaretMoved(e))) => (e.item, true),
            Ok(_) => continue,
            Err(e) => {
                tracing::debug!("a11y event: {e}");
                continue;
            }
        };
        if from_caret {
            // The caret moves on every keystroke; only a new object matters.
            let same = tracked
                .read()
                .await
                .as_ref()
                .is_some_and(|t| t.object == object);
            if same {
                continue;
            }
        }
        if let Some(t) = describe(conn.connection(), &object, from_caret).await {
            tracing::debug!("focused text object: {} ({})", t.app, t.role);
            *tracked.write().await = Some(t);
        }
    }
    tracing::warn!("a11y event stream ended; focus is no longer followed");
}

/// One call to an application, bounded by [`CALL_TIMEOUT`].
async fn bounded<T, E>(what: &str, fut: impl Future<Output = Result<T, E>>) -> anyhow::Result<T>
where
    E: std::fmt::Display,
{
    match tokio::time::timeout(CALL_TIMEOUT, fut).await {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(e)) => Err(anyhow::anyhow!("a11y {what}: {e}")),
        Err(_) => Err(anyhow::anyhow!(
            "a11y {what}: no answer in {} ms",
            CALL_TIMEOUT.as_millis()
        )),
    }
}

/// How a run of typed text was found at the end of the field.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SuffixMatch {
    /// The field ends with exactly the text; this many characters.
    Exact(usize),
    /// The field ends with something close to the text (autocorrect,
    /// autocomplete); this many characters cover it.
    Fuzzy(usize),
}

impl SuffixMatch {
    pub fn len(self) -> usize {
        match self {
            SuffixMatch::Exact(n) | SuffixMatch::Fuzzy(n) => n,
        }
    }

    pub fn is_empty(self) -> bool {
        self.len() == 0
    }
}

/// Lowest similarity a fuzzy suffix match is accepted at.
const MIN_SIMILARITY: f64 = 0.8;

/// Find `expected` at the end of `before`, allowing for an application
/// having changed a little of it. The fuzzy search tries every suffix
/// length within 30% of the expected length and keeps the one with the
/// highest similarity (1 - Levenshtein distance / longer length), if that
/// reaches [`MIN_SIMILARITY`].
pub fn match_suffix(before: &str, expected: &str) -> Option<SuffixMatch> {
    let expected: Vec<char> = expected.chars().collect();
    let n = expected.len();
    if n == 0 {
        return None;
    }
    let before: Vec<char> = before.chars().collect();
    if before.len() >= n && before[before.len() - n..] == expected[..] {
        return Some(SuffixMatch::Exact(n));
    }
    let slack = n * 3 / 10;
    let min_len = n.saturating_sub(slack).max(1);
    let max_len = (n + slack).min(before.len());
    if max_len < min_len {
        return None;
    }
    // Reverse both so every suffix of `before` is a prefix of `rev_before`,
    // and one edit-distance table gives the distance for every length.
    let rev_before: Vec<char> = before[before.len() - max_len..]
        .iter()
        .rev()
        .copied()
        .collect();
    let rev_expected: Vec<char> = expected.iter().rev().copied().collect();
    // row[j] = distance between the first i chars of rev_before and the
    // first j chars of rev_expected
    let mut row: Vec<usize> = (0..=n).collect();
    let mut best: Option<(f64, usize)> = None;
    for (i, &b) in rev_before.iter().enumerate() {
        let mut prev_diag = row[0];
        row[0] = i + 1;
        for j in 1..=n {
            let cost = usize::from(b != rev_expected[j - 1]);
            let next = (row[j] + 1).min(row[j - 1] + 1).min(prev_diag + cost);
            prev_diag = row[j];
            row[j] = next;
        }
        let len = i + 1;
        if len < min_len {
            continue;
        }
        let similarity = 1.0 - row[n] as f64 / len.max(n) as f64;
        let better = match best {
            None => true,
            Some((s, l)) => similarity > s || (similarity == s && len.abs_diff(n) < l.abs_diff(n)),
        };
        if better {
            best = Some((similarity, len));
        }
    }
    match best {
        Some((s, len)) if s >= MIN_SIMILARITY => Some(SuffixMatch::Fuzzy(len)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_suffix() {
        assert_eq!(
            match_suffix("hello there, see you tuesday", " see you tuesday"),
            Some(SuffixMatch::Exact(16))
        );
        assert_eq!(match_suffix("abc", "abc"), Some(SuffixMatch::Exact(3)));
        assert_eq!(match_suffix("", "abc"), None);
        assert_eq!(match_suffix("abc", ""), None);
    }

    #[test]
    fn autocorrected_word_inside_is_still_found() {
        // "teh" autocorrected to "the": one substitution, one insertion
        let m = match_suffix(
            "note: send it to teh team on Tuesday",
            " send it to the team on tuesday",
        );
        assert_eq!(m, Some(SuffixMatch::Fuzzy(31)));
        // an application capitalised the first letter
        let m = match_suffix("Hello world", "hello world");
        assert_eq!(m, Some(SuffixMatch::Fuzzy(11)));
        // autocomplete added a closing bracket
        let m = match_suffix("call foo(bar)", "foo(bar");
        assert!(matches!(m, Some(SuffixMatch::Fuzzy(7 | 8))), "{m:?}");
    }

    #[test]
    fn wholly_different_text_does_not_match() {
        assert_eq!(
            match_suffix("the quick brown fox", "see you on tuesday"),
            None
        );
        assert_eq!(match_suffix("x", "see you on tuesday"), None);
    }

    #[test]
    fn leading_edit_keeps_only_the_dictated_part() {
        // the user typed a word before the dictation; the dictation is
        // still at the end and is what gets matched
        let m = match_suffix("typed hello world", "hello world");
        assert_eq!(m, Some(SuffixMatch::Exact(11)));
    }
}
