//! 本地 SQLite：每条捕获的请求一行，按 GET / POST / OTHER 分类，并各有一个视图。

use std::path::Path;

use rusqlite::{Connection, params};

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS projects (
    id         INTEGER PRIMARY KEY,
    name       TEXT NOT NULL UNIQUE,
    created_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
INSERT OR IGNORE INTO projects (id, name) VALUES (1, 'Default');
CREATE TABLE IF NOT EXISTS sessions (
    id         INTEGER PRIMARY KEY,
    started_at TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now'))
);
CREATE TABLE IF NOT EXISTS requests (
    id            INTEGER PRIMARY KEY,
    session_id    INTEGER NOT NULL REFERENCES sessions(id),
    ts            TEXT NOT NULL DEFAULT (strftime('%Y-%m-%dT%H:%M:%fZ','now')),
    category      TEXT NOT NULL CHECK (category IN ('GET','POST','OTHER')),
    method        TEXT NOT NULL,
    url           TEXT NOT NULL,
    host          TEXT NOT NULL,
    path          TEXT NOT NULL,
    query         TEXT,
    resource_type TEXT NOT NULL,
    req_headers   TEXT NOT NULL,   -- JSON 对象
    post_data     TEXT,
    status        INTEGER,         -- NULL = 传输失败
    res_headers   TEXT,            -- JSON 对象
    res_mime      TEXT,
    res_body      TEXT,
    res_body_b64  INTEGER NOT NULL DEFAULT 0,
    error         TEXT,
    imported      INTEGER NOT NULL DEFAULT 0, -- 已导入 Germal 的已保存请求
    started_ms    INTEGER,         -- 发起时刻，Unix 毫秒
    ttfb_ms       REAL,            -- 首字节耗时
    duration_ms   REAL,            -- 总耗时
    res_size      INTEGER,         -- 传输字节数
    project_id    INTEGER NOT NULL DEFAULT 1 REFERENCES projects(id),
    page_url      TEXT,            -- 发起请求的页面（document URL），录制界面按它分页
    meta          TEXT             -- JSON：协议、远端地址、TLS、分段耗时、发起者、重定向链等其余全部信息
);
CREATE INDEX IF NOT EXISTS requests_cat_host ON requests(category, host);
CREATE VIEW IF NOT EXISTS get_requests   AS SELECT * FROM requests WHERE category = 'GET';
CREATE VIEW IF NOT EXISTS post_requests  AS SELECT * FROM requests WHERE category = 'POST';
CREATE VIEW IF NOT EXISTS other_requests AS SELECT * FROM requests WHERE category = 'OTHER';
";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    pub id: i64,
    pub name: String,
}

/// 一条已完成（或失败）的请求 / 响应。
#[derive(Debug, Clone, Default)]
pub struct Record {
    pub method: String,
    pub url: String,
    pub resource_type: String,
    pub req_headers: String,
    pub post_data: Option<String>,
    pub status: Option<u16>,
    pub res_headers: Option<String>,
    pub res_mime: Option<String>,
    pub res_body: Option<String>,
    pub res_body_b64: bool,
    pub error: Option<String>,
    pub started_ms: Option<i64>,
    pub ttfb_ms: Option<f64>,
    pub duration_ms: Option<f64>,
    pub res_size: Option<i64>,
    pub meta: Option<String>,
    pub page_url: Option<String>,
}

/// 库里的一行。列表查询不带请求 / 响应体（`post_data`、`res_body` 为 None），点开详情再取全。
#[derive(Debug, Clone)]
pub struct Entry {
    pub id: i64,
    pub category: String,
    pub method: String,
    pub url: String,
    pub host: String,
    pub path: String,
    pub query: Option<String>,
    pub resource_type: String,
    pub req_headers: String,
    pub post_data: Option<String>,
    pub status: Option<u16>,
    pub res_headers: Option<String>,
    pub res_mime: Option<String>,
    pub res_body: Option<String>,
    pub res_body_b64: bool,
    pub error: Option<String>,
    pub started_ms: Option<i64>,
    pub ttfb_ms: Option<f64>,
    pub duration_ms: Option<f64>,
    pub res_size: Option<i64>,
    pub meta: Option<String>,
    pub page_url: Option<String>,
}

