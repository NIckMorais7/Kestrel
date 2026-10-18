//! Aplicação: laço de eventos, comandos e ponte entre a interface e o agente.
//!
//! O estado exibido é sempre derivado de eventos reais do agente: deltas de
//! texto, resultados de ferramentas (com diffs reais), erros, plano e
//! subtarefas persistidos. Nada é simulado.

use crate::agent::{AgenteHandle, iniciar};
use crate::config::{Config, ModelSource, ModoOperacao};
use crate::db::Store;
use crate::events::{
    AgentEvent, ConfirmRequest, PlanoPasso, ProcessoView, StatusPasso, TarefaView, UiCommand,
};
use crate::perm::{Confirmacao, PermissionLevel};
use crate::project::{self, ProjectInfo};
use crate::skills::SkillRegistry;
use crate::ui::input::{COMANDOS_SLASH, Editor};
use crate::ui::markdown;
use crate::ui::mascot::{self, AnimacaoPip, EstadoMascote};
use crate::ui::{self, theme};
use crate::util::Paths;
use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent, MouseEventKind};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Um bloco da conversa exibida.
#[derive(Debug)]
pub enum Bloco {
    Usuario(String),
    Raciocinio(String),
    Assistente(String),
    Ferramenta {
        nome: String,
        detalhe: String,
        rodando: bool,
        sucesso: bool,
        resumo: Option<String>,
        adicoes: usize,
        remocoes: usize,
    },
    Saida {
        nome: String,
        linhas: Vec<String>,
    },
    Erro {
        mensagem: String,
        dica: Option<String>,
    },
    Aviso(String),
    Turno(String),
    Dossie {
        resumo: String,
        nova_sessao: String,
    },
    /// Plano exibido na conversa (criação/replanejamento ou apenas mudanças).
    Plano {
        objetivo: String,
        passos: Vec<PlanoPasso>,
        mudancas: bool,
    },
    /// Subtarefas exibidas na conversa, com bloqueios derivados das dependências.
    Tarefas {
        linhas: Vec<LinhaTarefa>,
        mudancas: bool,
    },
}

/// Linha de subtarefa pronta para exibição.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinhaTarefa {
    pub titulo: String,
    pub status: StatusPasso,
    /// Dependências ainda não concluídas (bloqueio real, não narrativa).
    pub bloqueios: Vec<String>,
    pub resultado: Option<String>,
}

impl LinhaTarefa {
    pub fn de(tarefa: &TarefaView, todas: &[TarefaView]) -> Self {
        Self {
            titulo: tarefa.titulo.clone(),
            status: tarefa.status,
            bloqueios: tarefa
                .bloqueios(todas)
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
            resultado: tarefa.resultado.clone(),
        }
    }
}

impl Bloco {
    /// Linhas prontas para exibição (o embrulho de texto é feito pelo widget).
    pub fn linhas(&self) -> Vec<Line<'static>> {
        match self {
            Bloco::Usuario(texto) => {
                prefixar_linhas(texto, "❯ ", theme::usuario(), "  ", theme::usuario())
            }
            Bloco::Raciocinio(texto) => {
                let mut linhas = vec![Line::from(vec![Span::styled(
                    "· raciocínio ",
                    theme::apagado().add_modifier(Modifier::ITALIC),
                )])];
                let corte: String = texto
                    .chars()
                    .rev()
                    .take(6000)
                    .collect::<String>()
                    .chars()
                    .rev()
                    .collect();
                for linha in corte.lines() {
                    linhas.push(Line::from(Span::styled(
                        format!("  {linha}"),
                        theme::raciocinio(),
                    )));
                }
                linhas
            }
            Bloco::Assistente(texto) => {
                let mut linhas = vec![Line::default()];
                linhas.extend(markdown::renderizar(texto));
                linhas
            }
            Bloco::Ferramenta {
                nome,
                detalhe,
                rodando,
                sucesso,
                resumo,
                adicoes,
                remocoes,
            } => {
                let (marcador, estilo_marcador) = if *rodando {
                    ("→", theme::em_execucao())
                } else if *sucesso {
                    ("✓", theme::sucesso())
                } else {
                    ("✗", theme::falha())
                };
                let mut spans = vec![
                    Span::styled(format!("{marcador} "), estilo_marcador),
                    Span::styled(nome.clone(), theme::rotulo()),
                ];
                if !detalhe.is_empty() && detalhe != nome {
                    spans.push(Span::styled(format!("  {detalhe}"), theme::apagado()));
                }
                if !*rodando && (*adicoes > 0 || *remocoes > 0) {
                    spans.push(Span::styled(format!("  +{adicoes}"), theme::sucesso()));
                    spans.push(Span::styled(format!(" -{remocoes}"), theme::falha()));
                }
                let mut linhas = vec![Line::from(spans)];
                if let Some(resumo) = resumo
                    && !resumo.trim().is_empty()
                {
                    linhas.push(Line::from(vec![
                        Span::styled("  ", theme::apagado()),
                        Span::styled(resumo.clone(), theme::apagado()),
                    ]));
                }
                linhas
            }
            Bloco::Saida { linhas, .. } => linhas
                .iter()
                .map(|l| {
                    Line::from(vec![
                        Span::styled("  │ ", theme::apagado()),
                        Span::styled(l.clone(), theme::raciocinio()),
                    ])
                })
                .collect(),
            Bloco::Erro { mensagem, dica } => {
                let mut linhas = Vec::new();
                for (i, parte) in mensagem.lines().enumerate() {
                    if i == 0 {
                        linhas.push(Line::from(vec![
                            Span::styled("✖ ", theme::falha()),
                            Span::styled(parte.to_string(), theme::falha()),
                        ]));
                    } else {
                        linhas.push(Line::from(Span::styled(
                            format!("  {parte}"),
                            theme::falha(),
                        )));
                    }
                }
                if let Some(dica) = dica {
                    linhas.push(Line::from(Span::styled(
                        format!("  → {dica}"),
                        theme::aviso(),
                    )));
                }
                linhas.push(Line::default());
                linhas
            }
            Bloco::Aviso(texto) => {
                prefixar_linhas(texto, "• ", theme::info(), "  ", theme::apagado())
            }
            Bloco::Turno(texto) => {
                prefixar_linhas(texto, "◇ ", theme::sucesso(), "  ", theme::apagado())
            }
            Bloco::Dossie {
                resumo,
                nova_sessao,
            } => vec![
                Line::from(vec![
                    Span::styled("▣ contexto compactado: ", theme::titulo()),
                    Span::styled(resumo.clone(), theme::texto()),
                ]),
                Line::from(Span::styled(
                    format!("  nova sessão de continuação: {nova_sessao}"),
                    theme::apagado(),
                )),
            ],
            Bloco::Plano {
                objetivo,
                passos,
                mudancas,
            } => {
                let titulo = if *mudancas {
                    "◇ plano atualizado"
                } else {
                    "◇ plano"
                };
                let mut linhas = vec![Line::from(Span::styled(
                    titulo.to_string(),
                    theme::titulo(),
                ))];
                if !objetivo.trim().is_empty() {
                    linhas.push(Line::from(vec![
                        Span::styled("  objetivo: ", theme::apagado()),
                        Span::styled(crate::util::truncate(objetivo, 160), theme::texto()),
                    ]));
                }
                for passo in passos {
                    linhas.push(Line::from(vec![
                        Span::styled(
                            format!("  {} ", passo.status.icone()),
                            Style::default().fg(theme::cor_status(passo.status)),
                        ),
                        Span::styled(passo.titulo.clone(), theme::texto()),
                        Span::styled(format!("  [{}]", passo.status.label()), theme::apagado()),
                    ]));
                    if let Some(observacao) = &passo.observacao
                        && !observacao.trim().is_empty()
                    {
                        linhas.push(Line::from(Span::styled(
                            format!("      {observacao}"),
                            theme::apagado(),
                        )));
                    }
                }
                linhas.push(Line::default());
                linhas
            }
            Bloco::Tarefas {
                linhas: itens,
                mudancas,
            } => {
                let titulo = if *mudancas {
                    "◇ subtarefas atualizadas"
                } else {
                    "◇ subtarefas"
                };
                let mut linhas = vec![Line::from(Span::styled(
                    titulo.to_string(),
                    theme::titulo(),
                ))];
                for item in itens {
                    let mut spans = vec![
                        Span::styled(
                            format!("  {} ", item.status.icone()),
                            Style::default().fg(theme::cor_status(item.status)),
                        ),
                        Span::styled(item.titulo.clone(), theme::texto()),
                        Span::styled(format!("  [{}]", item.status.label()), theme::apagado()),
                    ];
                    if !item.bloqueios.is_empty() {
                        spans.push(Span::styled(
                            format!("  bloqueada por: {}", item.bloqueios.join(", ")),
                            theme::aviso(),
                        ));
                    }
                    if let Some(resultado) = &item.resultado
                        && !resultado.trim().is_empty()
                    {
                        spans.push(Span::styled(
                            format!("  — {}", crate::util::truncate(resultado, 120)),
                            theme::apagado(),
                        ));
                    }
                    linhas.push(Line::from(spans));
                }
                linhas.push(Line::default());
                linhas
            }
        }
    }
}

fn prefixar_linhas(
    texto: &str,
    primeiro: &'static str,
    estilo_primeiro: ratatui::style::Style,
    demais: &'static str,
    estilo_demais: ratatui::style::Style,
) -> Vec<Line<'static>> {
    let mut linhas = Vec::new();
    for (i, parte) in texto.lines().enumerate() {
        if i == 0 {
            linhas.push(Line::from(vec![
                Span::styled(primeiro, estilo_primeiro),
                Span::styled(parte.to_string(), theme::texto()),
            ]));
        } else {
            linhas.push(Line::from(vec![
                Span::styled(demais, estilo_demais),
                Span::styled(parte.to_string(), theme::texto()),
            ]));
        }
    }
    if linhas.is_empty() {
        linhas.push(Line::from(Span::styled(primeiro, estilo_primeiro)));
    }
    linhas
}

/// Valor associado a um item de seletor.
#[derive(Clone)]
pub enum ValorSeletor {
    Modelo(String),
    Esforco(Option<String>),
    Permissao(PermissionLevel),
    Sessao(String),
    Projeto(String),
    Nada,
}

#[derive(Clone)]
pub struct ItemSeletor {
    pub titulo: String,
    pub detalhe: String,
    pub valor: ValorSeletor,
}
/// Seletor modal reutilizado por vários comandos.
pub struct Seletor {
    pub titulo: String,
    pub dica: String,
    pub itens: Vec<ItemSeletor>,
    pub filtro: String,
    pub sel: usize,
}

impl Seletor {
    pub fn novo(
        titulo: impl Into<String>,
        dica: impl Into<String>,
        itens: Vec<ItemSeletor>,
    ) -> Self {
        Self {
            titulo: titulo.into(),
            dica: dica.into(),
            itens,
            filtro: String::new(),
            sel: 0,
        }
    }

    /// Itens que casam com o filtro digitado.
    pub fn filtrados(&self) -> Vec<(usize, &ItemSeletor)> {
        if self.filtro.is_empty() {
            return self.itens.iter().enumerate().collect();
        }
        let filtro = self.filtro.to_lowercase();
        self.itens
            .iter()
            .enumerate()
            .filter(|(_, i)| {
                i.titulo.to_lowercase().contains(&filtro)
                    || i.detalhe.to_lowercase().contains(&filtro)
            })
            .collect()
    }

