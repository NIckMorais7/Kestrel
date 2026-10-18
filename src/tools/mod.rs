//! Registro de ferramentas, validação de entradas, controle de permissões e
//! concorrência sobre arquivos.

pub mod fs;
pub mod git;
pub mod plan;
pub mod search;
pub mod shell;
pub mod skills_tools;
pub mod subagent;
pub mod web;

use crate::config::WebConfig;
use crate::db::Store;
use crate::events::{AgentEvent, ConfirmRequest};
use crate::perm::{Confirmacao, Decision, PermissionLevel};
use crate::project::ProjectInfo;
use crate::provider::ToolSpec;
use crate::skills::SkillRegistry;
use crate::util::normalize_path;
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Erros de ferramenta, sempre com mensagens acionáveis para o modelo.
#[derive(Debug, thiserror::Error, Clone)]
pub enum ToolError {
    #[error("{0}")]
    Mensagem(String),
    #[error("argumentos inválidos para a ferramenta: {0}")]
    Argumentos(String),
    #[error("operação bloqueada pelas permissões: {0}")]
    Permissao(String),
    #[error("operação cancelada pelo usuário")]
    Cancelado,
}

impl From<anyhow::Error> for ToolError {
    fn from(valor: anyhow::Error) -> Self {
        ToolError::Mensagem(valor.to_string())
    }
}

impl ToolError {
    pub fn msg(m: impl Into<String>) -> Self {
        ToolError::Mensagem(m.into())
    }
}

/// Resultado estruturado de uma ferramenta.
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    /// Texto devolvido ao modelo.
    pub conteudo: String,
    /// Resumo curto para a interface.
    pub resumo: String,
    /// Linhas adicionadas (calculadas a partir do diff real).
    pub adicoes: usize,
    /// Linhas removidas.
    pub remocoes: usize,
}

impl ToolOutput {
    pub fn texto(conteudo: impl Into<String>) -> Self {
        let conteudo = conteudo.into();
        let resumo = crate::util::truncate(conteudo.lines().next().unwrap_or(""), 120);
        Self {
            conteudo,
            resumo,
            adicoes: 0,
            remocoes: 0,
        }
    }

    pub fn com_resumo(mut self, resumo: impl Into<String>) -> Self {
        self.resumo = resumo.into();
        self
    }

    pub fn com_diff(mut self, adicoes: usize, remocoes: usize) -> Self {
        self.adicoes = adicoes;
        self.remocoes = remocoes;
        self
    }
}

/// Controle de concorrência e detecção de alterações externas por arquivo.
#[derive(Default)]
pub struct FileRegistry {
    locks: Mutex<HashMap<PathBuf, Arc<tokio::sync::Mutex<()>>>>,
    hashes: Mutex<HashMap<PathBuf, u64>>,
}

impl FileRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Lock exclusivo por arquivo (compartilhado entre tarefas paralelas).
    pub fn lock_de(&self, path: &Path) -> Arc<tokio::sync::Mutex<()>> {
        let chave = normalize_path(path);
        let mut mapa = self.locks.lock().expect("lock do registro");
        mapa.entry(chave)
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    /// Registra o conteúdo conhecido de um arquivo após leitura ou escrita.
    pub fn registrar(&self, path: &Path, conteudo: &str) {
        let chave = normalize_path(path);
        let mut mapa = self.hashes.lock().expect("lock do registro");
        mapa.insert(chave, hash(conteudo));
    }

    /// Indica se o arquivo mudou desde o último registro.
    pub fn mudou_desde_registro(&self, path: &Path, atual: &str) -> bool {
        let chave = normalize_path(path);
        let mapa = self.hashes.lock().expect("lock do registro");
        match mapa.get(&chave) {
            Some(anterior) => *anterior != hash(atual),
            None => false,
        }
    }
}

fn hash(texto: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    texto.hash(&mut h);
    h.finish()
}

