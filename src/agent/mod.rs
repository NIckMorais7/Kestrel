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
    ChatMessage, ChatRequest, Provider, ProviderError, StreamEvent, ToolCall, ToolSpec,
};
use crate::skills::SkillRegistry;
use crate::tools::shell::ProcessManager;
use crate::tools::{FileRegistry, SubagentRunner, ToolContext, ToolError, ToolRegistry};
use crate::util::Paths;
use serde_json::Value;
use std::collections::HashSet;
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
    pub registro: ToolRegistry,
    pub processos: Arc<ProcessManager>,
    pub arquivos: Arc<FileRegistry>,
    pub http: reqwest::Client,
    pub eventos: mpsc::UnboundedSender<AgentEvent>,
    pub confirmacoes: mpsc::UnboundedSender<ConfirmRequest>,
    pub permitidos: Arc<Mutex<HashSet<String>>>,
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

    pub fn estado_ui(&self, mensagem: impl Into<String>) {
        self.evento(AgentEvent::Estado {
            mensagem: mensagem.into(),
        });
    }

    pub fn definir_sessao(&self, sessao: EstadoSessao) {
        self.estado.write().expect("estado").sessao = sessao;
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
            estado.config.agent.permission = nivel;
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

    /// Troca o projeto ativo: valida e persiste o diretório, reanalisa o
    /// projeto, recarrega as skills e abre uma nova sessão no novo local.
    pub fn trocar_projeto(&self, caminho: &str) -> anyhow::Result<()> {
        let bruto = crate::util::expand_tilde(caminho.trim());
        if !bruto.is_dir() {
            anyhow::bail!("`{}` não é um diretório válido", bruto.display());
        }
        let caminho = std::fs::canonicalize(&bruto).unwrap_or(bruto);
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

        let info = self.reanalisar_projeto();
        let skills = self.recarregar_skills();

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
        self.evento(AgentEvent::SessaoAtiva { id, titulo });
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
            workdir: config.workdir(),
            roots: config.allowed_roots(),
            level: config.agent.permission,
            modo: config.agent.modo,
            cancel,
            store: self.store.clone(),
            session_id: self.sessao().id,
            processos: self.processos.clone(),
            arquivos: self.arquivos.clone(),
            eventos: self.eventos.clone(),
            confirmacoes: self.confirmacoes.clone(),
            permitidos: self.permitidos.clone(),
            skills: self.skills(),
            skills_dir: self.paths.skills_dir(),
            comps_dir: self.paths.comps_dir(),
            projeto: self.projeto(),
            web: config.web.clone(),
            http: self.http.clone(),
            max_saida: 30_000,
            subagente: None,
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

    let http = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(config.web.request_timeout_secs.max(10)))
        .user_agent(config.web.user_agent.clone())
        .build()?;

    let comp = Arc::new(Compartilhado {
        paths,
        store,
        registro: ToolRegistry::novo(),
        processos: Arc::new(ProcessManager::new()),
        arquivos: Arc::new(FileRegistry::new()),
        http,
        eventos: eventos_tx,
        confirmacoes: confirm_tx,
        permitidos: Arc::new(Mutex::new(HashSet::new())),
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

    // Estado inicial para a interface (a sessão já existe no banco).
    {
        let sessao = comp.sessao();
        comp.evento(AgentEvent::SessaoAtiva {
            id: sessao.id,
            titulo: sessao.titulo,
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

        // Sessões interrompidas do mesmo projeto que podem ser retomadas.
        if let Ok(sessoes) = self.comp.store.sessions_para_recuperar() {
            let atual = self.comp.sessao().id;
            let projeto = self.comp.config().workdir().display().to_string();
            let retomaveis = sessoes_retomaveis(sessoes, &projeto, &atual);
            if let Some(ultima) = retomaveis.first() {
                self.comp.evento(AgentEvent::Aviso {
                    mensagem: format!(
                        "A sessão anterior em {} foi interrompida. Use /retomar para continuar de onde parou.",
                        ultima.project_path
                    ),
                });
            }
        }

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
                                self.comp.evento(AgentEvent::Aviso {
                                    mensagem: "Nova sessão: interrompendo a execução atual.".into(),
                                });
                            }
                            self.nova_sessao();
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
                                self.comp.evento(AgentEvent::Aviso {
                                    mensagem: "Troca de projeto: interrompendo a execução atual."
                                        .into(),
                                });
                            }
                            if let Err(e) = self.comp.trocar_projeto(&caminho) {
                                self.comp.evento(AgentEvent::Erro {
                                    mensagem: format!("falha ao trocar de projeto: {e}"),
                                    dica: Some(
                                        "escolha um diretório existente em /projetos".into(),
                                    ),
                                    recuperavel: true,
                                });
                            }
                        }
                        UiCommand::ListarProcessos => {
                            self.comp.evento(AgentEvent::Processos {
                                lista: self.comp.processos.listar(),
                            });
                        }
                        UiCommand::Encerrar => {
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
        self.comp.evento(AgentEvent::SessaoAtiva { id, titulo });
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
            comp_erro
                .store
                .update_session_status(&comp_erro.sessao().id, "interrompida")
                .ok();
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
        let ferramentas = comp.registro.specs_do_modo(cfg.agent.modo);
        let usados = context::estimar_prompt(&sistema, &historico, &ferramentas);
        let orcamento = context::OrcamentoContexto::novo(
            cfg.context_window(),
            cfg.agent.reserve_response_tokens,
        );
        comp.evento(AgentEvent::Contexto {
            usados,
            capacidade: orcamento.capacidade_utilizavel(),
        });
        if orcamento.precisa_compactar(usados, cfg.agent.context_compact_threshold) {
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
                    comp.evento(AgentEvent::Erro {
                        mensagem: format!("falha ao compactar o contexto: {e}"),
                        dica: Some(
                            "o histórico original permanece salvo; você pode reiniciar com /nova"
                                .into(),
                        ),
                        recuperavel: true,
                    });
                }
            }
        }

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
        let provider = comp.provider();
        let chamada = provider.chat(req, cancel.clone(), Some(delta_tx));
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
            Ok(c) => {
                // Contabilidade real: o usage vem do provedor e era descartado.
                if let Some(uso) = c.usage {
                    comp.store
                        .record_usage(
                            &sessao.id,
                            uso.prompt_tokens,
                            uso.completion_tokens,
                            uso.reasoning_tokens,
                        )
                        .ok();
                    tokens_gerados_turno +=
                        u64::from(uso.completion_tokens) + u64::from(uso.reasoning_tokens);
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
            recuperacao.limpar();
            break ResultadoTurno {
                resumo: resumo_turno(&comp, &sessao.id),
                concluido: true,
            };
        }

        // Valida os argumentos de todas as chamadas antes de executar qualquer uma.
        let (validas, malformadas) = separar_chamadas(tool_calls);
        if validas.is_empty() && !malformadas.is_empty() {
            malformadas_seguidas += 1;
            let classe = recovery::Classificacao::RespostaMalformada;
            let diag = recuperacao.diagnosticar(classe);
            // Primeiro os `tool` results, depois o pedido de nova resposta: a
            // API exige um resultado para cada `tool_call` já gravado.
            registrar_malformadas(&comp, &sessao, &mut historico, malformadas)?;
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
            if let Err(limite) = recuperacao.registrar_tentativa(classe, "nova resposta solicitada")
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
        malformadas_seguidas = 0;

        // Executa as chamadas. O contexto é recriado aqui (e não no início do
        // turno) para refletir sessão ativa, nível, modo e raízes atuais —
        // inclusive depois de uma compactação ou de /permission no meio do turno.
        let mut ctx_ferramentas = comp.contexto_ferramentas(cancel.clone());
        ctx_ferramentas.subagente = Some(Compartilhado::novo_subagente(&comp));
        comp.estado_ui(format!(
            "executando {} ferramenta(s)",
            validas.len() + malformadas.len()
        ));
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
                nome: call.name.clone(),
                sucesso,
                resumo: crate::util::redact_secrets(&resumo),
                adicoes,
                remocoes,
            });
        }

        // Chamadas malformadas viram resultados de erro para o modelo corrigir.
        registrar_malformadas(&comp, &sessao, &mut historico, malformadas)?;

        // Só depois de persistir e devolver todos os resultados é que o turno
        // termina — nunca no meio do laço.
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
        comp.store.update_session_status(&sessao.id, "ativa").ok();
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
        comp.store
            .update_session_status(&sessao.id, "interrompida")
            .ok();
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
        context::estimar_prompt("", historico, &[]),
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
    });

    if cancel.is_cancelled() {
        anyhow::bail!("cancelado durante a compactação");
    }
    Ok((
        texto_dossie.clone(),
        vec![ChatMessage::user(format!(
            "Continue o trabalho a partir do dossiê a seguir. Confirme o estado real do projeto antes de agir.\n\n{texto_dossie}"
        ))],
    ))
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
fn pode_paralelizar(chamadas: &[ToolCall]) -> bool {
    if chamadas.len() < 2 {
        return false;
    }
    let mut caminhos = HashSet::new();
    for chamada in chamadas {
        let risco = crate::perm::tool_risk(&chamada.name);
        if risco == ToolRisk::Sensivel {
            return false;
        }
        match caminho_da_chamada(chamada) {
            Some(caminho) => {
                if !caminhos.insert(caminho) {
                    return false;
                }
            }
            None => {
                // Sem caminho explícito, só leituras são seguras em paralelo.
                if risco == ToolRisk::Escrita {
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
            nome: chamada.name.clone(),
            detalhe: detalhe_chamada(chamada),
        });
    }

    let paralelo = pode_paralelizar(chamadas);
    let mut resultados: Vec<(ToolCall, ResultadoFerramenta)> = Vec::new();

    if paralelo {
        let futuros: Vec<_> = chamadas
            .iter()
            .map(|chamada| {
                let args: Value = serde_json::from_str(&chamada.arguments).unwrap_or(Value::Null);
                let chamada = chamada.clone();
                let registro = &comp.registro;
                async move {
                    let resultado = registro.executar(&chamada.name, args, ctx).await;
                    (chamada, converter(resultado))
                }
            })
            .collect();
        resultados.extend(futures_util::future::join_all(futuros).await);
    } else {
        for chamada in chamadas {
            let args: Value = serde_json::from_str(&chamada.arguments).unwrap_or(Value::Null);
            let resultado = comp.registro.executar(&chamada.name, args, ctx).await;
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
        } else if recuperacao.estrategia_repetida(
            recovery::classificar(Some(&chamada.name), "", false),
            &format!("{} {}", chamada.name, detalhe_chamada(chamada)),
        ) {
            // Estratégia que já havia falhado agora funcionou: resolve o incidente.
            recuperacao.resolver(recovery::classificar(Some(&chamada.name), "", false));
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
        executar_subagente_interno(self.comp.clone(), tarefa, contexto, cancel, eventos).await
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
            nome: call.name,
            sucesso: false,
            resumo: format!("argumentos inválidos: {}", crate::util::truncate(&erro, 80)),
            adicoes: 0,
            remocoes: 0,
        });
    }
    Ok(())
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
        .specs()
        .into_iter()
        .filter(|f| FERRAMENTAS_SUBAGENTE.contains(&f.name.as_str()))
        .collect();
    let ctx = comp.contexto_ferramentas(cancel.clone());
    let provider = comp.provider();
    let mut relatorio = String::new();

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
                    if let StreamEvent::ToolCall { name: Some(nome), .. } = evento {
                        let _ = eventos.send(AgentEvent::FerramentaIniciada {
                            nome: format!("subagente:{nome}"),
                            detalhe: tarefa.to_string(),
                        });
                    }
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
            break;
        }
        // Chamadas malformadas também precisam de um `tool` result: sem ele o
        // histórico fica inválido para a próxima requisição.
        for (chamada, erro) in malformadas {
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
            let resultado = comp.registro.executar(&chamada.name, args, &ctx).await;
            let texto = match resultado {
                Ok(saida) => saida.conteudo,
                Err(erro) => format!("ERRO: {erro}"),
            };
            // Segredos não podem voltar ao modelo nem ao relatório.
            let texto = crate::util::redact_secrets(&texto);
            historico.push(ChatMessage::tool_result(
                chamada.id.clone(),
                chamada.name.clone(),
                texto,
            ));
        }
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
        ajuste(&mut cfg);
        let (eventos_tx, eventos) = mpsc::unbounded_channel();
        let (confirmacoes_tx, confirmacoes) = mpsc::unbounded_channel();
        let comp = Arc::new(Compartilhado {
            paths: Paths::with_root("/tmp/kestrel-teste"),
            store,
            registro: ToolRegistry::novo(),
            processos: Arc::new(ProcessManager::new()),
            arquivos: Arc::new(FileRegistry::new()),
            http: reqwest::Client::new(),
            eventos: eventos_tx,
            confirmacoes: confirmacoes_tx,
            permitidos: Arc::new(Mutex::new(HashSet::new())),
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
            if let Some(sink) = sink
                && !resposta.text.is_empty()
            {
                let _ = sink.send(StreamEvent::Texto(resposta.text.clone()));
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

    #[test]
    fn paralelismo_respeita_risco_e_arquivos() {
        let leitura = |id: &str, path: &str| ToolCall {
            id: id.into(),
            name: "read_file".into(),
            arguments: json!({"path": path}).to_string(),
        };
        assert!(pode_paralelizar(&[
            leitura("1", "a.rs"),
            leitura("2", "b.rs")
        ]));
        assert!(!pode_paralelizar(&[
            leitura("1", "a.rs"),
            leitura("2", "a.rs")
        ]));
        let mut escrita = leitura("3", "b.rs");
        escrita.name = "write_file".into();
        assert!(pode_paralelizar(&[leitura("1", "a.rs"), escrita.clone()]));
        let mut escrita_mesmo_caminho = leitura("4", "a.rs");
        escrita_mesmo_caminho.name = "edit_file".into();
        assert!(!pode_paralelizar(&[
            leitura("1", "a.rs"),
            escrita_mesmo_caminho
        ]));
        let comando = ToolCall {
            id: "5".into(),
            name: "run_command".into(),
            arguments: json!({"comando": "ls"}).to_string(),
        };
        assert!(!pode_paralelizar(&[leitura("1", "a.rs"), comando]));
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
        assert_eq!(novos.len(), 1);
        assert!(
            novos[0]
                .content
                .contains("Continue o trabalho a partir do dossiê")
        );

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
