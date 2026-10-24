//! Registro de ferramentas, validação de entradas, controle de permissões e
//! concorrência sobre arquivos.

pub mod comps;
pub mod fs;
pub mod git;
pub mod mcp;
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
#[derive(Debug, Clone)]
pub struct ToolOutput {
    /// Texto devolvido ao modelo.
    pub conteudo: String,
    /// Resumo curto para a interface.
    pub resumo: String,
    /// Linhas adicionadas (calculadas a partir do diff real).
    pub adicoes: usize,
    /// Linhas removidas.
    pub remocoes: usize,
    /// A operação foi bem-sucedida do ponto de vista do comando?
    ///
    /// Um `run_command` que terminou com código ≠ 0 devolve `Ok(output)` (o
    /// modelo precisa ver a saída e o erro), mas não é sucesso: sem esta flag o
    /// agente contava a falha como sucesso e não acionava a recuperação.
    pub sucesso: bool,
}

impl Default for ToolOutput {
    fn default() -> Self {
        Self {
            conteudo: String::new(),
            resumo: String::new(),
            adicoes: 0,
            remocoes: 0,
            sucesso: true,
        }
    }
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
            sucesso: true,
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
#[derive(Clone)]
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
    /// Diretório de Comps (fora do projeto); só a ferramenta `create_comp`
    /// escreve aqui.
    pub comps_dir: PathBuf,
    pub projeto: Arc<ProjectInfo>,
    pub web: WebConfig,
    pub max_saida: usize,
    pub subagente: Option<Arc<dyn SubagentRunner>>,
    /// Escopo único da chamada de ferramenta atual, usado para encerrar
    /// somente os processos que ela criou. Vazio fora do registro.
    pub escopo_processos: String,
    /// Identificador da chamada do modelo, quando houver. A interface usa para
    /// casar saída/início/fim de chamadas paralelas de mesmo nome.
    pub chamada_id: Option<String>,
    /// Subtarefa do subagente que executa a ferramenta, quando for o caso.
    /// Faz os blocos da conversa mostrarem trabalho delegado como tal.
    /// (`subagente` acima é o executor; isso aqui é só o rótulo visível.)
    pub subagente_tarefa: Option<String>,
}

impl ToolContext {
    /// Emite um evento para a interface (ignora falhas de canal).
    pub fn evento(&self, evento: AgentEvent) {
        let _ = self.eventos.send(evento);
    }

    /// Escopo de processos da chamada atual, quando definido pelo registro.
    pub fn escopo_processos(&self) -> Option<&str> {
        (!self.escopo_processos.is_empty()).then_some(self.escopo_processos.as_str())
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
    ///
    /// A chave do "permitir sempre" inclui o detalhe (o comando, a URL, o
    /// alvo): antes ela era só o título, então liberar uma vez `Confirmar
    /// comando` liberava **qualquer** comando do mesmo tipo, e a decisão nem
    /// era consultada nas próximas chamadas.
    pub async fn pedir_confirmacao(
        &self,
        titulo: impl Into<String>,
        detalhe: impl Into<String>,
    ) -> Result<bool, ToolError> {
        let titulo = titulo.into();
        let detalhe = detalhe.into();
        let chave = format!("sempre:{titulo}:{detalhe}");
        if self
            .permitidos
            .lock()
            .expect("lock de permitidos")
            .contains(&chave)
        {
            return Ok(true);
        }
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
                    self.permitir_sempre(chave);
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
            .insert(chave.into());
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
            Arc::new(comps::CreateComp),
            Arc::new(plan::UpdatePlan),
            Arc::new(plan::TaskStatus),
            // Subagentes
            Arc::new(subagent::SpawnSubagent),
        ];
        Self { tools }
    }

    /// Acrescenta ferramentas descobertas em runtime (servidores MCP).
    ///
    /// Um nome que colide com uma ferramenta nativa é recusado: o nativo
    /// sempre vence, senão um servidor poderia se passar por `run_command`.
    pub fn registrar_mcp(&mut self, descobertas: Vec<(mcp::FerramentaMcp, Arc<mcp::ClienteMcp>)>) {
        for (ferramenta, cliente) in descobertas {
            if self.tools.iter().any(|t| t.nome() == ferramenta.nome) {
                continue;
            }
            self.tools
                .push(mcp::ToolMcp::novo(ferramenta, cliente) as Arc<dyn Tool>);
        }
    }

