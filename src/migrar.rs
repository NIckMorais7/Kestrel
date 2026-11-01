//! Migrações do banco, versionadas por `PRAGMA user_version`.
//!
//! Antes cada `open` apenas rodava `CREATE TABLE IF NOT EXISTS`: numa base
//! antiga, uma coluna nova simplesmente não existia e o primeiro `map_session`
//! falhava em runtime com `no such column`, sem detecção na abertura. Agora a
//! versão fica gravada no banco e cada migração é aplicada na ordem, uma única
//! vez, dentro de uma transação.

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Versão atual do esquema. Suba sempre que uma migração for acrescentada.
pub const VERSAO_ATUAL: i64 = 5;

/// Migrações; o índice 0 corresponde à versão 1.
const MIGRACOES: &[(i64, &str)] = &[
    // 1 → 2: coluna usada para marcar o objetivo da sessão (continuidade).
    (2, "ALTER TABLE sessions ADD COLUMN objective TEXT;"),
    // 2 → 3: tabela de uso de tokens. Ela nasceu apenas no `SCHEMA_BASE`; um
    // banco criado antes ficava sem a tabela e `/cost`/estatísticas falhavam
    // (ou mostravam zero) sem nenhum aviso.
    (
        3,
        "CREATE TABLE IF NOT EXISTS usage (
            id          INTEGER PRIMARY KEY AUTOINCREMENT,
            session_id  TEXT NOT NULL,
            prompt_tokens     INTEGER NOT NULL DEFAULT 0,
            completion_tokens INTEGER NOT NULL DEFAULT 0,
            reasoning_tokens  INTEGER NOT NULL DEFAULT 0,
            created_at  TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_usage_session ON usage(session_id, id);",
    ),
    // 3 → 4: a chave primária de `tasks` era só `id`, então o `t1` de uma
    // sessão sobrescrevia o `t1` de outra. Reconstrói a tabela com chave
    // composta (id, session_id).
    (
        4,
        "CREATE TABLE tasks_novo (
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
            PRIMARY KEY (id, session_id)
        );
        INSERT INTO tasks_novo
            (id, session_id, parent_id, title, description, status, depends_on, order_idx, result, created_at, updated_at)
            SELECT id, session_id, parent_id, title, description, status, depends_on, order_idx, result, created_at, updated_at
            FROM tasks;
        DROP TABLE tasks;
        ALTER TABLE tasks_novo RENAME TO tasks;
        CREATE INDEX IF NOT EXISTS idx_tasks_session ON tasks(session_id, order_idx);",
    ),
    // 4 → 5: marca alterações já desfeitas pelo `/undo`. Sem isso, desfazer
    // duas vezes seguidas reaplicava o mesmo diff em vez de voltar à alteração
    // anterior (e o segundo diff nem batia mais com o arquivo).
    (
        5,
        "ALTER TABLE file_changes ADD COLUMN undone INTEGER NOT NULL DEFAULT 0;",
    ),
];

/// Aplica o esquema base e as migrações pendentes.
pub fn aplicar(conn: &Connection) -> Result<()> {
    // `user_version` começa em 0 num banco novo: o esquema base precisa rodar
    // antes de qualquer migração, e é o que traz a versão para 1.
    let versao: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    if versao == 0 {
        conn.execute_batch(crate::db::SCHEMA_BASE)
            .context("aplicando esquema base")?;
        conn.execute_batch("PRAGMA user_version = 1")
            .context("marcando versão 1")?;
    }
    let mut atual: i64 = conn
        .query_row("PRAGMA user_version", [], |r| r.get(0))
        .unwrap_or(0);
    for (alvo, sql) in MIGRACOES {
        if atual >= *alvo {
            continue;
        }
        let tx = conn
            .unchecked_transaction()
            .with_context(|| format!("transação da migração para {alvo}"))?;
        // Uma coluna que já existe (base antiga corrigida à mão) não deve
        // derrubar a migração: detectamos e seguimos.
        let resultado = tx.execute_batch(sql);
        match resultado {
            Ok(()) => {}
            Err(erro) => {
                if erro.to_string().contains("duplicate column name") {
                    // Já aplicada fora da migração; apenas marca a versão.
                } else {
                    return Err(erro).with_context(|| format!("migrando para {alvo}"));
                }
            }
        }
        tx.execute_batch(&format!("PRAGMA user_version = {alvo}"))
            .with_context(|| format!("marcando versão {alvo}"))?;
        tx.commit()
            .with_context(|| format!("commit da migração {alvo}"))?;
        atual = *alvo;
    }
    if atual > VERSAO_ATUAL {
        // Banco de versão mais nova que este binário: não tentamos rebaixar.
        return Err(anyhow::anyhow!(
            "o banco está na versão {atual}, mas este Kestrel conhece até {VERSAO_ATUAL}. \
             Atualize o Kestrel para abrir este banco."
        ));
    }
    Ok(())
}
