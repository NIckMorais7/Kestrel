//! Camada de persistência (SQLite) para sessões, mensagens, planos, tarefas,
//! execuções de ferramentas, decisões e estado de contexto.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension, params};
use std::path::Path;
use std::sync::{Arc, Mutex};

pub const SCHEMA_BASE: &str = r#"
CREATE TABLE IF NOT EXISTS sessions (
    id            TEXT PRIMARY KEY,
    project_path  TEXT NOT NULL,
    title         TEXT NOT NULL,
    model         TEXT NOT NULL,
    reasoning_effort TEXT,
    permission    TEXT NOT NULL,
    status        TEXT NOT NULL DEFAULT 'ativa',
    parent_id     TEXT,
    objective     TEXT,
    created_at    TEXT NOT NULL,
    updated_at    TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS messages (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    role        TEXT NOT NULL,
    content     TEXT NOT NULL,
    tool_calls  TEXT,
    tool_call_id TEXT,
    tool_name   TEXT,
    reasoning   TEXT,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_messages_session ON messages(session_id, id);

CREATE TABLE IF NOT EXISTS tool_runs (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT,
    name        TEXT NOT NULL,
    args        TEXT,
    result      TEXT,
    status      TEXT NOT NULL,
    error       TEXT,
    started_at  TEXT NOT NULL,
    finished_at TEXT,
    duration_ms INTEGER
);

CREATE TABLE IF NOT EXISTS plans (
    id        INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT NOT NULL,
    objective TEXT NOT NULL,
    version   INTEGER NOT NULL,
    steps     TEXT NOT NULL,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS tasks (
    id          TEXT NOT NULL,
    session_id  TEXT NOT NULL,
    parent_id   TEXT,
    title       TEXT NOT NULL,
    description TEXT,
    status      TEXT NOT NULL,
    depends_on  TEXT,
    order_idx   INTEGER NOT NULL DEFAULT 0,
    result      TEXT,
    created_at  TEXT NOT NULL,
    updated_at  TEXT NOT NULL,
    -- O id é escolhido pelo modelo (ex.: `t1`) e só precisa ser único dentro
    -- da sessão; com `id` como chave global, duas sessões colidiam e uma
    -- sobrescrevia as tarefas da outra.
    PRIMARY KEY (id, session_id)
);
CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id, order_idx);

CREATE TABLE IF NOT EXISTS file_changes (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT,
    path        TEXT NOT NULL,
    change_type TEXT NOT NULL,
    additions   INTEGER NOT NULL DEFAULT 0,
    deletions   INTEGER NOT NULL DEFAULT 0,
    diff        TEXT,
    validated   INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS decisions (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT,
    kind       TEXT NOT NULL,
    summary    TEXT NOT NULL,
    detail     TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS audit (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id TEXT,
    kind       TEXT NOT NULL,
    payload    TEXT,
    decision   TEXT,
    created_at TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS context_state (
    session_id   TEXT PRIMARY KEY,
    used_tokens  INTEGER NOT NULL,
    capacity     INTEGER NOT NULL,
    dossier      TEXT,
    updated_at   TEXT NOT NULL
);

CREATE TABLE IF NOT EXISTS usage (
    id          INTEGER PRIMARY KEY AUTOINCREMENT,
    session_id  TEXT NOT NULL,
    prompt_tokens     INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    reasoning_tokens  INTEGER NOT NULL DEFAULT 0,
    created_at  TEXT NOT NULL
);
CREATE INDEX IF NOT EXISTS idx_usage_session ON usage(session_id, id);

CREATE TABLE IF NOT EXISTS kv (
    k TEXT PRIMARY KEY,
    v TEXT NOT NULL
);
"#;

/// Linha da tabela de sessões (espelha o esquema; nem toda coluna é usada
/// por todos os fluxos da interface).
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct SessionRow {
    pub id: String,
    pub project_path: String,
    pub title: String,
    pub model: String,
    pub reasoning_effort: Option<String>,
    pub permission: String,
    pub status: String,
    pub parent_id: Option<String>,
    pub objective: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Linha da tabela de mensagens (espelha o esquema; nem toda coluna é usada
/// por todos os fluxos da interface).
#[derive(Clone, Debug)]
#[allow(dead_code)]
pub struct MessageRow {
    pub id: i64,
    pub session_id: String,
    pub role: String,
    pub content: String,
    pub tool_calls: Option<String>,
    pub tool_call_id: Option<String>,
    pub tool_name: Option<String>,
    pub reasoning: Option<String>,
    pub created_at: String,
}

/// Linha da tabela de tarefas.
#[derive(Clone, Debug)]
pub struct TaskRow {
    pub id: String,
    pub session_id: String,
    pub parent_id: Option<String>,
    pub title: String,
    pub description: Option<String>,
    pub status: String,
    pub depends_on: Vec<String>,
    pub order_idx: i64,
    pub result: Option<String>,
}

/// Resultado de busca no histórico.
#[derive(Clone, Debug)]
pub struct SearchHit {
    pub session_id: String,
    pub session_title: String,
    pub role: String,
    pub content: String,
    pub created_at: String,
}

/// Handle compartilhado do banco.
#[derive(Clone)]
pub struct Store {
    conn: Arc<Mutex<Connection>>,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("criando diretório {}", parent.display()))?;
        }
        let conn = Connection::open(path)
            .with_context(|| format!("abrindo banco em {}", path.display()))?;
        // Com WAL, duas instâncias (ou a interface e um subagente) podem
        // disputar o lock; sem espera o segundo recebe SQLITE_BUSY na hora.
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .context("configurando espera de lock do banco")?;
        conn.execute_batch("PRAGMA journal_mode = WAL; PRAGMA foreign_keys = ON;")
            .context("aplicando pragmas do banco")?;
        crate::migrar::aplicar(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    #[cfg(test)]
    pub fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()?;
        crate::migrar::aplicar(&conn)?;
        Ok(Self {
            conn: Arc::new(Mutex::new(conn)),
        })
    }

    fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self
            .conn
            .lock()
            .map_err(|e| anyhow::anyhow!("lock do banco: {e}"))?;
        f(&guard)
    }

    /// Registra o uso de tokens de uma resposta do provedor.
    pub fn record_usage(
        &self,
        session_id: &str,
        prompt_tokens: u32,
        completion_tokens: u32,
        reasoning_tokens: u32,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO usage (session_id, prompt_tokens, completion_tokens, reasoning_tokens, created_at) \
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    session_id,
                    prompt_tokens,
                    completion_tokens,
                    reasoning_tokens,
                    crate::util::now_rfc3339()
                ],
            )?;
            Ok(())
        })
    }

    // ---------- sessões ----------

    #[allow(clippy::too_many_arguments)]
    pub fn create_session(
        &self,
        id: &str,
        project_path: &str,
        title: &str,
        model: &str,
        reasoning_effort: Option<&str>,
        permission: &str,
        parent_id: Option<&str>,
    ) -> Result<()> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "INSERT INTO sessions (id, project_path, title, model, reasoning_effort, permission, status, parent_id, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'ativa', ?7, ?8, ?8)",
                params![id, project_path, title, model, reasoning_effort, permission, parent_id, now],
            )?;
            Ok(())
        })
    }

    pub fn update_session_model(
        &self,
        id: &str,
        model: &str,
        reasoning_effort: Option<&str>,
    ) -> Result<()> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "UPDATE sessions SET model = ?2, reasoning_effort = ?3, updated_at = ?4 WHERE id = ?1",
                params![id, model, reasoning_effort, now],
            )?;
            Ok(())
        })
    }

    pub fn update_session_status(&self, id: &str, status: &str) -> Result<()> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "UPDATE sessions SET status = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, status, now],
            )?;
            Ok(())
        })
    }

    pub fn update_session_objective(&self, id: &str, objective: &str) -> Result<()> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "UPDATE sessions SET objective = ?2, updated_at = ?3 WHERE id = ?1",
                params![id, objective, now],
            )?;
            Ok(())
        })
    }

    pub fn touch_session(&self, id: &str) -> Result<()> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "UPDATE sessions SET updated_at = ?2 WHERE id = ?1",
                params![id, now],
            )?;
            Ok(())
        })
    }

    pub fn get_session(&self, id: &str) -> Result<Option<SessionRow>> {
        self.with(|c| {
            let row = c
                .query_row(
                    "SELECT id, project_path, title, model, reasoning_effort, permission, status, parent_id, objective, created_at, updated_at
                     FROM sessions WHERE id = ?1",
                    params![id],
                    map_session,
                )
                .optional()?;
            Ok(row)
        })
    }

    /// Sessões recentes de um projeto (ou de todos, quando `project` é `None`).
    pub fn list_sessions(&self, project: Option<&str>, limit: usize) -> Result<Vec<SessionRow>> {
        self.with(|c| {
            let mut rows = Vec::new();
            match project {
                Some(p) => {
                    let mut stmt = c.prepare(
                        "SELECT id, project_path, title, model, reasoning_effort, permission, status, parent_id, objective, created_at, updated_at
                         FROM sessions WHERE project_path = ?1 ORDER BY updated_at DESC LIMIT ?2",
                    )?;
                    let iter = stmt.query_map(params![p, limit as i64], map_session)?;
                    for r in iter {
                        rows.push(r?);
                    }
                }
                None => {
                    let mut stmt = c.prepare(
                        "SELECT id, project_path, title, model, reasoning_effort, permission, status, parent_id, objective, created_at, updated_at
                         FROM sessions ORDER BY updated_at DESC LIMIT ?1",
                    )?;
                    let iter = stmt.query_map(params![limit as i64], map_session)?;
                    for r in iter {
                        rows.push(r?);
                    }
                }
            }
            Ok(rows)
        })
    }

    /// Exclui sessões inteiras numa transação atômica: a sessão e tudo o que
    /// pertence a ela (mensagens, planos, tarefas, auditoria, uso de tokens…).
    /// Devolve quantas sessões foram excluídas.
    pub fn delete_sessions(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let marcadores = vec!["?"; ids.len()].join(",");
        self.with(|c| {
            c.execute_batch("BEGIN IMMEDIATE")?;
            let delecao = (|| -> Result<usize> {
                // Sem chave estrangeira com cascade, cada tabela filha precisa
                // ser limpa explicitamente; o banco inteiro fica consistente.
                let dependentes = [
                    "messages",
                    "tool_runs",
                    "plans",
                    "tasks",
                    "file_changes",
                    "decisions",
                    "audit",
                    "context_state",
                    "usage",
                ];
                for tabela in dependentes {
                    let sql = format!("DELETE FROM {tabela} WHERE session_id IN ({marcadores})");
                    let mut stmt = c.prepare(&sql)?;
                    stmt.execute(rusqlite::params_from_iter(ids.iter()))?;
                }
                let sql = format!("DELETE FROM sessions WHERE id IN ({marcadores})");
                let mut stmt = c.prepare(&sql)?;
                let excluidas = stmt.execute(rusqlite::params_from_iter(ids.iter()))?;
                Ok(excluidas as usize)
            })();
            match delecao {
                Ok(n) => {
                    c.execute_batch("COMMIT")?;
                    Ok(n)
                }
                Err(e) => {
                    let _ = c.execute_batch("ROLLBACK");
                    Err(e)
                }
            }
        })
    }

    /// Última sessão de um projeto que pode ser retomada.
    pub fn last_resumable_session(&self, project: &str) -> Result<Option<SessionRow>> {
        self.with(|c| {
            let row = c
                .query_row(
                    "SELECT id, project_path, title, model, reasoning_effort, permission, status, parent_id, objective, created_at, updated_at
                     FROM sessions WHERE project_path = ?1 AND status IN ('ativa','interrompida','pausada')
                     ORDER BY updated_at DESC LIMIT 1",
                    params![project],
                    map_session,
                )
                .optional()?;
            Ok(row)
        })
    }

    /// Sessões marcadas como ativas no encerramento anterior (recuperação).
    #[cfg(test)]
    pub fn sessions_para_recuperar(&self) -> Result<Vec<SessionRow>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, project_path, title, model, reasoning_effort, permission, status, parent_id, objective, created_at, updated_at
                 FROM sessions WHERE status = 'ativa' ORDER BY updated_at DESC",
            )?;
            let iter = stmt.query_map([], map_session)?;
            let mut rows = Vec::new();
            for r in iter {
                rows.push(r?);
            }
            Ok(rows)
        })
    }

    // ---------- mensagens ----------

    /// Insere uma mensagem e devolve o id gerado.
    #[allow(clippy::too_many_arguments)]
    pub fn append_message(
        &self,
        session_id: &str,
        role: &str,
        content: &str,
        tool_calls: Option<&str>,
        tool_call_id: Option<&str>,
        tool_name: Option<&str>,
        reasoning: Option<&str>,
    ) -> Result<i64> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "INSERT INTO messages (session_id, role, content, tool_calls, tool_call_id, tool_name, reasoning, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                params![session_id, role, content, tool_calls, tool_call_id, tool_name, reasoning, now],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// Últimas `limit` mensagens (ordem cronológica).
    pub fn recent_messages(&self, session_id: &str, limit: usize) -> Result<Vec<MessageRow>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT * FROM (
                    SELECT id, session_id, role, content, tool_calls, tool_call_id, tool_name, reasoning, created_at
                    FROM messages WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2
                 ) ORDER BY id ASC",
            )?;
            let iter = stmt.query_map(params![session_id, limit as i64], |r| {
                Ok(MessageRow {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    role: r.get(2)?,
                    content: r.get(3)?,
                    tool_calls: r.get(4)?,
                    tool_call_id: r.get(5)?,
                    tool_name: r.get(6)?,
                    reasoning: r.get(7)?,
                    created_at: r.get(8)?,
                })
            })?;
            let mut rows = Vec::new();
            for r in iter {
                rows.push(r?);
            }
            Ok(rows)
        })
    }

    pub fn search_messages(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>> {
        // `%`/`_` são curingas do LIKE: sem escapar, buscar `%` devolvia tudo.
        let termo = query
            .to_lowercase()
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        let like = format!("%{termo}%");
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT m.session_id, s.title, m.role, m.content, m.created_at
                 FROM messages m JOIN sessions s ON s.id = m.session_id
                 WHERE lower(m.content) LIKE ?1 ESCAPE '\\'
                 ORDER BY m.id DESC LIMIT ?2",
            )?;
            let iter = stmt.query_map(params![like, limit as i64], |r| {
                Ok(SearchHit {
                    session_id: r.get(0)?,
                    session_title: r.get(1)?,
                    role: r.get(2)?,
                    content: r.get(3)?,
                    created_at: r.get(4)?,
                })
            })?;
            let mut hits = Vec::new();
            for r in iter {
                hits.push(r?);
            }
            Ok(hits)
        })
    }

    // ---------- execuções de ferramentas ----------

    #[allow(clippy::too_many_arguments)]
    pub fn record_tool_run(
        &self,
        session_id: &str,
        name: &str,
        args: &str,
        result: Option<&str>,
        status: &str,
        error: Option<&str>,
        duration_ms: u64,
    ) -> Result<i64> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "INSERT INTO tool_runs (session_id, name, args, result, status, error, started_at, finished_at, duration_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7, ?8)",
                params![
                    session_id,
                    name,
                    args,
                    result,
                    status,
                    error,
                    now,
                    duration_ms as i64
                ],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    // ---------- planos e tarefas ----------

    pub fn save_plan(&self, session_id: &str, objective: &str, steps: &str) -> Result<i64> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            // Ler o máximo e inserir na mesma transação: antes, dois planos
            // simultâneos podiam receber a mesma `version`.
            let tx = c.unchecked_transaction()?;
            let version: i64 = tx.query_row(
                "SELECT COALESCE(MAX(version), 0) + 1 FROM plans WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )?;
            tx.execute(
                "INSERT INTO plans (session_id, objective, version, steps, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, objective, version, steps, now],
            )?;
            tx.commit()?;
            Ok(version)
        })
    }

    pub fn latest_plan(&self, session_id: &str) -> Result<Option<(String, String, i64)>> {
        self.with(|c| {
            let row = c
                .query_row(
                    "SELECT objective, steps, version FROM plans WHERE session_id = ?1 ORDER BY version DESC LIMIT 1",
                    params![session_id],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .optional()?;
            Ok(row)
        })
    }

    pub fn upsert_task(&self, t: &TaskRow) -> Result<()> {
        let now = crate::util::now_rfc3339();
        let deps = serde_json::to_string(&t.depends_on)?;
        self.with(|c| {
            c.execute(
                "INSERT INTO tasks (id, session_id, parent_id, title, description, status, depends_on, order_idx, result, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?10)
                 ON CONFLICT(id, session_id) DO UPDATE SET
                    parent_id = excluded.parent_id,
                    title = excluded.title,
                    description = excluded.description,
                    status = excluded.status,
                    depends_on = excluded.depends_on,
                    order_idx = excluded.order_idx,
                    result = excluded.result,
                    updated_at = excluded.updated_at",
                params![
                    t.id, t.session_id, t.parent_id, t.title, t.description,
                    t.status, deps, t.order_idx, t.result, now
                ],
            )?;
            Ok(())
        })
    }

    pub fn tasks(&self, session_id: &str) -> Result<Vec<TaskRow>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT id, session_id, parent_id, title, description, status, depends_on, order_idx, result
                 FROM tasks WHERE session_id = ?1 ORDER BY order_idx ASC, id ASC",
            )?;
            let iter = stmt.query_map(params![session_id], |r| {
                let deps: Option<String> = r.get(6)?;
                Ok(TaskRow {
                    id: r.get(0)?,
                    session_id: r.get(1)?,
                    parent_id: r.get(2)?,
                    title: r.get(3)?,
                    description: r.get(4)?,
                    status: r.get(5)?,
                    depends_on: deps
                        .and_then(|d| serde_json::from_str(&d).ok())
                        .unwrap_or_default(),
                    order_idx: r.get(7)?,
                    result: r.get(8)?,
                })
            })?;
            let mut rows = Vec::new();
            for r in iter {
                rows.push(r?);
            }
            Ok(rows)
        })
    }

    // ---------- alterações de arquivo ----------

    /// Última alteração registrada numa sessão, com o diff completo — usada
    /// pelo `/undo` para desfazer sem depender de git.
    pub fn ultima_alteracao(&self, session_id: &str) -> Result<Option<AlteracaoRecente>> {
        self.with(|c| {
            let linha = c
                .query_row(
                    "SELECT path, change_type, diff FROM file_changes \
                     WHERE session_id = ?1 AND diff IS NOT NULL ORDER BY id DESC LIMIT 1",
                    params![session_id],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, String>(1)?,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?;
            Ok(linha.map(|(path, tipo, diff)| AlteracaoRecente {
                path,
                change_type: tipo,
                diff: diff.unwrap_or_default(),
            }))
        })
    }

    /// Todas as alterações da sessão (para o `/diff`).
    pub fn alteracoes(&self, session_id: &str) -> Result<Vec<AlteracaoRecente>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT path, change_type, diff FROM file_changes \
                 WHERE session_id = ?1 AND diff IS NOT NULL ORDER BY id ASC",
            )?;
            let linhas = stmt
                .query_map(params![session_id], |r| {
                    Ok(AlteracaoRecente {
                        path: r.get(0)?,
                        change_type: r.get(1)?,
                        diff: r.get::<_, Option<String>>(2)?.unwrap_or_default(),
                    })
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(linhas)
        })
    }

    pub fn record_file_change(
        &self,
        session_id: &str,
        path: &str,
        change_type: &str,
        additions: usize,
        deletions: usize,
        diff: Option<&str>,
    ) -> Result<i64> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "INSERT INTO file_changes (session_id, path, change_type, additions, deletions, diff, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                params![session_id, path, change_type, additions as i64, deletions as i64, diff, now],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn file_changes(&self, session_id: &str) -> Result<Vec<(String, String, usize, usize)>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT path, change_type, additions, deletions FROM file_changes WHERE session_id = ?1 ORDER BY id ASC",
            )?;
            let iter = stmt.query_map(params![session_id], |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, i64>(2)? as usize,
                    r.get::<_, i64>(3)? as usize,
                ))
            })?;
            let mut rows = Vec::new();
            for r in iter {
                rows.push(r?);
            }
            Ok(rows)
        })
    }

    // ---------- decisões e auditoria ----------

    pub fn record_decision(
        &self,
        session_id: &str,
        kind: &str,
        summary: &str,
        detail: Option<&str>,
    ) -> Result<i64> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "INSERT INTO decisions (session_id, kind, summary, detail, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, kind, summary, detail, now],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    pub fn decisions(
        &self,
        session_id: &str,
        limit: usize,
    ) -> Result<Vec<(String, String, String)>> {
        self.with(|c| {
            let mut stmt = c.prepare(
                "SELECT kind, summary, created_at FROM decisions WHERE session_id = ?1 ORDER BY id DESC LIMIT ?2",
            )?;
            let iter = stmt.query_map(params![session_id, limit as i64], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?;
            let mut rows = Vec::new();
            for r in iter {
                rows.push(r?);
            }
            Ok(rows)
        })
    }

    pub fn audit(
        &self,
        session_id: Option<&str>,
        kind: &str,
        payload: Option<&str>,
        decision: Option<&str>,
    ) -> Result<()> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "INSERT INTO audit (session_id, kind, payload, decision, created_at) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![session_id, kind, payload, decision, now],
            )?;
            Ok(())
        })
    }

    // ---------- contexto ----------

    pub fn set_context_state(
        &self,
        session_id: &str,
        used: u32,
        capacity: u32,
        dossier: Option<&str>,
    ) -> Result<()> {
        let now = crate::util::now_rfc3339();
        self.with(|c| {
            c.execute(
                "INSERT INTO context_state (session_id, used_tokens, capacity, dossier, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(session_id) DO UPDATE SET used_tokens = excluded.used_tokens,
                   capacity = excluded.capacity, dossier = excluded.dossier, updated_at = excluded.updated_at",
                params![session_id, used as i64, capacity as i64, dossier, now],
            )?;
            Ok(())
        })
    }

    pub fn context_state(&self, session_id: &str) -> Result<Option<(u32, u32, Option<String>)>> {
        self.with(|c| {
            let row = c
                .query_row(
                    "SELECT used_tokens, capacity, dossier FROM context_state WHERE session_id = ?1",
                    params![session_id],
                    |r| {
                        Ok((
                            r.get::<_, i64>(0)? as u32,
                            r.get::<_, i64>(1)? as u32,
                            r.get::<_, Option<String>>(2)?,
                        ))
                    },
                )
                .optional()?;
            Ok(row)
        })
    }

    // ---------- kv ----------

    pub fn kv_set(&self, key: &str, value: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO kv (k, v) VALUES (?1, ?2) ON CONFLICT(k) DO UPDATE SET v = excluded.v",
                params![key, value],
            )?;
            Ok(())
        })
    }

    /// Estatísticas gerais para o relatório de conclusão.
    pub fn session_stats(&self, session_id: &str) -> Result<SessionStats> {
        self.with(|c| {
            let tools: i64 = c.query_row(
                "SELECT COUNT(*) FROM tool_runs WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )?;
            let failed: i64 = c.query_row(
                "SELECT COUNT(*) FROM tool_runs WHERE session_id = ?1 AND status = 'falha'",
                params![session_id],
                |r| r.get(0),
            )?;
            let (adds, dels): (i64, i64) = c.query_row(
                "SELECT COALESCE(SUM(additions),0), COALESCE(SUM(deletions),0) FROM file_changes WHERE session_id = ?1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?;
            let files: i64 = c.query_row(
                "SELECT COUNT(DISTINCT path) FROM file_changes WHERE session_id = ?1",
                params![session_id],
                |r| r.get(0),
            )?;
            let (p_tok, c_tok, r_tok): (i64, i64, i64) = c.query_row(
                "SELECT COALESCE(SUM(prompt_tokens),0), COALESCE(SUM(completion_tokens),0), \
                 COALESCE(SUM(reasoning_tokens),0) FROM usage WHERE session_id = ?1",
                params![session_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?;
            Ok(SessionStats {
                tool_runs: tools as usize,
                tool_failures: failed as usize,
                additions: adds as usize,
                deletions: dels as usize,
                files_touched: files as usize,
                prompt_tokens: p_tok.max(0) as u64,
                completion_tokens: c_tok.max(0) as u64,
                reasoning_tokens: r_tok.max(0) as u64,
            })
        })
    }
}

/// Alteração de arquivo registrada, com o diff guardado — usado pelo
/// `/undo` (desfazer) e pelo `/diff` (revisar).
#[derive(Clone, Debug)]
pub struct AlteracaoRecente {
    pub path: String,
    pub change_type: String,
    pub diff: String,
}

#[derive(Clone, Copy, Debug, Default)]
pub struct SessionStats {
    pub tool_runs: usize,
    pub tool_failures: usize,
    pub additions: usize,
    pub deletions: usize,
    pub files_touched: usize,
    /// Tokens de entrada somados de todas as respostas do provedor.
    pub prompt_tokens: u64,
    /// Tokens de saída (incluindo raciocínio, quando o provedor separa).
    pub completion_tokens: u64,
    /// Tokens de raciocínio, quando informados.
    pub reasoning_tokens: u64,
}

impl SessionStats {
    /// Total de tokens gastos na sessão.
    pub fn total_tokens(&self) -> u64 {
        self.prompt_tokens + self.completion_tokens
    }
}

fn map_session(r: &rusqlite::Row<'_>) -> rusqlite::Result<SessionRow> {
    Ok(SessionRow {
        id: r.get(0)?,
        project_path: r.get(1)?,
        title: r.get(2)?,
        model: r.get(3)?,
        reasoning_effort: r.get(4)?,
        permission: r.get(5)?,
        status: r.get(6)?,
        parent_id: r.get(7)?,
        objective: r.get(8)?,
        created_at: r.get(9)?,
        updated_at: r.get(10)?,
    })
}

/// Um banco antigo (versão 1, sem `objective`) precisa migrar sozinho, sem
/// quebrar a abertura — antes o `map_session` falhava em runtime com
/// `no such column`.
#[test]
fn migracao_de_banco_antigo_e_automatica() {
    use rusqlite::Connection;
    let dir = std::env::temp_dir().join(format!("kestrel-migra-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let caminho = dir.join("kestrel.db");
    // Cria um banco "versão 1": o esquema base sem a coluna nova.
    {
        let conn = Connection::open(&caminho).unwrap();
        conn.execute_batch(SCHEMA_BASE).unwrap();
        conn.execute_batch("PRAGMA user_version = 1").unwrap();
        // Uma sessão gravada no formato antigo.
        conn.execute(
            "INSERT INTO sessions (id, project_path, title, model, permission, status, created_at, updated_at)              VALUES ('s1', '/tmp', 'antiga', 'm', 'semi', 'ativa', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
    }
    // Abrir pelo Store aplica a migração.
    let store = Store::open(&caminho).unwrap();
    let linha = store.get_session("s1").unwrap().unwrap();
    // A coluna nova existe e quem a lê funciona.
    let _ = linha.objective;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Banco na versão 2 (anterior à tabela `usage`) ganha a tabela ao abrir.
#[test]
fn migracao_2_para_3_cria_usage() {
    use rusqlite::Connection;
    let dir = std::env::temp_dir().join(format!("kestrel-usage-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let caminho = dir.join("kestrel.db");
    {
        let conn = Connection::open(&caminho).unwrap();
        conn.execute_batch(SCHEMA_BASE).unwrap();
        // Simula o banco antigo: a tabela ainda não existia.
        conn.execute_batch("DROP TABLE usage; PRAGMA user_version = 2;")
            .unwrap();
    }
    let store = Store::open(&caminho).unwrap();
    store.record_usage("s1", 123, 45, 6).unwrap();
    let stats = store.session_stats("s1").unwrap();
    assert_eq!(stats.prompt_tokens, 123);
    assert_eq!(stats.completion_tokens, 45);
    assert_eq!(stats.reasoning_tokens, 6);
    let _ = std::fs::remove_dir_all(&dir);
}

/// Banco na versão 3 tinha `tasks.id` como chave global. A migração precisa
/// preservar as tarefas e permitir o mesmo id em sessões diferentes.
#[test]
fn migracao_3_para_4_torna_id_de_tarefa_por_sessao() {
    use rusqlite::Connection;
    let dir = std::env::temp_dir().join(format!("kestrel-tarefas-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let caminho = dir.join("kestrel.db");
    {
        let conn = Connection::open(&caminho).unwrap();
        conn.execute_batch(SCHEMA_BASE).unwrap();
        // Recria a tabela no formato antigo (id global).
        conn.execute_batch(
            "DROP TABLE tasks;
             CREATE TABLE tasks (
                id TEXT PRIMARY KEY,
                session_id TEXT NOT NULL,
                parent_id TEXT,
                title TEXT NOT NULL,
                description TEXT,
                status TEXT NOT NULL,
                depends_on TEXT,
                order_idx INTEGER NOT NULL DEFAULT 0,
                result TEXT,
                created_at TEXT NOT NULL,
                updated_at TEXT NOT NULL
             );
             INSERT INTO tasks (id, session_id, title, status, created_at, updated_at)
                VALUES ('t1', 's1', 'antiga', 'pendente', '2026-01-01', '2026-01-01');
             PRAGMA user_version = 3;",
        )
        .unwrap();
    }
    let store = Store::open(&caminho).unwrap();
    assert_eq!(store.tasks("s1").unwrap()[0].title, "antiga");
    store
        .upsert_task(&TaskRow {
            id: "t1".into(),
            session_id: "s2".into(),
            parent_id: None,
            title: "nova".into(),
            description: None,
            status: "pendente".into(),
            depends_on: Vec::new(),
            order_idx: 0,
            result: None,
        })
        .unwrap();
    assert_eq!(store.tasks("s1").unwrap()[0].title, "antiga");
    assert_eq!(store.tasks("s2").unwrap()[0].title, "nova");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Abrir um banco de versão futura falha com mensagem clara, em vez de
/// silenciosamente rebaixar o esquema.
#[test]
fn banco_de_versao_futura_e_recusado() {
    use rusqlite::Connection;
    let dir = std::env::temp_dir().join(format!("kestrel-futuro-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let caminho = dir.join("kestrel.db");
    {
        let conn = Connection::open(&caminho).unwrap();
        conn.execute_batch(SCHEMA_BASE).unwrap();
        conn.execute_batch("PRAGMA user_version = 99").unwrap();
    }
    let erro = match Store::open(&caminho) {
        Ok(_) => panic!("banco de versão futura deveria ser recusado"),
        Err(e) => e.to_string(),
    };
    assert!(
        erro.contains("versão") || erro.contains("versao"),
        "mensagem pouco clara: {erro}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> Store {
        Store::open_in_memory().expect("banco em memória")
    }

    #[test]
    fn cria_e_recupera_sessao() {
        let s = store();
        s.create_session(
            "s1",
            "/proj",
            "tarefa",
            "modelo/x",
            Some("high"),
            "semi",
            None,
        )
        .unwrap();
        let row = s.get_session("s1").unwrap().expect("sessão existe");
        assert_eq!(row.model, "modelo/x");
        assert_eq!(row.reasoning_effort.as_deref(), Some("high"));
        s.update_session_status("s1", "concluida").unwrap();
        assert_eq!(s.get_session("s1").unwrap().unwrap().status, "concluida");
    }

    #[test]
    fn mensagens_em_ordem_e_busca() {
        let s = store();
        s.create_session("s1", "/p", "t", "m", None, "read", None)
            .unwrap();
        s.append_message(
            "s1",
            "user",
            "corrigir bug do parser",
            None,
            None,
            None,
            None,
        )
        .unwrap();
        s.append_message(
            "s1",
            "assistant",
            "vou analisar",
            None,
            None,
            None,
            Some("pensando"),
        )
        .unwrap();
        let msgs = s.recent_messages("s1", 10).unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].reasoning.as_deref(), Some("pensando"));
        let hits = s.search_messages("parser", 5).unwrap();
        assert_eq!(hits.len(), 1);
    }

    #[test]
    fn planos_tarefas_e_alteracoes() {
        let s = store();
        s.create_session("s1", "/p", "t", "m", None, "semi", None)
            .unwrap();
        s.save_plan("s1", "objetivo", "[{\"passo\":1}]").unwrap();
        s.save_plan("s1", "objetivo v2", "[]").unwrap();
        let (objective, _, version) = s.latest_plan("s1").unwrap().unwrap();
        assert_eq!(objective, "objetivo v2");
        assert_eq!(version, 2);
        s.upsert_task(&TaskRow {
            id: "t1".into(),
            session_id: "s1".into(),
            parent_id: None,
            title: "primeira".into(),
            description: None,
            status: "pendente".into(),
            depends_on: vec!["t0".into()],
            order_idx: 1,
            result: None,
        })
        .unwrap();
        let tasks = s.tasks("s1").unwrap();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].depends_on, vec!["t0".to_string()]);
        s.record_file_change("s1", "src/main.rs", "editar", 10, 2, None)
            .unwrap();
        let stats = s.session_stats("s1").unwrap();
        assert_eq!(stats.additions, 10);
        assert_eq!(stats.files_touched, 1);
    }

    /// O uso de tokens informado pelo provedor precisa ser persistido: antes
    /// `completion.usage` era parseado e descartado, então não havia como
    /// saber quanto a sessão tinha gasto.
    #[test]
    fn usage_e_somado_na_sessao() {
        let s = store();
        s.create_session("s1", "/tmp", "t", "m", None, "semi", None)
            .unwrap();
        assert_eq!(s.session_stats("s1").unwrap().total_tokens(), 0);
        s.record_usage("s1", 1200, 350, 100).unwrap();
        s.record_usage("s1", 800, 120, 0).unwrap();
        let stats = s.session_stats("s1").unwrap();
        assert_eq!(stats.prompt_tokens, 2000);
        assert_eq!(stats.completion_tokens, 470);
        assert_eq!(stats.reasoning_tokens, 100);
        assert_eq!(stats.total_tokens(), 2470);
    }

    #[test]
    fn recuperacao_de_sessao_ativa() {
        let s = store();
        s.create_session("s1", "/p", "t", "m", None, "semi", None)
            .unwrap();
        let pend = s.sessions_para_recuperar().unwrap();
        assert_eq!(pend.len(), 1);
        assert!(s.last_resumable_session("/p").unwrap().is_some());
        assert!(s.last_resumable_session("/outro").unwrap().is_none());
    }

    #[test]
    fn contexto_persistido() {
        let s = store();
        s.create_session("s1", "/p", "t", "m", None, "semi", None)
            .unwrap();
        s.set_context_state("s1", 100, 1000, Some("dossie"))
            .unwrap();
        let (used, cap, dossier) = s.context_state("s1").unwrap().unwrap();
        assert_eq!((used, cap), (100, 1000));
        assert_eq!(dossier.as_deref(), Some("dossie"));
    }

    /// Excluir sessões remove a sessão e tudo o que pertence a ela, sem
    /// tocar nas outras.
    #[test]
    fn deletar_sessoes_remove_tudo_associado() {
        let s = store();
        s.create_session("s1", "/p", "t1", "m", None, "semi", None)
            .unwrap();
        s.create_session("s2", "/p", "t2", "m", None, "semi", None)
            .unwrap();
        s.create_session("s3", "/p", "t3", "m", None, "semi", None)
            .unwrap();
        for (id, texto) in [("s1", "msg1"), ("s2", "msg2"), ("s3", "msg3")] {
            let conn = s.conn.lock().unwrap();
            conn.execute(
                "INSERT INTO messages (session_id, role, content, created_at) \
                 VALUES (?1, 'user', ?2, '2026-01-01')",
                rusqlite::params![id, texto],
            )
            .unwrap();
        }
        s.record_usage("s1", 10, 0, 0).unwrap();
        s.audit(Some("s1"), "teste", None, Some("permitido"))
            .unwrap();
        s.set_context_state("s2", 5, 1000, None).unwrap();

        let excluidas = s.delete_sessions(&["s1".into(), "s2".into()]).unwrap();
        assert_eq!(excluidas, 2);
        assert!(s.get_session("s1").unwrap().is_none());
        assert!(s.get_session("s2").unwrap().is_none());
        assert!(s.get_session("s3").unwrap().is_some());

        // Consulta direta sob o lock; o guard é solto antes de qualquer
        // próxima chamada via `Store::with` (o mutex não é reentrante).
        {
            let conn = s.conn.lock().unwrap();
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE session_id IN ('s1','s2')",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "mensagens das sessões excluídas deveriam sumir");
            let n: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM messages WHERE session_id = 's3'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 1, "mensagens da sessão restante não podem ser tocadas");
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM usage", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0);
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM audit", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0);
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM context_state", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0);
        }
        let restantes = s.list_sessions(None, 10).unwrap();
        assert_eq!(restantes.len(), 1);
        assert_eq!(restantes[0].id, "s3");
    }

    /// Nenhuma lista vazia de sessões deve tocar no banco.
    #[test]
    fn deletar_sessoes_vazias_e_nao_op() {
        let s = store();
        assert_eq!(s.delete_sessions(&[]).unwrap(), 0);
    }
}
