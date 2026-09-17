//! Push-to-talk hotkeys registered through kglobalaccel's D-Bus API.
//!
//! Plasma 6 hosts kglobalaccel in kwin_wayland; there is no KF6
//! registerGlobalShortcutAction — third-party apps replicate the Qt client
//! flow: doRegister(actionId) + setShortcutKeys(actionId, keys, flags), then
//! subscribe to org.kde.kglobalaccel.Component.globalShortcut{Pressed,Released}
//! on /component/parla. The Released signal is what makes hold-to-talk work
//! without evdev hacks.

use anyhow::Context as _;
use futures_util::StreamExt;
use tokio::sync::mpsc;
use zbus::message::Type as MessageType;
use zbus::{Connection, MatchRule, MessageStream};

const SERVICE: &str = "org.kde.kglobalaccel";
const MAIN_PATH: &str = "/kglobalaccel";
const MAIN_IFACE: &str = "org.kde.KGlobalAccel";
const COMPONENT_IFACE: &str = "org.kde.kglobalaccel.Component";

pub const COMPONENT: &str = "parla";
const COMPONENT_PATH: &str = "/component/parla";
const COMPONENT_FRIENDLY: &str = "Parla Voice";
pub const ACTION_DICTATE: &str = "dictate";
pub const ACTION_COMMAND: &str = "command";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HotkeyEvent {
    DictatePressed,
    DictateReleased,
    CommandPressed,
    CommandReleased,
}

/// Qt modifier bits (QKeySequence encoding used on the D-Bus wire).
const QT_SHIFT: u32 = 0x0200_0000;
const QT_CTRL: u32 = 0x0400_0000;
const QT_ALT: u32 = 0x0800_0000;
const QT_META: u32 = 0x1000_0000;

/// Parse "ctrl+shift+space" into a Qt key-combination int.
pub fn parse_chord(chord: &str) -> anyhow::Result<u32> {
    let mut mods = 0u32;
    let mut key: Option<u32> = None;
    for part in chord.split('+').map(str::trim).filter(|s| !s.is_empty()) {
        let lower = part.to_lowercase();
        // modifiers
        let mod_bit = match lower.as_str() {
            "ctrl" | "control" => Some(QT_CTRL),
            "shift" => Some(QT_SHIFT),
            "alt" => Some(QT_ALT),
            "meta" | "super" | "win" => Some(QT_META),
            _ => None,
        };
        if let Some(bit) = mod_bit {
            mods |= bit;
            continue;
        }
        // main key
        let v = match lower.as_str() {
            "space" => 0x20,
            "enter" | "return" => 0x01000004, // Qt::Key_Return
            "tab" => 0x01000001,
            "escape" | "esc" => 0x01000000,
            "backspace" => 0x01000003,
            "delete" | "del" => 0x01000005,
            "up" => 0x01000013,
            "down" => 0x01000015,
            "left" => 0x01000012,
            "right" => 0x01000014,
            "f1" => 0x01000030,
            "f2" => 0x01000031,
            "f3" => 0x01000032,
            "f4" => 0x01000033,
            "f5" => 0x01000034,
            "f6" => 0x01000035,
            "f7" => 0x01000036,
            "f8" => 0x01000037,
            "f9" => 0x01000038,
            "f10" => 0x01000039,
            "f11" => 0x0100003a,
            "f12" => 0x0100003b,
            other => {
                let mut c = other.chars();
                match (c.next(), c.next()) {
                    // Qt key values for letters/digits are the uppercase ASCII
                    (Some(ch), None) if ch.is_ascii_alphanumeric() => {
                        ch.to_ascii_uppercase() as u32
                    }
                    (Some(ch), None) if ch.is_ascii_punctuation() => ch as u32,
                    _ => anyhow::bail!("cannot parse hotkey part {part:?}"),
                }
            }
        };
        anyhow::ensure!(key.is_none(), "multiple main keys in chord {chord:?}");
        key = Some(v);
    }
    let key = key.context("chord has no main key")?;
    Ok(mods | key)
}

fn action_id(action: &str) -> Vec<String> {
    // order live-verified against Plasma 6.7.5 kglobalaccel
    vec![
        COMPONENT.to_string(),
        action.to_string(),
        COMPONENT_FRIENDLY.to_string(),
        action.to_string(),
    ]
}

pub struct HotkeyManager {
    conn: Connection,
}