/// Permite ao agente rodar subagentes a partir de uma ferramenta.
#[async_trait::async_trait]
pub trait SubagentRunner: Send + Sync {
    async fn executar_subagente(
        &self,
        tarefa: &str,
        contexto: &str,
        contexto_extra: CancellationToken,
        eventos: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<String, String>;
}

/// Contexto de execução compartilhado por todas as ferramentas.
pub struct ToolContext {
    pub workdir: PathBuf,
    pub roots: Vec<PathBuf>,
    pub level: PermissionLevel,
    pub modo: crate::config::ModoOperacao,
    pub cancel: CancellationToken,
    pub store: Store,
    pub session_id: String,
    pub processos: Arc<shell::ProcessManager>,
    pub arquivos: Arc<FileRegistry>,
    pub eventos: mpsc::UnboundedSender<AgentEvent>,
    pub confirmacoes: mpsc::UnboundedSender<ConfirmRequest>,
    pub permitidos: Arc<Mutex<HashSet<String>>>,
    pub skills: Arc<SkillRegistry>,
    pub skills_dir: PathBuf,
    pub projeto: Arc<ProjectInfo>,
    pub web: WebConfig,
    pub http: reqwest::Client,
    pub max_saida: usize,
    pub subagente: Option<Arc<dyn SubagentRunner>>,
}

impl ToolContext {
    /// Emite um evento para a interface (ignora falhas de canal).
    pub fn evento(&self, evento: AgentEvent) {
        let _ = self.eventos.send(evento);
    }

    /// Verifica cancelamento e devolve erro coerente.
    pub fn checar_cancelamento(&self) -> Result<(), ToolError> {
        if self.cancel.is_cancelled() {
            Err(ToolError::Cancelado)
        } else {
            Ok(())
        }
    }

    /// Solicita confirmação ao usuário e aguarda a resposta.
    pub async fn pedir_confirmacao(
        &self,
        titulo: impl Into<String>,
        detalhe: impl Into<String>,
    ) -> Result<bool, ToolError> {
        let titulo = titulo.into();
        let detalhe = detalhe.into();
        let (tx, rx) = tokio::sync::oneshot::channel();
        let pedido = ConfirmRequest {
            titulo: titulo.clone(),
            detalhe,
            nivel: self.level,
            resposta: tx,
        };
        if self.confirmacoes.send(pedido).is_err() {
            return Ok(false);
        }
        tokio::select! {
            _ = self.cancel.cancelled() => Err(ToolError::Cancelado),
            resposta = rx => match resposta {
                Ok(Confirmacao::Permitir) => Ok(true),
                // "Permitir sempre" precisa ser lembrado: sem gravar a decisão,
                // cada acesso fora do projeto perguntaria de novo.
                Ok(Confirmacao::PermitirSempre) => {
                    self.permitir_sempre(titulo);
                    Ok(true)
                }
                Ok(Confirmacao::Negar) => Ok(false),
                Err(_) => Ok(false),
            },
        }
    }

    /// Memoriza uma decisão "permitir sempre" para o resto da sessão.
    pub fn permitir_sempre(&self, chave: impl Into<String>) {
        self.permitidos
            .lock()
            .expect("lock de permitidos")
            .insert(format!("sempre:{chave}", chave = chave.into()));
    }

    /// Consulta uma decisão "permitir sempre" já tomada na sessão.
    pub fn ja_permitido_sempre(&self, chave: &str) -> bool {
        self.permitidos
            .lock()
            .expect("lock de permitidos")
            .contains(&format!("sempre:{chave}"))
    }
}

/// Interface de uma ferramenta.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn nome(&self) -> &'static str;
    fn spec(&self) -> ToolSpec;

    /// Caminhos usados pela chamada, para verificação de escopo.
    fn caminhos(&self, _args: &Value) -> Vec<PathBuf> {
        Vec::new()
    }

    /// Descrição curta da ação, exibida na interface.
    fn detalhe(&self, args: &Value) -> String {
        args.get("path")
            .or_else(|| args.get("caminho"))
            .and_then(|p| p.as_str())
            .map(|p| p.to_string())
            .unwrap_or_else(|| self.nome().to_string())
    }

    /// Tempo máximo padrão de execução.
    fn timeout(&self) -> Duration {
        Duration::from_secs(120)
    }

    /// Ferramentas que fazem a própria análise de permissões (ex.: comandos de
    /// terminal, cujo risco depende do texto do comando).
    fn checagem_propria(&self) -> bool {
        false
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError>;
}

/// Registro completo de ferramentas disponíveis.
pub struct ToolRegistry {
    tools: Vec<Arc<dyn Tool>>,
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::novo()
    }
}