const ENTRY_COLS: &str = "id, category, method, url, host, path, query, resource_type, req_headers, {post}, status, res_headers, res_mime, {body}, res_body_b64, error, started_ms, ttfb_ms, duration_ms, res_size, meta, page_url";

fn entry(r: &rusqlite::Row) -> rusqlite::Result<Entry> {
    Ok(Entry {
        id: r.get(0)?,
        category: r.get(1)?,
        method: r.get(2)?,
        url: r.get(3)?,
        host: r.get(4)?,
        path: r.get(5)?,
        query: r.get(6)?,
        resource_type: r.get(7)?,
        req_headers: r.get(8)?,
        post_data: r.get(9)?,
        status: r.get(10)?,
        res_headers: r.get(11)?,
        res_mime: r.get(12)?,
        res_body: r.get(13)?,
        res_body_b64: r.get(14)?,
        error: r.get(15)?,
        started_ms: r.get(16)?,
        ttfb_ms: r.get(17)?,
        duration_ms: r.get(18)?,
        res_size: r.get(19)?,
        meta: r.get(20)?,
        page_url: r.get(21)?,
    })
}

pub fn category(method: &str) -> &'static str {
    match method.to_ascii_uppercase().as_str() {
        "GET" => "GET",
        "POST" => "POST",
        _ => "OTHER",
    }
}

pub struct Db(pub Connection);

impl Db {
    pub fn open(path: &Path) -> rusqlite::Result<Db> {
        let conn = Connection::open(path)?;
        conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
        conn.execute_batch(SCHEMA)?;
        let db = Db(conn);
        // 旧版本建的库缺这些列：补上（新库已有，检查后跳过）
        for (col, decl) in [
            ("imported", "INTEGER NOT NULL DEFAULT 0"),
            ("started_ms", "INTEGER"),
            ("ttfb_ms", "REAL"),
            ("duration_ms", "REAL"),
            ("res_size", "INTEGER"),
            ("meta", "TEXT"),
            ("page_url", "TEXT"),
            ("project_id", "INTEGER NOT NULL DEFAULT 1"),
        ] {
            db.ensure_column(col, decl)?;
        }
        db.0.execute_batch(
            "CREATE INDEX IF NOT EXISTS requests_project ON requests(project_id, id)",
        )?;
        Ok(db)
    }

    fn ensure_column(&self, name: &str, decl: &str) -> rusqlite::Result<()> {
        let has = self
            .0
            .prepare("PRAGMA table_info(requests)")?
            .query_map([], |r| r.get::<_, String>(1))?
            .any(|c| c.is_ok_and(|c| c == name));
        if !has {
            self.0
                .execute_batch(&format!("ALTER TABLE requests ADD COLUMN {name} {decl}"))?;
        }
        Ok(())
    }

    /// 项目列表（按创建顺序）。
    pub fn list_projects(&self) -> rusqlite::Result<Vec<Project>> {
        self.0
            .prepare("SELECT id, name FROM projects ORDER BY id")?
            .query_map([], |r| {
                Ok(Project {
                    id: r.get(0)?,
                    name: r.get(1)?,
                })
            })?
            .collect()
    }

    /// 新建项目并返回 id；同名（忽略首尾空白）已存在就返回已有的那个，名字为空报错。
    pub fn create_project(&self, name: &str) -> anyhow::Result<i64> {
        let name = name.trim();
        anyhow::ensure!(!name.is_empty(), "project name is empty");
        self.0
            .execute("INSERT OR IGNORE INTO projects (name) VALUES (?1)", [name])?;
        Ok(self
            .0
            .query_row("SELECT id FROM projects WHERE name = ?1", [name], |r| {
                r.get(0)
            })?)
    }

    /// 该项目里 id 大于 `after` 的行，按 id 升序、最多 `limit` 条（`after` 用来增量拉新行）；不含请求 / 响应体。
    pub fn list_after(&self, project: i64, after: i64, limit: i64) -> rusqlite::Result<Vec<Entry>> {
        let cols = ENTRY_COLS
            .replace("{post}", "NULL")
            .replace("{body}", "NULL");
        self.0
            .prepare(&format!(
                "SELECT {cols} FROM requests WHERE project_id = ?1 AND id > ?2 ORDER BY id LIMIT ?3"
            ))?
            .query_map([project, after, limit], entry)?
            .collect()
    }

