//! One session bus connection for the whole crate, opened on first use.
//! zbus connections are cheap to clone (an `Arc` inside) and expensive to
//! open (hello + auth round trips), so nobody opens their own.

use tokio::sync::OnceCell;
use zbus::Connection;

static SESSION: OnceCell<Connection> = OnceCell::const_new();

/// The shared session bus connection.
pub async fn session() -> anyhow::Result<Connection> {
    let conn = SESSION
        .get_or_try_init(|| async {
            Connection::session()
                .await
                .map_err(|e| anyhow::anyhow!("session bus: {e}"))
        })
        .await?;
    Ok(conn.clone())
}
