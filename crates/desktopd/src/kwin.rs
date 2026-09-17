//! KWin D-Bus: virtual desktops (VirtualDesktopManager) and KRunner.

use zbus::proxy;

use crate::bus;

#[proxy(
    interface = "org.kde.KWin.VirtualDesktopManager",
    default_service = "org.kde.KWin",
    default_path = "/VirtualDesktopManager"
)]
trait VirtualDesktopManager {
    /// Id (UUID string) of the current desktop.
    #[zbus(property, name = "current")]
    fn current(&self) -> zbus::Result<String>;
    #[zbus(property, name = "current")]
    fn set_current(&self, id: &str) -> zbus::Result<()>;
    /// (position, id, name) per desktop.
    #[zbus(property, name = "desktops")]
    fn desktops(&self) -> zbus::Result<Vec<(u32, String, String)>>;
    #[zbus(property, name = "navigationWrappingAround")]
    fn navigation_wrapping_around(&self) -> zbus::Result<bool>;
}

async fn vdm() -> anyhow::Result<VirtualDesktopManagerProxy<'static>> {
    let conn = bus::session().await?;
    Ok(VirtualDesktopManagerProxy::new(&conn).await?)
}

async fn sorted_desktops(
    vdm: &VirtualDesktopManagerProxy<'_>,
) -> anyhow::Result<Vec<(u32, String, String)>> {
    let mut desktops = vdm.desktops().await?;
    desktops.sort_by_key(|(pos, _, _)| *pos);
    Ok(desktops)
}

pub async fn list_desktops() -> anyhow::Result<Vec<(u32, String, String)>> {
    sorted_desktops(&vdm().await?).await
}

/// (1-based current desktop, desktop count) in one round of calls.
pub async fn desktop_position() -> anyhow::Result<(u32, u32)> {
    let vdm = vdm().await?;
    let current = vdm.current().await?;
    let desktops = sorted_desktops(&vdm).await?;
    let idx = desktops
        .iter()
        .position(|(_, id, _)| *id == current)
        .ok_or_else(|| anyhow::anyhow!("current desktop {current} not in the desktop list"))?;
    Ok((idx as u32 + 1, desktops.len() as u32))
}

/// 1-based number of the current desktop.
pub async fn current_desktop() -> anyhow::Result<u32> {
    desktop_position().await.map(|(cur, _)| cur)
}

/// Switch to the 1-based desktop number n.
pub async fn switch_to(n: u32) -> anyhow::Result<()> {
    let vdm = vdm().await?;
    let desktops = sorted_desktops(&vdm).await?;
    let (_, id, name) = n
        .checked_sub(1)
        .and_then(|i| desktops.get(i as usize))
        .ok_or_else(|| {
            anyhow::anyhow!(
                "desktop {n} does not exist (valid range: 1 to {})",
                desktops.len()
            )
        })?;
    vdm.set_current(id).await?;
    tracing::info!("switched to desktop {n} ({name})");
    Ok(())
}

/// Switch relative to the current desktop (+1/-1, wrapping per KWin setting).
pub async fn switch_rel(delta: i32) -> anyhow::Result<()> {
    let vdm = vdm().await?;
    let desktops = sorted_desktops(&vdm).await?;
    anyhow::ensure!(!desktops.is_empty(), "no virtual desktops");
    let current = vdm.current().await?;
    let idx = desktops
        .iter()
        .position(|(_, id, _)| *id == current)
        .unwrap_or(0) as i32;
    let len = desktops.len() as i32;
    let wrap = vdm.navigation_wrapping_around().await.unwrap_or(true);
    let mut target = idx + delta;
    if wrap {
        target = target.rem_euclid(len);
    } else {
        target = target.clamp(0, len - 1);
    }
    let (_, id, name) = &desktops[target as usize];
    vdm.set_current(id).await?;
    tracing::info!("switched to desktop {} ({name})", target + 1);
    Ok(())
}

/// Open KRunner pre-filled with a query.
pub async fn krunner_query(term: &str) -> anyhow::Result<()> {
    let conn = bus::session().await?;
    conn.call_method(
        Some("org.kde.krunner"),
        "/App",
        Some("org.kde.krunner.App"),
        "query",
        &(term),
    )
    .await?;
    Ok(())
}