    /// 该项目最新的 `limit` 条（升序返回），载入历史用。
    pub fn list_latest(&self, project: i64, limit: i64) -> rusqlite::Result<Vec<Entry>> {
        let cols = ENTRY_COLS
            .replace("{post}", "NULL")
            .replace("{body}", "NULL");
        let mut rows: Vec<Entry> = self
            .0
            .prepare(&format!(
                "SELECT {cols} FROM requests WHERE project_id = ?1 ORDER BY id DESC LIMIT ?2"
            ))?
            .query_map([project, limit], entry)?
            .collect::<rusqlite::Result<_>>()?;
        rows.reverse();
        Ok(rows)
    }

    /// 完整的一行（带请求 / 响应体）。
    pub fn get(&self, id: i64) -> rusqlite::Result<Option<Entry>> {
        let cols = ENTRY_COLS
            .replace("{post}", "post_data")
            .replace("{body}", "res_body");
        self.0
            .prepare(&format!("SELECT {cols} FROM requests WHERE id = ?1"))?
            .query_map([id], entry)?
            .next()
            .transpose()
    }

    /// 一致性快照：整库序列化成 SQLite 文件字节（VACUUM INTO，不受 WAL 影响）。
    pub fn snapshot(&self) -> anyhow::Result<Vec<u8>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("snapshot.db");
        self.0.execute("VACUUM INTO ?1", [path.to_string_lossy()])?;
        Ok(std::fs::read(path)?)
    }

    pub fn new_session(&self) -> rusqlite::Result<i64> {
        self.0.execute("INSERT INTO sessions DEFAULT VALUES", [])?;
        Ok(self.0.last_insert_rowid())
    }

    pub fn insert(&self, session: i64, project: i64, r: &Record) -> rusqlite::Result<i64> {
        let (host, path, query) = match url::Url::parse(&r.url) {
            Ok(u) => (
                u.host_str().unwrap_or_default().to_string(),
                u.path().to_string(),
                u.query().map(str::to_string),
            ),
            Err(_) => Default::default(),
        };
        self.0.execute(
            "INSERT INTO requests (session_id, project_id, category, method, url, host, path, query, resource_type,
                req_headers, post_data, status, res_headers, res_mime, res_body, res_body_b64, error,
                started_ms, ttfb_ms, duration_ms, res_size, meta, page_url)
             VALUES (?1,?23,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22)",
            params![
                session,
                category(&r.method),
                r.method.to_ascii_uppercase(),
                r.url,
                host,
                path,
                query,
                r.resource_type,
                r.req_headers,
                r.post_data,
                r.status,
                r.res_headers,
                r.res_mime,
                r.res_body,
                r.res_body_b64,
                r.error,
                r.started_ms,
                r.ttfb_ms,
                r.duration_ms,
                r.res_size,
                r.meta,
                r.page_url,
                project,
            ],
        )?;
        Ok(self.0.last_insert_rowid())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_land_in_matching_view() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let s = db.new_session().unwrap();
        for m in ["GET", "post", "DELETE", "PUT"] {
            let r = Record {
                method: m.into(),
                url: "https://a.test/x?y=1".into(),
                req_headers: "{}".into(),
                resource_type: "fetch".into(),
                ..Default::default()
            };
            db.insert(s, 1, &r).unwrap();
        }
        let n = |v: &str| -> i64 {
            db.0.query_row(&format!("SELECT COUNT(*) FROM {v}"), [], |r| r.get(0))
                .unwrap()
        };
        assert_eq!(
            (n("get_requests"), n("post_requests"), n("other_requests")),
            (1, 1, 2)
        );
        let (host, q): (String, String) =
            db.0.query_row("SELECT host, query FROM requests LIMIT 1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!((host.as_str(), q.as_str()), ("a.test", "y=1"));
        let all = db.list_after(1, 0, 100).unwrap();
        assert_eq!(all.len(), 4);
        assert!(all[0].post_data.is_none() && all[0].res_body.is_none());
        assert_eq!(db.list_after(1, all[2].id, 100).unwrap().len(), 1);
        assert_eq!(db.list_latest(1, 2).unwrap()[0].id, all[2].id);
        assert_eq!(
            db.get(all[0].id).unwrap().unwrap().url,
            "https://a.test/x?y=1"
        );
    }
}