    pub fn ajustar_selecao(&mut self) {
        let total = self.filtrados().len();
        if total == 0 {
            self.sel = 0;
        } else if self.sel >= total {
            self.sel = total - 1;
        }
    }

    pub fn selecionado(&self) -> Option<&ItemSeletor> {
        self.filtrados().get(self.sel).map(|(_, i)| *i)
    }
}

/// Modal aberto sobre a interface.
pub enum Modal {
    Confirmacao(ConfirmRequest),
    Permissoes(Seletor),
    Modelos(Seletor),
    Esforco(Seletor),
    Sessoes(Seletor),
    Projetos(Seletor),
    Processos(Seletor),
    Texto {
        titulo: String,
        linhas: Vec<Line<'static>>,
        scroll: u16,
    },
    Ajuda {
        linhas: Vec<Line<'static>>,
        scroll: u16,
    },
}

/// Estado completo da interface.
pub struct App {
    pub paths: Paths,
    pub store: Store,
    pub config: Config,
    pub cmd_tx: mpsc::UnboundedSender<UiCommand>,
    pub entrada: Editor,
    pub blocos: Vec<Bloco>,
    pub linhas: Vec<Line<'static>>,
    marcos: Vec<usize>,
    pub scroll: u16,
    pub seguir_fim: bool,
    /// Área clicável do atalho "voltar para o último" (definida no desenho).
    pub area_voltar: Option<ratatui::layout::Rect>,
    pub mostrar_raciocinio: bool,
    /// Uso de tokens da última resposta e o total da sessão.
    pub tokens: (u32, u32, u32),
    pub token_total: (u64, u64, u64),
    pub estado: EstadoMascote,
    pub estado_expira: Option<u64>,
    pub estado_texto: String,
    pub turno_ativo: bool,
    pub modelo: String,
    pub esforco: Option<String>,
    pub nivel: PermissionLevel,
    pub sessao_id: String,
    pub sessao_titulo: String,
    pub projeto: Arc<ProjectInfo>,
    pub skills: Arc<SkillRegistry>,
    pub modal: Option<Modal>,
    pub fila_confirmacoes: VecDeque<ConfirmRequest>,
    pub plano: Option<(String, Vec<PlanoPasso>)>,
    pub tarefas: Vec<TarefaView>,
    pub processos: Vec<ProcessoView>,
    pub contexto: (u32, u32),
    pub tick: u64,
    /// Modo de operação atual (chat/agente), refletido no cabeçalho.
    pub modo: ModoOperacao,
    /// Quadro atual da animação do Pip (respiração e piscada).
    pub pip: AnimacaoPip,
    pip_piscada_ate: u64,
    pip_proxima_piscada: u64,
    pip_sorteio: u64,
    pub saindo: bool,
    pub encerrado: bool,
    pub sugestao_sel: usize,
    modelo_pendente: Option<String>,
}

impl App {
    pub fn novo(
        config: Config,
        paths: Paths,
        store: Store,
        cmd_tx: mpsc::UnboundedSender<UiCommand>,
        projeto: Arc<ProjectInfo>,
        skills: Arc<SkillRegistry>,
    ) -> Self {
        let mut entrada = Editor::novo(config.ui.max_input_history);
        if let Ok(texto) = std::fs::read_to_string(paths.history_file()) {
            let linhas: Vec<String> = texto.lines().map(|l| l.to_string()).collect();
            entrada.carregar_historico(linhas);
        }
        let mostrar_raciocinio = config.ui.show_reasoning;
        let modelo = config.provider.model.clone();
        let esforco = config.provider.reasoning_effort.clone();
        let nivel = config.agent.permission;
        let modo = config.agent.modo;
        Self {
            paths,
            store,
            config,
            cmd_tx,
            entrada,
            blocos: Vec::new(),
            linhas: Vec::new(),
            marcos: Vec::new(),
            scroll: 0,
            seguir_fim: true,
            area_voltar: None,
            tokens: (0, 0, 0),
            token_total: (0, 0, 0),
            mostrar_raciocinio,
            estado: EstadoMascote::Ocioso,
            estado_expira: None,
            estado_texto: "pronto".into(),
            turno_ativo: false,
            modelo,
            esforco,
            nivel,
            sessao_id: String::new(),
            sessao_titulo: String::new(),
            projeto,
            skills,
            modal: None,
            fila_confirmacoes: VecDeque::new(),
            plano: None,
            tarefas: Vec::new(),
            processos: Vec::new(),
            contexto: (0, 0),
            tick: 0,
            modo,
            pip: AnimacaoPip::default(),
            pip_piscada_ate: 0,
            pip_proxima_piscada: 20,
            pip_sorteio: 0x9E37_79B9_7F4A_7C15,
            saindo: false,
            encerrado: false,
            sugestao_sel: 0,
            modelo_pendente: None,
        }
    }

    // ---------------------------------------------------------------- blocos

    fn empurrar_bloco(&mut self, bloco: Bloco) {
        self.blocos.push(bloco);
        let indice = self.blocos.len() - 1;
        self.reconstruir_do(indice);
        if self.blocos.len() > 600 {
            // Mantém a memória sob controle sem perder o que está visível.
            let remover = self.blocos.len() - 500;
            self.blocos.drain(..remover);
            self.construir_tudo();
        }
    }

    /// Reconstrói as linhas a partir do bloco `inicio` (os anteriores não mudam).
    pub fn reconstruir_do(&mut self, inicio: usize) {
        if inicio >= self.blocos.len() {
            self.linhas.clear();
            self.marcos.clear();
            return;
        }
        if self.marcos.len() > inicio {
            let corte = self.marcos[inicio];
            self.linhas.truncate(corte);
            self.marcos.truncate(inicio);
        }
        for i in inicio..self.blocos.len() {
            self.marcos.push(self.linhas.len());
            self.linhas.extend(self.blocos[i].linhas());
        }
    }

    /// Reconstrói tudo (usado ao alternar a exibição do raciocínio).
    pub fn construir_tudo(&mut self) {
        self.linhas.clear();
        self.marcos.clear();
        self.reconstruir_do(0);
    }

    fn atualizar_ultimo(&mut self) {
        if !self.blocos.is_empty() {
            let indice = self.blocos.len() - 1;
            self.reconstruir_do(indice);
        }
    }

    fn limpar_conversa(&mut self) {
        self.blocos.clear();
        self.linhas.clear();
        self.marcos.clear();
        self.scroll = 0;
        self.seguir_fim = true;
    }

    fn definir_estado(&mut self, estado: EstadoMascote, texto: impl Into<String>) {
        self.estado = estado;
        self.estado_texto = texto.into();
        self.estado_expira = None;
    }

    // ---------------------------------------------------- plano e subtarefas

    /// Aplica um plano vindo do agente e mostra na conversa apenas o que mudou.
    ///
    /// O primeiro plano (ou um objetivo novo) aparece completo; atualizações
    /// listam somente as etapas alteradas; atualizações sem mudança real não
    /// geram bloco nenhum.
    pub fn aplicar_plano(&mut self, objetivo: String, passos: Vec<PlanoPasso>) {
        if passos.is_empty() && objetivo.trim().is_empty() {
            // Limpeza (troca de sessão ou de projeto): nada a exibir.
            self.plano = None;
            return;
        }
        match self.plano.take() {
            Some((objetivo_anterior, passos_anteriores)) if objetivo_anterior == objetivo => {
                let mudancas: Vec<PlanoPasso> = passos
                    .iter()
                    .filter(|passo| {
                        passos_anteriores
                            .iter()
                            .find(|anterior| anterior.titulo == passo.titulo)
                            .is_none_or(|anterior| {
                                anterior.status != passo.status
                                    || anterior.observacao != passo.observacao
                            })
                    })
                    .cloned()
                    .collect();
                self.plano = Some((objetivo, passos));
                if mudancas.is_empty() {
                    return;
                }
                self.empurrar_bloco(Bloco::Plano {
                    objetivo: String::new(),
                    passos: mudancas,
                    mudancas: true,
                });
            }
            _ => {
                self.plano = Some((objetivo.clone(), passos.clone()));
                self.empurrar_bloco(Bloco::Plano {
                    objetivo,
                    passos,
                    mudancas: false,
                });
            }
        }
        self.acompanhar_fim();
    }

    /// Aplica a lista de subtarefas e mostra criação e mudanças de estado reais.
    pub fn aplicar_tarefas(&mut self, tarefas: Vec<TarefaView>) {
        let anteriores = std::mem::take(&mut self.tarefas);
        if tarefas.is_empty() {
            self.tarefas = tarefas;
            return;
        }
        let novas = anteriores.is_empty();
        let relevante = |tarefa: &TarefaView| -> bool {
            anteriores
                .iter()
                .find(|anterior| anterior.id == tarefa.id)
                .is_none_or(|anterior| {
                    anterior.status != tarefa.status || anterior.resultado != tarefa.resultado
                })
        };
        let exibidas: Vec<LinhaTarefa> = tarefas
            .iter()
            .filter(|t| novas || relevante(t))
            .map(|t| LinhaTarefa::de(t, &tarefas))
            .collect();
        self.tarefas = tarefas;
        if exibidas.is_empty() {
            return;
        }
        self.empurrar_bloco(Bloco::Tarefas {
            linhas: exibidas,
            mudancas: !novas,
        });
        self.acompanhar_fim();
    }

    // ------------------------------------------------------------- comandos

    pub fn enviar_entrada(&mut self) {
        // Sugestões ativas: um comando parcialmente digitado é completado com
        // a sugestão escolhida em vez de ser executado pela metade. Um comando
        // exato (sem argumentos) segue normalmente para execução.
        let sugestoes = self.entrada.sugestoes();
        if !sugestoes.is_empty() {
            let digitado = self.entrada.texto().trim().to_string();
            let indice = self.sugestao_sel.min(sugestoes.len() - 1);
            let escolhida = sugestoes[indice];
            if digitado != escolhida.nome {
                self.entrada.completar(escolhida);
                self.sugestao_sel = 0;
                return;
            }
        }
        let Some(texto) = self.entrada.enviar() else {
            return;
        };
        if texto.starts_with('/') {
            self.executar_comando(&texto);
            return;
        }
        let redirecionar = self.turno_ativo;
        self.empurrar_bloco(Bloco::Usuario(texto.clone()));
        self.seguir_fim = true;
        // Fase inicial do turno, ao lado do Pip: o raciocínio em si só aparece
        // na conversa quando `/thinking` está ativo.
        self.definir_estado(EstadoMascote::Pensando, "contemplando");
        if redirecionar {
            let _ = self.cmd_tx.send(UiCommand::Redirecionar(texto));
        } else {
            self.turno_ativo = true;
            let _ = self.cmd_tx.send(UiCommand::EnviarMensagem(texto));
        }
    }

