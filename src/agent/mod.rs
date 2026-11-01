//! Ciclo autônomo do agente: estado explícito, execução assíncrona, streaming,
//! chamadas de ferramentas, recuperação de erros e continuidade.

pub mod context;
pub mod prompt;
pub mod recovery;

use crate::comps;
use crate::config::Config;
use crate::db::Store;
use crate::events::{AgentEvent, ConfirmRequest, PlanoPasso, StatusPasso, TarefaView, UiCommand};
use crate::perm::{PermissionLevel, ToolRisk};
use crate::project::{self, ProjectInfo};
use crate::provider::{
    ChatMessage, ChatRequest, Completion, Provider, ProviderError, StreamEvent, ToolCall, ToolSpec,
};
use crate::skills::SkillRegistry;
use crate::tools::shell::ProcessManager;
use crate::tools::{FileRegistry, SubagentRunner, ToolContext, ToolError, ToolRegistry};
use crate::util::Paths;
use serde_json::Value;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Estado da sessão ativa.
#[derive(Clone, Debug)]
pub struct EstadoSessao {
    pub id: String,
    pub titulo: String,
    pub objetivo: String,
}

/// Estado mutável do agente (configuração, provedor, projeto, skills).
pub struct EstadoAgente {
    pub config: Config,
    pub provider: Arc<dyn Provider>,
    pub projeto: Arc<ProjectInfo>,
    pub skills: Arc<SkillRegistry>,
    pub sessao: EstadoSessao,
}

/// Recursos compartilhados entre o agente, as ferramentas e a interface.
pub struct Compartilhado {
    pub paths: Paths,
    pub store: Store,
    /// Registro de ferramentas. `RwLock` assíncrono porque ferramentas de
    /// servidores MCP entram depois do início (o aperto de mão é assíncrono) e
    /// porque a execução de uma ferramenta segura a trava entre `await`s —
    /// com `std::sync::RwLock` isso seria acesso simultâneo ou deadlock.
    pub registro: Arc<tokio::sync::RwLock<ToolRegistry>>,
    pub processos: Arc<ProcessManager>,
    pub arquivos: Arc<FileRegistry>,
    pub eventos: mpsc::UnboundedSender<AgentEvent>,
    pub confirmacoes: mpsc::UnboundedSender<ConfirmRequest>,
    pub permitidos: Arc<Mutex<HashSet<String>>>,
    /// Indica que o aplicativo está sendo encerrado (`/quit`): o turno em
    /// andamento não deve sobrescrever o status `pausada` gravado pelo
    /// encerramento (`ativa`/`interrompida`).
    pub encerrando: AtomicBool,
    estado: RwLock<EstadoAgente>,
}

impl Compartilhado {
    pub fn config(&self) -> Config {
        self.estado.read().expect("estado").config.clone()
    }

    pub fn provider(&self) -> Arc<dyn Provider> {
        self.estado.read().expect("estado").provider.clone()
    }

    pub fn sessao(&self) -> EstadoSessao {
        self.estado.read().expect("estado").sessao.clone()
    }

    pub fn projeto(&self) -> Arc<ProjectInfo> {
        self.estado.read().expect("estado").projeto.clone()
    }

    pub fn skills(&self) -> Arc<SkillRegistry> {
        self.estado.read().expect("estado").skills.clone()
    }

    pub fn evento(&self, evento: AgentEvent) {
        let _ = self.eventos.send(evento);
    }

    /// Verifica o catálogo remoto do provedor em segundo plano e mescla os
    /// modelos novos com o catálogo local, persistindo e avisando a interface.
    ///
    /// Só acontece quando algo mudou de verdade; falha de rede, chave ausente
    /// ou endpoint indisponível passam em silêncio — a atualização automática
    /// não pode atrapalhar a abertura nem o trabalho do agente.
    pub fn atualizar_catalogo_async(self: &Arc<Self>) {
        let cfg = self.config();
        let Some(chave) = cfg.api_key() else {
            return;
        };
        if cfg.provider.base_url.trim().is_empty() {
            return;
        }
        let base_url = cfg.provider.base_url.clone();
        let existentes = cfg.provider.models.clone();
        let comp = Arc::clone(self);
        tokio::spawn(async move {
            let Ok(descobertos) = crate::provider::catalog::buscar_modelos(&base_url, &chave).await
            else {
                return;
            };
            let mesclados = crate::provider::catalog::merge_discovered(&existentes, descobertos);
            let novos = mesclados
                .iter()
                .filter(|m| !existentes.iter().any(|e| e.id == m.id))
                .count();
            if novos == 0 && mesclados.len() == existentes.len() {
                return; // nada mudou: não persiste nem incomoda
            }
            {
                let mut estado = comp.estado.write().expect("estado");
                estado.config.provider.models = mesclados.clone();
                if let Err(e) = estado.config.save(&comp.paths) {
                    let _ = comp.eventos.send(AgentEvent::Aviso {
                        mensagem: format!("catálogo atualizado, mas falha ao salvar: {e}"),
                    });
                    return;
                }
            }
            let _ = comp.eventos.send(AgentEvent::CatalogoAtualizado {
                modelos: mesclados,
                novos,
            });
        });
    }

    pub fn estado_ui(&self, mensagem: impl Into<String>) {
        self.evento(AgentEvent::Estado {
            mensagem: mensagem.into(),
        });
    }

    pub fn definir_sessao(&self, sessao: EstadoSessao) {
        self.estado.write().expect("estado").sessao = sessao;
    }

    /// Cria um ramo (checkpoint) da sessão atual: histórico, plano, tarefas,
    /// decisões e alterações de arquivo são copiados para uma sessão nova,
    /// que passa a ser a ativa. A original é preservada.
    pub fn ramificar(&self, titulo: Option<String>) -> anyhow::Result<()> {
        let sessao = self.sessao();
        let id = uuid::Uuid::new_v4().to_string();
        let titulo = titulo
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| format!("ramo de {}", crate::util::truncate(&sessao.titulo, 50)));
        let mensagens = self.store.branch_session(&sessao.id, &id, &titulo)?;
        self.definir_sessao(EstadoSessao {
            id: id.clone(),
            titulo: titulo.clone(),
            objetivo: sessao.objetivo.clone(),
        });
        self.evento(AgentEvent::Ramificada {
            id,
            titulo,
            mensagens,
        });
        Ok(())
    }

    /// Atualiza o modelo e o esforço, recriando o provedor e persistindo.
    pub fn definir_modelo(&self, modelo: String, esforco: Option<String>) -> anyhow::Result<()> {
        let mut estado = self.estado.write().expect("estado");
        estado.config.provider.model = modelo.clone();
        estado.config.provider.reasoning_effort = esforco.clone();
        estado.provider = Arc::from(crate::provider::nvidia::create_provider(&estado.config)?);
        estado
            .config
            .save(&self.paths)
            .map_err(|e| anyhow::anyhow!("falha ao salvar configuração: {e}"))?;
        let cfg = estado.config.clone();
        drop(estado);
        self.store
            .update_session_model(
                &self.sessao().id,
                &cfg.provider.model,
                cfg.provider.reasoning_effort.as_deref(),
            )
            .ok();
        self.evento(AgentEvent::ModeloAtual { modelo, esforco });
        Ok(())
    }

    /// Define o esforço de raciocínio mantendo o modelo atual.
    pub fn definir_esforco(&self, esforco: Option<String>) -> anyhow::Result<()> {
        let modelo = self.config().provider.model;
        self.definir_modelo(modelo, esforco)
    }

    /// Liga/desliga a exibição do raciocínio do modelo.
    ///
    /// Persistido na configuração: a escolha vale para as próximas sessões. O
    /// consumo em si é decidido no laço do turno, que consulta
    /// `cfg.ui.show_reasoning` antes de emitir os delas de raciocínio.
    pub fn definir_raciocinio(&self, visivel: bool) {
        {
            let mut estado = self.estado.write().expect("estado");
            estado.config.ui.show_reasoning = visivel;
        }
        self.store
            .audit(
                Some(&self.sessao().id),
                "config",
                Some(&format!(
                    "raciocínio do modelo: {}",
                    if visivel { "visível" } else { "oculto" }
                )),
                Some("definir_raciocinio"),
            )
            .ok();
    }

    /// Define a skin visual e persiste, mantendo a cópia do agente alinhada
    /// com a da interface (senão um `/model` posterior gravaria a skin antiga).
    pub fn definir_skin(&self, skin: String) -> anyhow::Result<()> {
        {
            let mut estado = self.estado.write().expect("estado");
            estado.config.ui.skin = skin.clone();
            estado
                .config
                .save(&self.paths)
                .map_err(|e| anyhow::anyhow!("falha ao salvar configuração: {e}"))?;
        }
        self.store
            .audit(
                Some(&self.sessao().id),
                "config",
                Some(&format!("skin: {skin}")),
                Some("definir_skin"),
            )
            .ok();
        Ok(())
    }

    /// Ativa ou desativa um Comp. O arquivo já precisa existir; a criação é
    /// feita pela interface ou pela ferramenta `create_comp`.
    pub fn definir_comp(&self, comp: Option<String>) -> anyhow::Result<()> {
        if let Some(nome) = &comp
            && comps::encontrar(&self.paths.comps_dir(), nome).is_none()
        {
            anyhow::bail!(
                "Comp `{nome}` não encontrado em {}",
                self.paths.comps_dir().display()
            );
        }
        {
            let mut estado = self.estado.write().expect("estado");
            estado.config.ui.active_comp = comp.clone();
            estado
                .config
                .save(&self.paths)
                .map_err(|e| anyhow::anyhow!("falha ao salvar configuração: {e}"))?;
        }
        self.store
            .audit(
                Some(&self.sessao().id),
                "config",
                Some(&format!(
                    "Comp ativo: {}",
                    comp.as_deref().unwrap_or("(nenhum)")
                )),
                Some("definir_comp"),
            )
            .ok();
        Ok(())
    }

    /// Define o nível de permissão e persiste.
    pub fn definir_permissao(&self, nivel: PermissionLevel) -> anyhow::Result<()> {
        {
            let mut estado = self.estado.write().expect("estado");
            estado.config.definir_permissao(nivel);
            estado
                .config
                .save(&self.paths)
                .map_err(|e| anyhow::anyhow!("falha ao salvar configuração: {e}"))?;
        }
        // O allowlist por ferramenta é limpo ao reduzir o nível.
        self.permitidos.lock().expect("lock").clear();
        self.store
            .audit(
                Some(&self.sessao().id),
                "permissao",
                Some(&format!("nível alterado para {nivel}")),
                Some(nivel.label()),
            )
            .ok();
        Ok(())
    }

    /// Define o modo de operação (chat ou agente) e persiste.
    pub fn definir_modo(&self, modo: crate::config::ModoOperacao) -> anyhow::Result<()> {
        {
            let mut estado = self.estado.write().expect("estado");
            estado.config.agent.modo = modo;
            estado
                .config
                .save(&self.paths)
                .map_err(|e| anyhow::anyhow!("falha ao salvar configuração: {e}"))?;
        }
        self.store
            .audit(
                Some(&self.sessao().id),
                "modo",
                Some(&format!("modo alterado para {}", modo.label())),
                Some(modo.label()),
            )
            .ok();
        Ok(())
    }

    /// Reanalisa o projeto e atualiza o estado.
    pub fn reanalisar_projeto(&self) -> Arc<ProjectInfo> {
        let workdir = self.config().workdir();
        let info = Arc::new(project::analyze(&workdir));
        self.estado.write().expect("estado").projeto = info.clone();
        info
    }

    /// Recarrega as skills dos diretórios monitorados.
    pub fn recarregar_skills(&self) -> Arc<SkillRegistry> {
        let config = self.config();
        let skills = Arc::new(SkillRegistry::discover(&self.paths, &config.workdir()));
        self.estado.write().expect("estado").skills = skills.clone();
        skills
    }

    /// Fixa o diretório de trabalho, persiste e reanalisa projeto e skills.
    ///
    /// Retomar uma sessão de outro projeto também passa por aqui: sem isso o
    /// histórico de um projeto era continuado com as ferramentas apontando
    /// para o projeto atual.
    pub fn definir_projeto(&self, caminho: &Path) -> anyhow::Result<PathBuf> {
        let caminho = std::fs::canonicalize(caminho).unwrap_or_else(|_| caminho.to_path_buf());
        {
            let mut estado = self.estado.write().expect("estado");
            estado.config.agent.workdir = Some(caminho.clone());
            // Escolha explícita: o diretório passa a ser fixo entre execuções.
            estado.config.agent.pin_workdir = true;
            estado
                .config
                .save(&self.paths)
                .map_err(|e| anyhow::anyhow!("falha ao salvar configuração: {e}"))?;
        }
        self.store
            .kv_set("ultimo_projeto", &caminho.display().to_string())
            .ok();
        self.reanalisar_projeto();
        self.recarregar_skills();
        Ok(caminho)
    }

    /// Troca o projeto ativo: valida e persiste o diretório, reanalisa o
    /// projeto, recarrega as skills e abre uma nova sessão no novo local.
    pub fn trocar_projeto(&self, caminho: &str) -> anyhow::Result<()> {
        let bruto = crate::util::expand_tilde(caminho.trim());
        if !bruto.is_dir() {
            anyhow::bail!("`{}` não é um diretório válido", bruto.display());
        }
        let caminho = self.definir_projeto(&bruto)?;
        let info = self.projeto();
        let skills = self.skills();

        // Cada projeto tem seu próprio histórico: abre uma sessão nova.
        let cfg = self.config();
        let id = uuid::Uuid::new_v4().to_string();
        let titulo = format!(
            "sessão em {}",
            crate::util::truncate(&caminho.display().to_string(), 60)
        );
        self.store.create_session(
            &id,
            &caminho.display().to_string(),
            &titulo,
            &cfg.provider.model,
            cfg.provider.reasoning_effort.as_deref(),
            cfg.agent.permission.label(),
            None,
        )?;
        self.permitidos.lock().expect("lock").clear();
        self.definir_sessao(EstadoSessao {
            id: id.clone(),
            titulo: titulo.clone(),
            objetivo: String::new(),
        });
        self.evento(AgentEvent::SessaoAtiva {
            id,
            titulo,
            workdir: caminho.display().to_string(),
        });
        // O plano e as subtarefas pertencem à sessão anterior.
        self.evento(AgentEvent::PlanoAtualizado {
            objetivo: String::new(),
            passos: Vec::new(),
        });
        self.evento(AgentEvent::TarefasAtualizadas {
            tarefas: Vec::new(),
        });
        self.evento(AgentEvent::Aviso {
            mensagem: format!(
                "Projeto alterado para {} ({} skills carregadas).\n{}",
                caminho.display(),
                skills.all().len(),
                info.summary()
            ),
        });
        Ok(())
    }

    /// Constrói o contexto de execução das ferramentas.
    pub fn contexto_ferramentas(&self, cancel: CancellationToken) -> ToolContext {
        let config = self.config();
        ToolContext {
            subagente_tarefa: None,
            workdir: config.workdir(),
            roots: config.allowed_roots(),
            level: config.agent.permission,
            modo: config.agent.modo,
            cancel,
            store: self.store.clone(),
            session_id: self.sessao().id,
            processos: self.processos.clone(),
            sandbox: crate::sandbox::SandboxSettings::novo(
                config.agent.sandbox,
                config.agent.sandbox_allow_network,
                config.agent.sandbox_extra_writable.clone(),
            ),
            arquivos: self.arquivos.clone(),
            eventos: self.eventos.clone(),
            confirmacoes: self.confirmacoes.clone(),
            permitidos: self.permitidos.clone(),
            skills: self.skills(),
            skills_dir: self.paths.skills_dir(),
            comps_dir: self.paths.comps_dir(),
            projeto: self.projeto(),
            web: config.web.clone(),
            max_saida: 30_000,
            subagente: None,
            escopo_processos: String::new(),
            chamada_id: None,
        }
    }
}

/// Handle devolvido à interface.
pub struct AgenteHandle {
    pub cmd_tx: mpsc::UnboundedSender<UiCommand>,
    pub eventos: mpsc::UnboundedReceiver<AgentEvent>,
}