impl ToolRegistry {
    /// Registra todas as ferramentas nativas.
    pub fn novo() -> Self {
        let tools: Vec<Arc<dyn Tool>> = vec![
            // Sistema de arquivos
            Arc::new(fs::ListFiles),
            Arc::new(fs::ReadFile),
            Arc::new(fs::InspectFileMetadata),
            Arc::new(fs::WriteFile),
            Arc::new(fs::CreateFile),
            Arc::new(fs::EditFile),
            Arc::new(fs::DeleteFile),
            // Busca e análise
            Arc::new(search::SearchCode),
            Arc::new(search::AnalyzeProject),
            // Terminal e processos
            Arc::new(shell::RunCommand),
            Arc::new(shell::RunTests),
            Arc::new(shell::ListProcesses),
            Arc::new(shell::ProcessStatus),
            Arc::new(shell::ReadProcessOutput),
            Arc::new(shell::KillProcess),
            Arc::new(shell::WriteProcessStdin),
            // Git
            Arc::new(git::GitStatus),
            Arc::new(git::GitDiff),
            Arc::new(git::GitLog),
            Arc::new(git::GitStage),
            Arc::new(git::GitCommit),
            Arc::new(git::GitBranch),
            // Web e downloads
            Arc::new(web::WebSearch),
            Arc::new(web::FetchUrl),
            Arc::new(web::DownloadFile),
            Arc::new(web::BrowserRender),
            // Skills e planejamento
            Arc::new(skills_tools::ListSkills),
            Arc::new(skills_tools::LoadSkill),
            Arc::new(plan::UpdatePlan),
            Arc::new(plan::TaskStatus),
            // Subagentes
            Arc::new(subagent::SpawnSubagent),
        ];
        Self { tools }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        let mut specs: Vec<ToolSpec> = self.tools.iter().map(|t| t.spec()).collect();
        specs.sort_by(|a, b| a.name.cmp(&b.name));
        specs
    }

    /// Especificações visíveis no modo informado.
    ///
    /// No modo chat o modelo só enxerga ferramentas de leitura; a checagem em
    /// [`ToolRegistry::executar`] garante o mesmo limite se a chamada acontecer
    /// mesmo assim (defesa em profundidade).
    pub fn specs_do_modo(&self, modo: crate::config::ModoOperacao) -> Vec<ToolSpec> {
        let mut specs = self.specs();
        if modo == crate::config::ModoOperacao::Chat {
            specs.retain(|s| permitida_no_chat(&s.name));
        }
        specs
    }

