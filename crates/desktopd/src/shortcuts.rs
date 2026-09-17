//! Fire any existing KDE global shortcut through kglobalaccel's component
//! objects (e.g. Spectacle's ActiveWindowScreenShot) without knowing its key.

use crate::bus;

/// Component unique names map to object paths with dots/dashes replaced:
/// `org_kde_spectacle_desktop` -> `/component/org_kde_spectacle_desktop`.
fn component_path(component: &str) -> String {
    format!("/component/{}", component.replace(['.', '-'], "_"))
}

pub async fn invoke(component: &str, action: &str) -> anyhow::Result<()> {
    let conn = bus::session().await?;
    conn.call_method(
        Some("org.kde.kglobalaccel"),
        component_path(component).as_str(),
        Some("org.kde.kglobalaccel.Component"),
        "invokeShortcut",
        &(action),
    )
    .await
    .map_err(|e| {
        anyhow::anyhow!("invokeShortcut({component}, {action}) failed: {e} (wrong component/action name?)")
    })?;
    Ok(())
}

/// List a component's shortcut names (useful for config authoring and the
/// agent's tool inventory).
pub async fn shortcut_names(component: &str) -> anyhow::Result<Vec<String>> {
    let conn = bus::session().await?;
    let reply = conn
        .call_method(
            Some("org.kde.kglobalaccel"),
            component_path(component).as_str(),
            Some("org.kde.kglobalaccel.Component"),
            "shortcutNames",
            &(),
        )
        .await?;
    let names: Vec<String> = reply.body().deserialize()?;
    Ok(names)
}