/// Inicia o agente e devolve o canal de comandos e o fluxo de eventos.
pub fn iniciar(config: Config, paths: Paths, store: Store) -> anyhow::Result<AgenteHandle> {
    let (eventos_tx, eventos_rx) = mpsc::unbounded_channel::<AgentEvent>();
    let (cmd_tx, cmd_rx) = mpsc::unbounded_channel::<UiCommand>();
    let (confirm_tx, mut confirm_rx) = mpsc::unbounded_channel::<ConfirmRequest>();

    // Encaminha pedidos de confirmação para a interface.
    let eventos_confirmacao = eventos_tx.clone();
    tokio::spawn(async move {
        while let Some(pedido) = confirm_rx.recv().await {
            if eventos_confirmacao
                .send(AgentEvent::Confirmacao { pedido })
                .is_err()
            {
                break;
            }
        }
    });

    let workdir = config.workdir();
    let sessao_id = uuid::Uuid::new_v4().to_string();
    let titulo = format!(
        "sessão em {}",
        crate::util::truncate(&workdir.display().to_string(), 60)
    );
    store.create_session(
        &sessao_id,
        &workdir.display().to_string(),
        &titulo,
        &config.provider.model,
        config.provider.reasoning_effort.as_deref(),
        config.agent.permission.label(),
        None,
    )?;
    store
        .kv_set("ultimo_projeto", &workdir.display().to_string())
        .ok();

    let projeto = Arc::new(project::analyze(&workdir));
    let skills = Arc::new(SkillRegistry::discover(&paths, &workdir));
    let provider: Arc<dyn Provider> = Arc::from(crate::provider::nvidia::create_provider(&config)?);

    let comp = Arc::new(Compartilhado {
        paths,
        store,
        registro: Arc::new(tokio::sync::RwLock::new(ToolRegistry::novo())),
        processos: Arc::new(ProcessManager::new()),
        arquivos: Arc::new(FileRegistry::new()),
        eventos: eventos_tx,
        confirmacoes: confirm_tx,
        permitidos: Arc::new(Mutex::new(HashSet::new())),
        encerrando: AtomicBool::new(false),
        estado: RwLock::new(EstadoAgente {
            config,
            provider,
            projeto,
            skills,
            sessao: EstadoSessao {
                id: sessao_id,
                titulo,
                objetivo: String::new(),
            },
        }),
    });

    // Servidores MCP: o aperto de mão é assíncrono, então sobe em paralelo à
    // interface. Cada servidor tem seu próprio tempo; a falha de um não impede
    // os outros nem a abertura do Kestrel.
    if !comp.config().mcp.is_empty() {
        let comp_mcp = comp.clone();
        let cfg_mcp = comp.config();
        let servidores = cfg_mcp.mcp.clone();
        let confiaveis = cfg_mcp.mcp_trusted_keys.clone();
        let sandbox = crate::sandbox::SandboxSettings::novo(
            cfg_mcp.agent.sandbox,
            cfg_mcp.agent.sandbox_allow_network,
            cfg_mcp.agent.sandbox_extra_writable.clone(),
        );
        tokio::spawn(async move {
            let (descobertas, avisos) =
                crate::tools::mcp::conectar_todos(&servidores, &confiaveis, &sandbox).await;
            for aviso in avisos {
                comp_mcp.evento(AgentEvent::Aviso { mensagem: aviso });
            }
            if !descobertas.is_empty() {
                let total = descobertas.len();
                comp_mcp.registro.write().await.registrar_mcp(descobertas);
                comp_mcp.evento(AgentEvent::Aviso {
                    mensagem: format!(
                        "MCP: {total} ferramentas de servidores remotos/locais disponíveis                          (nomes com prefixo `mcp__`)."
                    ),
                });
            }
        });
    }

    // Catálogo de modelos: atualização automática em segundo plano (a falha
    // é silenciosa e o resultado só aparece quando há modelos novos).
    comp.atualizar_catalogo_async();

    // Estado inicial para a interface (a sessão já existe no banco).
    {
        let sessao = comp.sessao();
        comp.evento(AgentEvent::SessaoAtiva {
            id: sessao.id,
            titulo: sessao.titulo,
            workdir: workdir.display().to_string(),
        });
        let cfg = comp.config();
        comp.evento(AgentEvent::ModeloAtual {
            modelo: cfg.provider.model.clone(),
            esforco: cfg.provider.reasoning_effort.clone(),
        });
        if !cfg.api_key_presente() {
            comp.evento(AgentEvent::Aviso {
                mensagem: format!(
                    "a variável de ambiente {} não está definida: defina-a e reinicie o Kestrel \
                     para conversar com o modelo.",
                    cfg.provider.api_key_env
                ),
            });
        } else if cfg.provider.model.trim().is_empty() {
            comp.evento(AgentEvent::Aviso {
                mensagem: "nenhum modelo selecionado: use /model para escolher um modelo.".into(),
            });
        }
    }

    let agente = Agente { comp, cmd_rx };
    tokio::spawn(async move { agente.run().await });

    Ok(AgenteHandle {
        cmd_tx,
        eventos: eventos_rx,
    })
}

/// Ação que só pode ser aplicada quando o turno em andamento terminar.
///
/// Criar uma sessão ou trocar de projeto no meio de um turno faria o turno
/// antigo continuar gravando mensagens, ferramentas e status na sessão nova.
#[derive(Debug)]
enum AcaoPosTurno {
    NovaSessao,
    TrocarProjeto(String),
}

/// Motor do agente: laço de comandos e turnos.
pub struct Agente {
    comp: Arc<Compartilhado>,
    cmd_rx: mpsc::UnboundedReceiver<UiCommand>,
}

impl Agente {
    pub async fn run(mut self) {
        let (fim_tx, mut fim_rx) = mpsc::unbounded_channel::<()>();
        let mut turno_ativo = false;
        let mut cancel_atual: Option<CancellationToken> = None;
        // Fila, não um único slot: duas mensagens enviadas durante um turno
        // não podem se sobrescrever (a primeira era perdida em silêncio).
        let mut pendentes: std::collections::VecDeque<(String, bool)> =
            std::collections::VecDeque::new();
        // Troca de sessão/projeto pedida durante um turno: fica para depois
        // que o turno cancelado realmente terminar.
        let mut acao_pendente: Option<AcaoPosTurno> = None;

        // Sem aviso de “sessão anterior interrompida”: ele aparecia em quase
        // toda abertura e virava ruído. A recuperação continua existindo, mas
        // quem decide usá-la é o usuário, via /retomar.

        loop {
            tokio::select! {
                comando = self.cmd_rx.recv() => {
                    let Some(comando) = comando else { break };
                    match comando {
                        UiCommand::EnviarMensagem(texto) => {
                            if turno_ativo {
                                pendentes.push_back((texto, false));
                                if let Some(c) = &cancel_atual { c.cancel(); }
                            } else {
                                let cancel = CancellationToken::new();
                                cancel_atual = Some(cancel.clone());
                                turno_ativo = true;
                                spawn_turno(self.comp.clone(), texto, false, cancel, fim_tx.clone());
                            }
                        }
                        UiCommand::Redirecionar(texto) => {
                            if turno_ativo {
                                // Redirecionar substitui os pedidos enfileirados:
                                // as instruções novas são o que importa agora.
                                pendentes.clear();
                                pendentes.push_back((texto, true));
                                if let Some(c) = &cancel_atual { c.cancel(); }
                                self.comp.evento(AgentEvent::Aviso {
                                    mensagem: "Redirecionamento recebido: interrompendo a execução atual e aplicando as novas instruções.".into(),
                                });
                            } else {
                                let cancel = CancellationToken::new();
                                cancel_atual = Some(cancel.clone());
                                turno_ativo = true;
                                spawn_turno(self.comp.clone(), texto, true, cancel, fim_tx.clone());
                            }
                        }
                        UiCommand::Interromper => {
                            if let Some(c) = &cancel_atual { c.cancel(); }
                            // A mensagem enfileirada também é descartada: quem
                            // pede "interromper" não quer que o agente continue
                            // trabalhando no pedido anterior assim que o turno
                            // atual terminar.
                            if !pendentes.is_empty() {
                                pendentes.clear();
                                self.comp.evento(AgentEvent::Aviso {
                                    mensagem: "Turno interrompido; o pedido enfileirado foi descartado."
                                        .into(),
                                });
                            }
                        }
                        UiCommand::DefinirPermissao(nivel) => {
                            // A interface já confirmou a troca; aqui só persistimos.
                            if let Err(e) = self.comp.definir_permissao(nivel) {
                                self.comp.evento(AgentEvent::Erro {
                                    mensagem: format!("falha ao salvar o nível de permissão: {e}"),
                                    dica: None,
                                    recuperavel: true,
                                });
                            }
                        }
                        UiCommand::DefinirModo(modo) => {
                            // A interface já confirmou a troca; aqui só persistimos.
                            if let Err(e) = self.comp.definir_modo(modo) {
                                self.comp.evento(AgentEvent::Erro {
                                    mensagem: format!("falha ao salvar o modo de operação: {e}"),
                                    dica: None,
                                    recuperavel: true,
                                });
                            }
                        }
                        UiCommand::DefinirModelo { modelo, esforco } => {
                            if let Err(e) = self.comp.definir_modelo(modelo, esforco) {
                                self.comp.evento(AgentEvent::Erro {
                                    mensagem: format!("falha ao trocar o modelo: {e}"),
                                    dica: None,
                                    recuperavel: true,
                                });
                            }
                        }
                        UiCommand::DefinirEsforco(esforco) => {
                            if let Err(e) = self.comp.definir_esforco(esforco) {
                                self.comp.evento(AgentEvent::Erro {
                                    mensagem: format!("falha ao alterar o esforço de raciocínio: {e}"),
                                    dica: None,
                                    recuperavel: true,
                                });
                            }
                        }
                        UiCommand::DefinirRaciocinio(visivel) => {
                            // A escolha passa a valer para os turnos seguintes
                            // (e para o atual, no próximo delta).
                            self.comp.definir_raciocinio(visivel);
                        }
                        UiCommand::DefinirSkin(skin) => {
                            if let Err(e) = self.comp.definir_skin(skin) {
                                self.comp.evento(AgentEvent::Erro {
                                    mensagem: format!("falha ao salvar a skin: {e}"),
                                    dica: None,
                                    recuperavel: true,
                                });
                            }
                        }
                        UiCommand::DefinirComp(comp) => {
                            if let Err(e) = self.comp.definir_comp(comp) {
                                self.comp.evento(AgentEvent::Erro {
                                    mensagem: format!("falha ao ativar o Comp: {e}"),
                                    dica: Some("use /person para listar os Comps disponíveis".into()),
                                    recuperavel: true,
                                });
                            }
                        }
                        UiCommand::NovaSessao => {
                            if turno_ativo {
                                if let Some(c) = &cancel_atual { c.cancel(); }
                                pendentes.clear();
                                acao_pendente = Some(AcaoPosTurno::NovaSessao);
                                self.comp.evento(AgentEvent::Aviso {
                                    mensagem: "Nova sessão: interrompendo a execução atual.".into(),
                                });
                            } else {
                                self.nova_sessao();
                            }
                        }
                        UiCommand::RetomarSessao(id) => {
                            if turno_ativo {
                                // Trocar a sessão ativa no meio de um turno faria o
                                // turno em andamento gravar ferramentas na sessão
                                // errada. Melhor recusar com instrução clara.
                                self.comp.evento(AgentEvent::Aviso {
                                    mensagem: "Há um turno em andamento. Interrompa-o (Esc) antes de retomar outra sessão.".into(),
                                });
                            } else {
                                self.retomar_sessao(&id, &mut turno_ativo, &mut cancel_atual, &fim_tx);
                            }
                        }
                        UiCommand::Ramificar { titulo } => {
                            if turno_ativo {
                                self.comp.evento(AgentEvent::Aviso {
                                    mensagem: "Há um turno em andamento. Interrompa-o (Esc) antes de criar um ramo.".into(),
                                });
                            } else if let Err(e) = self.comp.ramificar(titulo) {
                                self.comp.evento(AgentEvent::Aviso {
                                    mensagem: format!("não foi possível criar o ramo: {e}"),
                                });
                            }
                        }
                        UiCommand::RecarregarSkills => {
                            let workdir = self.comp.config().workdir();
                            let skills = Arc::new(SkillRegistry::discover(
                                &self.comp.paths,
                                &workdir,
                            ));
                            let total = skills.all().len();
                            self.comp.estado.write().expect("estado").skills = skills;
                            self.comp.evento(AgentEvent::Aviso {
                                mensagem: format!(
                                    "skills recarregadas: {total} disponível(is) em {}",
                                    self.comp.paths.skills_dir().display()
                                ),
                            });
                        }
                        UiCommand::AnalisarProjeto => {
                            let info = self.comp.reanalisar_projeto();
                            self.comp.evento(AgentEvent::Aviso {
                                mensagem: format!("Análise do projeto atualizada:\n{}", info.summary()),
                            });
                        }
                        UiCommand::TrocarProjeto(caminho) => {
                            if turno_ativo {
                                if let Some(c) = &cancel_atual { c.cancel(); }
                                pendentes.clear();
                                acao_pendente = Some(AcaoPosTurno::TrocarProjeto(caminho));
                                self.comp.evento(AgentEvent::Aviso {
                                    mensagem: "Troca de projeto: interrompendo a execução atual."
                                        .into(),
                                });
                            } else {
                                self.trocar_projeto(&caminho);
                            }
                        }
                        UiCommand::ListarProcessos => {
                            self.comp.evento(AgentEvent::Processos {
                                lista: self.comp.processos.listar(),
                            });
                        }
                        UiCommand::Encerrar => {
                            // Marca o encerramento ANTES de cancelar: o turno
                            // destacado não pode sobrescrever `pausada`.
                            self.comp.encerrando.store(true, Ordering::SeqCst);
                            if let Some(c) = &cancel_atual { c.cancel(); }
                            self.comp.processos.encerrar_todos();
                            self.comp
                                .store
                                .update_session_status(&self.comp.sessao().id, "pausada")
                                .ok();
                            self.comp.evento(AgentEvent::Encerrado);
                            break;
                        }
                    }
                }
                Some(()) = fim_rx.recv() => {
                    turno_ativo = false;
                    cancel_atual = None;
                    // A troca de sessão/projeto só acontece agora: o turno
                    // cancelado já terminou de gravar no banco.
                    match acao_pendente.take() {
                        Some(AcaoPosTurno::NovaSessao) => self.nova_sessao(),
                        Some(AcaoPosTurno::TrocarProjeto(caminho)) => {
                            self.trocar_projeto(&caminho);
                        }
                        None => {}
                    }
                    if let Some((texto, redirecionado)) = pendentes.pop_front() {
                        let cancel = CancellationToken::new();
                        cancel_atual = Some(cancel.clone());
                        turno_ativo = true;
                        spawn_turno(self.comp.clone(), texto, redirecionado, cancel, fim_tx.clone());
                    }
                }
            }
        }
    }

    fn trocar_projeto(&self, caminho: &str) {
        if let Err(e) = self.comp.trocar_projeto(caminho) {
            self.comp.evento(AgentEvent::Erro {
                mensagem: format!("falha ao trocar de projeto: {e}"),
                dica: Some("escolha um diretório existente em /projetos".into()),
                recuperavel: true,
            });
        }
    }

    fn nova_sessao(&self) {
        let cfg = self.comp.config();
        let workdir = cfg.workdir();
        let id = uuid::Uuid::new_v4().to_string();
        let titulo = format!(
            "sessão em {}",
            crate::util::truncate(&workdir.display().to_string(), 60)
        );
        if let Err(e) = self.comp.store.create_session(
            &id,
            &workdir.display().to_string(),
            &titulo,
            &cfg.provider.model,
            cfg.provider.reasoning_effort.as_deref(),
            cfg.agent.permission.label(),
            None,
        ) {
            self.comp.evento(AgentEvent::Erro {
                mensagem: format!("falha ao criar sessão: {e}"),
                dica: None,
                recuperavel: true,
            });
            return;
        }
        // Nova sessão, nova allowlist: um "permitir sempre" dado no contexto
        // anterior não deve valer para outro trabalho.
        self.comp.permitidos.lock().expect("lock").clear();
        self.comp.definir_sessao(EstadoSessao {
            id: id.clone(),
            titulo: titulo.clone(),
            objetivo: String::new(),
        });
        self.comp.evento(AgentEvent::SessaoAtiva {
            id,
            titulo,
            workdir: workdir.display().to_string(),
        });
        self.comp.evento(AgentEvent::Aviso {
            mensagem: "Nova sessão iniciada. O histórico anterior continua salvo e pode ser retomado com /retomar.".into(),
        });
    }

