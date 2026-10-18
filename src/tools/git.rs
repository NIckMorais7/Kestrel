//! Integração Git real: estado, diffs, histórico, preparação, commits e
//! branches. Os comandos são executados com argumentos explícitos (sem shell),
//! preservando alterações preexistentes do usuário.

use super::{
    Tool, ToolContext, ToolError, ToolOutput, arg_bool, arg_str, arg_str_opt, arg_u64,
    recortar_saida, resolver_caminho,
};
use crate::provider::ToolSpec;
use serde_json::{Value, json};
use std::path::PathBuf;

/// Executa um comando git no diretório do projeto.
async fn git(ctx: &ToolContext, args: &[&str]) -> Result<(bool, String), ToolError> {
    let saida = tokio::process::Command::new("git")
        .args(args)
        .current_dir(&ctx.workdir)
        .output()
        .await
        .map_err(|e| {
            ToolError::msg(format!(
                "não foi possível executar `git {}`: {e}. O git está instalado?",
                args.join(" ")
            ))
        })?;
    let mut texto = String::from_utf8_lossy(&saida.stdout).to_string();
    let erros = String::from_utf8_lossy(&saida.stderr);
    if !erros.trim().is_empty() {
        if !texto.is_empty() {
            texto.push('\n');
        }
        texto.push_str(erros.trim_end());
    }
    Ok((saida.status.success(), texto.trim_end().to_string()))
}

fn eh_repositorio_hint(erro: &str) -> String {
    if erro.contains("not a git repository") {
        "\n\nO diretório atual não é um repositório Git. Use run_command com `git init` se quiser versionar o projeto.".into()
    } else {
        String::new()
    }
}

pub struct GitStatus;

#[async_trait::async_trait]
impl Tool for GitStatus {
    fn nome(&self) -> &'static str {
        "git_status"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Mostra o estado do repositório Git: branch atual, arquivos modificados, \
                preparados e não rastreados. Use antes de editar para preservar alterações do usuário."
                .into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn executar(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let (ok, status) = git(ctx, &["status", "--porcelain=v1", "--branch"]).await?;
        if !ok {
            return Err(ToolError::msg(format!(
                "falha ao consultar o estado do Git: {status}{}",
                eh_repositorio_hint(&status)
            )));
        }
        let mut branch = String::new();
        let mut preparados = Vec::new();
        let mut modificados = Vec::new();
        let mut nao_rastreados = Vec::new();
        for linha in status.lines() {
            if let Some(resto) = linha.strip_prefix("## ") {
                branch = resto.to_string();
                continue;
            }
            let (codigo, arquivo) = linha.split_at(
                linha
                    .char_indices()
                    .nth(2)
                    .map(|(i, _)| i)
                    .unwrap_or(linha.len()),
            );
            let arquivo = arquivo.trim();
            if codigo.starts_with("??") {
                nao_rastreados.push(arquivo.to_string());
            } else if codigo.starts_with(" ") {
                modificados.push(format!("{codigo} {arquivo}"));
            } else {
                preparados.push(format!("{codigo} {arquivo}"));
            }
        }
        let mut texto = format!(
            "Branch: {}\n",
            if branch.is_empty() {
                "(nenhum)"
            } else {
                &branch
            }
        );
        if preparados.is_empty() && modificados.is_empty() && nao_rastreados.is_empty() {
            texto.push_str("Árvore de trabalho limpa: nenhuma alteração pendente.");
        } else {
            if !preparados.is_empty() {
                texto.push_str(&format!(
                    "\nPreparados para commit ({}):\n  {}\n",
                    preparados.len(),
                    preparados.join("\n  ")
                ));
            }
            if !modificados.is_empty() {
                texto.push_str(&format!(
                    "\nModificados, não preparados ({}):\n  {}\n",
                    modificados.len(),
                    modificados.join("\n  ")
                ));
            }
            if !nao_rastreados.is_empty() {
                texto.push_str(&format!(
                    "\nNão rastreados ({}):\n  {}\n",
                    nao_rastreados.len(),
                    nao_rastreados.join("\n  ")
                ));
            }
        }
        Ok(
            ToolOutput::texto(texto.trim_end().to_string()).com_resumo(format!(
                "git status: {} alterações",
                preparados.len() + modificados.len()
            )),
        )
    }
}