    pub fn nomes(&self) -> Vec<&'static str> {
        let mut nomes: Vec<&'static str> = self.tools.iter().map(|t| t.nome()).collect();
        nomes.sort_unstable();
        nomes
    }

    pub fn get(&self, nome: &str) -> Option<Arc<dyn Tool>> {
        self.tools.iter().find(|t| t.nome() == nome).cloned()
    }

    /// Executa uma ferramenta aplicando validação, permissões e auditoria.
    pub async fn executar(
        &self,
        nome: &str,
        args: Value,
        ctx: &ToolContext,
    ) -> Result<ToolOutput, ToolError> {
        let Some(tool) = self.get(nome) else {
            return Err(ToolError::Mensagem(format!(
                "ferramenta `{nome}` não existe. Ferramentas disponíveis: {}",
                self.nomes().join(", ")
            )));
        };

        // O modo de operação vale para toda chamada, inclusive subagentes.
        if ctx.modo == crate::config::ModoOperacao::Chat && !permitida_no_chat(nome) {
            ctx.store
                .audit(
                    Some(&ctx.session_id),
                    "permissao",
                    Some(&format!("modo chat bloqueia `{nome}`")),
                    Some("bloqueado"),
                )
                .ok();
            return Err(ToolError::Permissao(format!(
                "a ferramenta `{nome}` não está disponível no modo chat (somente leitura). \
                 Use /agente para trabalhar no projeto."
            )));
        }

        validar_args(&tool.spec(), &args)?;
        // Argumentos e resultados podem conter segredos (chaves em `printenv`,
        // `.env` lido, token num diff): eles nunca vão para o banco crus.
        let args_audit = crate::util::redact_secrets(&args.to_string());
        ctx.store
            .audit(
                Some(&ctx.session_id),
                "ferramenta",
                Some(&crate::util::truncate(&args_audit, 2000)),
                Some(nome),
            )
            .ok();

        let detalhe = tool.detalhe(&args);

        // Verificação de escopo para caminhos (relativos ao projeto).
        for caminho in tool.caminhos(&args) {
            let norm = resolver_caminho(ctx, &caminho.to_string_lossy());
            if !ctx.roots.iter().any(|r| norm.starts_with(r)) {
                let decisao = match ctx.level {
                    PermissionLevel::Read | PermissionLevel::Write => Decision::Bloquear,
                    PermissionLevel::Semi => Decision::Confirmar,
                    PermissionLevel::Full => Decision::Permitir,
                };
                match decisao {
                    Decision::Bloquear => {
                        return Err(ToolError::Permissao(format!(
                            "o caminho {} está fora do projeto e o nível {} não permite. \
                             Adicione o diretório em /config ou mude o nível com /permissions.",
                            norm.display(),
                            ctx.level
                        )));
                    }
                    Decision::Confirmar => {
                        // "Permitir sempre" fica registrado na sessão: sem isso
                        // o mesmo caminho seria perguntado a cada acesso.
                        let chave = format!("Acesso fora do projeto: {}", norm.display());
                        if !ctx.ja_permitido_sempre(&chave)
                            && !ctx
                                .pedir_confirmacao(
                                    chave.clone(),
                                    format!("A ferramenta `{nome}` quer operar em um caminho fora do projeto."),
                                )
                                .await?
                        {
                            return Err(ToolError::Permissao(format!(
                                "acesso a {} negado pelo usuário",
                                norm.display()
                            )));
                        }
                    }
                    Decision::Permitir => {}
                }
            }
        }

        // Verificação de permissão da ferramenta.
        if !tool.checagem_propria() {
            let analise = crate::perm::check_tool(nome, ctx.level, &detalhe);
            match analise.decision {
                Decision::Bloquear => {
                    ctx.store
                        .audit(
                            Some(&ctx.session_id),
                            "permissao",
                            Some(&analise.reason),
                            Some("bloqueado"),
                        )
                        .ok();
                    return Err(ToolError::Permissao(analise.reason));
                }
                Decision::Confirmar => {
                    let chave = format!("substituivel:{nome}");
                    let liberado = ctx.permitidos.lock().expect("lock").contains(&chave);
                    if !liberado {
                        let (tx, rx) = tokio::sync::oneshot::channel();
                        let pedido = ConfirmRequest {
                            titulo: format!("Confirmar `{nome}`"),
                            detalhe: format!(
                                "{}\nAlvo: {detalhe}\nMotivo: {}",
                                analise.reason, "operação sensível"
                            ),
                            nivel: ctx.level,
                            resposta: tx,
                        };
                        if ctx.confirmacoes.send(pedido).is_err() {
                            return Err(ToolError::Permissao(
                                "interface indisponível para confirmação".into(),
                            ));
                        }
                        let resposta: Option<Confirmacao> = tokio::select! {
                            _ = ctx.cancel.cancelled() => return Err(ToolError::Cancelado),
                            r = rx => r.ok(),
                        };
                        match resposta {
                            Some(Confirmacao::Permitir) => {}
                            Some(Confirmacao::PermitirSempre) => {
                                ctx.permitidos.lock().expect("lock").insert(chave);
                            }
                            _ => {
                                ctx.store
                                    .audit(
                                        Some(&ctx.session_id),
                                        "permissao",
                                        Some(&analise.reason),
                                        Some("negado"),
                                    )
                                    .ok();
                                return Err(ToolError::Permissao(format!(
                                    "operação `{nome}` negada pelo usuário ({})",
                                    analise.reason
                                )));
                            }
                        }
                    }
                }
                Decision::Permitir => {}
            }
        }

        ctx.checar_cancelamento()?;
        let inicio = std::time::Instant::now();
        // Fotografia dos processos já em execução: se o tempo limite estourar
        // aqui (e não dentro da ferramenta), os processos que ela criou
        // precisam ser encerrados — senão ficam órfãos, porque o future
        // descartado no estouro do timeout ninguém mais os mata.
        let processos_antes = ctx.processos.ids_ativas();
        let resultado = tokio::select! {
            _ = ctx.cancel.cancelled() => Err(ToolError::Cancelado),
            r = tokio::time::timeout(tool.timeout(), tool.executar(args.clone(), ctx)) => {
                match r {
                    Ok(Ok(out)) => Ok(out),
                    Ok(Err(e)) => Err(e),
                    Err(_) => {
                        // Rede de segurança: mata só o que esta chamada criou.
                        let novos: Vec<String> = ctx
                            .processos
                            .ids_ativas()
                            .into_iter()
                            .filter(|id| !processos_antes.contains(id))
                            .collect();
                        ctx.processos.encerrar_varios(&novos);
                        Err(ToolError::Mensagem(format!(
                            "a ferramenta `{nome}` excedeu o tempo limite de {}s",
                            tool.timeout().as_secs()
                        )))
                    }
                }
            }
        };
        let duracao = inicio.elapsed().as_millis() as u64;
        let (registro, erro) = match &resultado {
            Ok(out) => (Ok(out.conteudo.clone()), None),
            Err(e) => (Err(()), Some(e.to_string())),
        };
        let status = if registro.is_ok() { "sucesso" } else { "falha" };
        let recorte = match &registro {
            Ok(texto) => Some(crate::util::truncate(
                &crate::util::redact_secrets(texto),
                4000,
            )),
            Err(_) => None,
        };
        let args_gravados = crate::util::redact_secrets(&args.to_string());
        ctx.store
            .record_tool_run(
                &ctx.session_id,
                nome,
                &crate::util::truncate(&args_gravados, 4000),
                recorte.as_deref(),
                status,
                erro.as_deref(),
                duracao,
            )
            .ok();
        resultado
    }
}