    fn retomar_sessao(
        &self,
        id: &str,
        turno_ativo: &mut bool,
        cancel_atual: &mut Option<CancellationToken>,
        fim_tx: &mpsc::UnboundedSender<()>,
    ) {
        let Ok(Some(sessao)) = self.comp.store.get_session(id) else {
            self.comp.evento(AgentEvent::Erro {
                mensagem: format!("sessão `{id}` não encontrada"),
                dica: Some("use /historico para listar as sessões salvas".into()),
                recuperavel: true,
            });
            return;
        };
        // O projeto da sessão é a fonte da verdade. Retomar uma sessão de
        // outro diretório sem trocar o workdir faria o agente ler e escrever
        // no projeto errado enquanto exibe o histórico antigo.
        let bruto = crate::util::expand_tilde(sessao.project_path.trim());
        if !bruto.is_dir() {
            self.comp.evento(AgentEvent::Erro {
                mensagem: format!(
                    "o projeto da sessão `{id}` não existe mais: {}",
                    bruto.display()
                ),
                dica: Some(
                    "abra a sessão pelo projeto original ou crie o diretório novamente".into(),
                ),
                recuperavel: true,
            });
            return;
        }
        let caminho = std::fs::canonicalize(&bruto).unwrap_or(bruto);
        if caminho != self.comp.config().workdir()
            && let Err(e) = self.comp.definir_projeto(&caminho)
        {
            self.comp.evento(AgentEvent::Erro {
                mensagem: format!("falha ao trocar para o projeto da sessão: {e}"),
                dica: None,
                recuperavel: true,
            });
            return;
        }
        self.comp.permitidos.lock().expect("lock").clear();
        self.comp.definir_sessao(EstadoSessao {
            id: sessao.id.clone(),
            titulo: sessao.title.clone(),
            objetivo: sessao.objective.clone().unwrap_or_default(),
        });
        // Reconstrói o estado real do projeto antes de continuar.
        let info = self.comp.reanalisar_projeto();
        let tarefas = self.comp.store.tasks(&sessao.id).unwrap_or_default();
        let pendentes = tarefas.iter().filter(|t| t.status != "concluido").count();
        let contexto = self.comp.store.context_state(&sessao.id).ok().flatten();
        let dossie = contexto
            .as_ref()
            .and_then(|(_, _, d)| d.as_deref())
            .filter(|d| !d.trim().is_empty());
        self.comp.evento(AgentEvent::SessaoAtiva {
            id: sessao.id.clone(),
            titulo: sessao.title.clone(),
            workdir: self.comp.config().workdir().display().to_string(),
        });
        self.comp.evento(AgentEvent::Aviso {
            mensagem: format!(
                "Sessão retomada: {} ({}). Estado do projeto: {} linguagens, branch {}; {} subtarefas pendentes.{}",
                sessao.title,
                if sessao.status == "ativa" { "interrompida anteriormente" } else { &sessao.status },
                info.languages.len(),
                info.git_branch.clone().unwrap_or_else(|| "-".into()),
                pendentes,
                match &contexto {
                    Some((used, capacity, _)) => format!(
                        " Contexto: {used}/{capacity} tokens; dossiê de continuidade {}.",
                        if dossie.is_some() { "aplicado" } else { "ausente" }
                    ),
                    None => String::new(),
                }
            ),
        });
        // Continuação automática com verificação do estado real.
        let mut instrucao = "Retome o trabalho interrompido. Antes de continuar: verifique o estado real do projeto \
            (git status, arquivos citados no plano) para confirmar o que já foi feito, e então siga a partir do \
            ponto de interrupção usando o dossiê de continuidade. Não refaça trabalho já concluído."
            .to_string();
        if let Some(dossie) = dossie {
            instrucao.push_str("\n\n## Dossiê de continuidade registrado\n");
            instrucao.push_str(dossie);
        }
        if !*turno_ativo {
            let cancel = CancellationToken::new();
            *cancel_atual = Some(cancel.clone());
            *turno_ativo = true;
            spawn_turno(self.comp.clone(), instrucao, true, cancel, fim_tx.clone());
        }
    }
}

/// Sinaliza o fim do turno ao dono do canal mesmo que a task paniquee.
///
/// Sem este guard, um pânico dentro de `executar_turno` (por exemplo, um
/// fatiamento inválido numa ferramenta) deixaria `fim_tx.send(())` sem
/// executar: a interface nunca receberia o fim do turno, `turno_ativo`
/// continuaria `true` e nenhum turno novo seria iniciado — a sessão travaria
/// por completo.
struct SinalizaFim(mpsc::UnboundedSender<()>);

impl Drop for SinalizaFim {
    fn drop(&mut self) {
        let _ = self.0.send(());
    }
}

fn spawn_turno(
    comp: Arc<Compartilhado>,
    texto: String,
    redirecionado: bool,
    cancel: CancellationToken,
    fim_tx: mpsc::UnboundedSender<()>,
) {
    tokio::spawn(async move {
        // O guard roda no Drop: cobre erro E pânico.
        let _fim = SinalizaFim(fim_tx);
        let comp_erro = comp.clone();
        if let Err(e) = executar_turno(comp, texto, redirecionado, cancel).await {
            eprintln!("kestrel: falha inesperada no turno: {e}");
            // Antes só ia para o stderr: a interface ficava sem saber que o
            // turno morreu (e a sessão continuava marcada como ativa).
            comp_erro.evento(AgentEvent::Erro {
                mensagem: format!("falha interna ao executar o turno: {e}"),
                dica: Some(
                    "as alterações já feitas foram preservadas; verifique o disco/banco e tente novamente"
                        .into(),
                ),
                recuperavel: true,
            });
            if !comp_erro.encerrando.load(Ordering::SeqCst) {
                comp_erro
                    .store
                    .update_session_status(&comp_erro.sessao().id, "interrompida")
                    .ok();
            }
            comp_erro.evento(AgentEvent::TurnoConcluido {
                resumo: format!("Turno encerrado por falha interna: {e}"),
            });
        }
    });
}

/// Resultado interno de um turno.
struct ResultadoTurno {
    resumo: String,
    concluido: bool,
}