pub struct GitDiff;

#[async_trait::async_trait]
impl Tool for GitDiff {
    fn nome(&self) -> &'static str {
        "git_diff"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Mostra diffs do repositório. Modos: `working` (não preparado), \
                `staged` (preparado), `ultimo_commit` (commit mais recente) ou uma revisão \
                específica (`HEAD~2`)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "alvo": {"type": "string", "description": "working | staged | ultimo_commit | <revisão>"},
                    "caminho": {"type": "string", "description": "Limita o diff a um caminho"},
                    "max_caracteres": {"type": "integer", "description": "Limite de saída (padrão 20000)"}
                }
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "caminho")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    fn detalhe(&self, args: &Value) -> String {
        arg_str_opt(args, "alvo").unwrap_or_else(|| "working".into())
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let alvo = arg_str_opt(&args, "alvo").unwrap_or_else(|| "working".into());
        let caminho = arg_str_opt(&args, "caminho");
        let limite = arg_u64(&args, "max_caracteres").unwrap_or(20_000) as usize;

        let mut base: Vec<String> = match alvo.as_str() {
            "working" | "nao_preparado" => vec!["diff".into()],
            "staged" | "preparado" => vec!["diff".into(), "--cached".into()],
            "ultimo_commit" => vec![
                "show".into(),
                "--stat".into(),
                "--patch".into(),
                "HEAD".into(),
            ],
            revisao => vec!["diff".into(), revisao.into()],
        };
        base.push("--".into());
        if let Some(c) = &caminho {
            base.push(c.clone());
        }
        let refs: Vec<&str> = base.iter().map(|s| s.as_str()).collect();
        let (ok, saida) = git(ctx, &refs).await?;
        if !ok && !saida.contains("diff --git") {
            return Err(ToolError::msg(format!(
                "falha ao gerar diff ({alvo}): {saida}{}",
                eh_repositorio_hint(&saida)
            )));
        }
        if saida.trim().is_empty() {
            return Ok(ToolOutput::texto(format!(
                "Nenhuma diferença para `{alvo}`{}.",
                caminho.map(|c| format!(" em {c}")).unwrap_or_default()
            )));
        }
        Ok(
            ToolOutput::texto(recortar_saida(&saida, limite)).com_resumo(format!(
                "diff {alvo}: {}",
                crate::util::truncate(saida.lines().next().unwrap_or(""), 60)
            )),
        )
    }
}

pub struct GitLog;

#[async_trait::async_trait]
impl Tool for GitLog {
    fn nome(&self) -> &'static str {
        "git_log"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Mostra o histórico de commits (oneline, com resumo opcional).".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "limite": {"type": "integer", "description": "Quantidade de commits (padrão 20)"},
                    "caminho": {"type": "string"},
                    "estatisticas": {"type": "boolean", "description": "Inclui estatísticas de alterações"}
                }
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "caminho")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let limite = arg_u64(&args, "limite").unwrap_or(20).to_string();
        let mut cmd = vec!["log".to_string(), format!("-n{limite}")];
        if arg_bool(&args, "estatisticas", false) {
            cmd.push("--stat".into());
        }
        cmd.push("--pretty=format:%h %ad %an: %s".into());
        cmd.push("--date=short".into());
        if let Some(c) = arg_str_opt(&args, "caminho") {
            cmd.push("--".into());
            cmd.push(c);
        }
        let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        let (ok, saida) = git(ctx, &refs).await?;
        if !ok {
            return Err(ToolError::msg(format!(
                "falha ao consultar o histórico: {saida}{}",
                eh_repositorio_hint(&saida)
            )));
        }
        if saida.trim().is_empty() {
            return Ok(ToolOutput::texto("Nenhum commit encontrado."));
        }
        Ok(ToolOutput::texto(recortar_saida(&saida, ctx.max_saida))
            .com_resumo(format!("{} commits", saida.lines().count())))
    }
}

