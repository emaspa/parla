//! What was dictated, when, and where it went. One JSON object per line,
//! appended as utterances finish; the daemon is the only writer, the UI
//! reads through the daemon.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Context as _;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Record {
    /// Unique per daemon lifetime and monotonic within it.
    pub id: String,
    /// Unix time in milliseconds.
    pub at_ms: u64,
    /// "dictate", "command" or "edit".
    pub mode: String,
    /// Window class of the window that had focus.
    pub app: String,
    /// Its title, when the daemon was allowed to keep it.
    pub title: String,
    /// What whisper heard.
    pub raw: String,
    /// What was typed or done, after snippets, dictionary and cleanup.
    pub text: String,
    /// Name of the profile that shaped `text`, if cleanup ran.
    pub profile: String,
    pub words: u32,
    pub audio_ms: u64,
    /// From end of capture to the text landing.
    pub latency_ms: u64,
    /// "typed", "snippet", "raw", "command", "confirm", "refused", "error".
    pub outcome: String,
    /// Details for the outcome: the command run, the refusal reason.
    pub detail: String,
}

impl Record {
    pub fn now_ms() -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Aggregates the UI shows on its home page.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Stats {
    pub utterances: u64,
    pub words: u64,
    pub audio_ms: u64,
    /// Words spoken per minute of audio, over everything.
    pub words_per_minute: f64,
    /// Last 30 local calendar days, oldest first, days without dictation
    /// included with zeros.
    pub days: Vec<DayStats>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct DayStats {
    /// Local date as YYYY-MM-DD.
    pub date: String,
    pub utterances: u64,
    pub words: u64,
}

pub struct History {
    path: PathBuf,
    counter: u64,
}

impl History {
    pub fn open(path: PathBuf) -> Self {
        Self { path, counter: 0 }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append one record, filling in `id` and `at_ms` if unset.
    pub fn append(&mut self, mut record: Record) -> anyhow::Result<Record> {
        if record.at_ms == 0 {
            record.at_ms = Record::now_ms();
        }
        if record.id.is_empty() {
            self.counter += 1;
            record.id = format!("{:x}-{:x}", record.at_ms, self.counter);
        }
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
        }
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .with_context(|| format!("opening {}", self.path.display()))?;
        let mut line = serde_json::to_string(&record)?;
        line.push('\n');
        f.write_all(line.as_bytes())
            .with_context(|| format!("appending to {}", self.path.display()))?;
        Ok(record)
    }

    /// Every record, oldest first. A line that does not parse is skipped
    /// rather than failing the read, so one bad write cannot hide the rest.
    pub fn read_all(&self) -> anyhow::Result<Vec<Record>> {
        let f = match std::fs::File::open(&self.path) {
            Ok(f) => f,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(e) => return Err(e).with_context(|| format!("opening {}", self.path.display())),
        };
        let mut out = Vec::new();
        for line in BufReader::new(f).lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            match serde_json::from_str::<Record>(&line) {
                Ok(r) => out.push(r),
                Err(e) => eprintln!("history: skipping unreadable line: {e}"),
            }
        }
        Ok(out)
    }

    /// The newest `limit` records after skipping `offset`, newest first.
    pub fn recent(&self, limit: usize, offset: usize) -> anyhow::Result<Vec<Record>> {
        let all = self.read_all()?;
        Ok(all.into_iter().rev().skip(offset).take(limit).collect())
    }

    /// Rewrite the file without the record `id`. Returns whether it existed.
    pub fn delete(&self, id: &str) -> anyhow::Result<bool> {
        let all = self.read_all()?;
        let kept: Vec<&Record> = all.iter().filter(|r| r.id != id).collect();
        if kept.len() == all.len() {
            return Ok(false);
        }
        self.rewrite(&kept)?;
        Ok(true)
    }

    pub fn clear(&self) -> anyhow::Result<()> {
        self.rewrite(&[])
    }

    fn rewrite(&self, records: &[&Record]) -> anyhow::Result<()> {
        let mut body = String::new();
        for r in records {
            body.push_str(&serde_json::to_string(r)?);
            body.push('\n');
        }
        crate::paths::write_atomic(&self.path, &body)
    }

    pub fn stats(&self) -> anyhow::Result<Stats> {
        Ok(stats_of(&self.read_all()?, Record::now_ms()))
    }
}

/// Aggregate `records` as of `now_ms`, in the local timezone.
pub fn stats_of(records: &[Record], now_ms: u64) -> Stats {
    const DAYS: u64 = 30;
    let mut s = Stats::default();
    let today = local_day(now_ms);
    let mut days: Vec<DayStats> = (0..DAYS)
        .rev()
        .map(|back| DayStats {
            date: today.saturating_sub(back).to_date(),
            ..Default::default()
        })
        .collect();
    for r in records {
        s.utterances += 1;
        s.words += u64::from(r.words);
        s.audio_ms += r.audio_ms;
        let day = local_day(r.at_ms);
        if let Some(back) = today.checked_sub(day) {
            if let Some(d) = days.get_mut((DAYS - 1).wrapping_sub(back) as usize) {
                if back < DAYS {
                    d.utterances += 1;
                    d.words += u64::from(r.words);
                }
            }
        }
    }
    if s.audio_ms > 0 {
        s.words_per_minute = s.words as f64 / (s.audio_ms as f64 / 60_000.0);
    }
    s.days = days;
    s
}

/// A local calendar day, counted from an arbitrary origin, plus its date.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct LocalDay(i64);

impl LocalDay {
    fn saturating_sub(self, days: u64) -> Self {
        LocalDay(self.0 - days as i64)
    }
    fn checked_sub(self, other: Self) -> Option<u64> {
        u64::try_from(self.0 - other.0).ok()
    }
    fn to_date(self) -> String {
        civil_from_days(self.0)
    }
}

fn local_day(unix_ms: u64) -> LocalDay {
    let secs = (unix_ms / 1000) as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r writes into the tm we own and reads a valid time_t.
    let ok = unsafe { !libc::localtime_r(&secs, &mut tm).is_null() };
    let offset = if ok { tm.tm_gmtoff } else { 0 };
    LocalDay((secs + offset).div_euclid(86_400))
}

/// Days since 1970-01-01 to YYYY-MM-DD (Howard Hinnant's algorithm).
fn civil_from_days(z: i64) -> String {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "parla-flow-test-{name}-{}-{}",
            std::process::id(),
            Record::now_ms()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn rec(words: u32, at_ms: u64) -> Record {
        Record {
            words,
            at_ms,
            audio_ms: 3000,
            mode: "dictate".into(),
            outcome: "typed".into(),
            ..Default::default()
        }
    }

    #[test]
    fn append_read_delete_clear() {
        let d = dir("crud");
        let mut h = History::open(d.join("history.jsonl"));
        assert!(h.read_all().unwrap().is_empty(), "missing file reads empty");
        let a = h.append(rec(3, 1000)).unwrap();
        let b = h.append(rec(5, 2000)).unwrap();
        assert_ne!(a.id, b.id);
        let recent = h.recent(1, 0).unwrap();
        assert_eq!(recent[0].id, b.id, "newest first");
        assert_eq!(h.recent(5, 1).unwrap()[0].id, a.id);
        assert!(h.delete(&a.id).unwrap());
        assert!(!h.delete(&a.id).unwrap());
        assert_eq!(h.read_all().unwrap().len(), 1);
        h.clear().unwrap();
        assert!(h.read_all().unwrap().is_empty());
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn bad_lines_are_skipped() {
        let d = dir("bad");
        let p = d.join("history.jsonl");
        std::fs::write(
            &p,
            "{\"id\":\"a\",\"words\":2}\nnot json\n\n{\"id\":\"b\"}\n",
        )
        .unwrap();
        let h = History::open(p);
        let ids: Vec<String> = h.read_all().unwrap().into_iter().map(|r| r.id).collect();
        assert_eq!(ids, vec!["a", "b"]);
        std::fs::remove_dir_all(d).unwrap();
    }

    #[test]
    fn stats_bucket_by_local_day() {
        let now = 1_726_660_800_000; // 2024-09-18 12:00 UTC
        let day = 86_400_000;
        let records = vec![rec(10, now), rec(5, now - day), rec(1, now - 40 * day)];
        let s = stats_of(&records, now);
        assert_eq!(s.utterances, 3);
        assert_eq!(s.words, 16);
        assert_eq!(s.days.len(), 30);
        assert_eq!(s.days[29].words, 10);
        assert_eq!(s.days[28].words, 5);
        assert_eq!(s.days.iter().map(|d| d.words).sum::<u64>(), 15);
        assert!((s.words_per_minute - 16.0 / (9.0 / 60.0)).abs() < 1e-9);
        assert!(s.days[29].date.starts_with("2024-09-1"));
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_from_days(0), "1970-01-01");
        assert_eq!(civil_from_days(19_984), "2024-09-18");
        assert_eq!(civil_from_days(-1), "1969-12-31");
    }
}