/// Executa um turno completo do agente.
async fn executar_turno(
    comp: Arc<Compartilhado>,
    entrada: String,
    redirecionado: bool,
    cancel: CancellationToken,
) -> anyhow::Result<()> {
    let inicio = std::time::Instant::now();
    // `mut`: a configuração pode mudar durante o turno (/model, /permission,
    // /mode); reler a cada iteração mantém o turno consistente com a escolha
    // atual do usuário em vez de congelar no início.
    let mut cfg = comp.config();
    // `mut`: a compactação de contexto troca a sessão ativa no meio do turno.
    let mut sessao = comp.sessao();
    // Contabilidade discreta exibida no fim do turno: só duração, velocidade
    // aproximada de geração, modelo e modo. Nada de arquivos/ferramentas.
    let mut tokens_gerados_turno: u64 = 0;
    let modelo_turno = cfg.provider.model.clone();
    let modo_turno = cfg.agent.modo;
    comp.evento(AgentEvent::TurnoIniciado);
    comp.evento(AgentEvent::SessaoAtiva {
        id: sessao.id.clone(),
        titulo: sessao.titulo.clone(),
        workdir: cfg.workdir().display().to_string(),
    });
    comp.evento(AgentEvent::ModeloAtual {
        modelo: cfg.provider.model.clone(),
        esforco: cfg.provider.reasoning_effort.clone(),
    });

    let texto = if redirecionado {
        format!("[redirecionamento]\n{entrada}")
    } else {
        entrada
    };
    comp.store
        .append_message(&sessao.id, "user", &texto, None, None, None, None)?;
    if sessao.objetivo.is_empty() {
        comp.store
            .update_session_objective(&sessao.id, &crate::util::truncate(&texto, 200))
            .ok();
    }

    let mut historico = carregar_historico(&comp, &sessao.id, cfg.agent.max_history_messages);
    let mut recuperacao = recovery::Recuperacao::novo(
        cfg.agent.recovery_max_attempts,
        Duration::from_secs(cfg.agent.recovery_timeout_secs),
    );
    let mut dossie_ativo: Option<String> = None;
    // Contador informativo (vai para o estado da interface). Não há mais
    // limite de iterações: o agente segue até terminar ou ser interrompido.
    let mut iteracoes = 0u32;
    let mut malformadas_seguidas = 0u32;
    // Quantas chamadas da resposta atual eram malformadas (a lista é movida
    // para o histórico antes da execução das válidas).
    let mut malformadas_contagem: usize;
    // Respostas interrompidas por `finish_reason: "length"`: tenta continuar
    // algumas vezes antes de encerrar o turno como incompleto.
    let mut continuacoes_truncadas = 0u32;
    // Uma compactação que não reduza o contexto abaixo do orçamento não pode
    // recomeçar o laço indefinidamente (criando sessões de continuação vazias).
    let mut compactou_sem_chamada = false;
    // Resumo de encerramento quando o orçamento de recuperação estoura no
    // caminho misto (chamadas válidas + malformadas na mesma resposta): o
    // turno só termina depois de todos os resultados irem para o histórico.
    let mut encerrar_malformada: Option<String> = None;

    let resultado = 'turno: loop {
        if cancel.is_cancelled() {
            break ResultadoTurno {
                resumo:
                    "Tarefa interrompida pelo usuário. As alterações já feitas foram preservadas."
                        .into(),
                concluido: false,
            };
        }
        // Sem limite de iterações: o agente segue até concluir a tarefa, ser
        // interrompido pelo usuário ou a compactação de contexto não conseguir
        // mais manter a conversa. Um teto fixo só fazia o trabalho parar no
        // meio e exigir que o usuario reenviasse a instrução — e iterações
        // longas são justamente o caso normal de uma tarefa grande.
        cfg = comp.config();
        iteracoes += 1;
        comp.estado_ui(format!("trabalhando… {iteracoes}ª interação"));

        // Orçamento de contexto.
        let sistema = montar_prompt(&comp, &cfg, dossie_ativo.as_deref());
        let ferramentas = comp.registro.read().await.specs_do_modo(cfg.agent.modo);
        let usados =
            context::estimar_prompt(&cfg.provider.model, &sistema, &historico, &ferramentas);
        let orcamento = context::OrcamentoContexto::novo(
            cfg.context_window(),
            cfg.agent.reserve_response_tokens,
        );
        comp.evento(AgentEvent::Contexto {
            usados,
            // A interface mostra a janela total do modelo (1M, 128k etc.),
            // não o orçamento útil da compactação.
            capacidade: cfg.context_window(),
        });
        if orcamento.precisa_compactar(usados, cfg.agent.context_compact_threshold) {
            if compactou_sem_chamada {
                comp.evento(AgentEvent::Erro {
                    mensagem: "a compactação de contexto não conseguiu reduzir o histórico abaixo do orçamento (prompt de sistema, ferramentas e dossiê podem ser grandes demais).".into(),
                    dica: Some(
                        "reduza o número de ferramentas MCP, o tamanho do projeto ou reinicie com /nova"
                            .into(),
                    ),
                    recuperavel: true,
                });
                break ResultadoTurno {
                    resumo: "Turno interrompido: o contexto não cabe na janela do modelo mesmo após a compactação.".into(),
                    concluido: false,
                };
            }
            compactou_sem_chamada = true;
            comp.estado_ui("compactando contexto (dossiê de continuidade)");
            match compactar_contexto(&comp, &cfg, &historico, &cancel) {
                Ok((dossie, novos)) => {
                    dossie_ativo = Some(dossie);
                    historico = novos;
                    // A compactação criou uma sessão de continuação e a tornou
                    // ativa: sem reler aqui, o resto do turno seguiria gravando
                    // mensagens, ferramentas e status na sessão antiga (que
                    // voltaria a aparecer como "ativa" no banco).
                    sessao = comp.sessao();
                    continue;
                }
                Err(e) => {
                    // A compactação pode ter criado a sessão de continuação e
                    // falhado logo depois (cancelamento). Relê o estado real
                    // antes de encerrar para não finalizar o turno na sessão
                    // antiga.
                    sessao = comp.sessao();
                    comp.evento(AgentEvent::Erro {
                        mensagem: format!("falha ao compactar o contexto: {e}"),
                        dica: Some(
                            "o histórico original permanece salvo; você pode reiniciar com /nova"
                                .into(),
                        ),
                        recuperavel: true,
                    });
                    // Sem este encerramento, a próxima iteração tentaria
                    // compactar de novo indefinidamente com o mesmo contexto.
                    break ResultadoTurno {
                        resumo: format!("Turno interrompido: falha ao compactar o contexto: {e}"),
                        concluido: false,
                    };
                }
            }
        }
        compactou_sem_chamada = false;

        comp.estado_ui("consultando o modelo");
        let mut req = ChatRequest::new(
            cfg.provider.model.clone(),
            montar_mensagens(&sistema, &historico),
        );
        req.tools = ferramentas.clone();
        req.stream = cfg.provider.stream;
        req.temperature = cfg.provider.temperature;
        req.max_tokens = cfg.provider.max_tokens;
        req.reasoning_effort = cfg.provider.reasoning_effort.clone();

        let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<StreamEvent>();
        let chamada = chamar_com_fallback(&comp, &req, &cfg, &cancel, Some(delta_tx));
        tokio::pin!(chamada);

        let completion = loop {
            tokio::select! {
                _ = cancel.cancelled() => {
                    break None;
                }
                resultado = &mut chamada => {
                    break Some(resultado);
                }
                Some(evento) = delta_rx.recv() => {
                    match evento {
                        StreamEvent::Texto(delta) => comp.evento(AgentEvent::Texto { delta }),
                        StreamEvent::Raciocinio(delta) => {
                            if cfg.ui.show_reasoning {
                                comp.evento(AgentEvent::Raciocinio { delta });
                            }
                        }
                        StreamEvent::ToolCall { name, .. } => {
                            if let Some(nome) = name {
                                comp.evento(AgentEvent::Estado { mensagem: format!("preparando ferramenta {nome}") });
                            }
                        }
                    }
                }
            }
        };

        let Some(resultado) = completion else {
            break ResultadoTurno {
                resumo:
                    "Tarefa interrompida pelo usuário. As alterações já feitas foram preservadas."
                        .into(),
                concluido: false,
            };
        };

        let completion = match resultado {
            Ok((c, modelo_usado)) => {
                // Contabilidade real: o usage vem do provedor e era descartado.
                if let Some(uso) = c.usage {
                    if modelo_usado == cfg.provider.model {
                        crate::tokenizer::calibrar(&modelo_usado, uso.prompt_tokens, usados);
                    }
                    comp.store
                        .record_usage(
                            &sessao.id,
                            uso.prompt_tokens,
                            uso.completion_tokens,
                            uso.reasoning_tokens,
                        )
                        .ok();
                    tokens_gerados_turno += tokens_gerados(&uso);
                    comp.evento(AgentEvent::Tokens {
                        prompt: uso.prompt_tokens,
                        completion: uso.completion_tokens,
                        raciocinio: uso.reasoning_tokens,
                    });
                }
                c
            }
            Err(erro) => {
                let transitorio = erro.transitorio();
                let classe = if erro.resposta_invalida() {
                    recovery::Classificacao::RespostaMalformada
                } else {
                    recovery::classificar(None, &erro.to_string(), transitorio)
                };
                let diag = recuperacao.diagnosticar(classe);
                comp.store
                    .record_decision(
                        &sessao.id,
                        &format!("erro/{}", classe.as_storage()),
                        &format!("{}: {}", classe.label(), erro),
                        None,
                    )
                    .ok();
                comp.evento(AgentEvent::Erro {
                    mensagem: erro.mensagem_util(),
                    dica: Some(if transitorio {
                        "falha transitória: o estado da tarefa foi preservado; envie uma nova instrução para continuar.".into()
                    } else if diag.sugestao.is_empty() {
                        "verifique a configuração do provedor em /model".to_string()
                    } else {
                        diag.sugestao
                    }),
                    recuperavel: transitorio || !matches!(erro, ProviderError::ChaveAusente(_)),
                });
                break ResultadoTurno {
                    resumo: format!("Turno interrompido por erro do provedor: {erro}"),
                    concluido: false,
                };
            }
        };

        let tool_calls = completion.tool_calls.clone();
        let texto_assistente = completion.text.clone();
        // O provedor exige que `arguments` seja JSON válido. Modelos (e a
        // NVIDIA) às vezes devolvem texto quebrado; a cópia segura preserva o
        // protocolo, enquanto a original ainda serve para diagnóstico.
        let chamadas_seguras = chamadas_para_historico(&tool_calls);

        let chamadas_json = if chamadas_seguras.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&chamadas_seguras)?)
        };
        comp.store.append_message(
            &sessao.id,
            "assistant",
            &texto_assistente,
            chamadas_json.as_deref(),
            None,
            None,
            if completion.reasoning.is_empty() {
                None
            } else {
                Some(&completion.reasoning)
            },
        )?;
        if !texto_assistente.trim().is_empty() {
            comp.evento(AgentEvent::MensagemFinal {
                conteudo: texto_assistente.clone(),
            });
        }
        historico.push(ChatMessage::assistant_with_calls(
            texto_assistente.clone(),
            chamadas_seguras.clone(),
        ));

        if tool_calls.is_empty() {
            // `length` significa que o modelo foi cortado pelo limite de
            // tokens: tratar como resposta concluída entregava um texto pela
            // metade como sucesso.
            match completion.finish_reason.as_str() {
                "length" if continuacoes_truncadas < 2 => {
                    continuacoes_truncadas += 1;
                    let aviso = "[sistema] A resposta anterior foi truncada pelo limite de tokens. \
                                 Continue exatamente de onde parou, sem repetir o que já foi escrito.";
                    comp.evento(AgentEvent::Erro {
                        mensagem:
                            "a resposta foi truncada pelo limite de tokens; pedindo continuação"
                                .into(),
                        dica: Some(
                            "aumente `max_tokens` na configuração se isso se repetir".into(),
                        ),
                        recuperavel: true,
                    });
                    comp.store
                        .append_message(&sessao.id, "user", aviso, None, None, None, None)?;
                    historico.push(ChatMessage::user(aviso));
                    continue;
                }
                "length" => {
                    comp.evento(AgentEvent::Erro {
                        mensagem: "a resposta continuou sendo truncada pelo limite de tokens"
                            .into(),
                        dica: Some(
                            "aumente `max_tokens` na configuração ou peça uma resposta menor"
                                .into(),
                        ),
                        recuperavel: true,
                    });
                    break ResultadoTurno {
                        resumo: "Turno encerrado: resposta truncada pelo limite de tokens.".into(),
                        concluido: false,
                    };
                }
                "content_filter" => {
                    comp.evento(AgentEvent::Erro {
                        mensagem: "o provedor interrompeu a resposta por filtro de conteúdo".into(),
                        dica: Some("reformule a instrução".into()),
                        recuperavel: true,
                    });
                    break ResultadoTurno {
                        resumo: "Turno encerrado: resposta bloqueada pelo filtro de conteúdo do provedor."
                            .into(),
                        concluido: false,
                    };
                }
                _ => {}
            }
            recuperacao.limpar();
            break ResultadoTurno {
                resumo: resumo_turno(&comp, &sessao.id),
                concluido: true,
            };
        }

        // Valida os argumentos de todas as chamadas antes de executar qualquer uma.
        if completion.finish_reason != "length" {
            continuacoes_truncadas = 0;
        }
        let (validas, malformadas) = separar_chamadas(tool_calls);
        // Chamadas malformadas gastam o orçamento de recuperação em QUALQUER
        // caminho — inclusive quando a mesma resposta também traz chamadas
        // válidas. Antes só o ramo "todas malformadas" registrava tentativa: no
        // ramo misto `malformadas_seguidas` era zerada e nada era contabilizado,
        // então um modelo que estoura `max_tokens` no meio dos argumentos
        // repetia a mesma resposta para sempre (a chamada válida executava, o
        // erro de JSON voltava no histórico e o ciclo recomeçava) — sem
        // `AgentEvent::Erro`, gastando tokens até o usuário apertar Esc.
        if !malformadas.is_empty() {
            malformadas_seguidas += 1;
            malformadas_contagem = malformadas.len();
            let classe = recovery::Classificacao::RespostaMalformada;
            // Primeiro os `tool` results das chamadas malformadas: a API exige
            // um resultado para cada `tool_call` já gravado.
            registrar_malformadas(&comp, &sessao, &mut historico, malformadas)?;
            if validas.is_empty() {
                let diag = recuperacao.diagnosticar(classe);
                comp.store.append_message(
                    &sessao.id,
                    "user",
                    "[sistema] A resposta anterior continha chamadas de ferramenta malformadas. Responda novamente com JSON válido.",
                    None,
                    None,
                    None,
                    None,
                )?;
                historico.push(ChatMessage::user(
                    "[sistema] As chamadas de ferramenta anteriores estavam malformadas (JSON inválido). \
                     Rejeite-as e produza novas chamadas com argumentos JSON válidos.",
                ));
                if let Err(limite) =
                    recuperacao.registrar_tentativa(classe, "nova resposta solicitada")
                {
                    comp.evento(AgentEvent::Erro {
                        mensagem: format!(
                            "O modelo enviou {} respostas malformadas seguidas ({}). Recuperação automática encerrada.\n\
                             Tentativas: {} em {:.1}s.\nEstratégias tentadas: {}.\n\
                             Próxima ação recomendada: verifique se o modelo selecionado \
                             realmente suporta chamadas de ferramentas (veja /model) ou escolha outro modelo.",
                            malformadas_seguidas,
                            limite.classificacao.label(),
                            limite.tentativas,
                            limite.tempo_gasto.as_secs_f64(),
                            if limite.estrategias.is_empty() { "(nenhuma)".to_string() } else { limite.estrategias.join("; ") }
                        ),
                        dica: Some("use /model para ver as capacidades conhecidas de cada modelo".into()),
                        recuperavel: false,
                    });
                    break ResultadoTurno {
                        resumo: "Encerrado por respostas malformadas do modelo.".into(),
                        concluido: false,
                    };
                }
                let _ = diag;
                continue;
            }
            // Ramo misto: as chamadas válidas rodam normalmente e o erro das
            // malformadas já está no histórico; a tentativa é contabilizada e,
            // ao estourar o orçamento, o turno acaba (depois de persistir os
            // resultados das chamadas válidas).
            if let Err(limite) = recuperacao
                .registrar_tentativa(classe, "chamada malformada junto de chamada válida")
            {
                comp.evento(AgentEvent::Erro {
                    mensagem: format!(
                        "O modelo enviou {} respostas com chamadas malformadas ({}). Recuperação automática encerrada.\n\
                         Tentativas: {} em {:.1}s.\nEstratégias tentadas: {}.\n\
                         Próxima ação recomendada: verifique se o modelo selecionado \
                         realmente suporta chamadas de ferramentas (veja /model) ou escolha outro modelo.",
                        malformadas_seguidas,
                        limite.classificacao.label(),
                        limite.tentativas,
                        limite.tempo_gasto.as_secs_f64(),
                        if limite.estrategias.is_empty() { "(nenhuma)".to_string() } else { limite.estrategias.join("; ") }
                    ),
                    dica: Some("use /model para ver as capacidades conhecidas de cada modelo".into()),
                    recuperavel: false,
                });
                encerrar_malformada = Some(format!(
                    "Recuperação automática encerrada para {} ({} tentativas em {:.1}s).",
                    limite.classificacao.label(),
                    limite.tentativas,
                    limite.tempo_gasto.as_secs_f64()
                ));
            }
        } else {
            malformadas_seguidas = 0;
            malformadas_contagem = 0;
        }

        // Executa as chamadas. O contexto é recriado aqui (e não no início do
        // turno) para refletir sessão ativa, nível, modo e raízes atuais —
        // inclusive depois de uma compactação ou de /permission no meio do turno.
        let total_chamadas = validas.len() + malformadas_contagem;
        let mut ctx_ferramentas = comp.contexto_ferramentas(cancel.clone());
        ctx_ferramentas.subagente = Some(Compartilhado::novo_subagente(&comp));
        comp.estado_ui(format!("executando {total_chamadas} ferramenta(s)"));
        let resultados =
            executar_chamadas(&comp, &ctx_ferramentas, &validas, &mut recuperacao).await;

        // Todos os resultados são persistidos e devolvidos ao histórico antes de
        // qualquer encerramento: as ferramentas já rodaram (arquivos escritos,
        // comandos executados) e,pular os tool_results deixaria a mensagem do
        // assistente com tool_calls sem resposta — histórico inválido para a API.
        let mut encerrar: Option<String> = None;
        for (call, resultado) in resultados {
            let (sucesso, conteudo, resumo, adicoes, remocoes) = resultado;
            // Nunca persiste nem devolve ao modelo segredos capturados de saídas.
            let conteudo = crate::util::redact_secrets(&conteudo);
            if !sucesso {
                let classe = recovery::classificar(Some(&call.name), &conteudo, false);
                let diag = recuperacao.diagnosticar(classe);
                if diag.restantes == 0 || diag.expirado {
                    comp.evento(AgentEvent::Erro {
                        mensagem: format!(
                            "Limite de recuperação automática atingido para {}.\n\n\
                             Problema: {}\n\n\
                             Tentativas realizadas: {}\n\
                             Estratégias tentadas: {}\n\
                             Tempo gasto: {:.1}s de {:.0}s\n\n\
                             Próxima ação recomendada: {}",
                            diag.classificacao.label(),
                            crate::util::truncate(&conteudo, 600),
                            diag.tentativas,
                            if diag.estrategias_usadas.is_empty() { "(nenhuma)".into() } else { diag.estrategias_usadas.join("; ") },
                            (cfg.agent.recovery_timeout_secs as f64) - diag.tempo_restante.as_secs_f64(),
                            cfg.agent.recovery_timeout_secs,
                            diag.sugestao
                        ),
                        dica: Some("as alterações já feitas foram preservadas; revise o diagnóstico e decida como prosseguir".into()),
                        recuperavel: false,
                    });
                    comp.store
                        .record_decision(
                            &sessao.id,
                            "limite_recuperacao",
                            &crate::util::truncate(&conteudo, 400),
                            None,
                        )
                        .ok();
                    encerrar = Some(format!(
                        "Recuperação automática encerrada para {} ({} tentativas em {:.1}s).",
                        diag.classificacao.label(),
                        diag.tentativas,
                        (cfg.agent.recovery_timeout_secs as f64)
                            - diag.tempo_restante.as_secs_f64()
                    ));
                }
            }
            comp.store.append_message(
                &sessao.id,
                "tool",
                &conteudo,
                None,
                Some(&call.id),
                Some(&call.name),
                None,
            )?;
            historico.push(ChatMessage::tool_result(
                call.id.clone(),
                call.name.clone(),
                conteudo.clone(),
            ));
            comp.evento(AgentEvent::FerramentaConcluida {
                id: Some(call.id.clone()),
                nome: call.name.clone(),
                sucesso,
                resumo: crate::util::redact_secrets(&resumo),
                adicoes,
                remocoes,
                diff: diff_para_interface(&conteudo, adicoes, remocoes),
            });
        }

        // As chamadas malformadas desta resposta já foram registradas antes da
        // execução (a API exige um resultado para cada `tool_call`).

        // Só depois de persistir e devolver todos os resultados é que o turno
        // termina — nunca no meio do laço.
        if let Some(resumo) = encerrar_malformada {
            break 'turno ResultadoTurno {
                resumo,
                concluido: false,
            };
        }

        if let Some(resumo) = encerrar {
            break 'turno ResultadoTurno {
                resumo,
                concluido: false,
            };
        }

        if cancel.is_cancelled() {
            break ResultadoTurno {
                resumo:
                    "Tarefa interrompida pelo usuário. As alterações já feitas foram preservadas."
                        .into(),
                concluido: false,
            };
        }
    };

    // Finalização do turno.
    comp.store.touch_session(&sessao.id).ok();
    let duracao = inicio.elapsed();
    if resultado.concluido {
        // Se o aplicativo foi encerrado (/quit), o status `pausada` gravado
        // pelo encerramento não pode ser sobrescrito pelo turno.
        if !comp.encerrando.load(Ordering::SeqCst) {
            comp.store.update_session_status(&sessao.id, "ativa").ok();
        }
        let segundos = duracao.as_secs_f64().max(0.001);
        let velocidade = tokens_gerados_turno as f64 / segundos;
        comp.evento(AgentEvent::TurnoConcluido {
            resumo: format!(
                "{} · {:.1} tok/s · {} · {}",
                crate::util::format_duration_ms(duracao.as_millis() as u64),
                velocidade,
                crate::util::truncate(&modelo_turno, 32),
                modo_turno.label()
            ),
        });
    } else if cancel.is_cancelled() {
        if !comp.encerrando.load(Ordering::SeqCst) {
            comp.store
                .update_session_status(&sessao.id, "interrompida")
                .ok();
        }
        comp.evento(AgentEvent::Interrompido {
            resumo: resultado.resumo,
        });
    } else {
        comp.evento(AgentEvent::TurnoConcluido {
            resumo: resultado.resumo,
        });
    }
    Ok(())
}

/// Monta o prompt de sistema com o estado atual.
fn montar_prompt(comp: &Compartilhado, cfg: &Config, dossie: Option<&str>) -> String {
    let sessao = comp.sessao();
    let projeto = comp.projeto();
    let skills = comp.skills();
    let plano = comp
        .store
        .latest_plan(&sessao.id)
        .ok()
        .flatten()
        .map(|(objetivo, passos, _)| {
            let lista: Vec<PlanoPasso> = serde_json::from_str::<Vec<Value>>(&passos)
                .unwrap_or_default()
                .into_iter()
                .filter_map(|v| {
                    Some(PlanoPasso {
                        titulo: v.get("titulo")?.as_str()?.to_string(),
                        status: v
                            .get("status")
                            .and_then(|s| s.as_str())
                            .map(StatusPasso::parse)
                            .unwrap_or(StatusPasso::Pendente),
                        observacao: v
                            .get("observacao")
                            .and_then(|o| o.as_str())
                            .map(|s| s.to_string()),
                    })
                })
                .collect();
            (objetivo, lista)
        });
    let tarefas: Vec<TarefaView> = comp
        .store
        .tasks(&sessao.id)
        .unwrap_or_default()
        .into_iter()
        .map(|t| TarefaView {
            id: t.id,
            titulo: t.title,
            status: StatusPasso::parse(&t.status),
            dependencias: t.depends_on,
            resultado: t.result,
        })
        .collect();
    let workdir = cfg.workdir();
    // A personalidade é carregada do disco a cada turno: editar o arquivo
    // (ou remover o Comp) vale imediatamente, sem reiniciar o Kestrel.
    let personalidade = cfg
        .ui
        .active_comp
        .as_deref()
        .and_then(|nome| comps::encontrar(&comp.paths.comps_dir(), nome).map(|(comp, _)| comp));
    prompt::construir(&prompt::PromptContext {
        projeto: &projeto,
        skills: &skills,
        comp: personalidade.as_ref(),
        nivel: cfg.agent.permission,
        modo: cfg.agent.modo,
        esforco: cfg.provider.reasoning_effort.as_deref(),
        plano,
        tarefas,
        dossie: dossie.map(|d| d.to_string()),
        workdir: &workdir,
        diretorios_extra: cfg
            .agent
            .external_dirs
            .iter()
            .map(|d| d.display().to_string())
            .collect(),
    })
}

fn montar_mensagens(sistema: &str, historico: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut mensagens = Vec::with_capacity(historico.len() + 1);
    mensagens.push(ChatMessage::system(sistema));
    mensagens.extend(historico.iter().cloned());
    mensagens
}