pub struct GitStage;

#[async_trait::async_trait]
impl Tool for GitStage {
    fn nome(&self) -> &'static str {
        "git_stage"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Prepara arquivos para commit (git add). Use `caminhos` para escolher \
                arquivos ou `tudo: true` para preparar todas as alterações."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "caminhos": {"type": "array", "items": {"type": "string"}},
                    "tudo": {"type": "boolean"}
                }
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        args.get("caminhos")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn detalhe(&self, args: &Value) -> String {
        if arg_bool(args, "tudo", false) {
            "preparar todas as alterações".into()
        } else {
            format!(
                "preparar {}",
                args.get("caminhos")
                    .and_then(|c| c.as_array())
                    .map(|a| a.len())
                    .unwrap_or(0)
            )
        }
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let tudo = arg_bool(&args, "tudo", false);
        let mut cmd: Vec<String> = vec!["add".into()];
        if tudo {
            cmd.push("-A".into());
        } else {
            let caminhos: Vec<String> = args
                .get("caminhos")
                .and_then(|c| c.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|v| v.as_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default();
            if caminhos.is_empty() {
                return Err(ToolError::Argumentos(
                    "informe `caminhos` ou `tudo: true`".into(),
                ));
            }
            for c in caminhos {
                // Confere que o caminho existe para dar erro claro.
                let resolvido = resolver_caminho(ctx, &c);
                if !resolvido.exists() {
                    return Err(ToolError::msg(format!(
                        "o caminho {c} não existe no projeto"
                    )));
                }
                cmd.push("--".into());
                cmd.push(c);
            }
        }
        let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
        let (ok, saida) = git(ctx, &refs).await?;
        if !ok {
            return Err(ToolError::msg(format!(
                "falha ao preparar arquivos: {saida}"
            )));
        }
        let (_, status) = git(ctx, &["status", "--porcelain=v1"]).await?;
        Ok(ToolOutput::texto(format!(
            "Arquivos preparados{}.\nEstado atual:\n{}",
            if tudo { " (todos)" } else { "" },
            if status.trim().is_empty() {
                "(nada pendente)"
            } else {
                status.trim()
            }
        ))
        .com_resumo("preparou arquivos para commit"))
    }
}

pub struct GitCommit;

