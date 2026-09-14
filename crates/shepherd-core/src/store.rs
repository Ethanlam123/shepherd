//! SQLite persistence: finished runs survive restarts, live sessions are a
//! write-through snapshot (cleared on open - real sessions are rediscovered
//! from disk within seconds), and settings hold the mute flag.

use crate::registry::UiSession;
use crate::Run;
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::Mutex;

pub struct Store {
    conn: Mutex<Connection>,
}

impl Store {
    pub fn open(path: &Path) -> rusqlite::Result<Self> {
        if let Some(dir) = path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "busy_timeout", 5_000)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS runs (
                 id TEXT PRIMARY KEY, agent TEXT NOT NULL, title TEXT NOT NULL,
                 project TEXT NOT NULL, ended_at INTEGER NOT NULL,
                 duration_ms INTEGER NOT NULL, tokens INTEGER NOT NULL,
                 stopped INTEGER NOT NULL, outcome TEXT NOT NULL,
                 files TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS sessions (
                 id TEXT PRIMARY KEY, agent TEXT NOT NULL, title TEXT NOT NULL,
                 project TEXT NOT NULL, cwd TEXT NOT NULL, status TEXT NOT NULL,
                 started_at INTEGER NOT NULL, elapsed_ms INTEGER NOT NULL,
                 tokens INTEGER NOT NULL, allowed_tools TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS settings (
                 key TEXT PRIMARY KEY, value TEXT NOT NULL);
             DELETE FROM sessions;",
        )?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    pub fn upsert_session(&self, s: &UiSession) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO sessions (id, agent, title, project, cwd, status, started_at,
                                   elapsed_ms, tokens, allowed_tools)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)
             ON CONFLICT(id) DO UPDATE SET
                 agent=?2, title=?3, project=?4, cwd=?5, status=?6,
                 started_at=?7, elapsed_ms=?8, tokens=?9, allowed_tools=?10",
            params![
                s.session.id,
                s.session.agent,
                s.session.title,
                s.session.project,
                s.session.cwd,
                serde_json::to_string(&s.session.status)
                    .unwrap_or_else(|_| "\"running\"".into())
                    .trim_matches('"')
                    .to_string(),
                s.session.started_at,
                s.session.elapsed_ms as i64,
                s.session.tokens as i64,
                serde_json::to_string(&s.session.allowed_tools).unwrap_or_else(|_| "[]".into()),
            ],
        )?;
        Ok(())
    }

    pub fn remove_session(&self, id: &str) -> rusqlite::Result<()> {
        self.conn
            .lock()
            .unwrap()
            .execute("DELETE FROM sessions WHERE id = ?1", params![id])?;
        Ok(())
    }

    pub fn insert_run(&self, r: &Run) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT OR REPLACE INTO runs VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",
            params![
                r.id,
                r.agent,
                r.title,
                r.project,
                r.ended_at,
                r.duration_ms as i64,
                r.tokens as i64,
                r.stopped as i64,
                r.outcome,
                serde_json::to_string(&r.files).unwrap_or_else(|_| "[]".into()),
            ],
        )?;
        Ok(())
    }

    /// Newest first, matching the registry's in-memory order.
    pub fn recent_runs(&self, limit: usize) -> rusqlite::Result<Vec<Run>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, agent, title, project, ended_at, duration_ms, tokens,
                    stopped, outcome, files FROM runs
             ORDER BY ended_at DESC LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit as i64], |row| {
            Ok(Run {
                id: row.get(0)?,
                agent: row.get(1)?,
                title: row.get(2)?,
                project: row.get(3)?,
                ended_at: row.get(4)?,
                duration_ms: row.get::<_, i64>(5)?.max(0) as u64,
                tokens: row.get::<_, i64>(6)?.max(0) as u64,
                stopped: row.get::<_, i64>(7)? != 0,
                outcome: row.get(8)?,
                files: serde_json::from_str(&row.get::<_, String>(9)?).unwrap_or_default(),
            })
        })?;
        rows.collect()
    }

    pub fn set_setting(&self, key: &str, value: &str) -> rusqlite::Result<()> {
        self.conn.lock().unwrap().execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = ?2",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn setting(&self, key: &str) -> Option<String> {
        self.conn
            .lock()
            .unwrap()
            .query_row(
                "SELECT value FROM settings WHERE key = ?1",
                params![key],
                |row| row.get(0),
            )
            .optional()
            .ok()
            .flatten()
    }

    pub fn set_muted(&self, muted: bool) -> rusqlite::Result<()> {
        self.set_setting("muted", if muted { "1" } else { "0" })
    }

    pub fn muted(&self) -> bool {
        self.setting("muted").as_deref() == Some("1")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::UiSession;
    use crate::{LogLine, Session, Status};

    fn store() -> (tempfile::TempDir, Store) {
        let tmp = tempfile::tempdir().unwrap();
        let s = Store::open(&tmp.path().join("t.db")).unwrap();
        (tmp, s)
    }

    fn run(id: &str, ended_at: i64) -> Run {
        Run {
            id: id.into(),
            agent: "cc".into(),
            title: "t".into(),
            project: "p".into(),
            ended_at,
            duration_ms: 1_000,
            tokens: 5_000,
            stopped: false,
            outcome: "did it".into(),
            files: vec!["a.ts".into(), "b.ts".into()],
        }
    }

    fn ui_session(id: &str) -> UiSession {
        UiSession {
            session: Session {
                id: id.into(),
                agent: "cc".into(),
                title: "t".into(),
                project: "p".into(),
                cwd: "/w/p".into(),
                status: Status::Waiting,
                started_at: 123,
                elapsed_ms: 45,
                tokens: 67,
                allowed_tools: vec!["Bash".into()],
            },
            pending: None,
            activity: vec![LogLine {
                ts: 1,
                kind: "tool".into(),
                text: "x".into(),
            }],
        }
    }

    #[test]
    fn runs_round_trip_newest_first() {
        let (_tmp, s) = store();
        s.insert_run(&run("r-old", 1_000)).unwrap();
        s.insert_run(&run("r-new", 2_000)).unwrap();
        let runs = s.recent_runs(50).unwrap();
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[0].id, "r-new");
        assert_eq!(runs[1].id, "r-old");
        assert_eq!(runs[0].files, vec!["a.ts", "b.ts"]);
        assert_eq!(runs[0].tokens, 5_000);
    }

    #[test]
    fn run_limit_is_respected() {
        let (_tmp, s) = store();
        for i in 0..10 {
            s.insert_run(&run(&format!("r{i}"), i)).unwrap();
        }
        assert_eq!(s.recent_runs(3).unwrap().len(), 3);
        assert_eq!(s.recent_runs(3).unwrap()[0].id, "r9");
    }

    #[test]
    fn sessions_upsert_and_clear_on_open() {
        let tmp = tempfile::tempdir().unwrap();
        {
            let s = Store::open(&tmp.path().join("t.db")).unwrap();
            s.upsert_session(&ui_session("s1")).unwrap();
            s.upsert_session(&ui_session("s1")).unwrap(); // idempotent
            assert_eq!(
                s.conn
                    .lock()
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                1
            );
            s.remove_session("s1").unwrap();
            assert_eq!(
                s.conn
                    .lock()
                    .unwrap()
                    .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                    .unwrap(),
                0
            );
            s.upsert_session(&ui_session("s2")).unwrap();
        }
        // reopening wipes stale sessions (they are rediscovered live)
        let s2 = Store::open(&tmp.path().join("t.db")).unwrap();
        assert_eq!(
            s2.conn
                .lock()
                .unwrap()
                .query_row("SELECT COUNT(*) FROM sessions", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn muted_setting_round_trip() {
        let (_tmp, s) = store();
        assert!(!s.muted());
        s.set_muted(true).unwrap();
        assert!(s.muted());
        s.set_muted(false).unwrap();
        assert!(!s.muted());
    }
}