/// Carrega o histórico recente do banco, preservando pares de chamadas.
fn carregar_historico(comp: &Compartilhado, sessao: &str, limite: usize) -> Vec<ChatMessage> {
    let linhas = comp
        .store
        .recent_messages(sessao, limite)
        .unwrap_or_default();
    let mut mensagens = Vec::new();
    for linha in linhas {
        match linha.role.as_str() {
            "user" => mensagens.push(ChatMessage::user(linha.content)),
            "assistant" => {
                let mut mensagem = if let Some(json) = &linha.tool_calls {
                    let calls: Vec<ToolCall> = serde_json::from_str(json).unwrap_or_default();
                    // Um histórico antigo (ou um modelo que mandou JSON
                    // quebrado) não pode derrubar o próximo turno com 400.
                    let calls = chamadas_para_historico(&calls);
                    ChatMessage::assistant_with_calls(linha.content, calls)
                } else {
                    ChatMessage::assistant(linha.content)
                };
                mensagem.tool_call_id = None;
                mensagens.push(mensagem);
            }
            "tool" => mensagens.push(ChatMessage::tool_result(
                linha.tool_call_id.unwrap_or_default(),
                linha.tool_name.unwrap_or_default(),
                linha.content,
            )),
            _ => {}
        }
    }
    // Fecha pares `assistant(tool_calls)` / `tool`: a API rejeita um
    // `tool_call` sem resultado (turno interrompido no meio, histórico cortado
    // no limite ou chamada nunca respondida). Resultados órfãos são descartados.
    let mut resultado: Vec<ChatMessage> = Vec::with_capacity(mensagens.len());
    let mut pendentes: Vec<(String, String)> = Vec::new();
    fn resolver_pendentes(resultado: &mut Vec<ChatMessage>, pendentes: &mut Vec<(String, String)>) {
        for (id, nome) in pendentes.drain(..) {
            resultado.push(ChatMessage::tool_result(
                id,
                nome,
                "ERRO: resultado da ferramenta perdido (turno interrompido); reexecute se necessário.",
            ));
        }
    }
    for m in mensagens {
        match m.role {
            crate::provider::Role::Assistant => {
                resolver_pendentes(&mut resultado, &mut pendentes);
                pendentes = m
                    .tool_calls
                    .iter()
                    .map(|c| (c.id.clone(), c.name.clone()))
                    .collect();
                resultado.push(m);
            }
            crate::provider::Role::Tool => {
                let id = m.tool_call_id.clone().unwrap_or_default();
                if pendentes.iter().any(|(p, _)| *p == id) {
                    pendentes.retain(|(p, _)| *p != id);
                    resultado.push(m);
                }
            }
            _ => {
                resolver_pendentes(&mut resultado, &mut pendentes);
                resultado.push(m);
            }
        }
    }
    resolver_pendentes(&mut resultado, &mut pendentes);
    resultado
}

/// Compacta o contexto: gera o dossiê e cria uma nova sessão de continuação.
/// Modelos a tentar numa chamada, na ordem: o principal e depois os fallbacks
/// configurados, sem repetições nem entradas vazias.
fn modelos_de_chamada(cfg: &Config) -> Vec<String> {
    let mut modelos = Vec::new();
    if !cfg.provider.model.trim().is_empty() {
        modelos.push(cfg.provider.model.trim().to_string());
    }
    for modelo in &cfg.provider.fallback_models {
        let modelo = modelo.trim();
        if !modelo.is_empty() && !modelos.iter().any(|m| m == modelo) {
            modelos.push(modelo.to_string());
        }
    }
    modelos
}

/// Chama o provedor com fallback entre modelos.
///
/// O primeiro modelo mantém o streaming ao vivo. Se ele falhar de um jeito que
/// outro modelo pode resolver (indisponível, limite, rede, 5xx), as tentativas
/// seguintes rodam sem `sink`: o texto parcial não se mistura e a resposta
/// final substitui o bloco por inteiro. O modelo escolhido no disco não muda.
async fn chamar_com_fallback(
    comp: &Arc<Compartilhado>,
    req: &ChatRequest,
    cfg: &Config,
    cancel: &CancellationToken,
    sink: Option<mpsc::UnboundedSender<StreamEvent>>,
) -> Result<(Completion, String), ProviderError> {
    let modelos = modelos_de_chamada(cfg);
    let provider = comp.provider();
    let mut ultimo: Option<ProviderError> = None;
    for (indice, modelo) in modelos.iter().enumerate() {
        if indice > 0 {
            let anterior = &modelos[indice - 1];
            comp.evento(AgentEvent::Aviso {
                mensagem: format!(
                    "modelo `{anterior}` falhou; tentando fallback `{modelo}` para este turno"
                ),
            });
            comp.store
                .record_decision(
                    &comp.sessao().id,
                    "fallback/modelo",
                    &format!("{anterior} → {modelo}"),
                    None,
                )
                .ok();
        }
        let mut tentativa = req.clone();
        tentativa.model = modelo.clone();
        let sink_tentativa = if indice == 0 { sink.clone() } else { None };
        match provider
            .chat(tentativa, cancel.clone(), sink_tentativa)
            .await
        {
            Ok(completion) => return Ok((completion, modelo.clone())),
            Err(erro) if erro.permite_fallback() && indice + 1 < modelos.len() => {
                ultimo = Some(erro);
            }
            Err(erro) => return Err(erro),
        }
    }
    Err(ultimo.unwrap_or_else(|| ProviderError::ModeloIndisponivel(cfg.provider.model.clone())))
}

fn compactar_contexto(
    comp: &Compartilhado,
    cfg: &Config,
    historico: &[ChatMessage],
    cancel: &CancellationToken,
) -> anyhow::Result<(String, Vec<ChatMessage>)> {
    let sessao = comp.sessao();
    let projeto = comp.projeto();
    let ponto = historico
        .iter()
        .rev()
        .find(|m| m.role == crate::provider::Role::Assistant && !m.content.trim().is_empty())
        .map(|m| crate::util::truncate(m.content.trim(), 400))
        .unwrap_or_else(|| "retomada após compactação de contexto".into());
    let dossie = context::construir_dossie(
        &comp.store,
        &sessao.id,
        &projeto.summary(),
        &ponto,
        "continuar a tarefa a partir do ponto de interrupção, validando o estado real do projeto",
    )?;
    let texto_dossie = dossie.para_texto();

    let novo_id = uuid::Uuid::new_v4().to_string();
    let titulo = format!("continuação: {}", crate::util::truncate(&sessao.titulo, 50));
    comp.store.create_session(
        &novo_id,
        &cfg.workdir().display().to_string(),
        &titulo,
        &cfg.provider.model,
        cfg.provider.reasoning_effort.as_deref(),
        cfg.agent.permission.label(),
        Some(&sessao.id),
    )?;
    comp.store
        .update_session_status(&sessao.id, "compactada")
        .ok();
    comp.store.set_context_state(
        &sessao.id,
        context::estimar_prompt(&cfg.provider.model, "", historico, &[]),
        cfg.context_window(),
        Some(&texto_dossie),
    )?;
    comp.store.append_message(
        &novo_id,
        "user",
        &format!("Continuação de sessão após atingir o limite de contexto.\n\n{texto_dossie}"),
        None,
        None,
        None,
        None,
    )?;

    comp.definir_sessao(EstadoSessao {
        id: novo_id.clone(),
        titulo: titulo.clone(),
        objetivo: dossie.objetivo.clone(),
    });
    comp.evento(AgentEvent::DossieCriado {
        resumo: dossie.resumo(),
        nova_sessao: novo_id.clone(),
    });
    comp.evento(AgentEvent::SessaoAtiva {
        id: novo_id.clone(),
        titulo,
        workdir: cfg.workdir().display().to_string(),
    });

    if cancel.is_cancelled() {
        anyhow::bail!("cancelado durante a compactação");
    }
    // Mantém as últimas mensagens de conversa (sem tool calls) junto do
    // dossiê: o resumo determinístico preserva o estado do projeto, mas perde
    // o fio imediato do diálogo. Quatro mensagens curtas dão continuidade sem
    // devolver o contexto que acabou de ser compactado.
    let recentes: Vec<ChatMessage> = historico
        .iter()
        .rev()
        .filter(|m| {
            matches!(
                m.role,
                crate::provider::Role::User | crate::provider::Role::Assistant
            ) && m.tool_calls.is_empty()
                && !m.content.trim().is_empty()
        })
        .take(4)
        .cloned()
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut novos = vec![ChatMessage::user(format!(
        "Continue o trabalho a partir do dossiê a seguir. Confirme o estado real do projeto antes de agir.\n\n{texto_dossie}"
    ))];
    novos.extend(recentes);
    Ok((texto_dossie.clone(), novos))
}

/// Separa chamadas válidas das malformadas.
/// Cópia das chamadas pronta para voltar ao provedor: os argumentos inválidos
/// do modelo não podem ser reenviar como texto quebrado (a API responde 400).
fn chamadas_para_historico(chamadas: &[ToolCall]) -> Vec<ToolCall> {
    chamadas
        .iter()
        .map(ToolCall::com_argumentos_validos)
        .collect()
}

fn separar_chamadas(chamadas: Vec<ToolCall>) -> (Vec<ToolCall>, Vec<(ToolCall, String)>) {
    let mut validas = Vec::new();
    let mut malformadas = Vec::new();
    for chamada in chamadas {
        if chamada.arguments.trim().is_empty() {
            malformadas.push((
                chamada,
                "argumentos vazios: envie um objeto JSON com os parâmetros".to_string(),
            ));
            continue;
        }
        match serde_json::from_str::<Value>(&chamada.arguments) {
            Ok(Value::Object(_)) => validas.push(chamada),
            Ok(_) => malformadas.push((
                chamada,
                "os argumentos devem ser um objeto JSON".to_string(),
            )),
            Err(e) => malformadas.push((chamada, format!("JSON inválido nos argumentos: {e}"))),
        }
    }
    (validas, malformadas)
}

/// Decide se as chamadas podem rodar em paralelo.
///
/// Só paraleliza operações de leitura, ou escritas em caminhos distintos;
/// duas chamadas que tocam o mesmo caminho (ou ferramentas sensíveis) são
/// executadas em sequência, na ordem pedida pelo modelo.
fn pode_paralelizar(chamadas: &[ToolCall], workdir: &std::path::Path) -> bool {
    if chamadas.len() < 2 {
        return false;
    }
    // Comparar caminhos crus deixava `a.rs` e `./a.rs` (ou o caminho absoluto)
    // passarem como alvos distintos e rodarem juntos.
    let normalizar = |caminho: &str| {
        let caminho = std::path::Path::new(caminho);
        let absoluto = if caminho.is_absolute() {
            caminho.to_path_buf()
        } else {
            workdir.join(caminho)
        };
        crate::util::normalize_path(&absoluto)
            .to_string_lossy()
            .into_owned()
    };
    let tem_escrita = chamadas
        .iter()
        .any(|c| crate::perm::tool_risk(&c.name) == ToolRisk::Escrita);
    let mut caminhos = HashSet::new();
    for chamada in chamadas {
        let risco = crate::perm::tool_risk(&chamada.name);
        if risco == ToolRisk::Sensivel {
            return false;
        }
        match caminho_da_chamada(chamada) {
            Some(caminho) => {
                if !caminhos.insert(normalizar(&caminho)) {
                    return false;
                }
            }
            None => {
                // Sem caminho explícito, só leituras são seguras em paralelo —
                // e nunca junto de uma escrita, porque ferramentas como
                // `git_diff` observam o repositório inteiro.
                if risco == ToolRisk::Escrita || (tem_escrita && risco == ToolRisk::Leitura) {
                    return false;
                }
            }
        }
    }
    true
}

/// Extrai o caminho alvo de uma chamada, quando declarado nos argumentos.
fn caminho_da_chamada(chamada: &ToolCall) -> Option<String> {
    serde_json::from_str::<Value>(&chamada.arguments)
        .ok()
        .and_then(|v| {
            v.get("path")
                .or_else(|| v.get("caminho"))
                .and_then(|p| p.as_str())
                .map(|s| s.to_string())
        })
}

/// Executa as chamadas de ferramenta, em paralelo quando seguro.
async fn executar_chamadas(
    comp: &Arc<Compartilhado>,
    ctx: &ToolContext,
    chamadas: &[ToolCall],
    recuperacao: &mut recovery::Recuperacao,
) -> Vec<(ToolCall, ResultadoFerramenta)> {
    for chamada in chamadas {
        comp.evento(AgentEvent::FerramentaIniciada {
            id: Some(chamada.id.clone()),
            nome: chamada.name.clone(),
            detalhe: detalhe_chamada(chamada),
            subagente: ctx.subagente_tarefa.clone(),
        });
    }

    let paralelo = pode_paralelizar(chamadas, &ctx.workdir);
    let mut resultados: Vec<(ToolCall, ResultadoFerramenta)> = Vec::new();

    if paralelo {
        let futuros: Vec<_> = chamadas
            .iter()
            .map(|chamada| {
                let args: Value = serde_json::from_str(&chamada.arguments).unwrap_or(Value::Null);
                let chamada = chamada.clone();
                // O lock de leitura vive dentro do futuro: precisa continuar
                // válido durante a execução da ferramenta.
                let registro = comp.registro.clone();
                let mut ctx = ctx.clone();
                ctx.chamada_id = Some(chamada.id.clone());
                async move {
                    let guarda = registro.read().await;
                    let resultado = guarda.executar(&chamada.name, args, &ctx).await;
                    (chamada, converter(resultado))
                }
            })
            .collect();
        resultados.extend(futures_util::future::join_all(futuros).await);
    } else {
        for chamada in chamadas {
            let args: Value = serde_json::from_str(&chamada.arguments).unwrap_or(Value::Null);
            let mut ctx_local = ctx.clone();
            ctx_local.chamada_id = Some(chamada.id.clone());
            let resultado = comp
                .registro
                .read()
                .await
                .executar(&chamada.name, args, &ctx_local)
                .await;
            resultados.push((chamada.clone(), converter(resultado)));
        }
    }

    // Registra tentativas de correção quando uma ferramenta falha.
    for (chamada, resultado) in &resultados {
        if !resultado.0 {
            let classe = recovery::classificar(Some(&chamada.name), &resultado.1, false);
            let estrategia = format!("{} {}", chamada.name, detalhe_chamada(chamada));
            let maximo = recuperacao.max_tentativas();
            if recuperacao.estrategia_repetida(classe, &estrategia) {
                comp.evento(AgentEvent::Recuperacao {
                    descricao: format!(
                        "a mesma estratégia falhou novamente em {}; o modelo deve adotar uma abordagem diferente",
                        classe.label()
                    ),
                    tentativa: recuperacao.diagnosticar(classe).tentativas,
                    maximo,
                });
            }
            if let Ok(tentativa) = recuperacao.registrar_tentativa(classe, &estrategia) {
                comp.evento(AgentEvent::Recuperacao {
                    descricao: format!("{} — tentativa de correção {tentativa}", classe.label()),
                    tentativa,
                    maximo,
                });
            }
        } else {
            // A mesma chamada que já tinha falhado agora deu certo: o incidente
            // é resolvido pela ESTRATÉGIA, não pela classe. A classe da falha
            // depende da mensagem ("error[E0308]" → `Compilacao`) e uma chamada
            // bem-sucedida não tem mensagem: classificar de novo com "" caía em
            // `ExecucaoFerramenta` e o incidente real sobrevivia — depois de
            // várias falhas corrigidas uma a uma, o turno morria com "Limite de
            // recuperação automática atingido".
            let estrategia = format!("{} {}", chamada.name, detalhe_chamada(chamada));
            recuperacao.resolver_por_estrategia(&estrategia);
        }
    }

    resultados
}

/// (sucesso, conteúdo, resumo, adições, remoções)
type ResultadoFerramenta = (bool, String, String, usize, usize);