/// Classificação de cada ferramenta quanto ao modo chat (somente leitura).
///
/// `None` indica que a ferramenta não foi classificada — o teste de completude
/// em `registro_tem_classificacao_de_modo` impede que isso passe despercebido.
fn somente_leitura(nome: &str) -> Option<bool> {
    match nome {
        // Leitura, análise e consulta: permitidas no modo chat.
        "list_files"
        | "read_file"
        | "inspect_file_metadata"
        | "search_code"
        | "analyze_project"
        | "list_processes"
        | "process_status"
        | "read_process_output"
        | "git_status"
        | "git_diff"
        | "git_log"
        | "web_search"
        | "fetch_url"
        | "list_skills"
        | "load_skill"
        | "task_status" => Some(true),
        // Escrita, execução, orquestração e planejamento: só no modo agente.
        "write_file"
        | "create_file"
        | "edit_file"
        | "delete_file"
        | "run_command"
        | "run_tests"
        | "kill_process"
        | "write_process_stdin"
        | "git_stage"
        | "git_commit"
        | "git_branch"
        | "download_file"
        | "browser_render"
        | "update_plan"
        | "spawn_subagent" => Some(false),
        _ => None,
    }
}

/// Ferramenta disponível no modo chat (somente leitura).
pub fn permitida_no_chat(nome: &str) -> bool {
    somente_leitura(nome).unwrap_or(false)
}

/// Validação estrutural mínima contra o esquema declarado pela ferramenta.
pub fn validar_args(spec: &ToolSpec, args: &Value) -> Result<(), ToolError> {
    let Some(obj) = args.as_object() else {
        return Err(ToolError::Argumentos(format!(
            "esperado um objeto JSON com os parâmetros de `{}`",
            spec.name
        )));
    };
    if let Some(obrigatorios) = spec.parameters.get("required").and_then(|r| r.as_array()) {
        for campo in obrigatorios {
            if let Some(nome) = campo.as_str()
                && !obj.contains_key(nome)
            {
                return Err(ToolError::Argumentos(format!(
                    "campo obrigatório `{nome}` ausente em `{}`",
                    spec.name
                )));
            }
        }
    }
    if let Some(props) = spec
        .parameters
        .get("properties")
        .and_then(|p| p.as_object())
    {
        for (chave, valor) in obj {
            let Some(esquema) = props.get(chave) else {
                continue; // campos extras são permitidos, mas ignorados pela ferramenta
            };
            let tipo = esquema
                .get("type")
                .and_then(|t| t.as_str())
                .unwrap_or("any");
            let ok = match tipo {
                "string" => valor.is_string(),
                "integer" => valor.is_i64() || valor.is_u64(),
                "number" => valor.is_number(),
                "boolean" => valor.is_boolean(),
                "array" => valor.is_array(),
                "object" => valor.is_object(),
                _ => true,
            };
            if !ok {
                return Err(ToolError::Argumentos(format!(
                    "campo `{chave}` de `{}` deveria ser `{tipo}`",
                    spec.name
                )));
            }
        }
    }
    Ok(())
}