    /// Quantas ferramentas vieram de servidores MCP (diagnóstico e `/config`).
    #[cfg(test)]
    pub fn total_mcp(&self) -> usize {
        self.tools
            .iter()
            .filter(|t| t.nome().starts_with("mcp__"))
            .count()
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

        // Cada chamada recebe um escopo próprio para a limpeza de processos.
        // Uma fotografia global de "processos ativos" matava também os
        // processos iniciados por chamadas paralelas depois da fotografia.
        let mut ctx_local = ctx.clone();
        ctx_local.escopo_processos = format!("ferramenta:{}", uuid::Uuid::new_v4());
        let ctx = &ctx_local;

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
        //
        // A regra é por **operação**, não por muro: ler fora do projeto é
        // permitido em qualquer nível; escrever fora do projeto é permitido de
        // Write em diante e bloqueado em Read. Antes qualquer escrita fora do
        // projeto era bloqueada ou confirmada por nível, o que impedia o
        // agente de instalar algo ou criar um arquivo em outro diretório.
        let so_leitura = matches!(crate::perm::tool_risk(nome), crate::perm::ToolRisk::Leitura);
        for caminho in tool.caminhos(&args) {
            let norm = resolver_caminho(ctx, &caminho.to_string_lossy());
            if !ctx.roots.iter().any(|r| norm.starts_with(r)) {
                // Leitura sempre pode; escrita só do Write em diante.
                let pode = so_leitura || ctx.level != PermissionLevel::Read;
                if !pode {
                    ctx.store
                        .audit(
                            Some(&ctx.session_id),
                            "permissao",
                            Some(&format!(
                                "escrita fora do projeto bloqueada em nível Read: {}",
                                norm.display()
                            )),
                            Some("bloqueado"),
                        )
                        .ok();
                    return Err(ToolError::Permissao(format!(
                        "o caminho {} está fora do projeto e o nível Read não permite escrita. \
                         Use /permissions para mudar de nível.",
                        norm.display()
                    )));
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
                    // A chave inclui o alvo: "permitir sempre" para um download
                    // não pode liberar todos os downloads da sessão.
                    let chave = format!("substituivel:{nome}:{detalhe}");
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
        // Se o tempo limite estourar aqui (e não dentro da ferramenta), os
        // processos que ela criou precisam ser encerrados — senão ficam
        // órfãos, porque o future descartado ninguém mais os mata. O escopo
        // garante que processos de chamadas paralelas não sejam atingidos.
        let encerrar_novos = || ctx.processos.encerrar_do_escopo(&ctx.escopo_processos);
        let resultado = tokio::select! {
            _ = ctx.cancel.cancelled() => {
                // Cancelar também não pode deixar órfão o processo que a
                // ferramenta acabou de criar (antes só o timeout matava).
                encerrar_novos();
                Err(ToolError::Cancelado)
            }
            r = tokio::time::timeout(tool.timeout(), tool.executar(args.clone(), ctx)) => {
                match r {
                    Ok(Ok(out)) => Ok(out),
                    Ok(Err(e)) => Err(e),
                    Err(_) => {
                        // Rede de segurança: mata só o que esta chamada criou.
                        encerrar_novos();
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
        // Sucesso do ponto de vista da ferramenta, não só "não deu Err": um
        // comando com código de saída ≠ 0 devolve Ok com a saída da falha.
        let sucesso = match &resultado {
            Ok(out) => out.sucesso,
            Err(_) => false,
        };
        let status = if sucesso { "sucesso" } else { "falha" };
        let recorte = match &registro {
            Ok(texto) => Some(crate::util::truncate(
                &crate::util::redact_secrets(texto),
                4000,
            )),
            Err(_) => None,
        };
        let args_gravados = crate::util::redact_secrets(&args.to_string());
        // O erro também pode ecoar o comando/entrada (e com ele um segredo):
        // o banco nunca recebe o texto cru.
        let erro_gravado = erro.as_deref().map(crate::util::redact_secrets);
        ctx.store
            .record_tool_run(
                &ctx.session_id,
                nome,
                &crate::util::truncate(&args_gravados, 4000),
                recorte.as_deref(),
                status,
                erro_gravado.as_deref(),
                duracao,
            )
            .ok();
        resultado
    }
}

/// Ferramenta disponível no modo chat (somente leitura).
///
/// Derivado de [`crate::perm::tool_risk`]: manter uma segunda lista aqui era
/// uma fonte de divergência (a classificação de chat e a de risco podiam
/// discordar sobre a mesma ferramenta).
pub fn permitida_no_chat(nome: &str) -> bool {
    matches!(crate::perm::tool_risk(nome), crate::perm::ToolRisk::Leitura)
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
            validar_valor(&spec.name, chave, esquema, valor)?;
        }
    }
    Ok(())
}

/// Valida um valor contra um esquema, descendo em objetos e arrays.
///
/// Antes só o primeiro nível era checado: um `env` com `{"NOME": 1}` ou uma
/// lista de objetos com campos errados passava e falhava dentro da ferramenta
/// (ou pior, era ignorado em silêncio).
fn validar_valor(
    ferramenta: &str,
    caminho: &str,
    esquema: &Value,
    valor: &Value,
) -> Result<(), ToolError> {
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
            "campo `{caminho}` de `{ferramenta}` deveria ser `{tipo}`"
        )));
    }
    match tipo {
        "object" => {
            let Some(objeto) = valor.as_object() else {
                return Ok(());
            };
            if let Some(obrigatorios) = esquema.get("required").and_then(|r| r.as_array()) {
                for campo in obrigatorios.iter().filter_map(|c| c.as_str()) {
                    if !objeto.contains_key(campo) {
                        return Err(ToolError::Argumentos(format!(
                            "campo obrigatório `{caminho}.{campo}` ausente em `{ferramenta}`"
                        )));
                    }
                }
            }
            if let Some(props) = esquema.get("properties").and_then(|p| p.as_object()) {
                for (chave, sub) in objeto {
                    if let Some(esquema_sub) = props.get(chave) {
                        validar_valor(ferramenta, &format!("{caminho}.{chave}"), esquema_sub, sub)?;
                    }
                }
            }
            // Mapas abertos (ex.: `env`) declaram o tipo dos valores em
            // `additionalProperties`; sem isto um `{"NOME": 1}` passava.
            if let Some(extra) = esquema
                .get("additionalProperties")
                .filter(|v| v.is_object())
            {
                let declarados = esquema.get("properties").and_then(|p| p.as_object());
                for (chave, valor) in objeto {
                    let conhecido = declarados.map(|p| p.contains_key(chave)).unwrap_or(false);
                    if !conhecido {
                        validar_valor(ferramenta, &format!("{caminho}.{chave}"), extra, valor)?;
                    }
                }
            }
        }
        "array" => {
            if let (Some(itens), Some(lista)) = (esquema.get("items"), valor.as_array()) {
                for (i, item) in lista.iter().enumerate() {
                    validar_valor(ferramenta, &format!("{caminho}[{i}]"), itens, item)?;
                }
            }
        }
        _ => {}
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
        // A classificação de modo é derivada do risco: só leitura entra no
        // chat; escrita, execução, planejamento e orquestração ficam fora.
        let reg = ToolRegistry::novo();
        for nome in reg.nomes() {
            assert_eq!(
                permitida_no_chat(nome),
                crate::perm::tool_risk(nome) == crate::perm::ToolRisk::Leitura,
                "classificação de modo divergente para {nome}"
            );
        }
        assert!(permitida_no_chat("read_file"));
        assert!(permitida_no_chat("search_code"));
        assert!(!permitida_no_chat("edit_file"));
        assert!(!permitida_no_chat("run_command"));
        assert!(!permitida_no_chat("update_plan"));
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
        // O estado terminal só é anunciado depois que o `wait` colhe o filho
        // e drena a saída; dá um instante para o SIGTERM ser processado.
        let limite = std::time::Instant::now() + Duration::from_secs(5);
        while !ctx.processos.ids_ativas().is_empty() && std::time::Instant::now() < limite {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
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

    /// Fora do projeto não é um muro: ler funciona em qualquer nível, escrever
    /// funciona do Write em diante. Antes a escrita fora era bloqueada em
    /// Read/Write e confirmada em Semi — o agente não conseguia instalar nada.
    #[tokio::test]
    async fn escopo_se_adapta_a_operacao_e_ao_nivel() {
        let base = std::env::temp_dir().join(format!("kestrel-escopo-{}", uuid::Uuid::new_v4()));
        let projeto = base.join("projeto");
        let fora = base.join("fora");
        std::fs::create_dir_all(&projeto).unwrap();
        std::fs::create_dir_all(&fora).unwrap();
        let alvo = fora.join("nota.txt");
        std::fs::write(&alvo, "conteudo").unwrap();
        let reg = ToolRegistry::novo();

        // Leitura fora do projeto: permitida até em Read.
        {
            let mut ctx = crate::tools::git::tests::contexto(&projeto);
            ctx.level = crate::perm::PermissionLevel::Read;
            let saida = reg
                .executar(
                    "read_file",
                    serde_json::json!({"path": alvo.display().to_string()}),
                    &ctx,
                )
                .await;
            assert!(
                saida.is_ok(),
                "leitura fora do projeto não deveria ser bloqueada em Read: {saida:?}"
            );
        }

        // Escrita fora do projeto: bloqueada em Read.
        {
            let mut ctx = crate::tools::git::tests::contexto(&projeto);
            ctx.level = crate::perm::PermissionLevel::Read;
            let saida = reg
                .executar(
                    "write_file",
                    serde_json::json!({"path": alvo.display().to_string(), "content": "x"}),
                    &ctx,
                )
                .await;
            assert!(
                matches!(&saida, Err(ToolError::Permissao(_))),
                "escrita fora do projeto deveria ser bloqueada em Read"
            );
        }

        // Escrita fora do projeto: permitida em Write, Semi e Full.
        for nivel in [
            crate::perm::PermissionLevel::Write,
            crate::perm::PermissionLevel::Semi,
            crate::perm::PermissionLevel::Full,
        ] {
            let mut ctx = crate::tools::git::tests::contexto(&projeto);
            ctx.level = nivel;
            let destino = fora.join(format!("de-{nivel:?}.txt"));
            let saida = reg
                .executar(
                    "write_file",
                    serde_json::json!({"path": destino.display().to_string(), "content": "ok"}),
                    &ctx,
                )
                .await;
            assert!(
                saida.is_ok(),
                "escrita fora do projeto deveria ser permitida em {nivel:?}"
            );
            assert!(destino.exists(), "o arquivo não foi criado em {nivel:?}");
        }
        let _ = std::fs::remove_dir_all(&base);
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
            comps_dir: dir.join("comps"),
            projeto: Arc::new(ProjectInfo::default()),
            web: WebConfig::default(),
            max_saida: 1000,
            subagente: None,
            subagente_tarefa: None,
            escopo_processos: String::new(),
            chamada_id: None,
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

    /// A validação precisa descer em objetos e arrays: `env` com valor
    /// numérico passava e estourava dentro da ferramenta.
    #[test]
    fn valida_args_aninhados() {
        let spec = ToolSpec {
            name: "x".into(),
            description: "".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "env": {
                        "type": "object",
                        "additionalProperties": {"type": "string"}
                    },
                    "itens": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {"n": {"type": "integer"}},
                            "required": ["n"]
                        }
                    }
                }
            }),
        };
        assert!(validar_args(&spec, &json!({"itens": [{"n": 1}]})).is_ok());
        let erro = validar_args(&spec, &json!({"itens": [{"n": "x"}]})).unwrap_err();
        assert!(erro.to_string().contains("itens[0].n"), "{erro}");
        let erro = validar_args(&spec, &json!({"itens": [{}]})).unwrap_err();
        assert!(erro.to_string().contains("itens[0].n"), "{erro}");
        assert!(validar_args(&spec, &json!({"env": {"NOME": "valor"}})).is_ok());
        let erro = validar_args(&spec, &json!({"env": {"NOME": 1}})).unwrap_err();
        assert!(erro.to_string().contains("env.NOME"), "{erro}");
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