fn converter(resultado: Result<crate::tools::ToolOutput, ToolError>) -> ResultadoFerramenta {
    match resultado {
        Ok(saida) => {
            let conteudo = if saida.conteudo.trim().is_empty() {
                saida.resumo.clone()
            } else {
                saida.conteudo.clone()
            };
            (
                saida.sucesso,
                conteudo,
                saida.resumo,
                saida.adicoes,
                saida.remocoes,
            )
        }
        Err(erro) => {
            let texto = match &erro {
                ToolError::Cancelado => "operação cancelada pelo usuário".to_string(),
                outro => format!("ERRO: {outro}"),
            };
            (false, texto, format!("falha: {erro}"), 0, 0)
        }
    }
}

fn detalhe_chamada(chamada: &ToolCall) -> String {
    serde_json::from_str::<Value>(&chamada.arguments)
        .ok()
        .and_then(|v| {
            v.get("path")
                .or_else(|| v.get("caminho"))
                .or_else(|| v.get("comando"))
                .or_else(|| v.get("consulta"))
                .or_else(|| v.get("url"))
                .or_else(|| v.get("objetivo"))
                .or_else(|| v.get("mensagem"))
                .and_then(|p| p.as_str())
                .map(|s| crate::util::truncate(s, 70))
        })
        .unwrap_or_else(|| chamada.name.clone())
}

/// Tokens contados como geração do turno — a base da taxa de tok/s do rodapé.
///
/// Só `completion_tokens`: `reasoning_tokens` é o detalhe de raciocínio já
/// contido dentro dele (o `SessionStats::total_tokens` também soma apenas
/// prompt + completion). Somar os dois inflava a taxa exibida — 75% a mais
/// num modelo que raciocina bastante.
fn tokens_gerados(uso: &crate::provider::Usage) -> u64 {
    u64::from(uso.completion_tokens)
}

/// Conteúdo que a interface pode pintar como diff.
///
/// A interface classifica linha a linha o que chega em `FerramentaConcluida`
/// (`diff`): `+` vira adição, `-` vira remoção. Enviar a saída bruta de
/// qualquer ferramenta fazia `run_command "cargo test"` (com `---- stdout
/// ----`, `failures:` e listas `-`) e `read_file` de YAML aparecerem como
/// remoções e adições de arquivo. Só entra aqui o que é diff de verdade:
/// - a ferramenta declarou linhas alteradas (`com_diff`: escrita/edição/remoção);
/// - ou a saída já é um diff unificado (`git_diff`), que não declara contagem.
fn diff_para_interface(conteudo: &str, adicoes: usize, remocoes: usize) -> String {
    if adicoes > 0 || remocoes > 0 || parece_diff_unificado(conteudo) {
        return crate::util::redact_secrets(conteudo);
    }
    String::new()
}

/// A saída tem a forma de um diff unificado (`diff --git`/bloco `@@`).
fn parece_diff_unificado(conteudo: &str) -> bool {
    conteudo
        .lines()
        .any(|l| l.starts_with("diff --git") || l.starts_with("@@"))
}

/// Resumo exibido no bloco `◇` ao fim do turno.
///
/// Ele **não** repete a resposta do agente: o texto completo já está no bloco
/// acima, e repeti-lo (incompleto, por causa do truncamento) era lido como
/// mensagem duplicada. Sem ferramentas nem arquivos envolvidos, o resumo é
/// apenas a contabilidade real do turno.
fn resumo_turno(comp: &Compartilhado, sessao: &str) -> String {
    let stats = comp.store.session_stats(sessao).unwrap_or_default();
    if stats.files_touched == 0 && stats.tool_runs == 0 {
        return "Turno concluído sem alterações no projeto.".to_string();
    }
    format!(
        "Resumo: {} arquivo(s) alterado(s) (+{} -{} linhas), {} ferramenta(s) executada(s), {} com falha.",
        stats.files_touched, stats.additions, stats.deletions, stats.tool_runs, stats.tool_failures
    )
}

/// Executor de subagentes (usado pela ferramenta `spawn_subagent`).
pub struct ExecutorSubagente {
    comp: Arc<Compartilhado>,
}

#[async_trait::async_trait]
impl SubagentRunner for ExecutorSubagente {
    async fn executar_subagente(
        &self,
        tarefa: &str,
        contexto: &str,
        cancel: CancellationToken,
        eventos: mpsc::UnboundedSender<AgentEvent>,
    ) -> Result<String, String> {
        let resultado = executar_subagente_interno(
            self.comp.clone(),
            tarefa,
            contexto,
            cancel,
            eventos.clone(),
        )
        .await;
        // Um único evento de conclusão, com o resultado real: antes ele era
        // emitido dentro do laço a cada rodada de ferramentas, fechando o grupo
        // como sucesso mesmo quando o subagente ainda trabalhava (ou falhava).
        let _ = eventos.send(AgentEvent::SubagenteConcluido {
            sucesso: resultado.is_ok(),
        });
        resultado
    }
}

/// Persiste e devolve ao histórico os resultados de erro de chamadas
/// malformadas.
///
/// A mensagem do assistente já foi gravada com `tool_calls`; a API exige um
/// `tool` para cada `id`. Sem isto, o caminho "todas malformadas" deixava o
/// histórico inválido e a requisição seguinte falhava com HTTP 400.
fn registrar_malformadas(
    comp: &Compartilhado,
    sessao: &EstadoSessao,
    historico: &mut Vec<ChatMessage>,
    malformadas: Vec<(ToolCall, String)>,
) -> anyhow::Result<()> {
    for (call, erro) in malformadas {
        let erro = crate::util::redact_secrets(&erro);
        comp.store.append_message(
            &sessao.id,
            "tool",
            &erro,
            None,
            Some(&call.id),
            Some(&call.name),
            None,
        )?;
        historico.push(ChatMessage::tool_result(
            call.id.clone(),
            call.name.clone(),
            erro.clone(),
        ));
        comp.evento(AgentEvent::FerramentaConcluida {
            id: Some(call.id),
            nome: call.name,
            sucesso: false,
            resumo: format!("argumentos inválidos: {}", crate::util::truncate(&erro, 80)),
            adicoes: 0,
            remocoes: 0,
            diff: String::new(),
        });
    }
    Ok(())
}

/// Anuncia na interface uma chamada do subagente — uma vez por chamada.
///
/// O `id` é o da chamada do modelo: com ele a conclusão casa o bloco certo
/// mesmo com duas ferramentas de mesmo nome na mesma rodada.
fn anunciar_chamada_subagente(
    eventos: &mpsc::UnboundedSender<AgentEvent>,
    rotulo: &str,
    tarefa: &str,
    chamada: &ToolCall,
) {
    let _ = eventos.send(AgentEvent::FerramentaIniciada {
        id: Some(chamada.id.clone()),
        nome: format!("subagente:{}", chamada.name),
        detalhe: tarefa.to_string(),
        subagente: Some(rotulo.to_string()),
    });
}

/// Ferramentas disponíveis para subagentes (sem planejamento nem recursão).
const FERRAMENTAS_SUBAGENTE: &[&str] = &[
    "list_files",
    "read_file",
    "search_code",
    "inspect_file_metadata",
    "write_file",
    "create_file",
    "edit_file",
    "run_command",
    "run_tests",
    "git_status",
    "git_diff",
    "web_search",
    "fetch_url",
    "list_skills",
    "load_skill",
];

async fn executar_subagente_interno(
    comp: Arc<Compartilhado>,
    tarefa: &str,
    contexto: &str,
    cancel: CancellationToken,
    eventos: mpsc::UnboundedSender<AgentEvent>,
) -> Result<String, String> {
    let cfg = comp.config();
    let projeto = comp.projeto();
    let sistema = format!(
        "Você é um subagente do Kestrel trabalhando em uma subtarefa isolada no diretório {}.\n\
         Regras: responda em português brasileiro; use ferramentas reais; verifique antes de afirmar; \
         não peça confirmações (o nível de permissão é {}); não crie planos extensos.\n\n\
         Projeto:\n{}\n\nSubtarefa:\n{tarefa}\n\nContexto fornecido:\n{contexto}\n\n\
         Ao terminar, devolva um relatório objetivo: o que foi descoberto/feito, arquivos alterados, \
         comandos executados, resultados e pendências.",
        cfg.workdir().display(),
        cfg.agent.permission.label(),
        projeto.summary()
    );
    let mut historico = vec![ChatMessage::user(tarefa.to_string())];
    let ferramentas: Vec<ToolSpec> = comp
        .registro
        .read()
        .await
        .specs()
        .into_iter()
        .filter(|f| FERRAMENTAS_SUBAGENTE.contains(&f.name.as_str()))
        .collect();
    let mut ctx = comp.contexto_ferramentas(cancel.clone());
    let provider = comp.provider();
    let mut relatorio = String::new();
    // Rótulo curto e estável do subagente: aparece em cada bloco de ferramenta
    // dele, para o trabalho delegado ser reconhecível no meio da conversa.
    let rotulo = crate::util::truncate(tarefa, 40);
    ctx.subagente_tarefa = Some(rotulo.clone());
    let _ = eventos.send(AgentEvent::SubagenteIniciado {
        tarefa: tarefa.to_string(),
    });

    let mut esgotou_rodadas = true;
    for _ in 0..12 {
        if cancel.is_cancelled() {
            return Err("subagente cancelado".into());
        }
        let mut req = ChatRequest::new(
            cfg.provider.model.clone(),
            montar_mensagens(&sistema, &historico),
        );
        req.tools = ferramentas.clone();
        req.stream = cfg.provider.stream;
        req.temperature = cfg.provider.temperature;
        req.max_tokens = cfg.provider.max_tokens;
        req.reasoning_effort = cfg.provider.reasoning_effort.clone();

        let (delta_tx, mut delta_rx) = mpsc::unbounded_channel::<StreamEvent>();
        let chamada = provider.chat(req, cancel.clone(), Some(delta_tx));
        tokio::pin!(chamada);
        let completion = loop {
            tokio::select! {
                _ = cancel.cancelled() => return Err("subagente cancelado".into()),
                resultado = &mut chamada => break resultado,
                Some(evento) = delta_rx.recv() => {
                    // Deltas parciais não anunciam bloco: os provedores
                    // repetem `function.name` em cada fragmento e a UI
                    // empilhava vários blocos "em execução" eternos. O anúncio
                    // (com o id da chamada) sai uma vez, na execução —
                    // exatamente como no caminho do agente principal.
                    let _ = evento;
                }
            }
        };
        let completion = completion.map_err(|e| e.to_string())?;
        if !completion.text.trim().is_empty() {
            relatorio = completion.text.clone();
        }
        let (validas, malformadas) = separar_chamadas(completion.tool_calls.clone());
        historico.push(ChatMessage::assistant_with_calls(
            completion.text.clone(),
            chamadas_para_historico(&completion.tool_calls),
        ));
        if validas.is_empty() && malformadas.is_empty() {
            esgotou_rodadas = false;
            break;
        }
        // Um bloco por chamada, anunciado uma única vez e com o id da chamada
        // (o caminho do agente principal faz o mesmo em `executar_chamadas`):
        // antes o anúncio saía a cada delta parcial do stream e a conclusão
        // chegava uma vez com `id: None`, sobrando blocos "em execução" eternos.
        for chamada in malformadas.iter().map(|(c, _)| c).chain(&validas) {
            anunciar_chamada_subagente(&eventos, &rotulo, tarefa, chamada);
        }
        // Chamadas malformadas também precisam de um `tool` result: sem ele o
        // histórico fica inválido para a próxima requisição.
        for (chamada, erro) in malformadas {
            let _ = eventos.send(AgentEvent::FerramentaConcluida {
                id: Some(chamada.id.clone()),
                nome: format!("subagente:{}", chamada.name),
                sucesso: false,
                resumo: format!("argumentos inválidos: {}", crate::util::truncate(&erro, 80)),
                adicoes: 0,
                remocoes: 0,
                diff: String::new(),
            });
            historico.push(ChatMessage::tool_result(
                chamada.id,
                chamada.name,
                format!("ERRO: argumentos inválidos: {erro}"),
            ));
        }
        for chamada in validas {
            // O filtro de specs só esconde a ferramenta do modelo; sem esta
            // checagem uma chamada explícita executava qualquer ferramenta,
            // inclusive as que o subagente não deveria ter (ex.: delete_file,
            // spawn_subagent).
            if !FERRAMENTAS_SUBAGENTE.contains(&chamada.name.as_str()) {
                let _ = eventos.send(AgentEvent::FerramentaConcluida {
                    id: Some(chamada.id.clone()),
                    nome: format!("subagente:{}", chamada.name),
                    sucesso: false,
                    resumo: "ferramenta indisponível para subagentes".into(),
                    adicoes: 0,
                    remocoes: 0,
                    diff: String::new(),
                });
                historico.push(ChatMessage::tool_result(
                    chamada.id,
                    chamada.name.clone(),
                    format!(
                        "ERRO: a ferramenta `{}` não está disponível para subagentes",
                        chamada.name
                    ),
                ));
                continue;
            }
            let args: Value = serde_json::from_str(&chamada.arguments).unwrap_or(Value::Null);
            let mut ctx_local = ctx.clone();
            ctx_local.chamada_id = Some(chamada.id.clone());
            let resultado = comp
                .registro
                .read()
                .await
                .executar(&chamada.name, args, &ctx_local)
                .await;
            let (sucesso, texto, resumo, adicoes, remocoes) = match resultado {
                Ok(saida) => {
                    let texto = if saida.conteudo.trim().is_empty() {
                        saida.resumo.clone()
                    } else {
                        saida.conteudo
                    };
                    (
                        saida.sucesso,
                        texto,
                        saida.resumo,
                        saida.adicoes,
                        saida.remocoes,
                    )
                }
                Err(erro) => (
                    false,
                    format!("ERRO: {erro}"),
                    format!("falha: {erro}"),
                    0,
                    0,
                ),
            };
            let _ = eventos.send(AgentEvent::FerramentaConcluida {
                id: Some(chamada.id.clone()),
                nome: format!("subagente:{}", chamada.name),
                sucesso,
                resumo: crate::util::redact_secrets(&resumo),
                adicoes,
                remocoes,
                // A saída da ferramenta não é diff: o conteúdo bruto pintado
                // como diff mostraria listas `-` da saída como remoções.
                diff: diff_para_interface(&texto, adicoes, remocoes),
            });
            // Segredos não podem voltar ao modelo nem ao relatório.
            let texto = crate::util::redact_secrets(&texto);
            historico.push(ChatMessage::tool_result(
                chamada.id.clone(),
                chamada.name.clone(),
                texto,
            ));
        }
    }
    if esgotou_rodadas {
        // Retornar o último texto como relatório de sucesso escondia que o
        // subagente parou no meio de uma sequência de ferramentas.
        return Err("o subagente atingiu o limite de 12 rodadas sem concluir a tarefa".into());
    }
    if relatorio.trim().is_empty() {
        Err("o subagente terminou sem produzir relatório".into())
    } else {
        Ok(relatorio)
    }
}

impl Compartilhado {
    /// Cria o executor de subagentes com acesso ao estado compartilhado.
    pub fn novo_subagente(comp: &Arc<Self>) -> Arc<dyn SubagentRunner> {
        Arc::new(ExecutorSubagente { comp: comp.clone() })
    }
}