/// Resolve um caminho informado pelo modelo, aceitando caminhos relativos ao
/// projeto, absolutos e com `~`.
/// Resolve um caminho para o alvo real no disco.
///
/// Sem o `canonicalize`, a verificação de escopo é puramente léxica: um symlink
/// dentro do projeto apontando para `/etc` passava no teste `starts_with(root)`
/// e as ferramentas liam/escreviam fora do projeto.
///
/// Quando o caminho ainda não existe (criação), canoniciza o ancestral mais
/// próximo existente e reconcat o resto — assim `novo/arquivo.txt` continua
/// resolvendo normalmente sem perder a checagem dos componentes que existem.
pub fn resolver_caminho(ctx: &ToolContext, entrada: &str) -> PathBuf {
    let bruto = crate::util::expand_tilde(entrada.trim());
    let caminho = if bruto.is_absolute() {
        bruto
    } else {
        ctx.workdir.join(bruto)
    };
    let normalizado = normalize_path(&caminho);
    match std::fs::canonicalize(&normalizado) {
        Ok(real) => real,
        Err(_) => {
            // Sobe até o primeiro ancestral que exista, canoniciza ele e
            // reconcató os componentes que faltam (do mais Fundo ao mais raso).
            let mut ancestral = normalizado.as_path();
            let mut faltando: Vec<std::ffi::OsString> = Vec::new();
            loop {
                if let Ok(base) = std::fs::canonicalize(ancestral) {
                    let mut saida = base;
                    for parte in faltando.iter().rev() {
                        saida.push(parte);
                    }
                    return normalize_path(&saida);
                }
                match (ancestral.file_name(), ancestral.parent()) {
                    (Some(nome), Some(pai)) => {
                        faltando.push(nome.to_os_string());
                        ancestral = pai;
                    }
                    _ => break,
                }
            }
            normalizado
        }
    }
}

/// Lê um argumento textual obrigatório.
pub fn arg_str(args: &Value, chave: &str) -> Result<String, ToolError> {
    args.get(chave)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| ToolError::Argumentos(format!("campo `{chave}` ausente ou não textual")))
}

/// Lê um argumento textual opcional.
pub fn arg_str_opt(args: &Value, chave: &str) -> Option<String> {
    args.get(chave)
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
}

/// Lê um argumento numérico opcional.
pub fn arg_u64(args: &Value, chave: &str) -> Option<u64> {
    args.get(chave).and_then(|v| v.as_u64())
}

/// Lê um argumento booleano opcional.
pub fn arg_bool(args: &Value, chave: &str, padrao: bool) -> bool {
    args.get(chave).and_then(|v| v.as_bool()).unwrap_or(padrao)
}

/// Recorta a saída de acordo com o limite do contexto.
pub fn recortar_saida(texto: &str, max: usize) -> String {
    if texto.chars().count() <= max {
        return texto.to_string();
    }
    let corte: String = texto.chars().take(max).collect();
    format!(
        "{corte}\n… [saída truncada: {} caracteres no total]",
        texto.chars().count()
    )
}

/// Ferramenta de teste que roda um comando longo — o tempo limite que estoura
/// é o do registro, não o dela.
#[cfg(test)]
struct FerramentaLonga;

