//! Screenshots via KWin's ScreenShot2 D-Bus interface (no portal dialog).
//! Raw pixels are written by KWin into an fd we pass; results come back as
//! a{sv} with width/height/stride/format (QImage::Format).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::os::fd::{AsFd, FromRawFd};

use zbus::zvariant::{Fd, OwnedValue, Value};
use zbus::Connection;

const SERVICE: &str = "org.kde.KWin.ScreenShot2";
const PATH: &str = "/org/kde/KWin/ScreenShot2";
const IFACE: &str = "org.kde.KWin.ScreenShot2";

pub struct Screenshot {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    /// QImage::Format value; ScreenShot2 typically returns RGB32/ARGB32.
    pub format: u32,
    /// Raw pixels, row-major, `stride` bytes per row.
    pub data: Vec<u8>,
}

impl Screenshot {
    /// Write as PPM (P6). QImage RGB32 family is little-endian B,G,R,X in
    /// memory, so swap to RGB on the way out. Convert to PNG with ffmpeg if
    /// needed.
    pub fn write_ppm(&self, path: &std::path::Path) -> anyhow::Result<()> {
        anyhow::ensure!(
            matches!(self.format, 4 | 5 | 6),
            "unsupported QImage format {} (expected RGB32/ARGB32 family)",
            self.format
        );
        let mut f = std::fs::File::create(path)?;
        use std::io::Write;
        writeln!(f, "P6\n{} {}\n255", self.width, self.height)?;
        for y in 0..self.height {
            let row = (y * self.stride) as usize;
            for x in 0..self.width as usize {
                let px = row + x * 4;
                f.write_all(&[self.data[px + 2], self.data[px + 1], self.data[px]])?;
            }
        }
        Ok(())
    }
}

pub async fn capture_active_window() -> anyhow::Result<Screenshot> {
    let conn = Connection::session().await?;

    // anonymous in-memory buffer for KWin to write pixels into
    let raw = unsafe { libc::memfd_create(c"parla-shot".as_ptr(), libc::MFD_CLOEXEC) };
    anyhow::ensure!(raw >= 0, "memfd_create failed: {}", std::io::Error::last_os_error());
    let mut file = unsafe { std::fs::File::from_raw_fd(raw) };
    let fd = Fd::from(file.as_fd());

    let options: HashMap<&str, Value<'_>> = HashMap::new();
    let reply = conn
        .call_method(
            Some(SERVICE),
            PATH,
            Some(IFACE),
            "CaptureActiveWindow",
            &(options, fd),
        )
        .await?;
    let results: HashMap<String, OwnedValue> = reply.body().deserialize()?;
    let get_u32 = |k: &str| -> anyhow::Result<u32> {
        let v = results
            .get(k)
            .ok_or_else(|| anyhow::anyhow!("ScreenShot2 result missing {k}"))?;
        u32::try_from(v.clone())
            .map_err(|e| anyhow::anyhow!("ScreenShot2 result {k} not u32: {e}"))
    };
    let width = get_u32("width")?;
    let height = get_u32("height")?;
    let stride = get_u32("stride")?;
    let format = get_u32("format")?;

    file.seek(SeekFrom::Start(0))?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    anyhow::ensure!(
        data.len() >= (stride as usize) * height as usize,
        "ScreenShot2 wrote {} bytes, expected {}",
        data.len(),
        stride as usize * height as usize
    );
    Ok(Screenshot {
        width,
        height,
        stride,
        format,
        data,
    })
}