/// Sessões interrompidas retomáveis: mesmo projeto, exceto a sessão atual.
///
/// O filtro por projeto é o que impede o aviso (e a retomada) de vazar entre
/// projetos diferentes.
#[cfg(test)]
fn sessoes_retomaveis(
    sessoes: Vec<crate::db::SessionRow>,
    projeto: &str,
    atual: &str,
) -> Vec<crate::db::SessionRow> {
    sessoes
        .into_iter()
        .filter(|s| s.id != atual && s.project_path == projeto)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn sessao(id: &str, projeto: &str) -> crate::db::SessionRow {
        crate::db::SessionRow {
            id: id.into(),
            project_path: projeto.into(),
            title: "t".into(),
            model: "m".into(),
            reasoning_effort: None,
            permission: "semi".into(),
            status: "interrompida".into(),
            parent_id: None,
            objective: None,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    #[test]
    fn retomada_nao_mistura_projetos() {
        let sessoes = vec![
            sessao("atual", "/projeto/a"),
            sessao("a1", "/projeto/a"),
            sessao("b1", "/projeto/b"),
        ];
        let retomaveis = sessoes_retomaveis(sessoes, "/projeto/a", "atual");
        assert_eq!(retomaveis.len(), 1);
        assert_eq!(retomaveis[0].id, "a1");
        // Nenhuma sessão do outro projeto nem a sessão atual entram na lista.
        let sessoes = vec![sessao("atual", "/projeto/a"), sessao("b1", "/projeto/b")];
        assert!(sessoes_retomaveis(sessoes, "/projeto/a", "atual").is_empty());
    }

    #[test]
    fn separa_chamadas_malformadas() {
        let chamadas = vec![
            ToolCall {
                id: "1".into(),
                name: "read_file".into(),
                arguments: r#"{"path":"a.rs"}"#.into(),
            },
            ToolCall {
                id: "2".into(),
                name: "read_file".into(),
                arguments: "{invalido".into(),
            },
            ToolCall {
                id: "3".into(),
                name: "read_file".into(),
                arguments: "[]".into(),
            },
        ];
        let (validas, malformadas) = separar_chamadas(chamadas);
        assert_eq!(validas.len(), 1);
        assert_eq!(malformadas.len(), 2);
        assert!(malformadas[0].1.contains("JSON inválido"));
    }

    #[test]
    fn detalhe_extrai_campo_mais_relevante() {
        let chamada = ToolCall {
            id: "1".into(),
            name: "run_command".into(),
            arguments: json!({"comando": "cargo test"}).to_string(),
        };
        assert_eq!(detalhe_chamada(&chamada), "cargo test");
    }

    /// Retomar uma sessão de outro diretório precisa mudar o workdir do
    /// agente; senão o histórico antigo é continuado com ferramentas
    /// apontando para o projeto errado.
    #[tokio::test]
    async fn retomar_sessao_troca_para_o_projeto_da_sessao() {
        let projeto_a = dir_de_teste("retomar-a");
        let projeto_b = dir_de_teste("retomar-b");
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s-a",
                &projeto_a.display().to_string(),
                "a",
                "modelo/teste",
                None,
                "semi",
                None,
            )
            .unwrap();
        store
            .create_session(
                "s-b",
                &projeto_b.display().to_string(),
                "b",
                "modelo/teste",
                None,
                "semi",
                None,
            )
            .unwrap();
        let mut ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(projeto_a.clone());
            cfg.agent.pin_workdir = true;
        });
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let agente = Agente {
            comp: ambiente.comp.clone(),
            cmd_rx,
        };
        let mut turno_ativo = true; // evita iniciar um turno automático
        let mut cancel = None;
        let (fim_tx, _fim_rx) = mpsc::unbounded_channel();
        agente.retomar_sessao("s-b", &mut turno_ativo, &mut cancel, &fim_tx);

        assert_eq!(ambiente.comp.sessao().id, "s-b");
        assert_eq!(
            ambiente.comp.config().workdir(),
            std::fs::canonicalize(&projeto_b).unwrap()
        );
        let eventos = drenar_eventos(&mut ambiente);
        let esperado = std::fs::canonicalize(&projeto_b)
            .unwrap()
            .display()
            .to_string();
        assert!(eventos.iter().any(|e| matches!(
            e,
            AgentEvent::SessaoAtiva { id, workdir, .. }
                if id == "s-b" && workdir == &esperado
        )));
        let _ = std::fs::remove_dir_all(&projeto_a);
        let _ = std::fs::remove_dir_all(&projeto_b);
    }

    #[test]
    fn historico_descarta_resultados_orfaos() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_session("s", "/p", "t", "m", None, "semi", None)
            .unwrap();
        store
            .append_message(
                "s",
                "tool",
                "órfã",
                None,
                Some("call_x"),
                Some("read_file"),
                None,
            )
            .unwrap();
        store
            .append_message("s", "user", "oi", None, None, None, None)
            .unwrap();
        let ambiente = ambiente_de_teste(store, |_| {});
        let historico = carregar_historico(&ambiente.comp, "s", 50);
        assert_eq!(historico.len(), 1);
        assert_eq!(historico[0].role, crate::provider::Role::User);
    }

    /// Ambiente de teste com canais observáveis (eventos e confirmações).
    struct Ambiente {
        comp: Arc<Compartilhado>,
        eventos: mpsc::UnboundedReceiver<AgentEvent>,
        confirmacoes: mpsc::UnboundedReceiver<ConfirmRequest>,
    }

    fn ambiente_de_teste(store: Store, ajuste: impl FnOnce(&mut Config)) -> Ambiente {
        let mut cfg = Config::default();
        cfg.provider.model = "modelo/teste".into();
        // Testes não devem depender de Landlock do kernel hospedeiro.
        cfg.agent.sandbox = crate::config::SandboxMode::Off;
        ajuste(&mut cfg);
        let (eventos_tx, eventos) = mpsc::unbounded_channel();
        let (confirmacoes_tx, confirmacoes) = mpsc::unbounded_channel();
        let comp = Arc::new(Compartilhado {
            paths: Paths::with_root("/tmp/kestrel-teste"),
            store,
            registro: Arc::new(tokio::sync::RwLock::new(ToolRegistry::novo())),
            processos: Arc::new(ProcessManager::new()),
            arquivos: Arc::new(FileRegistry::new()),
            eventos: eventos_tx,
            confirmacoes: confirmacoes_tx,
            permitidos: Arc::new(Mutex::new(HashSet::new())),
            encerrando: AtomicBool::new(false),
            estado: RwLock::new(EstadoAgente {
                provider: Arc::new(ProvedorFalso),
                projeto: Arc::new(ProjectInfo::default()),
                skills: Arc::new(SkillRegistry::vazio()),
                config: cfg,
                sessao: EstadoSessao {
                    id: "s".into(),
                    titulo: "t".into(),
                    objetivo: String::new(),
                },
            }),
        });
        Ambiente {
            comp,
            eventos,
            confirmacoes,
        }
    }

    /// Diretório temporário exclusivo para um teste.
    fn dir_de_teste(nome: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("kestrel-{nome}-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Esvazia o canal de eventos acumulados.
    fn drenar_eventos(ambiente: &mut Ambiente) -> Vec<AgentEvent> {
        let mut eventos = Vec::new();
        while let Ok(evento) = ambiente.eventos.try_recv() {
            eventos.push(evento);
        }
        eventos
    }

    /// O laço de turno persistia apenas os resultados das ferramentas até o
    /// `break` — quando a recuperação estourava no meio, os tool_results já
    /// executados eram descartados e a mensagem do assistente ficava com
    /// tool_calls sem resposta (histórico inválido para a API).
    #[tokio::test]
    async fn resultados_executados_sempre_vao_para_o_historico() {
        let dir = dir_de_teste("resultados");
        let store = Store::open_in_memory().unwrap();
        store
            .create_session("s", "/tmp", "t", "modelo/teste", None, "full", None)
            .unwrap();
        // Recuperação estoura na primeira falha: o turno termina, mas os dois
        // resultados precisam estar gravados.
        let ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
            cfg.agent.pin_workdir = true;
            cfg.agent.permission = PermissionLevel::Full;
            cfg.agent.recovery_max_attempts = 1;
            cfg.agent.recovery_timeout_secs = 1;
        });
        usar_roteiro(
            &ambiente,
            vec![
                // Duas chamadas: a segunda falha (arquivo inexistente) e a
                // primeira teria sido descartada pelo break.
                crate::provider::Completion {
                    tool_calls: vec![
                        ToolCall {
                            id: "c1".into(),
                            name: "list_files".into(),
                            arguments: json!({"path": "."}).to_string(),
                        },
                        ToolCall {
                            id: "c2".into(),
                            name: "read_file".into(),
                            arguments: json!({"path": "/inexistente/nao-existe"}).to_string(),
                        },
                    ],
                    ..Default::default()
                },
                resposta("pronto"),
            ],
        );

        executar_turno(
            ambiente.comp.clone(),
            "liste e leia".into(),
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let mensagens = ambiente.comp.store.recent_messages("s", 100).unwrap();
        let ferramentas: Vec<&str> = mensagens
            .iter()
            .filter(|m| m.role == "tool")
            .map(|m| m.tool_name.as_deref().unwrap_or(""))
            .collect();
        // As duas respostas de ferramenta estão no histórico, na ordem.
        assert_eq!(ferramentas, vec!["list_files", "read_file"]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn resposta(texto: &str) -> crate::provider::Completion {
        crate::provider::Completion {
            text: texto.into(),
            ..Default::default()
        }
    }

    fn chamada(id: &str, nome: &str, args: Value) -> crate::provider::Completion {
        crate::provider::Completion {
            tool_calls: vec![ToolCall {
                id: id.into(),
                name: nome.into(),
                arguments: args.to_string(),
            }],
            ..Default::default()
        }
    }

    fn usar_roteiro(ambiente: &Ambiente, respostas: Vec<crate::provider::Completion>) {
        ambiente.comp.estado.write().expect("estado").provider = Arc::new(ProvedorRoteirizado {
            respostas: Mutex::new(respostas.into()),
        });
    }

    /// Provedor que devolve respostas pré-programadas, na ordem pedida.
    struct ProvedorRoteirizado {
        respostas: Mutex<std::collections::VecDeque<crate::provider::Completion>>,
    }

    #[async_trait::async_trait]
    impl Provider for ProvedorRoteirizado {
        async fn chat(
            &self,
            _req: ChatRequest,
            _cancel: CancellationToken,
            sink: Option<mpsc::UnboundedSender<StreamEvent>>,
        ) -> Result<crate::provider::Completion, ProviderError> {
            let resposta = self
                .respostas
                .lock()
                .expect("roteiro")
                .pop_front()
                .expect("o provedor roteirizado não tem mais respostas");
            if let Some(sink) = sink {
                if !resposta.text.is_empty() {
                    let _ = sink.send(StreamEvent::Texto(resposta.text.clone()));
                }
                // provedores reais repetem `function.name` a cada fragmento da
                // chamada: dois deltas para a mesma chamada, de propósito.
                for chamada in &resposta.tool_calls {
                    for _ in 0..2 {
                        let _ = sink.send(StreamEvent::ToolCall {
                            name: Some(chamada.name.clone()),
                        });
                    }
                }
            }
            Ok(resposta)
        }
        async fn list_models(&self) -> Result<Vec<crate::config::ModelEntry>, ProviderError> {
            Ok(vec![])
        }
    }

    struct ProvedorFalso;

    #[async_trait::async_trait]
    impl Provider for ProvedorFalso {
        async fn chat(
            &self,
            _req: ChatRequest,
            _cancel: CancellationToken,
            _sink: Option<mpsc::UnboundedSender<StreamEvent>>,
        ) -> Result<crate::provider::Completion, ProviderError> {
            Ok(crate::provider::Completion {
                text: "ok".into(),
                ..Default::default()
            })
        }
        async fn list_models(&self) -> Result<Vec<crate::config::ModelEntry>, ProviderError> {
            Ok(vec![])
        }
    }

    /// `reasoning_tokens` é detalhe dentro de `completion_tokens`: somá-los
    /// inflava a taxa de tok/s do rodapé do turno.
    #[test]
    fn taxa_de_tokens_conta_so_a_geracao() {
        let uso = crate::provider::Usage {
            prompt_tokens: 1000,
            completion_tokens: 40,
            reasoning_tokens: 70,
        };
        assert_eq!(tokens_gerados(&uso), 40);
        assert_eq!(
            tokens_gerados(&crate::provider::Usage::default()),
            0,
            "resposta sem usage não deveria inventar tokens"
        );
    }

    #[test]
    fn modelos_de_chamada_incluem_fallbacks_sem_duplicar() {
        let mut cfg = Config::default();
        cfg.provider.model = "principal".into();
        cfg.provider.fallback_models = vec![
            " reserva ".into(),
            "principal".into(),
            "".into(),
            "outra".into(),
        ];
        assert_eq!(
            modelos_de_chamada(&cfg),
            vec!["principal", "reserva", "outra"]
        );
        cfg.provider.model.clear();
        // "principal" continua na lista porque foi declarado como fallback;
        // só o modelo ativo vazio não entra.
        assert_eq!(
            modelos_de_chamada(&cfg),
            vec!["reserva", "principal", "outra"]
        );
    }

    #[test]
    fn paralelismo_respeita_risco_e_arquivos() {
        let raiz = std::path::Path::new("/projeto");
        let leitura = |id: &str, path: &str| ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path": path}).to_string(),
        };
        assert!(pode_paralelizar(
            &[leitura("1", "a.rs"), leitura("2", "b.rs")],
            raiz
        ));
        assert!(!pode_paralelizar(
            &[leitura("1", "a.rs"), leitura("2", "a.rs")],
            raiz
        ));
        assert!(!pode_paralelizar(
            &[leitura("1", "a.rs"), leitura("2", "./a.rs")],
            raiz
        ));
        let mut escrita = leitura("3", "b.rs");
        escrita.name = "write_file".into();
        assert!(pode_paralelizar(
            &[leitura("1", "a.rs"), escrita.clone()],
            raiz
        ));
        let mut escrita_mesmo_caminho = leitura("4", "a.rs");
        escrita_mesmo_caminho.name = "edit_file".into();
        assert!(!pode_paralelizar(
            &[leitura("1", "a.rs"), escrita_mesmo_caminho],
            raiz
        ));
        let comando = ToolCall {
            id: "5".into(),
            name: "run_command".into(),
            arguments: json!({"comando": "ls"}).to_string(),
        };
        assert!(!pode_paralelizar(&[leitura("1", "a.rs"), comando], raiz));
        let git_diff = ToolCall {
            id: "6".into(),
            name: "git_diff".into(),
            arguments: json!({}).to_string(),
        };
        assert!(!pode_paralelizar(&[escrita.clone(), git_diff], raiz));
    }

    #[test]
    fn subagente_recebe_apenas_ferramentas_isoladas() {
        // A lista do subagente nunca pode conter ferramentas de planejamento,
        // gestão de processos ou recursão.
        for proibida in [
            "update_plan",
            "task_status",
            "kill_process",
            "write_process_stdin",
            "spawn_subagent",
        ] {
            assert!(
                !FERRAMENTAS_SUBAGENTE.contains(&proibida),
                "subagente não pode ter `{proibida}`"
            );
        }
        for esperada in [
            "read_file",
            "edit_file",
            "run_command",
            "run_tests",
            "search_code",
        ] {
            assert!(FERRAMENTAS_SUBAGENTE.contains(&esperada));
        }
    }

    /// O subagente anuncia as ferramentas que executa na interface; cada
    /// anúncio precisa de uma conclusão, senão o bloco fica "rodando" para
    /// sempre na conversa.
    #[tokio::test]
    async fn subagente_conclui_as_ferramentas_que_executou() {
        let dir = dir_de_teste("subagente-eventos");
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s",
                &dir.display().to_string(),
                "t",
                "modelo/teste",
                None,
                "full",
                None,
            )
            .unwrap();
        let ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
            cfg.agent.pin_workdir = true;
            cfg.agent.permission = PermissionLevel::Full;
        });
        usar_roteiro(
            &ambiente,
            vec![
                chamada("c1", "list_files", json!({"path": "."})),
                resposta("relatório do subagente"),
            ],
        );
        let (eventos_tx, mut eventos) = mpsc::unbounded_channel();
        let relatorio = executar_subagente_interno(
            ambiente.comp.clone(),
            "listar arquivos",
            "",
            CancellationToken::new(),
            eventos_tx,
        )
        .await
        .unwrap();
        assert!(relatorio.contains("relatório"));

        let mut concluidas = 0;
        while let Ok(evento) = eventos.try_recv() {
            if let AgentEvent::FerramentaConcluida {
                nome,
                sucesso,
                adicoes,
                remocoes,
                ..
            } = evento
                && nome == "subagente:list_files"
            {
                assert!(sucesso);
                assert_eq!((adicoes, remocoes), (0, 0));
                concluidas += 1;
            }
        }
        assert_eq!(
            concluidas, 1,
            "a ferramenta do subagente não recebeu conclusão"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Os provedores repetem `function.name` a cada delta parcial do stream.
    /// O subagente anunciava um bloco por delta e a conclusão chegava uma vez
    /// com `id: None`: sobravam blocos "em execução" eternos na conversa.
    #[tokio::test]
    async fn subagente_anuncia_cada_chamada_uma_unica_vez() {
        let dir = dir_de_teste("subagente-anuncio");
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s",
                &dir.display().to_string(),
                "t",
                "modelo/teste",
                None,
                "full",
                None,
            )
            .unwrap();
        let ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
            cfg.agent.pin_workdir = true;
            cfg.agent.permission = PermissionLevel::Full;
        });
        usar_roteiro(
            &ambiente,
            vec![
                chamada("c1", "list_files", json!({"path": "."})),
                resposta("relatório do subagente"),
            ],
        );
        let (eventos_tx, mut eventos) = mpsc::unbounded_channel();
        executar_subagente_interno(
            ambiente.comp.clone(),
            "listar arquivos",
            "",
            CancellationToken::new(),
            eventos_tx,
        )
        .await
        .unwrap();

        let mut iniciadas = 0;
        let mut concluidas = 0;
        while let Ok(evento) = eventos.try_recv() {
            match evento {
                AgentEvent::FerramentaIniciada { nome, id, .. }
                    if nome == "subagente:list_files" =>
                {
                    assert!(
                        id.is_some(),
                        "o anúncio precisa do id da chamada para casar com a conclusão"
                    );
                    iniciadas += 1;
                }
                AgentEvent::FerramentaConcluida { nome, .. } if nome == "subagente:list_files" => {
                    concluidas += 1;
                }
                _ => {}
            }
        }
        // Um anúncio e uma conclusão, mesmo com dois deltas de stream.
        assert_eq!(iniciadas, 1, "a chamada foi anunciada mais de uma vez");
        assert_eq!(concluidas, 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A classe da falha depende da mensagem (`error[E0308]` → compilação) e o
    /// sucesso não tem mensagem: resolver o incidente pela classe do sucesso
    /// deixava o incidente real aberto, e o turno morria por "Limite de
    /// recuperação automática atingido" com falhas já corrigidas.
    #[tokio::test]
    async fn falha_corrigida_restaura_o_orcamento_de_recuperacao() {
        let dir = dir_de_teste("orcamento-recuperacao");
        std::fs::create_dir_all(&dir).unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s",
                &dir.display().to_string(),
                "t",
                "modelo/teste",
                None,
                "full",
                None,
            )
            .unwrap();
        let mut ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
            cfg.agent.pin_workdir = true;
            cfg.agent.permission = PermissionLevel::Full;
            // Orçamento curto de propósito: duas tentativas por incidente.
            cfg.agent.recovery_max_attempts = 2;
            cfg.agent.recovery_timeout_secs = 600;
        });
        // Falha na primeira chamada, passa na segunda (mesma estratégia) e
        // volta a falhar na terceira — que só é permitida porque o incidente
        // foi resolvido no acerto.
        let comando = "n=$(cat contador 2>/dev/null || echo 0); n=$((n+1)); echo $n > contador; \
                       if [ $n -eq 2 ]; then echo compilou; else echo \"error[E0308]: falha $n\"; exit 1; fi";
        usar_roteiro(
            &ambiente,
            vec![
                chamada("c1", "run_command", json!({"comando": comando})),
                chamada("c2", "run_command", json!({"comando": comando})),
                chamada("c3", "run_command", json!({"comando": comando})),
                resposta("compilou e segui"),
            ],
        );

        executar_turno(
            ambiente.comp.clone(),
            "compile o projeto".into(),
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let eventos = drenar_eventos(&mut ambiente);
        // Nenhuma das falhas deveria encerrar o turno por limite de recuperação.
        for evento in &eventos {
            if let AgentEvent::Erro { mensagem, .. } = evento {
                assert!(
                    !mensagem.contains("Limite de recuperação"),
                    "o turno morreu por limite de recuperação: {mensagem}"
                );
            }
        }
        assert!(
            eventos
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnoConcluido { .. })),
            "o turno deveria ter sido concluído"
        );
        // As três execuções aconteceram (o orçamento foi restaurado no acerto).
        let ferramentas = ambiente
            .comp
            .store
            .recent_messages("s", 100)
            .unwrap()
            .iter()
            .filter(|m| m.role == "tool" && m.tool_name.as_deref() == Some("run_command"))
            .count();
        assert_eq!(ferramentas, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ------------------------------------------------- testes de ponta a ponta

    #[tokio::test]
    async fn turno_completo_le_arquivo_e_persiste_historico() {
        let dir = dir_de_teste("turno");
        std::fs::write(dir.join("nota.txt"), "conteudo-de-teste\n").unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s",
                &dir.display().to_string(),
                "t",
                "modelo/teste",
                None,
                "read",
                None,
            )
            .unwrap();
        let mut ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
            cfg.agent.pin_workdir = true;
            cfg.agent.permission = PermissionLevel::Read;
        });
        usar_roteiro(
            &ambiente,
            vec![
                chamada("c1", "read_file", json!({"path": "nota.txt"})),
                resposta("O arquivo contém conteudo-de-teste."),
            ],
        );

        executar_turno(
            ambiente.comp.clone(),
            "leia nota.txt".into(),
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        let eventos = drenar_eventos(&mut ambiente);
        let mensagens = ambiente.comp.store.recent_messages("s", 100).unwrap();
        assert!(eventos.iter().any(|e| matches!(
            e,
            AgentEvent::FerramentaConcluida { nome, sucesso, .. }
                if nome == "read_file" && *sucesso
        )));
        assert!(
            eventos
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnoConcluido { .. }))
        );

        let papeis: Vec<&str> = mensagens.iter().map(|m| m.role.as_str()).collect();
        assert_eq!(papeis, vec!["user", "assistant", "tool", "assistant"]);
        assert!(mensagens[2].content.contains("conteudo-de-teste"));
        assert!(mensagens[3].content.contains("conteudo-de-teste"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn fallback_troca_de_modelo_sem_mudar_o_principal() {
        let dir = dir_de_teste("fallback");
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s",
                &dir.display().to_string(),
                "t",
                "principal",
                None,
                "semi",
                None,
            )
            .unwrap();
        let mut ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
            cfg.agent.pin_workdir = true;
            cfg.provider.model = "principal".into();
            cfg.provider.fallback_models = vec!["reserva".into()];
        });
        let vistos = Arc::new(Mutex::new(Vec::new()));
        ambiente.comp.estado.write().expect("estado").provider = Arc::new(ProvedorFallback {
            vistos: vistos.clone(),
        });

        executar_turno(
            ambiente.comp.clone(),
            "responda".into(),
            false,
            CancellationToken::new(),
        )
        .await
        .unwrap();

        assert_eq!(vistos.lock().unwrap().as_slice(), ["principal", "reserva"]);
        let eventos = drenar_eventos(&mut ambiente);
        assert!(eventos.iter().any(|e| matches!(
            e,
            AgentEvent::Aviso { mensagem } if mensagem.contains("fallback")
        )));
        assert!(
            eventos
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnoConcluido { .. }))
        );
        assert_eq!(ambiente.comp.config().provider.model, "principal");
        let mensagens = ambiente.comp.store.recent_messages("s", 10).unwrap();
        assert!(
            mensagens
                .last()
                .is_some_and(|m| m.content.contains("ok do reserva"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    struct ProvedorFallback {
        vistos: Arc<Mutex<Vec<String>>>,
    }

    #[async_trait::async_trait]
    impl Provider for ProvedorFallback {
        async fn chat(
            &self,
            req: ChatRequest,
            _cancel: CancellationToken,
            _sink: Option<mpsc::UnboundedSender<StreamEvent>>,
        ) -> Result<crate::provider::Completion, ProviderError> {
            self.vistos.lock().unwrap().push(req.model.clone());
            if req.model == "principal" {
                return Err(ProviderError::ModeloIndisponivel(
                    "principal fora do ar".into(),
                ));
            }
            Ok(crate::provider::Completion {
                text: "ok do reserva".into(),
                usage: Some(crate::provider::Usage {
                    prompt_tokens: 10,
                    completion_tokens: 3,
                    reasoning_tokens: 0,
                }),
                ..Default::default()
            })
        }
        async fn list_models(&self) -> Result<Vec<crate::config::ModelEntry>, ProviderError> {
            Ok(vec![])
        }
    }

    #[tokio::test]
    async fn confirmacao_aprovada_permite_comando_sensivel() {
        let dir = dir_de_teste("confirmacao-ok");
        std::fs::create_dir_all(dir.join("dados")).unwrap();
        std::fs::write(dir.join("dados/arquivo.txt"), "x").unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s",
                &dir.display().to_string(),
                "t",
                "modelo/teste",
                None,
                "semi",
                None,
            )
            .unwrap();
        let mut ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
            cfg.agent.pin_workdir = true;
            cfg.agent.permission = PermissionLevel::Semi;
        });
        usar_roteiro(
            &ambiente,
            vec![
                chamada(
                    "c1",
                    "run_command",
                    json!({"comando": "chmod -R 777 dados"}),
                ),
                resposta("Comando executado."),
            ],
        );

        let tarefa = tokio::spawn(executar_turno(
            ambiente.comp.clone(),
            "limpe a pasta dados".into(),
            false,
            CancellationToken::new(),
        ));
        let pedido = tokio::time::timeout(Duration::from_secs(10), ambiente.confirmacoes.recv())
            .await
            .expect("a confirmação não chegou")
            .expect("canal de confirmação fechado");
        assert!(pedido.titulo.contains("Confirmar comando"));
        pedido
            .resposta
            .send(crate::perm::Confirmacao::Permitir)
            .unwrap();
        tarefa.await.unwrap().unwrap();

        // O chmod rodou: o arquivo continua lá, mas com permissão 777.
        assert!(
            dir.join("dados/arquivo.txt").exists(),
            "o comando aprovado não rodou"
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let modo = std::fs::metadata(dir.join("dados/arquivo.txt"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(modo & 0o777, 0o777, "o chmod não foi aplicado: {modo:o}");
        }
        let eventos = drenar_eventos(&mut ambiente);
        assert!(eventos.iter().any(|e| matches!(
            e,
            AgentEvent::FerramentaConcluida { nome, sucesso, .. }
                if nome == "run_command" && *sucesso
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn confirmacao_negada_nao_executa_e_informa_o_modelo() {
        let dir = dir_de_teste("confirmacao-nao");
        std::fs::create_dir_all(dir.join("dados")).unwrap();
        std::fs::write(dir.join("dados/arquivo.txt"), "x").unwrap();
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s",
                &dir.display().to_string(),
                "t",
                "modelo/teste",
                None,
                "semi",
                None,
            )
            .unwrap();
        let mut ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
            cfg.agent.pin_workdir = true;
            cfg.agent.permission = PermissionLevel::Semi;
        });
        usar_roteiro(
            &ambiente,
            vec![
                chamada(
                    "c1",
                    "run_command",
                    json!({"comando": "chmod -R 777 dados"}),
                ),
                resposta("Ok, não executei."),
            ],
        );

        let tarefa = tokio::spawn(executar_turno(
            ambiente.comp.clone(),
            "limpe a pasta dados".into(),
            false,
            CancellationToken::new(),
        ));
        let pedido = tokio::time::timeout(Duration::from_secs(10), ambiente.confirmacoes.recv())
            .await
            .expect("a confirmação não chegou")
            .expect("canal de confirmação fechado");
        pedido
            .resposta
            .send(crate::perm::Confirmacao::Negar)
            .unwrap();
        tarefa.await.unwrap().unwrap();

        assert!(
            dir.join("dados/arquivo.txt").exists(),
            "a operação negada foi executada"
        );
        let mensagens = ambiente.comp.store.recent_messages("s", 100).unwrap();
        let ferramenta = mensagens
            .iter()
            .find(|m| m.role == "tool")
            .expect("resultado da ferramenta não foi registrado");
        assert!(ferramenta.content.contains("comando negado pelo usuário"));
        let eventos = drenar_eventos(&mut ambiente);
        assert!(eventos.iter().any(|e| matches!(
            e,
            AgentEvent::FerramentaConcluida { nome, sucesso: false, .. }
                if nome == "run_command"
        )));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn retomada_usa_dossie_e_continua_a_sessao() {
        let dir = dir_de_teste("retomada");
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s2",
                &dir.display().to_string(),
                "tarefa antiga",
                "modelo/teste",
                None,
                "semi",
                None,
            )
            .unwrap();
        store
            .append_message("s2", "user", "tarefa original", None, None, None, None)
            .unwrap();
        store.update_session_status("s2", "ativa").unwrap();
        store
            .set_context_state(
                "s2",
                1200,
                8000,
                Some("# Dossiê de continuidade\n\n- feito: metade"),
            )
            .unwrap();

        let mut ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
            cfg.agent.pin_workdir = true;
        });
        let (_cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let agente = Agente {
            comp: ambiente.comp.clone(),
            cmd_rx,
        };
        let (fim_tx, mut fim_rx) = mpsc::unbounded_channel();
        let mut turno_ativo = false;
        let mut cancel = None;
        agente.retomar_sessao("s2", &mut turno_ativo, &mut cancel, &fim_tx);
        assert!(turno_ativo, "a retomada deveria iniciar um turno");
        fim_rx
            .recv()
            .await
            .expect("o turno de retomada não terminou");

        assert_eq!(ambiente.comp.sessao().id, "s2");
        let eventos = drenar_eventos(&mut ambiente);
        assert!(eventos.iter().any(|e| matches!(
            e,
            AgentEvent::SessaoAtiva { id, .. } if id == "s2"
        )));
        assert!(eventos.iter().any(|e| matches!(
            e,
            AgentEvent::Aviso { mensagem }
                if mensagem.contains("dossiê de continuidade aplicado")
        )));

        let mensagens = ambiente.comp.store.recent_messages("s2", 100).unwrap();
        let retomada = mensagens
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .expect("instrução de retomada não foi registrada");
        assert!(
            retomada
                .content
                .contains("Dossiê de continuidade registrado")
        );
        assert!(retomada.content.contains("- feito: metade"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn compactacao_cria_sessao_de_continuacao_com_dossie() {
        let dir = dir_de_teste("compactacao");
        let store = Store::open_in_memory().unwrap();
        store
            .create_session(
                "s",
                &dir.display().to_string(),
                "tarefa longa",
                "modelo/teste",
                None,
                "semi",
                None,
            )
            .unwrap();
        store
            .append_message("s", "user", "implemente a feature", None, None, None, None)
            .unwrap();
        store
            .append_message(
                "s",
                "assistant",
                "vou começar lendo os arquivos",
                None,
                None,
                None,
                None,
            )
            .unwrap();

        let ambiente = ambiente_de_teste(store, |cfg| {
            cfg.agent.workdir = Some(dir.clone());
        });
        let cfg = ambiente.comp.config();
        let historico = carregar_historico(&ambiente.comp, "s", 50);
        let (dossie, novos) =
            compactar_contexto(&ambiente.comp, &cfg, &historico, &CancellationToken::new())
                .unwrap();

        assert!(dossie.contains("# Dossiê de continuidade"));
        assert!(dossie.contains("implemente a feature"));
        // Dossiê + as duas mensagens recentes (continuidade imediata).
        assert_eq!(novos.len(), 3);
        assert!(
            novos[0]
                .content
                .contains("Continue o trabalho a partir do dossiê")
        );
        assert!(novos[1].content.contains("implemente a feature"));
        assert!(novos[2].content.contains("vou começar lendo"));

        // A sessão antiga ficou marcada e guardou o dossiê.
        let antiga = ambiente.comp.store.get_session("s").unwrap().unwrap();
        assert_eq!(antiga.status, "compactada");
        let (usados, capacidade, guardado) =
            ambiente.comp.store.context_state("s").unwrap().unwrap();
        assert!(usados > 0);
        assert_eq!(capacidade, cfg.context_window());
        assert_eq!(guardado.as_deref(), Some(dossie.as_str()));

        // A nova sessão está ativa e aponta para a anterior.
        let nova = ambiente.comp.sessao();
        assert_ne!(nova.id, "s");
        let nova_linha = ambiente.comp.store.get_session(&nova.id).unwrap().unwrap();
        assert_eq!(nova_linha.parent_id.as_deref(), Some("s"));
        let primeira = ambiente.comp.store.recent_messages(&nova.id, 10).unwrap();
        assert!(primeira[0].content.contains("# Dossiê de continuidade"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