impl HotkeyManager {
    /// Register both PTT actions and start forwarding press/release signals.
    pub async fn register(
        dictate_chord: &str,
        command_chord: &str,
    ) -> anyhow::Result<(Self, mpsc::Receiver<HotkeyEvent>)> {
        let conn = Connection::session().await?;
        let dictate_key = parse_chord(dictate_chord)
            .with_context(|| format!("bad dictate chord {dictate_chord:?}"))?;
        let command_key = parse_chord(command_chord)
            .with_context(|| format!("bad command chord {command_chord:?}"))?;

        for (action, key) in [(ACTION_DICTATE, dictate_key), (ACTION_COMMAND, command_key)] {
            Self::register_action(&conn, action, key).await?;
        }

        let (tx, rx) = mpsc::channel(32);
        let conn2 = conn.clone();
        tokio::spawn(async move {
            if let Err(e) = signal_loop(&conn2, tx).await {
                tracing::error!("hotkey signal loop died: {e:#}");
            }
        });

        Ok((Self { conn }, rx))
    }

    async fn register_action(conn: &Connection, action: &str, key: u32) -> anyhow::Result<()> {
        let action_id = action_id(action);
        // idempotent restart: drop any stale registration first
        let _ = conn
            .call_method(
                Some(SERVICE),
                MAIN_PATH,
                Some(MAIN_IFACE),
                "unRegister",
                &(&action_id),
            )
            .await;
        conn.call_method(
            Some(SERVICE),
            MAIN_PATH,
            Some(MAIN_IFACE),
            "doRegister",
            &(&action_id),
        )
        .await
        .with_context(|| format!("doRegister({action}) failed"))?;

        // setShortcut(actionId as, keys ai, flags u) — single QKeySequence as
        // signed Qt key ints. (setShortcutKeys wants a(ai), an array of
        // structs; the singular form is the simpler wire shape.)
        let keys: Vec<i32> = vec![key as i32];
        conn.call_method(
            Some(SERVICE),
            MAIN_PATH,
            Some(MAIN_IFACE),
            "setShortcut",
            &(&action_id, &keys, 0u32),
        )
        .await
        .with_context(|| format!("setShortcut({action}) failed"))?;
        tracing::info!("registered hotkey {action} (qt key 0x{key:08x})");
        Ok(())
    }

    /// Remove our registrations (best effort, called on clean shutdown).
    pub async fn unregister(&self) {
        for action in [ACTION_DICTATE, ACTION_COMMAND] {
            let _ = self
                .conn
                .call_method(
                    Some(SERVICE),
                    MAIN_PATH,
                    Some(MAIN_IFACE),
                    "unregister",
                    &(COMPONENT, action),
                )
                .await;
        }
    }
}

async fn signal_loop(conn: &Connection, tx: mpsc::Sender<HotkeyEvent>) -> anyhow::Result<()> {
    let pressed = MessageStream::for_match_rule(
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(SERVICE)?
            .path(COMPONENT_PATH)?
            .interface(COMPONENT_IFACE)?
            .member("globalShortcutPressed")?
            .build(),
        conn,
        None,
    )
    .await?;
    let released = MessageStream::for_match_rule(
        MatchRule::builder()
            .msg_type(MessageType::Signal)
            .sender(SERVICE)?
            .path(COMPONENT_PATH)?
            .interface(COMPONENT_IFACE)?
            .member("globalShortcutReleased")?
            .build(),
        conn,
        None,
    )
    .await?;
    tokio::pin!(pressed);
    tokio::pin!(released);
    loop {
        tokio::select! {
            msg = pressed.next() => deliver(msg, true, &tx).await?,
            msg = released.next() => deliver(msg, false, &tx).await?,
        }
    }
}

async fn deliver(
    msg: Option<zbus::Result<zbus::Message>>,
    pressed: bool,
    tx: &mpsc::Sender<HotkeyEvent>,
) -> anyhow::Result<()> {
    let msg = msg.context("hotkey signal stream ended")??;
    let (component, action, _timestamp): (String, String, i64) = msg.body().deserialize()?;
    if component != COMPONENT {
        return Ok(());
    }
    let event = match (action.as_str(), pressed) {
        (ACTION_DICTATE, true) => HotkeyEvent::DictatePressed,
        (ACTION_DICTATE, false) => HotkeyEvent::DictateReleased,
        (ACTION_COMMAND, true) => HotkeyEvent::CommandPressed,
        (ACTION_COMMAND, false) => HotkeyEvent::CommandReleased,
        _ => return Ok(()),
    };
    tracing::debug!("hotkey {event:?}");
    tx.send(event).await?;
    Ok(())
}

// kglobalaccel drops our component automatically when our bus name
// disappears, so an unclean exit leaves no stale registration behind.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chords() {
        assert_eq!(parse_chord("ctrl+space").unwrap(), QT_CTRL | 0x20);
        assert_eq!(
            parse_chord("ctrl+shift+space").unwrap(),
            QT_CTRL | QT_SHIFT | 0x20
        );
        assert_eq!(parse_chord("Meta+D").unwrap(), QT_META | 0x44);
        assert_eq!(parse_chord("f5").unwrap(), 0x01000034);
        assert!(parse_chord("ctrl").is_err()); // no main key
        assert!(parse_chord("delete+delete").is_err()); // dup main key
        assert!(parse_chord("foo bar+baz").is_err());
    }
}