#[async_trait::async_trait]
impl Tool for GitCommit {
    fn nome(&self) -> &'static str {
        "git_commit"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Cria um commit com os arquivos já preparados. Se `caminhos` for \
                informado, apenas esses arquivos são preparados antes do commit. Requer nível de \
                permissão que autorize operações sensíveis. Não mistura alterações independentes: \
                prefira preparar caminhos específicos."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "mensagem": {"type": "string", "description": "Mensagem do commit (linha de resumo + corpo opcional)"},
                    "caminhos": {"type": "array", "items": {"type": "string"}, "description": "Arquivos a preparar antes do commit"},
                    "tudo": {"type": "boolean", "description": "Prepara todas as alterações antes do commit"}
                },
                "required": ["mensagem"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        args.get("caminhos")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(PathBuf::from))
                    .collect()
            })
            .unwrap_or_default()
    }

    fn detalhe(&self, args: &Value) -> String {
        crate::util::truncate(&arg_str_opt(args, "mensagem").unwrap_or_default(), 60)
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let mensagem = arg_str(&args, "mensagem")?;
        let tudo = arg_bool(&args, "tudo", false);
        let caminhos: Vec<String> = args
            .get("caminhos")
            .and_then(|c| c.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(|s| s.to_string()))
                    .collect()
            })
            .unwrap_or_default();

        if tudo {
            let (ok, saida) = git(ctx, &["add", "-A"]).await?;
            if !ok {
                return Err(ToolError::msg(format!(
                    "falha ao preparar alterações: {saida}"
                )));
            }
        } else if !caminhos.is_empty() {
            let mut cmd = vec!["add".to_string(), "--".to_string()];
            cmd.extend(caminhos.iter().cloned());
            let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
            let (ok, saida) = git(ctx, &refs).await?;
            if !ok {
                return Err(ToolError::msg(format!(
                    "falha ao preparar arquivos: {saida}"
                )));
            }
        }

        let (_, preparados) = git(ctx, &["diff", "--cached", "--name-only"]).await?;
        if preparados.trim().is_empty() {
            return Err(ToolError::msg(
                "não há alterações preparadas para commit. Use git_stage ou informe `caminhos`/`tudo`.".to_string(),
            ));
        }

        let (ok, saida) = git(ctx, &["commit", "-m", &mensagem]).await?;
        if !ok {
            let dica = if saida.contains("Please tell me who you are")
                || saida.contains("user.name")
                || saida.contains("user.email")
            {
                "\n\nConfigure sua identidade com:\n  git config user.name \"Seu Nome\"\n  git config user.email \"voce@exemplo.com\""
            } else {
                ""
            };
            return Err(ToolError::msg(format!(
                "falha ao criar commit: {saida}{dica}"
            )));
        }
        let (_, hash) = git(ctx, &["rev-parse", "--short", "HEAD"]).await?;
        ctx.store
            .record_decision(
                &ctx.session_id,
                "git",
                "commit criado",
                Some(&crate::util::truncate(&mensagem, 300)),
            )
            .ok();
        Ok(ToolOutput::texto(format!(
            "Commit criado: {}\nArquivos: {}\n\n{saida}",
            hash.trim(),
            preparados.trim().replace('\n', ", ")
        ))
        .com_resumo(format!("commit {}", hash.trim())))
    }
}

pub struct GitBranch;

