//! Desktop notifications via the standard freedesktop interface (KNotify
//! answers it on Plasma).

use std::collections::HashMap;

use zbus::Connection;

pub async fn notify(summary: &str, body: &str) -> anyhow::Result<()> {
    let conn = Connection::session().await?;
    let actions: Vec<&str> = vec![];
    let hints: HashMap<&str, zbus::zvariant::Value<'_>> = HashMap::new();
    conn.call_method(
        Some("org.freedesktop.Notifications"),
        "/org/freedesktop/Notifications",
        Some("org.freedesktop.Notifications"),
        "Notify",
        &(
            "parla", // app_name
            0u32,    // replaces_id
            "audio-card", // app_icon
            summary,
            body,
            actions,
            hints,
            4000i32, // expire timeout ms
        ),
    )
    .await?;
    Ok(())
}