#[cfg(test)]
#[async_trait::async_trait]
impl Tool for FerramentaLonga {
    fn nome(&self) -> &'static str {
        "longa"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "teste".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }

    fn timeout(&self) -> Duration {
        // Curto de propósito: o registro estoura antes da ferramenta.
        Duration::from_millis(700)
    }

    async fn executar(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        // O comando leva 30s; só o timeout do registro pode pará-lo.
        shell::executar_comando(ctx, "sleep 30", &ctx.workdir, Some(Duration::from_secs(30)))
            .await?;
        Ok(ToolOutput::texto("nunca chega aqui"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn registro_tem_classificacao_de_modo() {
        // Toda ferramenta registrada precisa estar classificada para o modo
        // chat; esquecer uma significaria bloqueio silencioso (ou liberação).
        let reg = ToolRegistry::novo();
        for nome in reg.nomes() {
            assert!(
                somente_leitura(nome).is_some(),
                "ferramenta sem classificação de modo: {nome}"
            );
        }
        assert!(permitida_no_chat("read_file"));
        assert!(permitida_no_chat("search_code"));
        assert!(!permitida_no_chat("edit_file"));
        assert!(!permitida_no_chat("run_command"));
        assert!(!permitida_no_chat("spawn_subagent"));
        assert!(!permitida_no_chat("ferramenta-inexistente"));
    }

    /// O tempo limite do registro precisa encerrar os processos que a
    /// ferramenta criou: sem isso o future descartado os deixava órfãos.
    #[tokio::test]
    async fn timeout_do_registro_mata_o_processo_da_ferramenta() {
        let dir = std::env::temp_dir().join(format!("kestrel-reg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        // Registro só com a ferramenta de teste.
        let registro = ToolRegistry {
            tools: vec![Arc::new(FerramentaLonga)],
        };
        let inicio = std::time::Instant::now();
        let saida = registro
            .executar("longa", serde_json::json!({}), &ctx)
            .await;
        assert!(saida.is_err(), "deveria estourar o tempo limite");
        assert!(
            inicio.elapsed() < Duration::from_secs(5),
            "o timeout nao disparou: {:?}",
            inicio.elapsed()
        );
        let lista = ctx.processos.listar();
        eprintln!("processos apos o timeout: {lista:?}");
        let ativas = ctx.processos.ids_ativas();
        eprintln!("ativas: {ativas:?}");
        assert!(ativas.is_empty(), "processo orfao continuou rodando");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Um symlink dentro do projeto apontando para fora escapava da checagem de
    /// escopo, porque `normalize_path` é puramente léxico.
    #[test]
    fn symlink_para_fora_do_projeto_nao_passa_do_escopo() {
        let base = std::env::temp_dir().join(format!("kestrel-link-{}", uuid::Uuid::new_v4()));
        let projeto = base.join("projeto");
        let fora = base.join("fora");
        std::fs::create_dir_all(&projeto).unwrap();
        std::fs::create_dir_all(&fora).unwrap();
        std::fs::write(fora.join("segredo.txt"), "conteudo").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&fora, projeto.join("atalho")).unwrap();
        #[cfg(not(unix))]
        std::os::windows::fs::symlink_dir(&fora, projeto.join("atalho")).unwrap();

        let ctx = crate::tools::git::tests::contexto(&projeto);
        // O caminhoresolve para fora do projeto.
        let resolvido = super::resolver_caminho(&ctx, "atalho/segredo.txt");
        assert!(
            resolvido.starts_with(&fora),
            "deveria resolver para fora: {}",
            resolvido.display()
        );
        // Portanto a checagem de escopo (starts_with nas raízes) reprova.
        let dentro = ctx.roots.iter().any(|r| resolvido.starts_with(r));
        assert!(
            !dentro,
            "o symlink não deve ser considerado dentro do projeto"
        );
        let _ = std::fs::remove_dir_all(&base);
    }

    /// Caminho que ainda não existe continua resolvendo normalmente.
    #[test]
    fn caminho_inexistente_resolve_para_dentro_do_projeto() {
        let projeto = std::env::temp_dir().join(format!("kestrel-novo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&projeto).unwrap();
        let ctx = crate::tools::git::tests::contexto(&projeto);
        let resolvido = super::resolver_caminho(&ctx, "sub/novo-arquivo.txt");
        assert!(
            resolvido.starts_with(&projeto),
            "deveria continuar dentro do projeto: {}",
            resolvido.display()
        );
        assert!(resolvido.ends_with("sub/novo-arquivo.txt"), "{resolvido:?}");
        let _ = std::fs::remove_dir_all(&projeto);
    }

    #[test]
    fn run_command_declara_o_cwd_para_verificacao_de_escopo() {
        use crate::tools::shell::RunCommand;
        let cmd = RunCommand;
        assert!(
            cmd.caminhos(&serde_json::json!({"comando": "ls"}))
                .is_empty()
        );
        let dentro = cmd.caminhos(&serde_json::json!({"comando": "ls", "cwd": "./sub"}));
        assert_eq!(dentro.len(), 1);
        let fora = cmd.caminhos(&serde_json::json!({"comando": "ls", "cwd": "/etc"}));
        assert_eq!(fora.len(), 1);
        // cwd vazio não conta como caminho.
        assert!(
            cmd.caminhos(&serde_json::json!({"comando": "ls", "cwd": "  "}))
                .is_empty()
        );
    }

    #[test]
    fn specs_do_modo_chat_esconde_escrita() {
        let reg = ToolRegistry::novo();
        let todas = reg.specs().len();
        let chat = reg.specs_do_modo(crate::config::ModoOperacao::Chat);
        assert!(chat.len() < todas);
        assert!(chat.iter().all(|s| permitida_no_chat(&s.name)));
        assert!(chat.iter().any(|s| s.name == "read_file"));
        assert!(!chat.iter().any(|s| s.name == "edit_file"));
        let agente = reg.specs_do_modo(crate::config::ModoOperacao::Agente);
        assert_eq!(agente.len(), todas);
    }

    #[tokio::test]
    async fn modo_chat_bloqueia_ferramenta_de_escrita() {
        let reg = ToolRegistry::novo();
        let (eventos, _rx) = mpsc::unbounded_channel();
        let (confirmacoes, _rxc) = mpsc::unbounded_channel();
        let store = crate::db::Store::open_in_memory().unwrap();
        let dir = std::env::temp_dir().join(format!("kestrel-modo-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = ToolContext {
            workdir: dir.clone(),
            roots: vec![dir.clone()],
            level: PermissionLevel::Full,
            modo: crate::config::ModoOperacao::Chat,
            cancel: CancellationToken::new(),
            store,
            session_id: "s".into(),
            processos: Arc::new(shell::ProcessManager::new()),
            arquivos: Arc::new(FileRegistry::new()),
            eventos,
            confirmacoes,
            permitidos: Arc::new(Mutex::new(HashSet::new())),
            skills: Arc::new(SkillRegistry::vazio()),
            skills_dir: dir.join("skills"),
            projeto: Arc::new(ProjectInfo::default()),
            web: WebConfig::default(),
            http: reqwest::Client::new(),
            max_saida: 1000,
            subagente: None,
        };
        let erro = reg
            .executar(
                "create_file",
                json!({"path": "novo.txt", "content": "x"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(matches!(erro, ToolError::Permissao(_)), "{erro:?}");
        assert!(!dir.join("novo.txt").exists());
        // A mesma chamada é permitida no modo agente.
        let ctx_agente = ToolContext {
            modo: crate::config::ModoOperacao::Agente,
            ..ctx
        };
        assert!(
            reg.executar(
                "create_file",
                json!({"path": "novo.txt", "content": "x"}),
                &ctx_agente
            )
            .await
            .is_ok()
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn valida_campos_obrigatorios() {
        let spec = ToolSpec {
            name: "x".into(),
            description: "".into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        };
        assert!(validar_args(&spec, &json!({"path": "a"})).is_ok());
        let erro = validar_args(&spec, &json!({})).unwrap_err();
        assert!(erro.to_string().contains("obrigatório"));
        let erro = validar_args(&spec, &json!({"path": 1})).unwrap_err();
        assert!(erro.to_string().contains("string"));
    }

    #[test]
    fn registro_tem_ferramentas_essenciais() {
        let reg = ToolRegistry::novo();
        for nome in [
            "list_files",
            "read_file",
            "search_code",
            "edit_file",
            "write_file",
            "create_file",
            "delete_file",
            "inspect_file_metadata",
            "run_command",
            "run_tests",
            "git_status",
            "web_search",
            "fetch_url",
            "download_file",
            "list_skills",
            "load_skill",
            "update_plan",
            "spawn_subagent",
        ] {
            assert!(reg.get(nome).is_some(), "ferramenta ausente: {nome}");
        }
        assert_eq!(reg.specs().len(), reg.nomes().len());
    }

    #[test]
    fn registro_por_arquivo_detecta_mudanca_externa() {
        let reg = FileRegistry::new();
        let p = PathBuf::from("/tmp/kestrel-x/arq.rs");
        reg.registrar(&p, "conteudo A");
        assert!(!reg.mudou_desde_registro(&p, "conteudo A"));
        assert!(reg.mudou_desde_registro(&p, "conteudo B"));
        assert!(!reg.mudou_desde_registro(Path::new("/outro"), "x"));
    }

    #[test]
    fn locks_sao_estaveis_por_caminho() {
        let reg = FileRegistry::new();
        let a = reg.lock_de(Path::new("/p/a.rs"));
        let b = reg.lock_de(Path::new("/p/./a.rs"));
        assert!(Arc::ptr_eq(&a, &b));
        let c = reg.lock_de(Path::new("/p/b.rs"));
        assert!(!Arc::ptr_eq(&a, &c));
    }
}