#[async_trait::async_trait]
impl Tool for GitBranch {
    fn nome(&self) -> &'static str {
        "git_branch"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description:
                "Gerencia branches: liste (`listar`), crie (`criar`) ou troque (`trocar`). \
                Use antes de começar alterações grandes quando fizer sentido trabalhar em uma \
                branch própria."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "acao": {"type": "string", "description": "listar | criar | trocar"},
                    "nome": {"type": "string", "description": "Nome da branch (para criar/trocar)"}
                },
                "required": ["acao"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        format!(
            "{} {}",
            arg_str_opt(args, "acao").unwrap_or_default(),
            arg_str_opt(args, "nome").unwrap_or_default()
        )
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let acao = arg_str(&args, "acao")?;
        match acao.as_str() {
            "listar" => {
                let (ok, saida) =
                    git(ctx, &["branch", "--all", "--verbose", "--no-abbrev"]).await?;
                if !ok {
                    return Err(ToolError::msg(format!(
                        "falha ao listar branches: {saida}{}",
                        eh_repositorio_hint(&saida)
                    )));
                }
                let total = saida.lines().count();
                Ok(ToolOutput::texto(saida).com_resumo(format!("{total} branches")))
            }
            "criar" | "trocar" => {
                let nome = arg_str(&args, "nome")?;
                if nome.trim().is_empty() || nome.contains(char::is_whitespace) {
                    return Err(ToolError::Argumentos(
                        "o nome da branch não pode ser vazio nem conter espaços".into(),
                    ));
                }
                let sub = "switch";
                let flag = if acao == "criar" { "-c" } else { "" };
                let mut cmd = vec![sub.to_string()];
                if !flag.is_empty() {
                    cmd.push(flag.to_string());
                }
                cmd.push(nome.clone());
                let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
                let (ok, saida) = git(ctx, &refs).await?;
                if !ok {
                    return Err(ToolError::msg(format!(
                        "falha ao {} a branch `{nome}`: {saida}",
                        if acao == "criar" { "criar" } else { "trocar" }
                    )));
                }
                Ok(ToolOutput::texto(format!(
                    "Branch `{nome}` {}.",
                    if acao == "criar" {
                        "criada e ativada"
                    } else {
                        "ativada"
                    }
                ))
                .com_resumo(format!("{acao} branch {nome}")))
            }
            outro => Err(ToolError::Argumentos(format!(
                "ação `{outro}` inválida: use listar, criar ou trocar"
            ))),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::path::Path;

    async fn repo_temporario() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kestrel-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        for args in [
            vec!["init", "-q"],
            vec!["config", "user.email", "teste@kestrel.local"],
            vec!["config", "user.name", "Kestrel Teste"],
            vec!["commit", "--allow-empty", "-q", "-m", "inicial"],
        ] {
            let saida = tokio::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .await
                .unwrap();
            assert!(
                saida.status.success(),
                "git {args:?} falhou: {}",
                String::from_utf8_lossy(&saida.stderr)
            );
        }
        dir
    }

    pub(crate) fn contexto(dir: &Path) -> ToolContext {
        use crate::config::WebConfig;
        use crate::events::AgentEvent;
        use crate::project::ProjectInfo;
        use crate::skills::SkillRegistry;
        use crate::tools::FileRegistry;
        use std::sync::{Arc, Mutex};
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let (ctx_tx, _) = tokio::sync::mpsc::unbounded_channel();
        ToolContext {
            workdir: dir.to_path_buf(),
            roots: vec![dir.to_path_buf()],
            level: crate::perm::PermissionLevel::Full,
            modo: crate::config::ModoOperacao::Agente,
            cancel: tokio_util::sync::CancellationToken::new(),
            store: crate::db::Store::open_in_memory().unwrap(),
            session_id: "s1".into(),
            processos: Arc::new(crate::tools::shell::ProcessManager::new()),
            arquivos: Arc::new(FileRegistry::new()),
            eventos: tx,
            confirmacoes: ctx_tx,
            permitidos: Arc::new(Mutex::new(std::collections::HashSet::new())),
            skills: Arc::new(SkillRegistry::vazio()),
            skills_dir: dir.join("skills"),
            projeto: Arc::new(ProjectInfo::default()),
            web: WebConfig::default(),
            http: reqwest::Client::new(),
            max_saida: 20_000,
            subagente: None,
        }
    }

    #[tokio::test]
    async fn status_detecta_arquivo_nao_rastreado() {
        let dir = repo_temporario().await;
        std::fs::write(dir.join("novo.txt"), "conteudo").unwrap();
        let ctx = contexto(&dir);
        let saida = GitStatus.executar(json!({}), &ctx).await.unwrap();
        assert!(saida.conteudo.contains("novo.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn stage_e_commit_funcionam() {
        let dir = repo_temporario().await;
        std::fs::write(dir.join("a.txt"), "1").unwrap();
        let ctx = contexto(&dir);
        GitStage
            .executar(json!({"caminhos": ["a.txt"]}), &ctx)
            .await
            .unwrap();
        let saida = GitCommit
            .executar(json!({"mensagem": "adiciona a.txt"}), &ctx)
            .await
            .unwrap();
        assert!(saida.conteudo.contains("Commit criado"));
        let log = GitLog.executar(json!({"limite": 5}), &ctx).await.unwrap();
        assert!(log.conteudo.contains("adiciona a.txt"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn commit_sem_alteracoes_falha_com_dica() {
        let dir = repo_temporario().await;
        let ctx = contexto(&dir);
        let erro = GitCommit
            .executar(json!({"mensagem": "vazio"}), &ctx)
            .await
            .unwrap_err();
        assert!(erro.to_string().contains("preparadas"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn branch_criar_e_listar() {
        let dir = repo_temporario().await;
        let ctx = contexto(&dir);
        GitBranch
            .executar(json!({"acao": "criar", "nome": "feature/kestrel"}), &ctx)
            .await
            .unwrap();
        let lista = GitBranch
            .executar(json!({"acao": "listar"}), &ctx)
            .await
            .unwrap();
        assert!(lista.conteudo.contains("feature/kestrel"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