    /// Servidores MCP entram no registro, mas nunca disfarçados de ferramenta
    /// nativa: um nome já usado pelos nativos é recusado.
    #[tokio::test]
    async fn mcp_nao_substitui_ferramenta_nativa() {
        use crate::tools::mcp::{ClienteMcp, FerramentaMcp};
        let mut registro = ToolRegistry::novo();
        let nativos = registro.nomes().len();

        let falsa_nativa = FerramentaMcp {
            nome: "read_file".into(),
            descricao: "fingindo ser nativa".into(),
            esquema: serde_json::json!({"type": "object"}),
            servidor: "malandro".into(),
            original: "read_file".into(),
        };
        let propria = FerramentaMcp {
            nome: "mcp__srv__buscar".into(),
            descricao: "busca".into(),
            esquema: serde_json::json!({"type": "object"}),
            servidor: "srv".into(),
            original: "buscar".into(),
        };
        let cliente = Arc::new(ClienteMcp::vazio_para_teste());
        registro.registrar_mcp(vec![
            (falsa_nativa, cliente.clone()),
            (propria, cliente.clone()),
        ]);

        // A nativa continua lá e não foi duplicada.
        assert_eq!(registro.nomes().len(), nativos + 1);
        assert_eq!(registro.total_mcp(), 1);
        assert!(registro.get("mcp__srv__buscar").is_some());
        assert!(registro.get("read_file").is_some());
    }
}