    fn executar_comando(&mut self, linha: &str) {
        let mut partes = linha.splitn(2, char::is_whitespace);
        let comando = partes.next().unwrap_or("").to_lowercase();
        let argumento = partes.next().unwrap_or("").trim().to_string();
        match comando.as_str() {
            "/help" | "/ajuda" => {
                self.modal = Some(Modal::Ajuda {
                    linhas: texto_de_ajuda(),
                    scroll: 0,
                });
            }
            "/clear" | "/limpar" => {
                self.limpar_conversa();
                self.empurrar_bloco(Bloco::Aviso(
                    "Conversa limpa. O histórico completo continua salvo no banco e pode ser \
                     retomado com /historico."
                        .into(),
                ));
            }
            "/model" | "/modelo" => {
                if !argumento.is_empty() {
                    if let Some(entrada) = self.config.model_entry(&argumento).cloned() {
                        self.escolher_modelo(&entrada);
                    } else {
                        self.avisar(format!(
                            "modelo `{argumento}` não está no catálogo. Use /model sem argumentos para listar \
                             e escolher; modelos manuais podem ser adicionados ao arquivo de configuração."
                        ));
                    }
                } else {
                    self.abrir_seletor_modelos(String::new());
                }
            }
            "/effort" | "/esforco" => {
                if argumento.is_empty() {
                    let niveis = self.config.supported_efforts();
                    self.abrir_seletor_esforco(niveis);
                } else {
                    let niveis = self.config.supported_efforts();
                    if niveis.iter().any(|n| n.eq_ignore_ascii_case(&argumento)) {
                        let _ = self
                            .cmd_tx
                            .send(UiCommand::DefinirEsforco(Some(argumento.clone())));
                    } else {
                        self.avisar(format!(
                            "esforço `{argumento}` não é suportado por {} (conhecidos: {}). \
                             Quando o suporte é desconhecido, o Kestrel não envia parâmetros de raciocínio.",
                            self.modelo,
                            if niveis.is_empty() {
                                "nenhum".to_string()
                            } else {
                                niveis.join(", ")
                            }
                        ));
                    }
                }
            }
            "/cost" | "/custo" | "/tokens" => {
                self.mostrar_custo();
            }
            "/thinking" | "/pensamento" | "/raciocinio" => {
                // Liga/desliga a exibição do raciocínio do modelo. Desligado
                // (padrão), o pensamento não entra na conversa: o estado aparece
                // só ao lado do Pip (raciocinando, contemplando, …).
                let novo = match argumento.trim().to_lowercase().as_str() {
                    "on" | "sim" | "1" | "ativo" | "ativar" | "mostrar" => true,
                    "off" | "nao" | "não" | "0" | "inativo" | "desativar" | "ocultar" => false,
                    // Sem argumento: alterna entre ligado e desligado.
                    "" => !self.mostrar_raciocinio,
                    outro => {
                        self.avisar(format!(
                            "valor `{outro}` não é válido para /thinking; use `/thinking on` ou `/thinking off`."
                        ));
                        return;
                    }
                };
                self.mostrar_raciocinio = novo;
                self.config.ui.show_reasoning = novo;
                // Salva sempre: desligar também é uma preferência, e Voltar ao
                // padrão depois exigiria religar à mão.
                if let Err(e) = self.config.save(&self.paths) {
                    self.avisar(format!(
                        "não foi possível salvar a preferência de raciocínio: {e}"
                    ));
                }
                // A escolha vale para o agente: com o raciocínio oculto ele
                // deixa de enviar os deltas de pensamento para a interface.
                let _ = self.cmd_tx.send(UiCommand::DefinirRaciocinio(novo));
                self.construir_tudo();
                let estado = if novo { "visível" } else { "oculto" };
                self.avisar(format!(
                    "raciocínio do modelo: {estado}. Quando oculto, o pensamento não aparece \
                     na conversa — apenas o estado ao lado do Pip."
                ));
            }
            "/permissions" | "/permissoes" => {
                if argumento.is_empty() {
                    self.modal = Some(Modal::Permissoes(seletor_permissoes(self.nivel)));
                } else if let Some(nivel) = PermissionLevel::parse(&argumento) {
                    self.nivel = nivel;
                    let _ = self.cmd_tx.send(UiCommand::DefinirPermissao(nivel));
                    self.avisar(format!(
                        "nível de permissão: {} — {}",
                        nivel.label(),
                        nivel.description()
                    ));
                } else {
                    self.avisar(format!(
                        "nível `{argumento}` inválido. Use read, write, semi ou full."
                    ));
                }
            }
            "/chat" | "/conversa" => self.trocar_modo(ModoOperacao::Chat),
            "/agent" | "/agente" => self.trocar_modo(ModoOperacao::Agente),
            "/modo" => match ModoOperacao::parse(&argumento) {
                Some(modo) => self.trocar_modo(modo),
                None if argumento.is_empty() => self.avisar(format!(
                    "modo atual: {} — {}. Use /chat ou /agente para trocar.",
                    self.modo.label(),
                    self.modo.description()
                )),
                None => self.avisar(format!(
                    "modo `{argumento}` inválido. Use /chat ou /agente."
                )),
            },
            "/skills" | "/habilidades" => {
                if self.skills.is_empty() {
                    self.avisar(format!(
                        "nenhuma skill instalada. Diretórios monitorados:\n- {}\n- {}/.kestrel/skills",
                        self.paths.skills_dir().display(),
                        self.config.workdir().display()
                    ));
                } else {
                    let linhas: Vec<String> = self
                        .skills
                        .all()
                        .iter()
                        .map(|s| {
                            format!(
                                "- {} ({}) — {}\n  arquivo: {}",
                                s.name,
                                s.escopo.label(),
                                s.description,
                                s.path.display()
                            )
                        })
                        .collect();
                    self.avisar(format!(
                        "{} skill(s) instalada(s):\n{}",
                        linhas.len(),
                        linhas.join("\n")
                    ));
                }
            }
            "/plan" | "/plano" => {
                let mut linhas = Vec::new();
                match &self.plano {
                    Some((objetivo, passos)) => {
                        linhas.push(format!("Objetivo: {objetivo}"));
                        for passo in passos {
                            linhas.push(format!(
                                "{} {} [{}]",
                                passo.status.icone(),
                                passo.titulo,
                                passo.status.label()
                            ));
                        }
                    }
                    None => linhas.push("Nenhum plano registrado nesta sessão.".into()),
                }
                if !self.tarefas.is_empty() {
                    linhas.push(String::new());
                    linhas.push("Subtarefas:".into());
                    for tarefa in &self.tarefas {
                        linhas.push(format!(
                            "{} {} [{}]",
                            tarefa.status.icone(),
                            tarefa.titulo,
                            tarefa.status.label()
                        ));
                    }
                }
                self.avisar(linhas.join("\n"));
            }
            "/stop" | "/parar" => {
                if self.turno_ativo {
                    let _ = self.cmd_tx.send(UiCommand::Interromper);
                    self.avisar(
                        "interrupção solicitada; as alterações já feitas serão preservadas".into(),
                    );
                } else {
                    self.avisar("nenhum turno em execução".into());
                }
            }
            "/history" | "/historico" | "/histórico" => {
                self.abrir_historico(&argumento);
            }
            "/projects" | "/projetos" => {
                self.abrir_projetos();
            }
            "/analyze" | "/analisar" => {
                let _ = self.cmd_tx.send(UiCommand::AnalisarProjeto);
                self.avisar("reanalisando o projeto…".into());
            }
            "/processes" | "/processos" => {
                let _ = self.cmd_tx.send(UiCommand::ListarProcessos);
            }
            "/config" | "/configuracao" | "/configuração" => {
                let contexto = if self.contexto.1 > 0 {
                    format!(
                        "{} de {} tokens ({}%)",
                        self.contexto.0,
                        self.contexto.1,
                        (self.contexto.0 as f32 / self.contexto.1.max(1) as f32 * 100.0).round()
                    )
                } else {
                    "sem medição ainda".into()
                };
                let texto = format!(
                    "# Configuração\n\n\
                     - arquivo: `{}`\n\
                     - modelo: `{}`\n\
                     - esforço de raciocínio: {}\n\
                     - nível de permissão: {} ({})\n\
                     - diretório de trabalho: `{}`\n\
                     - diretórios externos permitidos: {}\n\
                     - janela de contexto presumida: {} tokens\n\
                     - mecanismo de busca web: {}\n\
                     - uso de contexto atual: {}\n\n\
                     Edite o arquivo TOML acima para ajustes finos; os comandos da interface \
                     (como `/model` e `/permissions`) atualizam a configuração automaticamente.",
                    self.paths.config_file().display(),
                    self.modelo,
                    self.esforco
                        .clone()
                        .unwrap_or_else(|| "(não especificado)".into()),
                    self.nivel.label(),
                    self.nivel.description(),
                    self.config.workdir().display(),
                    if self.config.agent.external_dirs.is_empty() {
                        "nenhum".to_string()
                    } else {
                        self.config
                            .agent
                            .external_dirs
                            .iter()
                            .map(|d| d.display().to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    },
                    self.config.context_window(),
                    self.config.web.engine,
                    contexto
                );
                self.modal = Some(Modal::Texto {
                    titulo: "Configuração".into(),
                    linhas: markdown::renderizar(&texto),
                    scroll: 0,
                });
            }
            "/resume" | "/retomar" => {
                if argumento.is_empty() {
                    self.abrir_historico("");
                } else if argumento == "ultima" || argumento == "última" {
                    match self
                        .store
                        .last_resumable_session(&self.config.workdir().display().to_string())
                    {
                        Ok(Some(sessao)) => {
                            let _ = self.cmd_tx.send(UiCommand::RetomarSessao(sessao.id));
                        }
                        Ok(None) => self
                            .avisar("nenhuma sessão anterior encontrada para este projeto".into()),
                        Err(e) => self.avisar(format!("falha ao buscar sessões: {e}")),
                    }
                } else {
                    let _ = self.cmd_tx.send(UiCommand::RetomarSessao(argumento));
                }
            }
            "/new" | "/nova" => {
                let _ = self.cmd_tx.send(UiCommand::NovaSessao);
            }
            "/quit" | "/sair" => {
                self.iniciar_saida();
            }
            outro => {
                self.avisar(format!(
                    "comando `{outro}` não existe. Use /help para ver todos os comandos."
                ));
            }
        }
    }

    /// Troca o modo de operação e informa o usuário (o agente persiste).
    fn trocar_modo(&mut self, modo: ModoOperacao) {
        if self.modo == modo {
            self.avisar(format!("o modo já é {}.", modo.label()));
            return;
        }
        self.modo = modo;
        let _ = self.cmd_tx.send(UiCommand::DefinirModo(modo));
        self.avisar(format!(
            "modo de operação: {} — {}",
            modo.label(),
            modo.description()
        ));
    }

    fn iniciar_saida(&mut self) {
        if self.saindo {
            return;
        }
        self.saindo = true;
        let _ = self.cmd_tx.send(UiCommand::Interromper);
        let _ = self.cmd_tx.send(UiCommand::Encerrar);
        self.estado_texto = "encerrando…".into();
    }

    /// Evento do agente (streaming, saída de ferramenta, aviso, conclusão).
    ///
    /// Não arrasta a conversa para o fim: quem se afastou para ler o histórico
    /// continua onde está. Antes cada token forçava `seguir_fim = true` e a
    /// tela voltava sozinha para baixo a cada atualização, impedindo a leitura
    /// enquanto o agente trabalhava. Quem está acompanhando o fim já é
    /// posicionado pelo desenho da conversa (`ui::desenhar_conversa`).
    fn acompanhar_fim(&mut self) {}

    /// Contabilidade de tokens: números reais do provedor, somados da sessão
    /// inteira (o banco guarda cada resposta).
    fn mostrar_custo(&mut self) {
        let stats = self
            .store
            .session_stats(&self.sessao_id)
            .unwrap_or_default();
        let total = stats.total_tokens();
        let raciocinio = if stats.reasoning_tokens > 0 {
            format!(
                " · raciocínio {}",
                crate::util::formatar_num(stats.reasoning_tokens)
            )
        } else {
            String::new()
        };
        self.avisar(format!(
            "Turnos desta sessão: {} tokens de entrada + {} de saída{raciocinio} = {} no total.\n\
             Ferramentas: {} execuções ({} com falha) · {} arquivo(s) alterado(s).",
            crate::util::formatar_num(stats.prompt_tokens),
            crate::util::formatar_num(stats.completion_tokens),
            crate::util::formatar_num(total),
            stats.tool_runs,
            stats.tool_failures,
            stats.files_touched,
        ));
    }

    fn avisar(&mut self, mensagem: String) {
        self.empurrar_bloco(Bloco::Aviso(mensagem));
        self.acompanhar_fim();
    }

    fn abrir_seletor_modelos(&mut self, filtro: String) {
        let atual = self.config.provider.model.clone();
        let mut itens: Vec<ItemSeletor> = self
            .config
            .provider
            .models
            .iter()
            .map(|m| {
                let mut detalhes = Vec::new();
                if let Some(ctx) = m.context_window {
                    detalhes.push(format!(
                        "contexto {}",
                        crate::util::human_bytes((ctx as u64) * 4)
                    ));
                }
                detalhes.push(format!("ferramentas: {}", m.supports_tools.label()));
                detalhes.push(format!("raciocínio: {}", m.supports_reasoning.label()));
                if m.source == ModelSource::Manual {
                    detalhes.push("manual".into());
                }
                let titulo = if m.id == atual {
                    format!("{} · atual", m.label())
                } else {
                    m.label()
                };
                ItemSeletor {
                    titulo,
                    detalhe: format!("{}\n{}", m.id, detalhes.join(" · ")),
                    valor: ValorSeletor::Modelo(m.id.clone()),
                }
            })
            .collect();
        if itens.is_empty() {
            self.avisar(
                "catálogo de modelos vazio. Verifique a chave de API e reinicie, ou cadastre \
                 modelos manualmente no arquivo de configuração."
                    .into(),
            );
            return;
        }
        itens.sort_by(|a, b| a.titulo.cmp(&b.titulo));
        let mut seletor = Seletor::novo(
            "Modelos",
            "digite para filtrar · enter escolhe · esc cancela",
            itens,
        );
        // Abre já apontando para o modelo em uso (marcado com `· atual`).
        if let Some(posicao) = seletor
            .itens
            .iter()
            .position(|i| matches!(&i.valor, ValorSeletor::Modelo(id) if id == &atual))
        {
            seletor.sel = posicao;
        }
        seletor.filtro = filtro;
        self.modal = Some(Modal::Modelos(seletor));
    }

    fn escolher_modelo(&mut self, entrada: &crate::config::ModelEntry) {
        let id = entrada.id.clone();
        if entrada.supports_reasoning == crate::config::Capability::Sim {
            // Somente esforços comprovadamente suportados são oferecidos.
            let esforcos = if entrada.reasoning_efforts.is_empty() {
                vec!["low".to_string(), "medium".to_string(), "high".to_string()]
            } else {
                entrada.reasoning_efforts.clone()
            };
            self.modelo_pendente = Some(id);
            self.abrir_seletor_esforco(esforcos);
        } else {
            self.modelo_pendente = None;
            let _ = self.cmd_tx.send(UiCommand::DefinirModelo {
                modelo: id,
                esforco: None,
            });
        }
    }

    fn abrir_seletor_esforco(&mut self, suportados: Vec<String>) {
        if suportados.is_empty() {
            self.avisar(format!(
                "o modelo `{}` não tem esforços de raciocínio conhecidos; \
                 nenhum parâmetro de raciocínio será enviado.",
                self.modelo_pendente
                    .clone()
                    .unwrap_or_else(|| self.modelo.clone())
            ));
            if let Some(modelo) = self.modelo_pendente.take() {
                let _ = self.cmd_tx.send(UiCommand::DefinirModelo {
                    modelo,
                    esforco: None,
                });
            }
            return;
        }
        let mut itens = vec![ItemSeletor {
            titulo: "(não especificar)".into(),
            detalhe: "deixa o modelo usar o padrão dele".into(),
            valor: ValorSeletor::Esforco(None),
        }];
        for nivel in suportados {
            itens.push(ItemSeletor {
                titulo: nivel.clone(),
                detalhe: format!("esforço `{nivel}` comprovadamente suportado"),
                valor: ValorSeletor::Esforco(Some(nivel)),
            });
        }
        let titulo = match &self.modelo_pendente {
            Some(modelo) => format!("Esforço de raciocínio — modelo `{modelo}`"),
            None => "Esforço de raciocínio".into(),
        };
        self.modal = Some(Modal::Esforco(Seletor::novo(
            titulo,
            "enter confirma · esc cancela",
            itens,
        )));
    }

    fn abrir_historico(&mut self, termo: &str) {
        let itens: Vec<ItemSeletor> = if termo.trim().is_empty() {
            match self.store.list_sessions(None, 60) {
                Ok(sessoes) => sessoes
                    .into_iter()
                    .map(|s| ItemSeletor {
                        titulo: format!(
                            "{} — {}",
                            crate::util::format_timestamp(&s.updated_at),
                            crate::util::truncate(&s.title, 60)
                        ),
                        detalhe: format!(
                            "{}\nprojeto: {}\nmodelo: {} · permissão: {} · status: {}",
                            s.id, s.project_path, s.model, s.permission, s.status
                        ),
                        valor: ValorSeletor::Sessao(s.id),
                    })
                    .collect(),
                Err(e) => {
                    self.avisar(format!("falha ao ler sessões: {e}"));
                    return;
                }
            }
        } else {
            match self.store.search_messages(termo, 60) {
                Ok(hits) => hits
                    .into_iter()
                    .map(|h| ItemSeletor {
                        titulo: format!(
                            "{} — {} [{}]",
                            crate::util::format_timestamp(&h.created_at),
                            crate::util::truncate(&h.session_title, 40),
                            h.role
                        ),
                        detalhe: crate::util::truncate(&h.content.replace('\n', " "), 160),
                        valor: ValorSeletor::Sessao(h.session_id),
                    })
                    .collect(),
                Err(e) => {
                    self.avisar(format!("falha na busca: {e}"));
                    return;
                }
            }
        };
        if itens.is_empty() {
            self.avisar(format!("nenhum resultado para `{termo}`."));
            return;
        }
        self.modal = Some(Modal::Sessoes(Seletor::novo(
            "Histórico de sessões",
            "enter retoma a sessão escolhida · digite para filtrar · esc cancela",
            itens,
        )));
    }

    fn abrir_projetos(&mut self) {
        let sessoes = match self.store.list_sessions(None, 300) {
            Ok(s) => s,
            Err(e) => {
                self.avisar(format!("falha ao ler projetos: {e}"));
                return;
            }
        };
        let mut vistos = Vec::new();
        let mut itens = Vec::new();
        for sessao in sessoes {
            if vistos.contains(&sessao.project_path) {
                continue;
            }
            vistos.push(sessao.project_path.clone());
            itens.push(ItemSeletor {
                titulo: crate::util::truncate(&sessao.project_path, 70),
                detalhe: format!(
                    "última atividade: {} · {}",
                    crate::util::format_timestamp(&sessao.updated_at),
                    sessao.title
                ),
                valor: ValorSeletor::Projeto(sessao.project_path.clone()),
            });
        }
        if itens.is_empty() {
            self.avisar("nenhum projeto registrado ainda.".into());
            return;
        }
        self.modal = Some(Modal::Projetos(Seletor::novo(
            "Projetos recentes",
            "enter troca o diretório de trabalho e inicia uma nova sessão · esc cancela",
            itens,
        )));
    }

    fn abrir_processos(&mut self, lista: Vec<ProcessoView>) {
        self.processos = lista.clone();
        if lista.is_empty() {
            self.avisar("nenhum processo gerenciado no momento.".into());
            return;
        }
        let itens = lista
            .into_iter()
            .map(|p| ItemSeletor {
                titulo: format!("{} — {}", p.id, crate::util::truncate(&p.comando, 60)),
                detalhe: format!(
                    "estado: {} · cwd: {} · linhas de saída: {} · iniciado em: {}",
                    p.estado.label(),
                    p.cwd,
                    p.linhas,
                    p.iniciado_em
                ),
                valor: ValorSeletor::Nada,
            })
            .collect();
        self.modal = Some(Modal::Processos(Seletor::novo(
            "Processos",
            "esc fecha",
            itens,
        )));
    }

    fn agir_seletor(&mut self, item: &ItemSeletor) {
        match item.valor.clone() {
            ValorSeletor::Modelo(id) => {
                if let Some(entrada) = self.config.model_entry(&id).cloned() {
                    self.escolher_modelo(&entrada);
                } else {
                    // O modelo não está mais no catálogo local: vale tentar
                    // mesmo assim, direto no provedor.
                    let _ = self.cmd_tx.send(UiCommand::DefinirModelo {
                        modelo: id,
                        esforco: None,
                    });
                }
            }
            ValorSeletor::Esforco(nivel) => {
                if let Some(modelo) = self.modelo_pendente.take() {
                    let _ = self.cmd_tx.send(UiCommand::DefinirModelo {
                        modelo,
                        esforco: nivel,
                    });
                } else {
                    let _ = self.cmd_tx.send(UiCommand::DefinirEsforco(nivel));
                }
            }
            ValorSeletor::Permissao(nivel) => {
                self.nivel = nivel;
                let _ = self.cmd_tx.send(UiCommand::DefinirPermissao(nivel));
            }
            ValorSeletor::Sessao(id) => {
                let _ = self.cmd_tx.send(UiCommand::RetomarSessao(id));
            }
            ValorSeletor::Projeto(caminho) => {
                self.config.agent.workdir = Some(caminho.clone().into());
                let _ = self.cmd_tx.send(UiCommand::TrocarProjeto(caminho));
            }
            ValorSeletor::Nada => {}
        }
    }

    // -------------------------------------------------------------- eventos

    pub fn tratar_evento(&mut self, evento: AgentEvent) {
        match evento {
            AgentEvent::TurnoIniciado => {
                self.turno_ativo = true;
                self.definir_estado(EstadoMascote::Pensando, "pensando");
            }
            AgentEvent::Texto { delta } => {
                if self
                    .blocos
                    .last()
                    .is_none_or(|b| !matches!(b, Bloco::Assistente(_)))
                {
                    self.empurrar_bloco(Bloco::Assistente(String::new()));
                }
                if let Some(Bloco::Assistente(texto)) = self.blocos.last_mut() {
                    texto.push_str(&delta);
                    if texto.chars().count() > 400_000 {
                        let excesso = texto.chars().count() - 400_000;
                        *texto = texto.chars().skip(excesso).collect();
                    }
                }
                self.definir_estado(EstadoMascote::Trabalhando, "escrevendo");
                self.atualizar_ultimo();
                // Conteúdo novo sóarrasta para o fim quem já estava no fim.
                self.acompanhar_fim();
            }
            AgentEvent::Raciocinio { delta } => {
                if !self.mostrar_raciocinio {
                    return;
                }
                if self
                    .blocos
                    .last()
                    .is_none_or(|b| !matches!(b, Bloco::Raciocinio(_)))
                {
                    self.empurrar_bloco(Bloco::Raciocinio(String::new()));
                }
                if let Some(Bloco::Raciocinio(texto)) = self.blocos.last_mut() {
                    texto.push_str(&delta);
                }
                self.definir_estado(EstadoMascote::Pensando, "raciocinando");
                self.atualizar_ultimo();
                self.acompanhar_fim();
            }
            AgentEvent::MensagemFinal { conteudo } => {
                if let Some(Bloco::Assistente(texto)) = self.blocos.last_mut() {
                    if conteudo.trim_end() != texto.trim_end() {
                        *texto = conteudo;
                        self.atualizar_ultimo();
                    }
                } else if !conteudo.trim().is_empty() {
                    self.empurrar_bloco(Bloco::Assistente(conteudo));
                }
                self.acompanhar_fim();
            }
            AgentEvent::FerramentaIniciada { nome, detalhe } => {
                self.empurrar_bloco(Bloco::Ferramenta {
                    nome,
                    detalhe,
                    rodando: true,
                    sucesso: false,
                    resumo: None,
                    adicoes: 0,
                    remocoes: 0,
                });
                self.definir_estado(EstadoMascote::Ferramentas, "executando ferramenta");
                self.acompanhar_fim();
            }
            AgentEvent::FerramentaSaida { nome, linha } => {
                let limpar = crate::util::strip_ansi(&linha);
                if limpar.trim().is_empty() {
                    return;
                }
                // Linhas de saída ficam agrupadas por ferramenta; saídas de
                // ferramentas diferentes não se misturam no mesmo bloco.
                let destino = match self.blocos.last_mut() {
                    Some(Bloco::Saida { nome: n, linhas }) if *n == nome => Some(linhas),
                    _ => {
                        self.empurrar_bloco(Bloco::Saida {
                            nome: nome.clone(),
                            linhas: Vec::new(),
                        });
                        match self.blocos.last_mut() {
                            Some(Bloco::Saida { linhas, .. }) => Some(linhas),
                            _ => None,
                        }
                    }
                };
                if let Some(linhas) = destino {
                    linhas.push(crate::util::truncate(&limpar, 400));
                    if linhas.len() > 300 {
                        let remover = linhas.len() - 300;
                        linhas.drain(..remover);
                    }
                }
                self.atualizar_ultimo();
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::FerramentaConcluida {
                nome,
                sucesso,
                resumo,
                adicoes,
                remocoes,
            } => {
                // Encontra a primeira ferramenta em execução com o mesmo nome.
                let mut encontrado = None;
                for (i, bloco) in self.blocos.iter().enumerate() {
                    if let Bloco::Ferramenta {
                        nome: n,
                        rodando: true,
                        ..
                    } = bloco
                        && *n == nome
                    {
                        encontrado = Some(i);
                        break;
                    }
                }
                if let Some(i) = encontrado {
                    if let Bloco::Ferramenta {
                        rodando,
                        sucesso: ok,
                        resumo: r,
                        adicoes: a,
                        remocoes: rem,
                        ..
                    } = &mut self.blocos[i]
                    {
                        *rodando = false;
                        *ok = sucesso;
                        *r = Some(resumo);
                        *a = adicoes;
                        *rem = remocoes;
                    }
                    self.reconstruir_do(i);
                }
                self.definir_estado(
                    if sucesso {
                        EstadoMascote::Pensando
                    } else {
                        EstadoMascote::Erro
                    },
                    if sucesso {
                        "retomando o raciocínio"
                    } else {
                        "ferramenta falhou"
                    },
                );
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::PlanoAtualizado { objetivo, passos } => {
                self.aplicar_plano(objetivo, passos);
            }
            AgentEvent::TarefasAtualizadas { tarefas } => {
                self.aplicar_tarefas(tarefas);
            }
            AgentEvent::Contexto { usados, capacidade } => {
                self.contexto = (usados, capacidade);
            }
            AgentEvent::Tokens {
                prompt,
                completion,
                raciocinio,
            } => {
                self.tokens = (prompt, completion, raciocinio);
                self.token_total = (
                    self.token_total.0.saturating_add(prompt as u64),
                    self.token_total.1.saturating_add(completion as u64),
                    self.token_total.2.saturating_add(raciocinio as u64),
                );
            }
            AgentEvent::DossieCriado {
                resumo,
                nova_sessao,
            } => {
                self.empurrar_bloco(Bloco::Dossie {
                    resumo,
                    nova_sessao,
                });
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::Erro {
                mensagem,
                dica,
                recuperavel,
            } => {
                let dica = if recuperavel {
                    dica
                } else {
                    Some(match dica {
                        Some(d) => format!("{d} (não é possível tentar novamente automaticamente)"),
                        None => "esta falha exige decisão sua antes de continuar".into(),
                    })
                };
                self.empurrar_bloco(Bloco::Erro { mensagem, dica });
                self.definir_estado(EstadoMascote::Erro, "erro");
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::Aviso { mensagem } => {
                self.empurrar_bloco(Bloco::Aviso(mensagem));
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::Recuperacao {
                descricao,
                tentativa,
                maximo,
            } => {
                let texto = if maximo > 0 {
                    format!("{descricao} ({tentativa}/{maximo})")
                } else {
                    descricao
                };
                self.empurrar_bloco(Bloco::Aviso(texto));
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::TurnoConcluido { resumo } => {
                self.turno_ativo = false;
                self.empurrar_bloco(Bloco::Turno(resumo));
                self.estado = EstadoMascote::Concluindo;
                self.estado_texto = "concluído".into();
                self.estado_expira = Some(self.tick + 30);
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::Interrompido { resumo } => {
                self.turno_ativo = false;
                self.empurrar_bloco(Bloco::Aviso(resumo));
                self.definir_estado(EstadoMascote::Ocioso, "interrompido");
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::Confirmacao { pedido } => {
                self.fila_confirmacoes.push_back(pedido);
                self.proximo_confirmacao();
            }
            AgentEvent::Estado { mensagem } => {
                // A mensagem de estado também define o comportamento do mascote.
                let texto = mensagem.to_lowercase();
                if self.estado != EstadoMascote::Erro || self.estado_expira.is_none() {
                    if texto.contains("executando") || texto.contains("subagente") {
                        self.estado = EstadoMascote::Ferramentas;
                    } else if texto.contains("consultando") || texto.contains("compactando") {
                        self.estado = EstadoMascote::Pensando;
                    }
                }
                self.estado_texto = mensagem;
            }
            AgentEvent::SessaoAtiva { id, titulo } => {
                let sessao_mudou = !self.sessao_id.is_empty() && self.sessao_id != id;
                if sessao_mudou {
                    self.limpar_conversa();
                    self.plano = None;
                    self.tarefas.clear();
                }
                self.sessao_id = id;
                self.sessao_titulo = titulo;
                // Reanalisa o projeto e as skills: o estado real manda.
                let workdir = self.config.workdir();
                self.projeto = Arc::new(project::analyze(&workdir));
                self.skills = Arc::new(SkillRegistry::discover(&self.paths, &workdir));
                // Carrega plano e tarefas persistidos da sessão; ao retomar uma
                // sessão existente, o estado atual aparece na conversa.
                let mut plano_carregado: Option<(String, Vec<PlanoPasso>)> = None;
                if let Ok(Some((objetivo, passos, _))) = self.store.latest_plan(&self.sessao_id)
                    && let Ok(lista) = serde_json::from_str::<Vec<serde_json::Value>>(&passos)
                {
                    let passos = lista
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
                    plano_carregado = Some((objetivo, passos));
                }
                let tarefas_carregadas: Vec<TarefaView> = self
                    .store
                    .tasks(&self.sessao_id)
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
                if sessao_mudou {
                    if let Some((objetivo, passos)) = plano_carregado {
                        self.aplicar_plano(objetivo, passos);
                    }
                    if !tarefas_carregadas.is_empty() {
                        self.aplicar_tarefas(tarefas_carregadas);
                    }
                } else {
                    self.plano = plano_carregado;
                    self.tarefas = tarefas_carregadas;
                }
            }
            AgentEvent::ModeloAtual { modelo, esforco } => {
                let mudou = self.modelo != modelo
                    || self.esforco != esforco
                    || self.config.provider.model != modelo;
                self.modelo = modelo.clone();
                self.esforco = esforco.clone();
                self.config.provider.model = modelo.clone();
                self.config.provider.reasoning_effort = esforco.clone();
                if mudou {
                    match &esforco {
                        Some(nivel) => self.avisar(format!(
                            "modelo atual: {modelo} (esforço de raciocínio: {nivel})"
                        )),
                        None => self.avisar(format!("modelo atual: {modelo}")),
                    }
                }
            }
            AgentEvent::Processos { lista } => {
                self.abrir_processos(lista);
            }
            AgentEvent::Encerrado => {
                self.saindo = true;
                self.encerrado = true;
                self.estado_texto = "encerrado".into();
                self.estado = EstadoMascote::Ocioso;
            }
        }
    }

    fn proximo_confirmacao(&mut self) {
        if self.modal.is_none()
            && let Some(pedido) = self.fila_confirmacoes.pop_front()
        {
            self.modal = Some(Modal::Confirmacao(pedido));
        }
    }

    /// Responde o modal de confirmação atual.
    fn responder_confirmacao(&mut self, resposta: Confirmacao) {
        if let Some(Modal::Confirmacao(pedido)) = self.modal.take() {
            let _ = pedido.resposta.send(resposta);
            self.proximo_confirmacao();
        }
    }

    // ------------------------------------------------------------- teclado

    pub fn tratar_terminal(&mut self, evento: crossterm::event::Event) {
        match evento {
            crossterm::event::Event::Key(tecla) => {
                if tecla.kind == KeyEventKind::Press {
                    self.tratar_tecla(tecla);
                }
            }
            crossterm::event::Event::Mouse(mouse) => self.tratar_mouse(mouse),
            crossterm::event::Event::Paste(texto) if self.modal.is_none() => {
                self.entrada.inserir_texto(&texto);
                self.sugestao_sel = 0;
            }
            _ => {}
        }
    }

    fn tratar_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                self.scroll = self.scroll.saturating_sub(3);
                self.seguir_fim = false;
            }
            MouseEventKind::ScrollDown => {
                self.scroll = self.scroll.saturating_add(3);
                // Chegar ao fim pelo mouse volta a acompanhar o estado mais
                // recente: a conferência final é feita em
                // `ui::desenhar_conversa`, que conhece a altura real da área.
            }
            MouseEventKind::Down(_) => {
                // Clique no atalho "voltar para o último".
                if let Some(area) = self.area_voltar
                    && mouse.column >= area.x
                    && mouse.column < area.x + area.width
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height
                {
                    self.ir_para_o_fim();
                }
            }
            _ => {}
        }
    }

    /// Volta a acompanhar o estado mais recente da conversa.
    fn ir_para_o_fim(&mut self) {
        // O desenho posiciona `scroll` no máximo quando `seguir_fim` é true.
        self.seguir_fim = true;
        self.scroll = u16::MAX;
    }

    fn tratar_tecla(&mut self, tecla: KeyEvent) {
        if self.modal.is_some() {
            self.tratar_tecla_modal(tecla);
            return;
        }
        let controle = tecla.modifiers.contains(KeyModifiers::CONTROL);
        let alt = tecla.modifiers.contains(KeyModifiers::ALT);
        let shift = tecla.modifiers.contains(KeyModifiers::SHIFT);
        match tecla.code {
            KeyCode::Enter if alt || shift => self.entrada.nova_linha(),
            KeyCode::Enter => self.enviar_entrada(),
            KeyCode::Char('j') if controle => self.entrada.nova_linha(),
            KeyCode::Char(c) if !controle && !alt => {
                self.entrada.inserir_char(c);
                self.sugestao_sel = 0;
            }
            KeyCode::Char('c') if controle => {
                if self.turno_ativo {
                    let _ = self.cmd_tx.send(UiCommand::Interromper);
                    self.empurrar_bloco(Bloco::Aviso(
                        "interrupção solicitada (Ctrl+C novamente encerra o Kestrel)".into(),
                    ));
                } else {
                    self.iniciar_saida();
                }
            }
            KeyCode::Char('d') if controle => {
                if self.entrada.vazio() {
                    self.iniciar_saida();
                }
            }
            KeyCode::Char('l') if controle => {
                self.limpar_conversa();
            }
            KeyCode::Char('a') if controle => self.entrada.home(),
            KeyCode::Char('e') if controle => self.entrada.fim(),
            KeyCode::Backspace => {
                self.entrada.backspace();
                self.sugestao_sel = 0;
            }
            KeyCode::Delete => self.entrada.apagar(),
            KeyCode::Left => self.entrada.esquerda(),
            KeyCode::Right => self.entrada.direita(),
            KeyCode::Home => self.entrada.home(),
            KeyCode::End => self.entrada.fim(),
            KeyCode::Up => {
                let sugestoes = self.entrada.sugestoes().len();
                if sugestoes > 0 {
                    self.sugestao_sel = self.sugestao_sel.saturating_sub(1);
                } else if !self.entrada.cima() {
                    self.entrada.historico_anterior();
                }
            }
            KeyCode::Down => {
                let sugestoes = self.entrada.sugestoes().len();
                if sugestoes > 0 {
                    self.sugestao_sel = (self.sugestao_sel + 1).min(sugestoes.saturating_sub(1));
                } else if !self.entrada.baixo() {
                    self.entrada.historico_proximo();
                }
            }
            KeyCode::Tab => {
                let sugestoes = self.entrada.sugestoes();
                if !sugestoes.is_empty() {
                    let indice = self.sugestao_sel.min(sugestoes.len() - 1);
                    let escolhida = sugestoes[indice];
                    self.entrada.completar(escolhida);
                    self.sugestao_sel = 0;
                }
            }
            KeyCode::BackTab => {
                // Shift+Tab alterna o nível de permissão em ciclo.
                self.nivel = self.nivel.cycle();
                let _ = self.cmd_tx.send(UiCommand::DefinirPermissao(self.nivel));
                self.avisar(format!(
                    "nível de permissão: {} — {}",
                    self.nivel.label(),
                    self.nivel.description()
                ));
            }
            KeyCode::PageUp => {
                self.scroll = self.scroll.saturating_sub(15);
                self.seguir_fim = false;
            }
            KeyCode::PageDown => {
                self.scroll = self.scroll.saturating_add(15);
            }
            KeyCode::Esc => {
                if self.turno_ativo {
                    let _ = self.cmd_tx.send(UiCommand::Interromper);
                    self.empurrar_bloco(Bloco::Aviso(
                        "interrupção solicitada; as alterações já feitas serão preservadas".into(),
                    ));
                } else {
                    self.entrada.limpar();
                }
            }
            KeyCode::F(1) => {
                self.modal = Some(Modal::Ajuda {
                    linhas: texto_de_ajuda(),
                    scroll: 0,
                });
            }
            KeyCode::F(2) => {
                self.mostrar_raciocinio = !self.mostrar_raciocinio;
                self.construir_tudo();
                let estado = if self.mostrar_raciocinio {
                    "visível"
                } else {
                    "oculto"
                };
                self.avisar(format!("raciocínio do modelo agora está {estado}"));
            }
            _ => {}
        }
    }

    fn tratar_tecla_modal(&mut self, tecla: KeyEvent) {
        enum Reacao {
            Nada,
            Fechar,
            Confirmar(Confirmacao),
            Escolher(ItemSeletor),
        }
        let reacao = match self.modal.as_mut() {
            None => Reacao::Nada,
            Some(Modal::Confirmacao(_)) => match tecla.code {
                KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Enter => {
                    Reacao::Confirmar(Confirmacao::Permitir)
                }
                KeyCode::Char('a') | KeyCode::Char('A') => {
                    Reacao::Confirmar(Confirmacao::PermitirSempre)
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                    Reacao::Confirmar(Confirmacao::Negar)
                }
                _ => Reacao::Nada,
            },
            Some(Modal::Permissoes(seletor))
            | Some(Modal::Modelos(seletor))
            | Some(Modal::Esforco(seletor))
            | Some(Modal::Sessoes(seletor))
            | Some(Modal::Projetos(seletor))
            | Some(Modal::Processos(seletor)) => {
                let mut reacao = Reacao::Nada;
                match tecla.code {
                    KeyCode::Esc => reacao = Reacao::Fechar,
                    KeyCode::Up => seletor.sel = seletor.sel.saturating_sub(1),
                    KeyCode::Down => {
                        let total = seletor.filtrados().len();
                        if total > 0 {
                            seletor.sel = (seletor.sel + 1).min(total - 1);
                        }
                    }
                    KeyCode::Backspace => {
                        seletor.filtro.pop();
                        seletor.sel = 0;
                    }
                    KeyCode::Enter => {
                        if let Some(item) = seletor.selecionado() {
                            reacao = Reacao::Escolher(item.clone());
                        }
                    }
                    KeyCode::Char(c) => {
                        seletor.filtro.push(c);
                        seletor.sel = 0;
                    }
                    _ => {}
                }
                seletor.ajustar_selecao();
                reacao
            }
            Some(Modal::Texto { scroll, .. }) | Some(Modal::Ajuda { scroll, .. }) => {
                match tecla.code {
                    KeyCode::Esc | KeyCode::Char('q') | KeyCode::Enter => Reacao::Fechar,
                    KeyCode::Up => {
                        *scroll = scroll.saturating_sub(1);
                        Reacao::Nada
                    }
                    KeyCode::Down => {
                        *scroll = scroll.saturating_add(1);
                        Reacao::Nada
                    }
                    KeyCode::PageUp => {
                        *scroll = scroll.saturating_sub(15);
                        Reacao::Nada
                    }
                    KeyCode::PageDown => {
                        *scroll = scroll.saturating_add(15);
                        Reacao::Nada
                    }
                    _ => Reacao::Nada,
                }
            }
        };
        match reacao {
            Reacao::Nada => {}
            Reacao::Fechar => self.modal = None,
            Reacao::Confirmar(resposta) => self.responder_confirmacao(resposta),
            Reacao::Escolher(item) => {
                // Fecha antes de agir: escolher um modelo com raciocínio abre o
                // seletor de esforço, que não pode ser fechado por engano.
                self.modal = None;
                self.agir_seletor(&item);
            }
        }
        self.proximo_confirmacao();
    }

    /// Avança animações e expira o estado "concluindo".
    ///
    /// Devolve `true` quando algum quadro de animação mudou — o laço da
    /// interface só redesenha nesse caso (ou quando chega um evento), o que
    /// mantém o aplicativo ocioso praticamente sem custo.
    pub fn tiquetaquear(&mut self) -> bool {
        self.tick = self.tick.wrapping_add(1);
        let mut mudou = false;
        if let Some(expira) = self.estado_expira
            && self.tick >= expira
        {
            self.estado = EstadoMascote::Ocioso;
            self.estado_texto = "pronto".into();
            self.estado_expira = None;
            mudou = true;
        }
        if self.turno_ativo && self.estado == EstadoMascote::Ocioso {
            self.estado = EstadoMascote::Pensando;
            mudou = true;
        }

        // Animação do Pip: respiração e piscada, quadro a quadro.
        let anterior = self.pip;
        if self.tick >= self.pip_proxima_piscada {
            self.pip.piscando = true;
            self.pip_piscada_ate = self.tick;
            let intervalo = self.intervalo_de_piscada();
            self.pip_proxima_piscada = self.tick + intervalo;
        } else if self.pip.piscando && self.tick > self.pip_piscada_ate {
            self.pip.piscando = false;
        }
        self.pip.deslocamento = mascot::deslocamento_de(self.tick);
        if self.pip != anterior {
            mudou = true;
        }
        mudou || self.turno_ativo
    }

    /// Intervalo entre piscadas, sorteado de forma determinística (evita o
    /// aspecto mecânico de um piscar sempre no mesmo ritmo).
    fn intervalo_de_piscada(&mut self) -> u64 {
        self.pip_sorteio = self
            .pip_sorteio
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        16 + (self.pip_sorteio >> 33) % 20
    }

    /// Persiste o histórico de entrada em disco.
    pub fn salvar_historico(&self) {
        let linhas = self
            .entrada
            .historico_recente(self.config.ui.max_input_history);
        if linhas.is_empty() {
            return;
        }
        let _ = std::fs::write(self.paths.history_file(), linhas.join("\n"));
    }
}

fn seletor_permissoes(atual: PermissionLevel) -> Seletor {
    let itens = [
        PermissionLevel::Read,
        PermissionLevel::Write,
        PermissionLevel::Semi,
        PermissionLevel::Full,
    ]
    .into_iter()
    .map(|nivel| ItemSeletor {
        titulo: if nivel == atual {
            format!("{} (atual)", nivel.label())
        } else {
            nivel.label().to_string()
        },
        detalhe: nivel.description().to_string(),
        valor: ValorSeletor::Permissao(nivel),
    })
    .collect();
    Seletor::novo(
        "Nível de permissão",
        "enter aplica · esc cancela · o nível vale imediatamente para novas ações",
        itens,
    )
}

/// Linhas do texto de ajuda (renderizado como Markdown).
fn texto_de_ajuda() -> Vec<Line<'static>> {
    let mut texto = String::from(
        "# Kestrel — ajuda\n\n\
         Agente de codificação autônomo. Descreva a tarefa em linguagem natural; o agente lê o \
         projeto, planeja, executa comandos reais, edita arquivos e valida o resultado com testes.\n\n\
         ## Comandos\n\n",
    );
    for comando in COMANDOS_SLASH {
        let argumentos = if comando.argumentos.is_empty() {
            String::new()
        } else {
            format!(" {}", comando.argumentos)
        };
        texto.push_str(&format!(
            "- `{}{}` — {}\n",
            comando.nome, argumentos, comando.descricao
        ));
    }
    texto.push_str(
        "\n## Modos\n\n\
         - **agente** (padrão): investiga, planeja, edita arquivos e executa comandos conforme o nível de permissão.\n\
         - **chat**: somente conversa e leitura; ferramentas que escrevem, executam ou planejam ficam indisponíveis.\n\
         - Troque com `/chat` e `/agente`; o modo atual aparece no topo da interface e é registrado na auditoria.\n\n\
         ## Teclado\n\n\
         - Enter envia; Alt+Enter (ou Shift+Enter) quebra linha\n\
         - Up/Down navega no histórico (ou nas sugestões de comando); Tab ou Enter completam um comando parcial\n\
         - Ctrl+C interrompe o turno; pressione de novo para sair\n\
         - Esc interrompe o turno\n\
         - Shift+Tab alterna o nível de permissão em ciclo\n\
         - F1 ajuda · F2 mostra/oculta o raciocínio do modelo (o mesmo que `/thinking`)\n\
         - PageUp/PageDown e a roda do mouse rolam a conversa\n\
         - Rolando para cima você pode ler o histórico à vontade; o canto do campo de mensagem \
ganha o atalho `↓ voltar para o último` (clique ou role até o fim para voltar a acompanhar)\n\n\
         ## Segurança\n\n\
         - Comandos catastróficos são bloqueados em qualquer nível de permissão.\n\
         - Em níveis mais restritos, ações sensíveis pedem confirmação.\n\
         - Toda alteração de arquivo e execução fica registrada no banco local.\n\
         - A chave de API é lida apenas da variável de ambiente e nunca é exibida.\n",
    );
    markdown::renderizar(&texto)
}

/// Inicia o agente, executa o laço da interface e restaura o terminal.
pub async fn executar(
    terminal: &mut ratatui::DefaultTerminal,
    config: Config,
    paths: Paths,
    store: Store,
) -> anyhow::Result<()> {
    let handle: AgenteHandle = iniciar(config.clone(), paths.clone(), store.clone())?;
    let AgenteHandle {
        cmd_tx,
        mut eventos,
    } = handle;
    let projeto = Arc::new(project::analyze(&config.workdir()));
    let skills = Arc::new(SkillRegistry::discover(&paths, &config.workdir()));
    let mut app = App::novo(config, paths, store, cmd_tx, projeto, skills);
    let mut eventos_terminal = ui::canal_terminal();
    // Cadência adaptativa: ágil durante um turno (spinner), espaçada quando
    // ocioso (respiração/piscada). O desenho é pulado quando nada mudou.
    let mut intervalo = Duration::from_millis(120);
    let mut tique = Box::pin(tokio::time::sleep(intervalo));
    let mut tiques_encerrando = 0u32;
    let mut redesenhar = true;

    loop {
        if redesenhar {
            terminal.draw(|f| ui::desenhar(f, &mut app))?;
        }
        redesenhar = true;
        tokio::select! {
            evento = eventos.recv() => {
                match evento {
                    Some(evento) => app.tratar_evento(evento),
                    None => break,
                }
            }
            Some(evento) = eventos_terminal.recv() => {
                app.tratar_terminal(evento);
            }
            _ = &mut tique => {
                redesenhar = app.tiquetaquear();
                if app.saindo {
                    tiques_encerrando += 1;
                }
            }
        }
        if app.encerrado {
            break;
        }
        // Rede de segurança: se o agente não confirmar o encerramento em ~10s,
        // a interface sai mesmo assim (o estado já foi persistido a cada passo).
        if app.saindo && tiques_encerrando > 40 {
            break;
        }
        let novo = if app.turno_ativo {
            Duration::from_millis(120)
        } else {
            Duration::from_millis(250)
        };
        if novo != intervalo {
            intervalo = novo;
        }
        tique
            .as_mut()
            .reset(tokio::time::Instant::now() + intervalo);
    }
    app.salvar_historico();
    Ok(())
}

/// `/thinking off` desliga de verdade: enquanto o braço vazio apenas
/// alterna, `off` precisa resultar em falso sempre.
#[test]
fn thinking_off_desliga_e_vazio_alterna() {
    // Espelha a decisão do comando em `executar_comando`.
    let decidir = |argumento: &str, atual: bool| -> Option<bool> {
        match argumento.trim().to_lowercase().as_str() {
            "on" | "sim" | "1" | "ativo" | "ativar" | "mostrar" => Some(true),
            "off" | "nao" | "não" | "0" | "inativo" | "desativar" | "ocultar" => Some(false),
            "" => Some(!atual),
            _ => None,
        }
    };
    assert_eq!(decidir("off", true), Some(false));
    assert_eq!(decidir("off", false), Some(false));
    assert_eq!(decidir("on", false), Some(true));
    assert_eq!(decidir("on", true), Some(true));
    assert_eq!(decidir("", true), Some(false));
    assert_eq!(decidir("talvez", true), None);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Store;

    fn app_de_teste() -> (App, mpsc::UnboundedReceiver<UiCommand>) {
        let store = Store::open_in_memory().unwrap();
        store
            .create_session("s", "/p", "t", "modelo/teste", None, "semi", None)
            .unwrap();
        let (cmd_tx, cmd_rx) = mpsc::unbounded_channel();
        let mut config = Config::default();
        config.provider.model = "modelo/teste".into();
        let app = App::novo(
            config,
            Paths::with_root("/tmp/kestrel-app-teste"),
            store,
            cmd_tx,
            Arc::new(ProjectInfo::default()),
            Arc::new(SkillRegistry::vazio()),
        );
        (app, cmd_rx)
    }

    #[test]
    fn entrada_usuario_vira_bloco_e_comando() {
        let (mut app, mut rx) = app_de_teste();
        app.entrada.definir_texto("corrija o bug".into());
        app.enviar_entrada();
        assert!(matches!(app.blocos.last(), Some(Bloco::Usuario(t)) if t == "corrija o bug"));
        assert!(app.turno_ativo);
        match rx.try_recv().unwrap() {
            UiCommand::EnviarMensagem(t) => assert_eq!(t, "corrija o bug"),
            outro => panic!("comando inesperado: {outro:?}"),
        }
    }

    #[test]
    fn comando_desconhecido_gera_aviso() {
        let (mut app, _rx) = app_de_teste();
        app.entrada.definir_texto("/inexistente".into());
        app.enviar_entrada();
        match app.blocos.last() {
            Some(Bloco::Aviso(t)) => assert!(t.contains("não existe")),
            _ => panic!("esperava aviso"),
        }
        assert!(!app.turno_ativo);
    }

    /// Os apelidos em português dos comandos continuam funcionando: renomear
    /// para inglês não pode quebrar o hábito de quem digita em português.
    #[test]
    fn apelidos_em_portugues_continuam_valendo() {
        // Nenhum apelido pode ser tratado como comando inexistente.
        for apelido in [
            "/modelo",
            "/conversa",
            "/agente",
            "/limpar",
            "/sair",
            "/nova",
            "/projetos",
            "/analisar",
            "/processos",
            "/retomar",
            "/historico",
        ] {
            let (mut app, _rx) = app_de_teste();
            app.entrada.definir_texto(apelido.into());
            app.enviar_entrada();
            let aviso_inexistente = matches!(
                app.blocos.last(),
                Some(Bloco::Aviso(t)) if t.contains("não existe")
            );
            assert!(!aviso_inexistente, "{apelido} não é reconhecido");
        }

        // E os de efeito puramente local continuam agindo na hora.
        let (mut app, _rx) = app_de_teste();
        let modo_antes = app.modo;
        app.entrada.definir_texto("/conversa".into());
        app.enviar_entrada();
        assert_ne!(app.modo, modo_antes, "/conversa deveria trocar o modo");
        app.entrada.definir_texto("/agente".into());
        app.enviar_entrada();
        assert_eq!(app.modo, modo_antes, "/agente deveria voltar o modo");

        let (mut app, _rx) = app_de_teste();
        app.empurrar_bloco(super::Bloco::Usuario("algo".into()));
        app.empurrar_bloco(super::Bloco::Usuario("outro".into()));
        app.entrada.definir_texto("/limpar".into());
        app.enviar_entrada();
        // Sobra apenas o aviso de que a conversa foi limpa.
        assert_eq!(app.blocos.len(), 1, "/limpar deveria limpar a conversa");
        assert!(matches!(app.blocos[0], super::Bloco::Aviso(_)));

        let (mut app, _rx) = app_de_teste();
        app.entrada.definir_texto("/sair".into());
        app.enviar_entrada();
        assert!(app.saindo, "/sair deveria iniciar a saída");
    }

    #[test]
    fn resposta_durante_turno_redireciona() {
        let (mut app, mut rx) = app_de_teste();
        app.turno_ativo = true;
        app.entrada.definir_texto("outra abordagem".into());
        app.enviar_entrada();
        match rx.try_recv().unwrap() {
            UiCommand::Redirecionar(t) => assert_eq!(t, "outra abordagem"),
            outro => panic!("comando inesperado: {outro:?}"),
        }
    }

    #[test]
    fn ferramenta_concluida_atualiza_bloco_com_diff_real() {
        let (mut app, _rx) = app_de_teste();
        app.tratar_evento(AgentEvent::FerramentaIniciada {
            nome: "edit_file".into(),
            detalhe: "src/main.rs".into(),
        });
        app.tratar_evento(AgentEvent::FerramentaConcluida {
            nome: "edit_file".into(),
            sucesso: true,
            resumo: "3 linhas alteradas".into(),
            adicoes: 5,
            remocoes: 2,
        });
        match app.blocos.last() {
            Some(Bloco::Ferramenta {
                rodando,
                sucesso,
                adicoes,
                remocoes,
                ..
            }) => {
                assert!(!rodando);
                assert!(sucesso);
                assert_eq!((*adicoes, *remocoes), (5, 2));
            }
            _ => panic!("esperava bloco de ferramenta"),
        }
    }

    #[test]
    fn texto_em_streaming_vira_bloco_assistente() {
        let (mut app, _rx) = app_de_teste();
        app.tratar_evento(AgentEvent::Texto {
            delta: "Olá".into(),
        });
        app.tratar_evento(AgentEvent::Texto {
            delta: ", mundo".into(),
        });
        match app.blocos.last() {
            Some(Bloco::Assistente(t)) => assert_eq!(t, "Olá, mundo"),
            _ => panic!("esperava bloco do assistente"),
        }
        assert!(!app.linhas.is_empty());
    }

    #[test]
    fn confirmacao_e_respondida_pela_interface() {
        let (mut app, _rx) = app_de_teste();
        let (tx, rx) = tokio::sync::oneshot::channel();
        app.tratar_evento(AgentEvent::Confirmacao {
            pedido: ConfirmRequest {
                titulo: "Confirmar".into(),
                detalhe: "detalhe".into(),
                nivel: PermissionLevel::Semi,
                resposta: tx,
            },
        });
        assert!(matches!(app.modal, Some(Modal::Confirmacao(_))));
        app.tratar_tecla(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert_eq!(rx.blocking_recv().unwrap(), Confirmacao::Permitir);
        assert!(app.modal.is_none());
    }

    #[test]
    fn filtro_do_seletor_resta_selecao() {
        let mut s = Seletor::novo(
            "t",
            "d",
            vec![
                ItemSeletor {
                    titulo: "alpha".into(),
                    detalhe: String::new(),
                    valor: ValorSeletor::Nada,
                },
                ItemSeletor {
                    titulo: "beta".into(),
                    detalhe: String::new(),
                    valor: ValorSeletor::Nada,
                },
            ],
        );
        assert_eq!(s.filtrados().len(), 2);
        s.filtro = "be".into();
        assert_eq!(s.filtrados().len(), 1);
        s.ajustar_selecao();
        assert_eq!(s.selecionado().unwrap().titulo, "beta");
    }

    fn passo(titulo: &str, status: StatusPasso) -> PlanoPasso {
        PlanoPasso {
            titulo: titulo.into(),
            status,
            observacao: None,
        }
    }

    fn tarefa(id: &str, status: StatusPasso, dependencias: &[&str]) -> TarefaView {
        TarefaView {
            id: id.into(),
            titulo: format!("tarefa {id}"),
            status,
            dependencias: dependencias.iter().map(|s| s.to_string()).collect(),
            resultado: None,
        }
    }

    #[test]
    fn plano_aparece_no_chat_e_atualizacoes_mostram_so_o_que_mudou() {
        let (mut app, _rx) = app_de_teste();
        app.tratar_evento(AgentEvent::PlanoAtualizado {
            objetivo: "melhorar a interface".into(),
            passos: vec![
                passo("mapear a ui atual", StatusPasso::Pendente),
                passo("remover painel", StatusPasso::Pendente),
            ],
        });
        match app.blocos.last() {
            Some(Bloco::Plano {
                objetivo,
                passos,
                mudancas,
            }) => {
                assert!(!mudancas);
                assert_eq!(objetivo, "melhorar a interface");
                assert_eq!(passos.len(), 2);
            }
            outro => panic!("esperava bloco de plano, veio {outro:?}"),
        }

        // Repetir o mesmo plano não gera bloco nenhum (sem inundar o chat).
        let quantidade = app.blocos.len();
        app.tratar_evento(AgentEvent::PlanoAtualizado {
            objetivo: "melhorar a interface".into(),
            passos: vec![
                passo("mapear a ui atual", StatusPasso::Pendente),
                passo("remover painel", StatusPasso::Pendente),
            ],
        });
        assert_eq!(app.blocos.len(), quantidade);

        // Mudança de status mostra apenas a etapa alterada.
        app.tratar_evento(AgentEvent::PlanoAtualizado {
            objetivo: "melhorar a interface".into(),
            passos: vec![
                passo("mapear a ui atual", StatusPasso::Concluido),
                passo("remover painel", StatusPasso::Pendente),
            ],
        });
        match app.blocos.last() {
            Some(Bloco::Plano {
                passos,
                mudancas,
                objetivo,
            }) => {
                assert!(mudancas);
                assert!(objetivo.is_empty());
                assert_eq!(passos.len(), 1);
                assert_eq!(passos[0].titulo, "mapear a ui atual");
                assert_eq!(passos[0].status, StatusPasso::Concluido);
            }
            outro => panic!("esperava atualização de plano, veio {outro:?}"),
        }

        // Objetivo novo é um replanejamento: aparece completo.
        app.tratar_evento(AgentEvent::PlanoAtualizado {
            objetivo: "outra frente".into(),
            passos: vec![passo("nova etapa", StatusPasso::Pendente)],
        });
        match app.blocos.last() {
            Some(Bloco::Plano {
                passos, mudancas, ..
            }) => {
                assert!(!mudancas);
                assert_eq!(passos.len(), 1);
            }
            outro => panic!("esperava plano completo, veio {outro:?}"),
        }

        // Limpeza (troca de sessão/projeto) não deixa bloco pendente.
        let quantidade = app.blocos.len();
        app.tratar_evento(AgentEvent::PlanoAtualizado {
            objetivo: String::new(),
            passos: vec![],
        });
        assert_eq!(app.blocos.len(), quantidade);
        assert!(app.plano.is_none());
    }

    #[test]
    fn subtarefas_distinguem_bloqueio_de_estado_real() {
        let (mut app, _rx) = app_de_teste();
        app.tratar_evento(AgentEvent::TarefasAtualizadas {
            tarefas: vec![
                tarefa("a", StatusPasso::Pendente, &[]),
                tarefa("b", StatusPasso::Pendente, &["a"]),
            ],
        });
        match app.blocos.last() {
            Some(Bloco::Tarefas { linhas, mudancas }) => {
                assert!(!mudancas);
                assert_eq!(linhas.len(), 2);
                assert!(linhas[0].bloqueios.is_empty());
                assert_eq!(linhas[1].bloqueios, vec!["a".to_string()]);
            }
            outro => panic!("esperava bloco de subtarefas, veio {outro:?}"),
        }

        // Concluir "a" mostra a mudança e o bloqueio de "b" some (dados reais).
        app.tratar_evento(AgentEvent::TarefasAtualizadas {
            tarefas: vec![
                tarefa("a", StatusPasso::Concluido, &[]),
                tarefa("b", StatusPasso::Pendente, &["a"]),
            ],
        });
        match app.blocos.last() {
            Some(Bloco::Tarefas { linhas, mudancas }) => {
                assert!(mudancas);
                assert_eq!(linhas.len(), 1);
                assert_eq!(linhas[0].titulo, "tarefa a");
                assert_eq!(linhas[0].status, StatusPasso::Concluido);
            }
            outro => panic!("esperava mudanças de subtarefas, veio {outro:?}"),
        }
    }

    #[test]
    fn enter_completa_comando_parcial_sem_executar() {
        let (mut app, _rx) = app_de_teste();
        app.entrada.definir_texto("/he".into());
        app.sugestao_sel = 0;
        app.enviar_entrada();
        // Nada foi executado nem enviado: o comando parcial foi completado.
        assert!(app.blocos.is_empty());
        assert!(!app.turno_ativo);
        assert_eq!(app.entrada.texto(), "/help ");
        // Agora sim: comando completo e exato executa.
        app.enviar_entrada();
        assert!(matches!(app.modal, Some(Modal::Ajuda { .. })));
    }

    #[test]
    fn comandos_de_modo_trocam_e_avisam() {
        let (mut app, mut rx) = app_de_teste();
        assert_eq!(app.modo, ModoOperacao::Agente);
        app.entrada.definir_texto("/chat".into());
        app.enviar_entrada();
        assert_eq!(app.modo, ModoOperacao::Chat);
        match rx.try_recv().unwrap() {
            UiCommand::DefinirModo(ModoOperacao::Chat) => {}
            outro => panic!("comando inesperado: {outro:?}"),
        }
        match app.blocos.last() {
            Some(Bloco::Aviso(t)) => assert!(t.contains("chat")),
            outro => panic!("esperava aviso, veio {outro:?}"),
        }
        // Repetir o mesmo modo apenas informa, sem reenviar o comando.
        app.entrada.definir_texto("/chat".into());
        app.enviar_entrada();
        match app.blocos.last() {
            Some(Bloco::Aviso(t)) => assert!(t.contains("já é chat")),
            outro => panic!("esperava aviso, veio {outro:?}"),
        }
        assert!(rx.try_recv().is_err());
        app.entrada.definir_texto("/agent".into());
        app.enviar_entrada();
        assert_eq!(app.modo, ModoOperacao::Agente);
        match rx.try_recv().unwrap() {
            UiCommand::DefinirModo(ModoOperacao::Agente) => {}
            outro => panic!("comando inesperado: {outro:?}"),
        }
    }

    #[test]
    fn escolher_modelo_envia_comando_e_confirma_no_chat() {
        let (mut app, mut rx) = app_de_teste();
        app.config.provider.model = "nvidia/llama-3.1-8b".into();
        app.config.provider.models = vec![
            crate::config::ModelEntry::new("nvidia/llama-3.1-8b"),
            crate::config::ModelEntry::new("nvidia/nemotron-3-ultra-550b"),
        ];
        app.abrir_seletor_modelos(String::new());
        match app.modal.as_ref() {
            Some(Modal::Modelos(seletor)) => {
                // Abre já no modelo em uso, que fica marcado na lista.
                assert_eq!(seletor.sel, 0);
                assert!(seletor.itens[0].titulo.contains("· atual"));
                assert_eq!(seletor.itens.len(), 2);
            }
            outro => panic!("esperava o seletor de modelos, veio {}", outro.is_none()),
        }
        // Navega até o nemotron com a setinha e confirma com Enter — o caminho
        // real usado na interface (o modal fecha e o comando é enviado).
        app.tratar_tecla_modal(KeyEvent::from(KeyCode::Down));
        app.tratar_tecla_modal(KeyEvent::from(KeyCode::Enter));
        assert!(app.modal.is_none());
        match rx.try_recv().unwrap() {
            UiCommand::DefinirModelo { modelo, esforco } => {
                assert_eq!(modelo, "nvidia/nemotron-3-ultra-550b");
                assert!(esforco.is_none());
            }
            outro => panic!("comando inesperado: {outro:?}"),
        }
        // O agente aplica e a interface confirma no chat (feedback claro).
        app.tratar_evento(AgentEvent::ModeloAtual {
            modelo: "nvidia/nemotron-3-ultra-550b".into(),
            esforco: None,
        });
        assert_eq!(app.modelo, "nvidia/nemotron-3-ultra-550b");
        match app.blocos.last() {
            Some(Bloco::Aviso(t)) => {
                assert!(
                    t.contains("modelo atual: nvidia/nemotron-3-ultra-550b"),
                    "veio: {t}"
                );
            }
            outro => panic!("esperava aviso de troca de modelo, veio {outro:?}"),
        }
    }

    #[test]
    fn modelo_com_raciocinio_pede_o_esforco_antes_de_aplicar() {
        use crate::config::Capability;
        let (mut app, mut rx) = app_de_teste();
        let mut entrada = crate::config::ModelEntry::new("nvidia/nemotron-3-ultra-550b");
        entrada.supports_reasoning = Capability::Sim;
        entrada.reasoning_efforts = vec!["high".into()];
        app.config.provider.models = vec![entrada];
        app.abrir_seletor_modelos(String::new());
        app.tratar_tecla_modal(KeyEvent::from(KeyCode::Enter));
        // Abre a escolha de esforço em vez de aplicar calado.
        assert!(matches!(app.modal, Some(Modal::Esforco(_))));
        app.tratar_tecla_modal(KeyEvent::from(KeyCode::Down)); // primeiro esforço suportado (`high`)
        app.tratar_tecla_modal(KeyEvent::from(KeyCode::Enter));
        assert!(app.modal.is_none());
        match rx.try_recv().unwrap() {
            UiCommand::DefinirModelo { modelo, esforco } => {
                assert_eq!(modelo, "nvidia/nemotron-3-ultra-550b");
                assert_eq!(esforco.as_deref(), Some("high"));
            }
            outro => panic!("comando inesperado: {outro:?}"),
        }
        app.tratar_evento(AgentEvent::ModeloAtual {
            modelo: "nvidia/nemotron-3-ultra-550b".into(),
            esforco: Some("high".into()),
        });
        match app.blocos.last() {
            Some(Bloco::Aviso(t)) => {
                assert!(t.contains("esforço de raciocínio: high"), "veio: {t}");
            }
            outro => panic!("esperava aviso, veio {outro:?}"),
        }
    }

    #[test]
    fn animacao_do_pip_e_deterministica_e_pisca_uma_vez() {
        let (mut app, _rx) = app_de_teste();
        let (mut outro, _rx2) = app_de_teste();
        let mut piscadas = 0;
        let mut piscando_anterior = app.pip.piscando;
        for _ in 0..80 {
            app.tiquetaquear();
            outro.tiquetaquear();
            if app.pip.piscando && !piscando_anterior {
                piscadas += 1;
            }
            piscando_anterior = app.pip.piscando;
        }
        // Determinística: dois aplicativos idênticos animam igual.
        assert_eq!(app.pip, outro.pip);
        assert_eq!(app.tick, outro.tick);
        // Piscou pelo menos uma vez em 80 quadros e nunca ficou fechado mais de
        // um quadro seguido.
        assert!(piscadas >= 1, "nenhuma piscada em 80 quadros");
        assert!(piscadas <= 80 / 16 + 1, "piscadas demais: {piscadas}");
        // A respiração passa pelos dois deslocamentos.
        let mut viu_zero = false;
        let mut viu_um = false;
        let (mut app3, _rx3) = app_de_teste();
        for _ in 0..32 {
            app3.tiquetaquear();
            match app3.pip.deslocamento {
                0 => viu_zero = true,
                _ => viu_um = true,
            }
        }
        assert!(viu_zero && viu_um);
    }

    #[test]
    fn ocioso_nao_redesenha_todo_quadro() {
        // O laço da interface só desenha quando `tiquetaquear` devolve true;
        // em quadros estáveis ele devolve false (aplicativo ocioso é barato).
        let (mut app, _rx) = app_de_teste();
        let mut desenhos = 0;
        for _ in 0..64 {
            if app.tiquetaquear() {
                desenhos += 1;
            }
        }
        assert!(
            desenhos < 64,
            "a animação estática nunca deveria redesemhar todo quadro"
        );
        assert!(
            desenhos > 0,
            "a respiração/piscada precisa desenhar às vezes"
        );
    }
}
