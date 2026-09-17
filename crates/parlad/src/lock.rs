//! Session-lock awareness.
//!
//! Discovered the hard way during P0 bring-up: while the screen is locked,
//! KWin routes ALL keyboard input (physical and injected) to the lock
//! surface — injection silently goes nowhere. Voice must also be inert on a
//! locked session (plan §5: voice is an unauthenticated input channel).
//! So: watch org.freedesktop.ScreenSaver and pause capture while locked.

use std::time::Duration;

use futures_util::StreamExt;
use tokio::sync::watch;
use zbus::message::Type as MessageType;
use zbus::{Connection, MatchRule, MessageStream};

const SS_SERVICE: &str = "org.freedesktop.ScreenSaver";
const SS_PATH: &str = "/ScreenSaver";
const SS_IFACE: &str = "org.freedesktop.ScreenSaver";

const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(30);

pub async fn is_locked(conn: &Connection) -> bool {
    conn.call_method(Some(SS_SERVICE), SS_PATH, Some(SS_IFACE), "GetActive", &())
        .await
        .and_then(|r| r.body().deserialize::<bool>())
        .unwrap_or(false)
}

/// Feed lock state changes into a watch channel for as long as anyone is
/// listening. A dead signal stream is reconnected with backoff, and the state
/// is re-read from the screensaver on every reconnect so nothing that
/// happened while disconnected is missed.
pub async fn watch_lock(conn: Connection, tx: watch::Sender<bool>) {
    let mut backoff = BACKOFF_MIN;
    loop {
        match watch_once(&conn, &tx).await {
            Ok(()) => tracing::error!("session lock watcher: signal stream ended"),
            Err(e) => tracing::error!("session lock watcher failed: {e:#}"),
        }
        if tx.is_closed() {
            return;
        }
        tracing::error!(
            "session lock state may be stale; reconnecting in {}s",
            backoff.as_secs()
        );
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn watch_once(conn: &Connection, tx: &watch::Sender<bool>) -> anyhow::Result<()> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(SS_SERVICE)?
        .path(SS_PATH)?
        .interface(SS_IFACE)?
        .member("ActiveChanged")?
        .build();
    let mut stream = MessageStream::for_match_rule(rule, conn, None).await?;
    // Subscribed; now resync so a change during the gap is not lost.
    let now = is_locked(conn).await;
    if *tx.borrow() != now {
        tracing::info!("session lock state: locked={now} (resynced)");
    }
    tx.send_replace(now);
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        match msg.body().deserialize::<bool>() {
            Ok(active) => {
                tracing::info!("session lock state: locked={active}");
                tx.send_replace(active);
            }
            Err(e) => tracing::warn!("ignoring malformed ActiveChanged signal: {e}"),
        }
    }
    Ok(())
}
