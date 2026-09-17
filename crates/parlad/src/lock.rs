//! Session-lock awareness.
//!
//! Discovered the hard way during P0 bring-up: while the screen is locked,
//! KWin routes ALL keyboard input (physical and injected) to the lock
//! surface — injection silently goes nowhere. Voice must also be inert on a
//! locked session (plan §5: voice is an unauthenticated input channel).
//! So: watch org.freedesktop.ScreenSaver and pause capture while locked.

use futures_util::StreamExt;
use tokio::sync::watch;
use zbus::message::Type as MessageType;
use zbus::{Connection, MatchRule, MessageStream};

const SS_SERVICE: &str = "org.freedesktop.ScreenSaver";
const SS_PATH: &str = "/ScreenSaver";
const SS_IFACE: &str = "org.freedesktop.ScreenSaver";

pub async fn is_locked(conn: &Connection) -> bool {
    conn.call_method(Some(SS_SERVICE), SS_PATH, Some(SS_IFACE), "GetActive", &())
        .await
        .and_then(|r| r.body().deserialize::<bool>())
        .unwrap_or(false)
}

/// Feed lock state changes into a watch channel until the stream dies.
pub async fn watch_lock(conn: Connection, tx: watch::Sender<bool>) -> anyhow::Result<()> {
    let rule = MatchRule::builder()
        .msg_type(MessageType::Signal)
        .sender(SS_SERVICE)?
        .path(SS_PATH)?
        .interface(SS_IFACE)?
        .member("ActiveChanged")?
        .build();
    let mut stream = MessageStream::for_match_rule(rule, &conn, None).await?;
    while let Some(msg) = stream.next().await {
        let msg = msg?;
        if let Ok(active) = msg.body().deserialize::<bool>() {
            tracing::info!("session lock state: locked={active}");
            let _ = tx.send(active);
        }
    }
    Ok(())
}
