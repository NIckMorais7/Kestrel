//! Aplicação: laço de eventos, comandos e ponte entre a interface e o agente.
//!
//! O estado exibido é sempre derivado de eventos reais do agente: deltas de
//! texto, resultados de ferramentas (com diffs reais), erros, plano e
//! subtarefas persistidos. Nada é simulado.

use crate::agent::{AgenteHandle, iniciar};
use crate::comps;
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
use crate::ui::mascot::{self, AnimacaoPip, EstadoMascote, PipGrafico};
use crate::ui::{self, theme};
use crate::util::Paths;
use crossterm::event::{
    KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseButton, MouseEvent, MouseEventKind,
};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;

/// Uma combinação de tecla configurável.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Atalho {
    pub code: KeyCode,
    pub mods: KeyModifiers,
}

/// Mapa ação → tecla, com padrões substituíveis por `ui.keybindings`.
#[derive(Clone, Debug)]
pub struct Keymap {
    mapa: HashMap<String, Atalho>,
}

impl Default for Keymap {
    fn default() -> Self {
        Self::novo(&std::collections::BTreeMap::new())
    }
}

impl Keymap {
    pub const ACOES: &'static [(&'static str, &'static str, &'static str)] = &[
        ("buscar", "ctrl+f", "buscar na conversa"),
        ("copiar", "ctrl+y", "copiar a última resposta (OSC 52)"),
        ("ajuda", "f1", "abrir a ajuda"),
        ("raciocinio", "f2", "mostrar/ocultar raciocínio"),
        ("permissoes", "backtab", "ciclar o nível de permissão"),
        ("rolar_cima", "pageup", "rolar para cima"),
        ("rolar_baixo", "pagedown", "rolar para baixo"),
        (
            "interromper",
            "esc",
            "interromper o turno / limpar o rascunho",
        ),
    ];

    pub fn novo(overrides: &std::collections::BTreeMap<String, String>) -> Self {
        let mut mapa = HashMap::new();
        for (acao, spec, _) in Self::ACOES {
            if let Some(atalho) = parse_atalho(spec) {
                mapa.insert((*acao).to_string(), atalho);
            }
        }
        for (acao, spec) in overrides {
            let acao = acao.trim().to_lowercase();
            if !Self::ACOES.iter().any(|(nome, _, _)| *nome == acao) {
                continue;
            }
            if let Some(atalho) = parse_atalho(spec) {
                // Teclas de caractere sem Ctrl/Alt colidiriam com a digitação
                // no campo de mensagem; o padrão da ação é mantido.
                let seguro = !matches!(atalho.code, KeyCode::Char(_))
                    || atalho
                        .mods
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT);
                if seguro {
                    mapa.insert(acao, atalho);
                }
            } else {
                mapa.remove(&acao);
            }
        }
        Self { mapa }
    }

    pub fn acao(&self, tecla: &KeyEvent) -> Option<&str> {
        self.mapa.iter().find_map(|(acao, atalho)| {
            (tecla.code == atalho.code && tecla.modifiers == atalho.mods).then_some(acao.as_str())
        })
    }

    pub fn descricao(&self, acao: &str) -> String {
        self.mapa
            .get(acao)
            .map(descrever_atalho)
            .unwrap_or_else(|| "(desativado)".into())
    }
}

fn descrever_atalho(atalho: &Atalho) -> String {
    let mut partes = Vec::new();
    if atalho.mods.contains(KeyModifiers::CONTROL) {
        partes.push("ctrl".to_string());
    }
    if atalho.mods.contains(KeyModifiers::ALT) {
        partes.push("alt".to_string());
    }
    if atalho.mods.contains(KeyModifiers::SHIFT) {
        partes.push("shift".to_string());
    }
    let tecla = match atalho.code {
        KeyCode::F(n) => format!("f{n}"),
        KeyCode::Char(c) => c.to_string(),
        KeyCode::PageUp => "pageup".into(),
        KeyCode::PageDown => "pagedown".into(),
        KeyCode::Tab => "tab".into(),
        KeyCode::BackTab => "shift+tab".into(),
        KeyCode::Enter => "enter".into(),
        KeyCode::Esc => "esc".into(),
        KeyCode::Backspace => "backspace".into(),
        KeyCode::Up => "up".into(),
        KeyCode::Down => "down".into(),
        KeyCode::Left => "left".into(),
        KeyCode::Right => "right".into(),
        KeyCode::Home => "home".into(),
        KeyCode::End => "end".into(),
        outro => format!("{outro:?}").to_lowercase(),
    };
    partes.push(tecla);
    partes.join("+")
}

fn parse_atalho(spec: &str) -> Option<Atalho> {
    let partes: Vec<String> = spec
        .split('+')
        .map(|p| p.trim().to_lowercase())
        .filter(|p| !p.is_empty())
        .collect();
    let (nome, mods) = partes.split_last()?;
    let mut modificadores = KeyModifiers::NONE;
    for parte in mods {
        match parte.as_str() {
            "ctrl" | "control" => modificadores |= KeyModifiers::CONTROL,
            "alt" => modificadores |= KeyModifiers::ALT,
            "shift" => modificadores |= KeyModifiers::SHIFT,
            _ => return None,
        }
    }
    let tecla = match nome.as_str() {
        "esc" | "escape" => KeyCode::Esc,
        "enter" | "return" => KeyCode::Enter,
        "backtab" => KeyCode::BackTab,
        "tab" if modificadores.contains(KeyModifiers::SHIFT) => {
            // Shift+Tab chega como BackTab no crossterm.
            modificadores.remove(KeyModifiers::SHIFT);
            KeyCode::BackTab
        }
        "tab" => KeyCode::Tab,
        "backspace" => KeyCode::Backspace,
        "up" => KeyCode::Up,
        "down" => KeyCode::Down,
        "left" => KeyCode::Left,
        "right" => KeyCode::Right,
        "home" => KeyCode::Home,
        "end" => KeyCode::End,
        "pageup" | "pgup" => KeyCode::PageUp,
        "pagedown" | "pgdn" => KeyCode::PageDown,
        "space" => KeyCode::Char(' '),
        "delete" | "del" => KeyCode::Delete,
        outra => {
            if let Some(numero) = outra.strip_prefix('f')
                && let Ok(n) = numero.parse::<u8>()
                && (1..=12).contains(&n)
            {
                KeyCode::F(n)
            } else if outra.chars().count() == 1 {
                KeyCode::Char(outra.chars().next()?)
            } else {
                return None;
            }
        }
    };
    Some(Atalho {
        code: tecla,
        mods: modificadores,
    })
}

/// Um bloco da conversa exibida.
#[derive(Debug)]
pub enum Bloco {
    Usuario(String),
    Raciocinio(String),
    Assistente(String),
    Ferramenta {
        id: Option<String>,
        nome: String,
        detalhe: String,
        rodando: bool,
        sucesso: bool,
        resumo: Option<String>,
        adicoes: usize,
        remocoes: usize,
        /// Linhas do diff real, quando a ferramenta as devolveu.
        diff: Vec<LinhaDiff>,
        /// Subtarefa do subagente que a chamou, quando não foi o principal.
        subagente: Option<String>,
    },
    Saida {
        id: Option<String>,
        nome: String,
        linhas: Vec<String>,
    },
    /// Cabeçalho de um subagente: a subtarefa que ele está executando.
    Subagente {
        tarefa: String,
        rodando: bool,
        sucesso: bool,
    },
    Erro {
        mensagem: String,
        dica: Option<String>,
    },
    Aviso(String),
    /// Falha de recuperação automática: erro em vermelho, com a tentativa.
    Recuperacao {
        descricao: String,
        tentativa: u32,
        maximo: u32,
    },
    Turno(String),
    /// Fio horizontal que abre um turno do usuário.
    Separador,
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

/// Uma linha de diff já classificada (para colorir no chat).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LinhaDiff {
    pub texto: String,
    pub tipo: TipoLinhaDiff,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TipoLinhaDiff {
    Contexto,
    Adicao,
    Remocao,
    /// Cabeçalho de arquivo/trecho (`diff --git`, `@@`, `+++`).
    Meta,
}

impl LinhaDiff {
    /// Classifica as linhas de um diff unificado.
    pub fn de_texto(texto: &str) -> Vec<LinhaDiff> {
        texto
            .lines()
            .map(|l| {
                let tipo = if l.starts_with("diff --git")
                    || l.starts_with("index ")
                    || l.starts_with("--- ")
                    || l.starts_with("+++ ")
                    || l.starts_with("@@")
                {
                    TipoLinhaDiff::Meta
                } else if l.starts_with('+') {
                    TipoLinhaDiff::Adicao
                } else if l.starts_with('-') {
                    TipoLinhaDiff::Remocao
                } else {
                    TipoLinhaDiff::Contexto
                };
                LinhaDiff {
                    texto: l.to_string(),
                    tipo,
                }
            })
            .collect()
    }
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
    /// Linhas prontas para exibição, com a trilha lateral colorida aplicada.
    pub fn linhas(&self) -> Vec<Line<'static>> {
        com_trilha(self.linhas_internas(), self.trilha())
    }

    /// Trilha lateral que identifica o tipo do bloco pela cor.
    pub fn trilha(&self) -> theme::Trilha {
        match self {
            // Trabalho delegado mantém a cor do subagente: é o que permite
            // distinguir, num relance, quem fez o quê.
            Bloco::Ferramenta {
                subagente: Some(_), ..
            } => theme::Trilha::Subagente,
            Bloco::Ferramenta {
                nome,
                sucesso,
                rodando,
                ..
            } => {
                if !*rodando && !*sucesso {
                    theme::Trilha::Erro
                } else {
                    theme::Trilha::da_ferramenta(nome)
                }
            }
            Bloco::Saida { nome, .. } => theme::Trilha::da_ferramenta(nome),
            Bloco::Subagente { .. } => theme::Trilha::Subagente,
            Bloco::Erro { .. } | Bloco::Recuperacao { .. } => theme::Trilha::Erro,
            Bloco::Raciocinio(_) => theme::Trilha::Subagente,
            Bloco::Plano { .. } | Bloco::Tarefas { .. } => theme::Trilha::Shell,
            Bloco::Separador => theme::Trilha::Leitura,
            _ => theme::Trilha::Leitura,
        }
    }

    /// Conteúdo do bloco, sem a trilha.
    fn linhas_internas(&self) -> Vec<Line<'static>> {
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
                let mut linhas = vec![
                    Line::default(),
                    Line::from(Span::styled("Kestrel", theme::rotulo())),
                ];
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
                diff,
                subagente,
                ..
            } => {
                let (marcador, estilo_marcador) = if *rodando {
                    ("┊", theme::em_execucao())
                } else if *sucesso {
                    ("┊", theme::sucesso())
                } else {
                    ("┊", theme::falha())
                };
                let mut spans = vec![
                    Span::styled(format!("{marcador} "), estilo_marcador),
                    Span::styled(nome.clone(), theme::rotulo()),
                ];
                // Ferramenta de subagente: recuada e identificada, para o
                // trabalho delegado não se misturar com o do agente principal.
                if let Some(sub) = subagente {
                    spans.push(Span::styled("  └ ", theme::apagado()));
                    spans.push(Span::styled(
                        crate::util::truncate(sub, 40),
                        Style::default().fg(theme::Trilha::Subagente.cor()),
                    ));
                }
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
                        Span::styled("  ⎿  ", theme::apagado()),
                        Span::styled(resumo.clone(), theme::apagado()),
                    ]));
                }
                // Diff real, colorido por tipo de linha: é a informação que o
                // usuário quer conferir sem abrir o arquivo.
                for linha in diff.iter().filter(|l| l.tipo != TipoLinhaDiff::Contexto) {
                    let estilo = match linha.tipo {
                        TipoLinhaDiff::Adicao => theme::sucesso().bg(theme::cor_diff_add()),
                        TipoLinhaDiff::Remocao => theme::falha().bg(theme::cor_diff_del()),
                        _ => theme::apagado(),
                    };
                    linhas.push(Line::from(Span::styled(
                        format!("    {}", linha.texto),
                        estilo,
                    )));
                }
                linhas
            }
            Bloco::Saida { linhas, .. } => linhas
                .iter()
                .map(|l| {
                    Line::from(vec![
                        Span::styled("  ┊ ", theme::acento_fraco()),
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
            Bloco::Recuperacao {
                descricao,
                tentativa,
                maximo,
            } => {
                let mut linhas = vec![Line::from(vec![
                    Span::styled("✖ erro ", theme::falha().add_modifier(Modifier::BOLD)),
                    Span::styled(
                        format!("· tentativa {tentativa}/{maximo}"),
                        theme::apagado(),
                    ),
                ])];
                for parte in descricao.lines() {
                    linhas.push(Line::from(Span::styled(
                        format!("  {parte}"),
                        theme::falha(),
                    )));
                }
                linhas.push(Line::default());
                linhas
            }
            Bloco::Turno(texto) => {
                // Rodapé de turno: uma linha curta e apagada, sem caixa nem
                // repetição do que o agente já disse.
                vec![Line::from(vec![
                    Span::styled("  └ ", theme::acento_fraco()),
                    Span::styled(texto.clone(), theme::apagado()),
                ])]
            }
            Bloco::Subagente {
                tarefa,
                rodando,
                sucesso,
            } => {
                // Cabeçalho do subagente: o que foi delegado, com estado.
                let rotulo = if *rodando {
                    "◆ subagente trabalhando"
                } else if *sucesso {
                    "◆ subagente concluiu"
                } else {
                    "◆ subagente falhou"
                };
                let cor = if *rodando {
                    theme::em_execucao()
                } else if *sucesso {
                    theme::sucesso()
                } else {
                    theme::falha()
                };
                vec![
                    Line::from(vec![Span::styled(rotulo, cor)]),
                    Line::from(Span::styled(format!("  └ {tarefa}"), theme::apagado())),
                ]
            }
            Bloco::Separador => {
                // Fio que abre um turno: dá costura à conversa, que sem isso é
                // um bloco contínuo de blocos.
                let largura = 48;
                vec![Line::from(Span::styled(
                    "  ".to_string() + &"─".repeat(largura),
                    Style::default().fg(theme::cor_borda()),
                ))]
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

/// Aplica a trilha lateral colorida a todas as linhas de um bloco.
///
/// A trilha é o que permite varrer a conversa por cor: leitura, escrita,
/// shell, web, erro e subagente têm cor própria na skin atual. O glifo é
/// estreito (`▎`) para não deslocar o texto.
fn com_trilha(linhas: Vec<Line<'static>>, trilha: theme::Trilha) -> Vec<Line<'static>> {
    let cor = trilha.cor();
    linhas
        .into_iter()
        .map(|mut linha| {
            let mut spans = vec![Span::styled(
                theme::GLIFO_TRILHA,
                ratatui::style::Style::default().fg(cor),
            )];
            spans.extend(linha.spans);
            linha.spans = spans;
            linha
        })
        .collect()
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
    /// Histórico de sessões com seleção múltipla para exclusão. `marcadas`
    /// guarda os índices (na lista original, antes de qualquer filtro) das
    /// sessões marcadas para exclusão.
    Sessoes {
        seletor: Seletor,
        marcadas: std::collections::HashSet<usize>,
    },
    /// Confirmação de exclusão de sessões marcadas (separada do fluxo de
    /// confirmações de ferramenta: a decisão é do usuário na interface).
    ConfirmarSessoes {
        ids: Vec<String>,
    },
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
    /// Editor de instruções de um Comp.
    CompEditor {
        nome: String,
        novo: bool,
        descricao: String,
        editor: Editor,
        erro: Option<String>,
        scroll: u16,
    },
    /// Confirmação de remoção de Comp (separada das confirmações de ferramenta).
    ConfirmarComp {
        nome: String,
    },
    /// Busca textual na conversa, com salto entre ocorrências.
    Busca {
        consulta: String,
        indice: usize,
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
    /// Momento em que o turno atual começou (timer da linha de atividade).
    turno_iniciado_em: Option<std::time::Instant>,
    /// Texto-chave da última linha de atividade renderizada (para não
    /// reconstruir a cauda quando nada mudou visualmente).
    atividade_chave: Option<String>,
    /// Linha de atividade (spinner + tempo + estado) no fim da conversa.
    atividade_linha: Option<Line<'static>>,
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
    /// Quadro atual da animação do Pip (caminhada, respiração e piscada).
    pub pip: AnimacaoPip,
    /// PNG do Pip já codificado no protocolo gráfico do terminal. `None`
    /// quando o mascote está desligado ou o terminal não pôde ser consultado.
    pub pip_grafico: Option<PipGrafico>,
    pip_piscada_ate: u64,
    pip_proxima_piscada: u64,
    pip_sorteio: u64,
    pub saindo: bool,
    pub encerrado: bool,
    pub sugestao_sel: usize,
    modelo_pendente: Option<String>,
    /// Blocos recolhidos (índice em `blocos`). Clica-se no bloco para
    /// abrir/fechar; começa aberto e o usuário decide o que esconder.
    pub recolhidos: std::collections::HashSet<usize>,
    /// Mapa de clique por bloco, preenchido durante o desenho: (índice do
    /// bloco, área na tela). Sem ele não há como saber em qual bloco o
    /// mouse foi clicado.
    pub areas_blocos: Vec<(usize, ratatui::layout::Rect)>,
    /// Aviso temporário sobreposto no topo da conversa (`(texto, tick de
    /// expiração)`); Some apenas enquanto visível.
    pub toast: Option<(String, u64)>,
    /// Valor animado do medidor de contexto (interpola até `contexto`).
    /// `0` significa "ainda não animado": o valor real é usado direto.
    pub contexto_exibido: u32,
    /// Alvo da rolagem suave: `scroll` caminha até ele em vez de pular.
    pub scroll_alvo: u16,
    /// Tick em que o modal atual foi aberto (animação de crescimento).
    pub modal_desde: Option<u64>,
    /// Versão das linhas renderizadas; invalida caches de altura/quebra.
    pub versao_linhas: u64,
    /// Cache de alturas: (largura, versão, prefixo de linhas renderizadas).
    pub cache_alturas: Option<(usize, u64, Vec<usize>)>,
    /// Atalhos efetivos (padrões + `ui.keybindings`).
    pub keymap: Keymap,
    /// Idioma da interface.
    pub idioma: crate::i18n::Idioma,
    /// Animações reduzidas.
    pub movimento_reduzido: bool,
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
            // Formato atual: array JSON, que preserva entradas multilinha.
            // Arquivos antigos (um item por linha) continuam sendo lidos.
            let itens: Vec<String> = serde_json::from_str(&texto)
                .unwrap_or_else(|_| texto.lines().map(|l| l.to_string()).collect());
            entrada.carregar_historico(itens);
        }
        let mostrar_raciocinio = config.ui.show_reasoning;
        let movimento_reduzido = config.ui.reduced_motion;
        let idioma = crate::i18n::Idioma::de_config(&config.ui.language);
        let keymap = Keymap::novo(&config.ui.keybindings);
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
            turno_iniciado_em: None,
            atividade_chave: None,
            atividade_linha: None,
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
            pip_grafico: None,
            pip_piscada_ate: 0,
            pip_proxima_piscada: 20,
            pip_sorteio: 0x9E37_79B9_7F4A_7C15,
            saindo: false,
            encerrado: false,
            sugestao_sel: 0,
            modelo_pendente: None,
            recolhidos: std::collections::HashSet::new(),
            areas_blocos: Vec::new(),
            toast: None,
            contexto_exibido: 0,
            scroll_alvo: 0,
            modal_desde: None,
            versao_linhas: 0,
            cache_alturas: None,
            keymap,
            idioma,
            movimento_reduzido,
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
            // Os índices recolhidos são por posição: deslocar junto, senão um
            // bloco qualquer passaria a abrir/fechar no lugar errado.
            self.recolhidos = self
                .recolhidos
                .iter()
                .filter_map(|i| i.checked_sub(remover))
                .collect();
            self.construir_tudo();
        }
    }

    /// Abre ou fecha um bloco (clique do mouse sobre ele).
    pub fn alternar_bloco(&mut self, indice: usize) {
        if self.recolhidos.contains(&indice) {
            self.recolhidos.remove(&indice);
        } else {
            self.recolhidos.insert(indice);
        }
        self.reconstruir_do(indice);
    }

    /// Aviso temporário no topo da conversa (some sozinho).
    ///
    /// Confirmações rápidas (skin trocada, pensamento ligado, permissão
    /// alterada) não precisam virar bloco permanente: a conversa fica limpa e
    /// a informação continua aparecendo.
    fn avisar_rapido(&mut self, mensagem: String, tiques: u64) {
        self.toast = Some((mensagem, self.tick + tiques));
    }

    /// Valor do medidor de contexto já animado (ou o real, antes da primeira
    /// interpolação). Mostrar zero com contexto medido seria informar errado.
    pub fn contexto_animado(&self) -> u32 {
        if self.contexto_exibido == 0 {
            self.contexto.0
        } else {
            self.contexto_exibido
        }
    }

    /// Deslocamento em `linhas` do começo de cada bloco (mesmo índice de
    /// `blocos`). A UI usa para saber onde cada bloco começa e termina.
    pub fn marcos(&self) -> &[usize] {
        &self.marcos
    }

    /// Reconstrói as linhas a partir do bloco `inicio` (os anteriores não mudam).
    pub fn reconstruir_do(&mut self, inicio: usize) {
        self.versao_linhas = self.versao_linhas.wrapping_add(1);
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
            if self.recolhidos.contains(&i) {
                // Recolhido: só a primeira linha (o resumo) aparece.
                self.linhas.extend(
                    self.blocos[i]
                        .linhas()
                        .into_iter()
                        .take(1)
                        .collect::<Vec<_>>(),
                );
            } else {
                self.linhas.extend(self.blocos[i].linhas());
            }
        }
        // A linha de atividade vive no fim da conversa, fora dos blocos: ela
        // acompanha o raciocínio/ação mais recente enquanto o turno roda.
        if let Some(linha) = &self.atividade_linha {
            self.linhas.push(linha.clone());
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

    /// Reconstrói só a cauda da conversa: o último bloco mais a linha de
    /// atividade (que vive fora dos blocos).
    fn reconstruir_cauda(&mut self) {
        if self.blocos.is_empty() {
            return;
        }
        let ultimo = self.blocos.len() - 1;
        self.reconstruir_do(ultimo);
    }

    /// Atualiza a linha de atividade do turno corrente (spinner + tempo
    /// decorrido + estado). Devolve `true` quando houve mudança visível.
    fn atualizar_atividade(&mut self) -> bool {
        let Some(iniciado) = self.turno_iniciado_em else {
            let tinha = self.atividade_linha.is_some();
            self.atividade_chave = None;
            self.atividade_linha = None;
            if tinha {
                self.reconstruir_cauda();
            }
            return tinha;
        };
        // "Quadrado" girando: cantos que rotacionam (▘▝▗▖). Com movimento
        // reduzido, fica parado.
        let glifo = if self.movimento_reduzido {
            "▘"
        } else {
            const QUADRADO: [&str; 4] = ["▘", "▝", "▗", "▖"];
            QUADRADO[(self.tick / 2) as usize % QUADRADO.len()]
        };
        let tempo = crate::util::format_duration_ms(
            iniciado.elapsed().as_millis().min(u64::MAX as u128) as u64,
        );
        let estado = if self.estado_texto.trim().is_empty() {
            "trabalhando".to_string()
        } else {
            self.estado_texto.to_lowercase()
        };
        let chave = format!("{glifo}|{tempo}|{estado}");
        if self.atividade_chave.as_deref() == Some(chave.as_str()) {
            return false;
        }
        self.atividade_chave = Some(chave);
        self.atividade_linha = Some(Line::from(vec![
            Span::styled("  ", theme::apagado()),
            Span::styled(glifo, theme::em_execucao().add_modifier(Modifier::BOLD)),
            Span::styled(format!(" {tempo}"), theme::apagado()),
            Span::styled(format!(" · {estado}"), theme::texto()),
        ]));
        self.reconstruir_cauda();
        true
    }

    fn limpar_conversa(&mut self) {
        self.versao_linhas = self.versao_linhas.wrapping_add(1);
        self.blocos.clear();
        self.linhas.clear();
        self.marcos.clear();
        // `recolhidos` guarda índices POSICIONAIS em `blocos`: sem zerar, os
        // índices da conversa anterior passam a valer para a nova e um bloco
        // que o usuário nunca fechou aparece com uma linha só (o resumo e o
        // diff desaparecem) depois de um `/clear` ou troca de projeto.
        self.recolhidos.clear();
        self.scroll = 0;
        // O alvo precisa nascer alinhado à posição: um valor antigo (ex.: 300)
        // fazia `tiquetaquear` puxar o scroll de volta todo quadro, e o
        // desenho o clampava de novo — nunca convergindo, com redesenho
        // forçado em todos os quadros.
        self.scroll_alvo = self.scroll;
        self.seguir_fim = true;
    }

    fn definir_estado(&mut self, estado: EstadoMascote, texto: impl Into<String>) {
        self.estado = estado;
        self.estado_texto = com_maiuscula(texto.into());
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
        // Fio de abertura do turno: dá costura visual à conversa.
        self.empurrar_bloco(Bloco::Separador);
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
                self.abrir_modal(Modal::Ajuda {
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
                    // `/effort` altera só o esforço: um modelo escolhido e
                    // depois cancelado não pode ser aplicado por engano aqui.
                    self.modelo_pendente = None;
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
            "/undo" | "/desfazer" => self.desfazer_ultima(),
            "/diff" | "/mudancas" => self.mostrar_diff(),
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
                self.definir_raciocinio(novo);
                let estado = if novo { "visível" } else { "oculto" };
                self.avisar_rapido(
                    format!(
                        "raciocínio do modelo: {estado}. Quando oculto, o pensamento não aparece \
                         na conversa — apenas o estado ao lado do Pip."
                    ),
                    20,
                );
            }
            "/person" | "/pessoa" | "/comp" => self.executar_person(&argumento),
            "/branch" | "/ramificar" | "/checkpoint" => {
                let titulo = if argumento.trim().is_empty() {
                    None
                } else {
                    Some(argumento.trim().to_string())
                };
                let _ = self.cmd_tx.send(UiCommand::Ramificar { titulo });
                self.avisar_rapido("criando ramo da sessão…".into(), 20);
            }
            "/keys" | "/atalhos" => {
                let mut linhas = vec![
                    "Atalhos configuráveis (em `ui.keybindings`, ação = \"tecla\"):".to_string(),
                ];
                for (acao, _, descricao) in Keymap::ACOES {
                    linhas.push(format!(
                        "  {:<14} {}  — {descricao}",
                        self.keymap.descricao(acao),
                        acao
                    ));
                }
                linhas.push(String::new());
                linhas.push("Padrões: ctrl+f busca, ctrl+y copia a última resposta, f1 ajuda, f2 raciocínio, shift+tab permissões, pageup/pagedown rolam.".into());
                self.abrir_modal(Modal::Texto {
                    titulo: self.idioma.t(crate::i18n::Chave::Atalhos).to_string(),
                    linhas: linhas.into_iter().map(Line::from).collect(),
                    scroll: 0,
                });
            }
            "/lang" | "/language" | "/idioma" => match argumento.trim().to_lowercase().as_str() {
                "" => self.avisar(format!(
                    "idioma atual: {}. Use `/lang pt` ou `/lang en`.",
                    self.idioma.codigo()
                )),
                "pt" | "pt-br" | "portugues" | "português" => {
                    self.idioma = crate::i18n::Idioma::PtBr;
                    self.config.ui.language = "pt-BR".into();
                    if let Err(e) = self.config.save(&self.paths) {
                        self.avisar(format!("idioma aplicado, mas não foi salvo: {e}"));
                    } else {
                        self.construir_tudo();
                        self.avisar_rapido("idioma: pt-BR".into(), 20);
                    }
                }
                "en" | "en-us" | "english" | "ingles" | "inglês" => {
                    self.idioma = crate::i18n::Idioma::EnUs;
                    self.config.ui.language = "en-US".into();
                    if let Err(e) = self.config.save(&self.paths) {
                        self.avisar(format!("language applied but not saved: {e}"));
                    } else {
                        self.construir_tudo();
                        self.avisar_rapido("language: en-US".into(), 20);
                    }
                }
                outro => self.avisar(format!("idioma `{outro}` inválido; use `pt` ou `en`.")),
            },
            "/skin" | "/tema" => {
                // Recarrega do disco: skins próprias criadas depois da sessão
                // entrarem aparecem sem reiniciar o Kestrel.
                theme::recarregar(&self.paths.skins_dir());
                if argumento.is_empty() {
                    self.avisar(format!(
                        "skin atual: {}. Disponíveis: {}.\nUse `/skin <nome>` para trocar na hora; \
                         a escolha fica salva e vale nas próximas sessões.",
                        theme::skin_atual(),
                        theme::skins().join(", ")
                    ));
                } else {
                    match theme::aplicar_skin(&argumento) {
                        Ok(()) => {
                            self.config.ui.skin = theme::skin_atual().to_string();
                            if let Err(e) = self.config.save(&self.paths) {
                                self.avisar(format!(
                                    "skin aplicada, mas não foi possível salvar: {e}"
                                ));
                            }
                            // Mantém a cópia do agente alinhada para que um
                            // /model posterior não regrave a skin antiga.
                            let _ = self
                                .cmd_tx
                                .send(UiCommand::DefinirSkin(theme::skin_atual().to_string()));
                            // Markdown e outros widgets guardam estilos já
                            // resolvidos; reconstruir aplica a nova paleta.
                            self.construir_tudo();
                            // A moldura do Pip é opaca na cor do app: sem
                            // recodificar, a skin antiga ficaria como um
                            // retângulo ao redor dele.
                            let erro_pip = self
                                .pip_grafico
                                .as_mut()
                                .and_then(|pip| pip.atualizar_tema().err());
                            if let Some(e) = erro_pip {
                                self.avisar(format!(
                                    "skin `{}` aplicada, mas o Pip não pôde ser recriado: {e}",
                                    theme::skin_atual()
                                ));
                            } else {
                                self.avisar_rapido(
                                    format!(
                                        "skin `{}` aplicada. Use /skin sem argumento para ver as opções.",
                                        theme::skin_atual()
                                    ),
                                    20,
                                );
                            }
                        }
                        Err(e) => self.avisar(e),
                    }
                }
            }
            "/permissions" | "/permissoes" => {
                if argumento.is_empty() {
                    self.abrir_modal(Modal::Permissoes(seletor_permissoes(self.nivel)));
                } else if let Some(nivel) = PermissionLevel::parse(&argumento) {
                    self.nivel = nivel;
                    self.config.definir_permissao(nivel);
                    let _ = self.cmd_tx.send(UiCommand::DefinirPermissao(nivel));
                    self.avisar_rapido(
                        format!(
                            "nível de permissão: {} — {}",
                            nivel.label(),
                            nivel.description()
                        ),
                        20,
                    );
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
                if matches!(
                    argumento.trim().to_lowercase().as_str(),
                    "reload" | "recarregar" | "atualizar"
                ) {
                    self.skills =
                        Arc::new(SkillRegistry::discover(&self.paths, &self.config.workdir()));
                    let _ = self.cmd_tx.send(UiCommand::RecarregarSkills);
                    self.construir_tudo();
                    self.avisar_rapido(
                        format!("{} skill(s) recarregada(s)", self.skills.all().len()),
                        20,
                    );
                    return;
                }
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
                self.avisar_rapido("reanalisando o projeto…".into(), 15);
            }
            "/processes" | "/processos" => {
                let _ = self.cmd_tx.send(UiCommand::ListarProcessos);
            }
            "/config" | "/configuracao" | "/configuração" => {
                let contexto = if self.contexto.1 > 0 {
                    format!(
                        "{} de {} tokens ({}%)",
                        crate::util::formatar_tokens(self.contexto.0 as u64),
                        crate::util::formatar_tokens(self.contexto.1 as u64),
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
                self.abrir_modal(Modal::Texto {
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

    // ------------------------------------------------------------ Comps

    fn executar_person(&mut self, argumento: &str) {
        let mut partes = argumento.splitn(2, char::is_whitespace);
        let acao = partes.next().unwrap_or("").trim().to_lowercase();
        let nome = partes.next().unwrap_or("").trim().to_string();
        match acao.as_str() {
            "" | "list" | "listar" => self.listar_comps(),
            "new" | "novo" | "criar" => {
                if nome.is_empty() {
                    self.avisar(
                        "uso: `/person new <nome>` — abre o editor de instruções do novo Comp."
                            .into(),
                    );
                } else if comps::slug_de(&nome).is_empty() {
                    self.avisar(
                        "o nome precisa ter ao menos uma letra ou número; use letras sem acento, \
                         números, espaço, `-` ou `_`."
                            .into(),
                    );
                } else {
                    self.abrir_comp_editor(&nome, true);
                }
            }
            "edit" | "editar" => {
                let alvo = if nome.is_empty() {
                    self.config.ui.active_comp.clone().unwrap_or_default()
                } else {
                    nome
                };
                if alvo.is_empty() {
                    self.avisar("uso: `/person edit <nome>` ou ative um Comp primeiro.".into());
                } else {
                    self.abrir_comp_editor(&alvo, false);
                }
            }
            "use" | "usar" | "ativar" => {
                if nome.is_empty() {
                    self.avisar(
                        "uso: `/person use <nome>` para ativar ou `/person off` para desativar."
                            .into(),
                    );
                } else {
                    self.ativar_comp(&nome);
                }
            }
            "off" | "desativar" | "none" => self.desativar_comp(),
            "see" | "ver" | "show" => {
                let alvo = if nome.is_empty() {
                    self.config.ui.active_comp.clone().unwrap_or_default()
                } else {
                    nome
                };
                if alvo.is_empty() {
                    self.avisar("uso: `/person see <nome>` ou ative um Comp primeiro.".into());
                } else {
                    self.ver_comp(&alvo);
                }
            }
            "remove" | "remover" | "rm" => {
                let alvo = if nome.is_empty() {
                    self.config.ui.active_comp.clone().unwrap_or_default()
                } else {
                    nome
                };
                if alvo.is_empty() {
                    self.avisar("uso: `/person remove <nome>`.".into());
                } else if let Some((comp, _)) = comps::encontrar(&self.paths.comps_dir(), &alvo) {
                    self.abrir_modal(Modal::ConfirmarComp { nome: comp.nome });
                } else {
                    self.avisar(format!(
                        "Comp `{alvo}` não encontrado em {}.",
                        self.paths.comps_dir().display()
                    ));
                }
            }
            outro => self.avisar(format!(
                "ação `{outro}` desconhecida. Use `/person`, `/person new <nome>`, \
                 `/person use <nome>`, `/person edit <nome>`, `/person see <nome>` ou \
                 `/person remove <nome>`."
            )),
        }
    }

    fn listar_comps(&mut self) {
        let dir = self.paths.comps_dir();
        let lista = comps::listar(&dir);
        if lista.is_empty() {
            self.avisar(format!(
                "nenhum Comp criado em {}.\n\
                 Crie um com `/person new <nome>` (editor dentro do Kestrel) ou peça ao agente \
                 em linguagem natural.",
                dir.display()
            ));
            return;
        }
        let ativo = self.config.ui.active_comp.as_deref();
        let mut linhas = vec![format!("{} Comp(s) em {}:", lista.len(), dir.display())];
        for comp in &lista {
            let marcador = if ativo.is_some_and(|a| comps::slug_de(a) == comp.slug()) {
                "●"
            } else {
                "○"
            };
            linhas.push(format!(
                "{marcador} {} — {} (~{} tokens)",
                comp.nome,
                if comp.descricao.is_empty() {
                    "(sem descrição)"
                } else {
                    &comp.descricao
                },
                comp.tokens()
            ));
        }
        linhas.push(
            "Use `/person use <nome>`, `/person see <nome>`, `/person edit <nome>` ou \
             `/person remove <nome>`."
                .into(),
        );
        self.avisar(linhas.join("\n"));
    }

    fn abrir_comp_editor(&mut self, nome: &str, novo: bool) {
        let dir = self.paths.comps_dir();
        let (nome_exibicao, descricao, instrucoes) = if novo {
            if comps::encontrar(&dir, nome).is_some() {
                self.avisar(format!(
                    "o Comp `{nome}` já existe. Use `/person edit {nome}` para alterar."
                ));
                return;
            }
            (nome.trim().to_string(), String::new(), String::new())
        } else {
            let Some((comp, _)) = comps::encontrar(&dir, nome) else {
                self.avisar(format!(
                    "Comp `{nome}` não encontrado em {}.",
                    dir.display()
                ));
                return;
            };
            (comp.nome, comp.descricao, comp.instrucoes)
        };
        let mut editor = Editor::novo(50);
        editor.definir_texto(instrucoes);
        self.abrir_modal(Modal::CompEditor {
            nome: nome_exibicao,
            novo,
            descricao,
            editor,
            erro: None,
            scroll: 0,
        });
    }

    fn ativar_comp(&mut self, nome: &str) {
        let Some((comp, _)) = comps::encontrar(&self.paths.comps_dir(), nome) else {
            self.avisar(format!(
                "Comp `{nome}` não encontrado. Use /person para listar os disponíveis."
            ));
            return;
        };
        self.config.ui.active_comp = Some(comp.nome.clone());
        if let Err(e) = self.config.save(&self.paths) {
            self.avisar(format!("Comp ativado, mas não foi possível salvar: {e}"));
        }
        let _ = self
            .cmd_tx
            .send(UiCommand::DefinirComp(Some(comp.nome.clone())));
        self.avisar_rapido(
            format!(
                "Comp ativo: {} (~{} tokens no system prompt). Vale a partir do próximo turno.",
                comp.nome,
                comp.tokens()
            ),
            20,
        );
    }

    fn desativar_comp(&mut self) {
        if self.config.ui.active_comp.take().is_none() {
            self.avisar("nenhum Comp está ativo.".into());
            return;
        }
        if let Err(e) = self.config.save(&self.paths) {
            self.avisar(format!("Comp desativado, mas não foi possível salvar: {e}"));
        }
        let _ = self.cmd_tx.send(UiCommand::DefinirComp(None));
        self.avisar_rapido("Comp desativado; o Kestrel volta ao tom padrão.".into(), 20);
    }

    fn ver_comp(&mut self, nome: &str) {
        let Some((comp, _)) = comps::encontrar(&self.paths.comps_dir(), nome) else {
            self.avisar(format!("Comp `{nome}` não encontrado."));
            return;
        };
        let mut linhas = vec![
            Line::from(vec![
                Span::styled("Comp — ", theme::apagado()),
                Span::styled(comp.nome.clone(), theme::titulo()),
            ]),
            Line::from(Span::styled(
                format!("descrição: {}", comp.descricao),
                theme::apagado(),
            )),
            Line::from(Span::styled(
                format!(
                    "custo aproximado: {} tokens · arquivo: {}",
                    comp.tokens(),
                    comp.caminho(&self.paths.comps_dir()).display()
                ),
                theme::apagado(),
            )),
            Line::default(),
            Line::from(Span::styled("instruções:", theme::rotulo())),
        ];
        linhas.extend(
            comp.instrucoes
                .lines()
                .map(|linha| Line::from(Span::styled(linha.to_string(), theme::texto()))),
        );
        self.abrir_modal(Modal::Texto {
            titulo: format!("Comp — {}", comp.nome),
            linhas,
            scroll: 0,
        });
    }

    fn salvar_comp_modal(&mut self) {
        let Some(Modal::CompEditor {
            nome,
            novo,
            descricao,
            editor,
            erro: _,
            scroll,
        }) = self.modal.take()
        else {
            return;
        };
        let instrucoes = editor.texto().to_string();
        let comp = if descricao.trim().is_empty() {
            comps::Comp::novo(&nome, instrucoes)
        } else {
            comps::Comp::com_descricao(&nome, descricao.clone(), instrucoes)
        };
        match comp.and_then(|comp| comp.salvar(&self.paths.comps_dir(), !novo).map(|_| comp)) {
            Ok(comp) => self.avisar(format!(
                "Comp `{}` salvo (~{} tokens). Use `/person use {}` para ativá-lo.",
                comp.nome,
                comp.tokens(),
                comp.nome
            )),
            Err(e) => {
                self.abrir_modal(Modal::CompEditor {
                    nome,
                    novo,
                    descricao,
                    editor,
                    erro: Some(e),
                    scroll,
                });
            }
        }
    }

    fn remover_comp_confirmado(&mut self) {
        let Some(Modal::ConfirmarComp { nome }) = self.modal.take() else {
            return;
        };
        match comps::remover(&self.paths.comps_dir(), &nome) {
            Ok(caminho) => {
                let era_ativo = self
                    .config
                    .ui
                    .active_comp
                    .as_deref()
                    .is_some_and(|ativo| comps::slug_de(ativo) == comps::slug_de(&nome));
                if era_ativo {
                    self.config.ui.active_comp = None;
                    let _ = self.config.save(&self.paths);
                    let _ = self.cmd_tx.send(UiCommand::DefinirComp(None));
                }
                self.avisar(format!(
                    "Comp `{nome}` removido ({}){}.",
                    caminho.display(),
                    if era_ativo {
                        "; era o ativo, então o tom padrão foi restaurado"
                    } else {
                        ""
                    }
                ));
            }
            Err(e) => self.avisar(e),
        }
    }

    /// Troca o modo de operação e informa o usuário (o agente persiste).
    fn trocar_modo(&mut self, modo: ModoOperacao) {
        if self.modo == modo {
            self.avisar_rapido(format!("o modo já é {}.", modo.label()), 15);
            return;
        }
        self.modo = modo;
        let _ = self.cmd_tx.send(UiCommand::DefinirModo(modo));
        self.avisar_rapido(
            format!(
                "modo de operação: {} — {}",
                modo.label(),
                modo.description()
            ),
            20,
        );
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

    /// `/undo`: desfaz a última alteração de arquivo da sessão, aplicando o
    /// diff guardado no sentido inverso. Não depende de git.
    fn desfazer_ultima(&mut self) {
        let Some(alt) = self.store.ultima_alteracao(&self.sessao_id).unwrap_or(None) else {
            self.avisar("Nada para desfazer: esta sessão ainda não alterou arquivos.".into());
            return;
        };
        // O caminho é relativo ao projeto; reconstrói o absoluto.
        let caminho = std::path::Path::new(&alt.path);
        let absoluto = if caminho.is_absolute() {
            caminho.to_path_buf()
        } else {
            self.config.workdir().join(caminho)
        };
        let atual = match std::fs::read_to_string(&absoluto) {
            Ok(t) => t,
            Err(e) => {
                self.avisar(format!(
                    "Não foi possível ler {}: {e}. O arquivo pode ter sido removido.",
                    absoluto.display()
                ));
                return;
            }
        };
        match crate::diff::reverter(&alt.diff, &atual) {
            Some(anterior) => {
                if std::fs::write(&absoluto, &anterior).is_err() {
                    self.avisar(format!("Falha ao escrever {}", absoluto.display()));
                    return;
                }
                if let Err(e) = self.store.marcar_alteracao_desfeita(alt.id) {
                    // O arquivo já foi restaurado; sem a marca, o próximo
                    // `/undo` tentaria reaplicar este mesmo diff (que não bate
                    // mais) e travaria a pilha. Desfaz a escrita para manter
                    // arquivo e banco consistentes: o usuário pode tentar de
                    // novo em vez de perder a possibilidade de desfazer.
                    if let Err(revert) = std::fs::write(&absoluto, &atual) {
                        self.avisar(format!(
                            "Arquivo restaurado, mas não foi possível registrar o desfazer ({e}) \
                             nem reverter a escrita ({revert}). Verifique {} manualmente.",
                            absoluto.display()
                        ));
                    } else {
                        self.avisar(format!(
                            "Não foi possível registrar o desfazer: {e}. O arquivo foi mantido como estava."
                        ));
                    }
                    return;
                }
                self.avisar(format!(
                    "Desfeito: {} voltou ao estado anterior ({} caracteres).",
                    alt.path,
                    anterior.chars().count()
                ));
            }
            None => {
                let motivo = if alt.diff.contains(crate::diff::DIFF_TRUNCADO) {
                    "o diff registrado foi truncado por ser grande demais; \
                     desfazer automaticamente aplicaria um patch parcial e corromperia o arquivo. \
                     Use o controle de versão para reverter."
                } else {
                    "o arquivo mudou desde a alteração registrada (o diff não bate mais com o conteúdo)."
                };
                self.avisar(format!("Não foi possível desfazer {}: {motivo}", alt.path));
            }
        }
    }

    /// `/diff`: lista as alterações da sessão com o diff real de cada uma.
    fn mostrar_diff(&mut self) {
        let alteracoes = self.store.alteracoes(&self.sessao_id).unwrap_or_default();
        if alteracoes.is_empty() {
            self.avisar("Nenhuma alteração de arquivo nesta sessão.".into());
            return;
        }
        let mut linhas = vec![Line::from(Span::styled(
            format!(" {} alteração(ões) nesta sessão ", alteracoes.len()),
            theme::rotulo(),
        ))];
        for alt in &alteracoes {
            linhas.push(Line::from(vec![
                Span::styled(format!(" {} ", alt.change_type), theme::apagado()),
                Span::styled(alt.path.clone(), theme::texto()),
            ]));
            for linha in alt.diff.lines().take(60) {
                let estilo = if let Some(resto) = linha.strip_prefix('+') {
                    Span::styled(format!(" +{resto}"), theme::sucesso())
                } else if let Some(resto) = linha.strip_prefix('-') {
                    Span::styled(format!(" -{resto}"), theme::falha())
                } else {
                    Span::styled(format!(" {}", linha.trim_start()), theme::apagado())
                };
                linhas.push(Line::from(estilo));
            }
        }
        self.abrir_modal(Modal::Texto {
            titulo: "Alterações da sessão".into(),
            linhas,
            scroll: 0,
        });
    }

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

    /// Aplica a preferência de exibição do raciocínio e propaga ao agente.
    ///
    /// `/thinking` e F2 precisam do mesmo efeito. Antes, F2 só trocava o
    /// booleano local: o agente continuava enviando os deltas (trabalho e
    /// tráfego desperdiçados) e a escolha não era salva para a próxima sessão.
    fn definir_raciocinio(&mut self, novo: bool) {
        self.mostrar_raciocinio = novo;
        self.config.ui.show_reasoning = novo;
        // Salva sempre: desligar também é uma preferência, e voltar ao padrão
        // depois exigiria religar à mão.
        if let Err(e) = self.config.save(&self.paths) {
            self.avisar(format!(
                "não foi possível salvar a preferência de raciocínio: {e}"
            ));
        }
        // A escolha vale para o agente: com o raciocínio oculto ele deixa de
        // enviar os deltas de pensamento para a interface.
        let _ = self.cmd_tx.send(UiCommand::DefinirRaciocinio(novo));
        self.construir_tudo();
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
        self.abrir_modal(Modal::Modelos(seletor));
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
        self.abrir_modal(Modal::Esforco(Seletor::novo(
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
        self.abrir_modal(Modal::Sessoes {
            seletor: Seletor::novo(
                "Histórico de sessões",
                "enter retoma · espaço marca para excluir · d exclui as marcadas · esc cancela",
                itens,
            ),
            marcadas: std::collections::HashSet::new(),
        });
    }

    /// Exclui as sessões dadas (o usuário já confirmou). A sessão ativa nunca
    /// chega aqui — quem filtra é o próprio fluxo de marcação.
    fn excluir_sessoes(&mut self, ids: Vec<String>) {
        let total = ids.len();
        match self.store.delete_sessions(&ids) {
            Ok(n) => {
                self.fechar_modal();
                if n == 0 {
                    self.avisar("nenhuma sessão encontrada para excluir.".into());
                    return;
                }
                self.avisar_rapido(format!("{n} de {total} sessão(ões) excluída(s)"), 25);
            }
            Err(e) => self.avisar(format!("falha ao excluir sessões: {e}")),
        }
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
        self.abrir_modal(Modal::Projetos(Seletor::novo(
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
        self.abrir_modal(Modal::Processos(Seletor::novo(
            "Processos",
            "esc fecha",
            itens_processos(&lista),
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
                // Mantém a cópia da interface em sincronia: os outros `save`
                // (skin, raciocínio) gravam o Config inteiro e, sem isto,
                // revertiam o nível recém-escolhido no disco.
                self.config.definir_permissao(nivel);
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
                self.turno_iniciado_em = Some(std::time::Instant::now());
                self.atividade_chave = None;
                self.atividade_linha = None;
                self.atualizar_atividade();
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
                    limitar_texto(texto);
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
                    limitar_texto(texto);
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
            AgentEvent::FerramentaIniciada {
                id,
                nome,
                detalhe,
                subagente,
            } => {
                self.empurrar_bloco(Bloco::Ferramenta {
                    id,
                    nome,
                    detalhe,
                    rodando: true,
                    sucesso: false,
                    resumo: None,
                    adicoes: 0,
                    remocoes: 0,
                    diff: Vec::new(),
                    subagente,
                });
                self.definir_estado(EstadoMascote::Ferramentas, "executando ferramenta");
                self.acompanhar_fim();
            }
            AgentEvent::FerramentaSaida { id, nome, linha } => {
                let limpar = crate::util::strip_ansi(&linha);
                if limpar.trim().is_empty() {
                    return;
                }
                // Linhas de saída ficam agrupadas por ferramenta; chamadas
                // paralelas de mesmo nome não se misturam porque o id da
                // chamada desempata.
                let existente = if id.is_some() {
                    self.blocos
                        .iter()
                        .rposition(|b| matches!(b, Bloco::Saida { id: bid, .. } if bid == &id))
                } else {
                    self.blocos
                        .last()
                        .filter(
                            |b| matches!(b, Bloco::Saida { id: None, nome: n, .. } if n == &nome),
                        )
                        .map(|_| self.blocos.len() - 1)
                };
                match existente {
                    Some(i) => {
                        if let Some(Bloco::Saida { linhas, .. }) = self.blocos.get_mut(i) {
                            linhas.push(crate::util::truncate(&limpar, 400));
                            if linhas.len() > 300 {
                                let remover = linhas.len() - 300;
                                linhas.drain(..remover);
                            }
                        }
                        // O bloco pode não ser o último (saídas paralelas se
                        // intercalam): reconstruir só o último deixaria a nova
                        // linha invisível até o próximo evento.
                        self.reconstruir_do(i);
                    }
                    None => {
                        self.empurrar_bloco(Bloco::Saida {
                            id,
                            nome,
                            linhas: vec![crate::util::truncate(&limpar, 400)],
                        });
                    }
                }
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::FerramentaConcluida {
                id,
                nome,
                sucesso,
                resumo,
                adicoes,
                remocoes,
                diff,
            } => {
                let encontrado = self.localizar_ferramenta(id.as_deref(), &nome);
                if let Some(i) = encontrado {
                    if let Bloco::Ferramenta {
                        rodando,
                        sucesso: ok,
                        resumo: r,
                        adicoes: a,
                        remocoes: rem,
                        diff: d,
                        ..
                    } = &mut self.blocos[i]
                    {
                        *rodando = false;
                        *ok = sucesso;
                        *r = Some(resumo);
                        *a = adicoes;
                        *rem = remocoes;
                        if !diff.trim().is_empty() {
                            *d = LinhaDiff::de_texto(&diff);
                        }
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
            AgentEvent::SubagenteIniciado { tarefa } => {
                // Grupo do subagente: cabeçalho com a subtarefa. As ferramentas
                // que ele chamar entram marcadas abaixo dela.
                self.empurrar_bloco(Bloco::Subagente {
                    tarefa: crate::util::truncate(&tarefa, 120),
                    rodando: true,
                    sucesso: false,
                });
                self.definir_estado(EstadoMascote::Ferramentas, "subagente trabalhando");
                self.acompanhar_fim();
            }
            AgentEvent::SubagenteConcluido { sucesso } => {
                // Fecha o grupo: o último bloco de subagente sai do estado
                // "rodando" para o resultado final.
                if let Some(Bloco::Subagente {
                    rodando,
                    sucesso: ok,
                    ..
                }) = self
                    .blocos
                    .iter_mut()
                    .rev()
                    .find(|b| matches!(b, Bloco::Subagente { .. }))
                {
                    *rodando = false;
                    *ok = sucesso;
                    self.construir_tudo();
                }
                if self.estado == EstadoMascote::Ferramentas {
                    self.definir_estado(
                        if sucesso {
                            EstadoMascote::Pensando
                        } else {
                            EstadoMascote::Erro
                        },
                        if sucesso {
                            "subagente concluiu"
                        } else {
                            "subagente falhou"
                        },
                    );
                }
                self.acompanhar_fim();
            }
            AgentEvent::PlanoAtualizado { objetivo, passos } => {
                self.aplicar_plano(objetivo, passos);
            }
            AgentEvent::CatalogoAtualizado { modelos, novos } => {
                // O catálogo chega já mesclado com as entradas manuais. Só
                // persiste quando mudou alguma coisa (novos modelos ou entradas
                // diferentes) — nada de reescrever o config toda abertura.
                let mudou = modelos.len() != self.config.provider.models.len()
                    || modelos
                        .iter()
                        .any(|m| !self.config.provider.models.iter().any(|e| e.id == m.id));
                crate::tokenizer::registrar_modelos(&modelos);
                self.config.provider.models = modelos;
                if mudou && let Err(e) = self.config.save(&self.paths) {
                    self.avisar(format!("catálogo atualizado, mas falha ao salvar: {e}"));
                    return;
                }
                if novos > 0 {
                    self.avisar_rapido(
                        format!("catálogo atualizado: {novos} modelo(s) novo(s) — veja /model"),
                        30,
                    );
                }
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
                self.empurrar_bloco(Bloco::Recuperacao {
                    descricao,
                    tentativa,
                    maximo,
                });
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::TurnoConcluido { resumo } => {
                self.turno_ativo = false;
                self.turno_iniciado_em = None;
                self.atividade_chave = None;
                self.atividade_linha = None;
                self.empurrar_bloco(Bloco::Turno(resumo));
                self.estado = EstadoMascote::Concluindo;
                self.estado_texto = "concluído".into();
                self.estado_expira = Some(self.tick + 30);
                // Evento do agente: so arrasta para o fim quem ja estava la.
                self.acompanhar_fim();
            }
            AgentEvent::Interrompido { resumo } => {
                self.turno_ativo = false;
                self.turno_iniciado_em = None;
                self.atividade_chave = None;
                self.atividade_linha = None;
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
                self.estado_texto = com_maiuscula(mensagem);
            }
            AgentEvent::Ramificada {
                id,
                titulo,
                mensagens,
            } => {
                self.sessao_id = id;
                self.sessao_titulo = titulo.clone();
                self.avisar_rapido(
                    format!(
                        "ramo criado: «{}» ({mensagens} mensagens copiadas)",
                        crate::util::truncate(&titulo, 50)
                    ),
                    30,
                );
            }
            AgentEvent::SessaoAtiva {
                id,
                titulo,
                workdir,
            } => {
                let sessao_mudou = !self.sessao_id.is_empty() && self.sessao_id != id;
                if sessao_mudou {
                    self.limpar_conversa();
                    self.plano = None;
                    self.tarefas.clear();
                }
                self.sessao_id = id;
                self.sessao_titulo = titulo;
                // O workdir do evento é a fonte da verdade: retomar uma sessão
                // de outro projeto precisa alinhar a interface ao diretório em
                // que o agente realmente está trabalhando.
                let caminho = crate::util::expand_tilde(&workdir);
                let caminho = std::fs::canonicalize(&caminho).unwrap_or(caminho);
                let atual = std::fs::canonicalize(self.config.workdir())
                    .unwrap_or_else(|_| self.config.workdir());
                if caminho != atual {
                    self.config.agent.workdir = Some(caminho.clone());
                    self.config.agent.pin_workdir = true;
                }
                self.projeto = Arc::new(project::analyze(&caminho));
                self.skills = Arc::new(SkillRegistry::discover(&self.paths, &caminho));
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
                        Some(nivel) => self.avisar_rapido(
                            format!("modelo atual: {modelo} (esforço de raciocínio: {nivel})"),
                            20,
                        ),
                        None => self.avisar_rapido(format!("modelo atual: {modelo}"), 20),
                    }
                }
            }
            AgentEvent::Processos { lista } => {
                self.abrir_processos(lista);
            }
            AgentEvent::ProcessosAtualizados { lista } => {
                // Ferramentas em segundo plano atualizam o estado sem abrir
                // um modal por cima do que o usuário está fazendo. Se o modal
                // de processos já está aberto, ele é atualizado no lugar.
                self.processos = lista.clone();
                if matches!(self.modal, Some(Modal::Processos(_))) {
                    if lista.is_empty() {
                        self.fechar_modal();
                        self.avisar("nenhum processo gerenciado no momento.".into());
                    } else if let Some(Modal::Processos(seletor)) = &mut self.modal {
                        seletor.itens = itens_processos(&lista);
                        seletor.ajustar_selecao();
                    }
                }
            }
            AgentEvent::Encerrado => {
                self.saindo = true;
                self.encerrado = true;
                self.estado_texto = "encerrado".into();
                self.estado = EstadoMascote::Ocioso;
            }
        }
    }

    /// Localiza o bloco de ferramenta que corresponde a uma conclusão.
    ///
    /// O id da chamada é a chave preferida; o nome é só o fallback para
    /// eventos de subagente, que não carregam id.
    fn localizar_ferramenta(&self, id: Option<&str>, nome: &str) -> Option<usize> {
        if let Some(id) = id
            && let Some(i) = self.blocos.iter().rposition(|b| {
                matches!(
                    b,
                    Bloco::Ferramenta {
                        id: Some(bid),
                        rodando: true,
                        ..
                    } if bid == id
                )
            })
        {
            return Some(i);
        }
        self.blocos.iter().rposition(|b| {
            matches!(
                b,
                Bloco::Ferramenta {
                    nome: n,
                    rodando: true,
                    ..
                } if n == nome
            )
        })
    }

    /// Fecha o modal atual. Cancelar a escolha de esforço também cancela a
    /// troca de modelo que a originou; senão o modelo ficaria "pendente" e um
    /// `/effort` posterior o aplicaria sem o usuário saber.
    fn fechar_modal(&mut self) {
        if matches!(self.modal, Some(Modal::Esforco(_))) {
            self.modelo_pendente = None;
        }
        self.modal = None;
        self.modal_desde = None;
    }

    /// Abre um modal registrando o tick de abertura (animação de crescimento).
    fn abrir_modal(&mut self, modal: Modal) {
        self.modal = Some(modal);
        self.modal_desde = Some(self.tick);
    }

    fn proximo_confirmacao(&mut self) {
        if self.modal.is_none()
            && let Some(pedido) = self.fila_confirmacoes.pop_front()
        {
            self.abrir_modal(Modal::Confirmacao(pedido));
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
            crossterm::event::Event::Paste(texto) => match &mut self.modal {
                None => {
                    self.entrada.inserir_texto(&texto);
                    self.sugestao_sel = 0;
                }
                // Em seletores, colar alimenta o filtro em vez de ser
                // descartado em silêncio.
                Some(
                    Modal::Permissoes(seletor)
                    | Modal::Modelos(seletor)
                    | Modal::Esforco(seletor)
                    | Modal::Sessoes { seletor, .. }
                    | Modal::Projetos(seletor)
                    | Modal::Processos(seletor),
                ) => {
                    let limpo: String = texto.chars().filter(|c| !c.is_control()).collect();
                    if !limpo.is_empty() {
                        seletor.filtro.push_str(&limpo);
                        seletor.sel = 0;
                        seletor.ajustar_selecao();
                    }
                }
                Some(Modal::CompEditor { editor, erro, .. }) => {
                    editor.inserir_texto(&texto);
                    *erro = None;
                }
                // Confirmação e texto não têm campo de entrada.
                Some(_) => {}
            },
            _ => {}
        }
    }

    fn tratar_mouse(&mut self, mouse: MouseEvent) {
        match mouse.kind {
            MouseEventKind::ScrollUp => {
                let alvo = self.scroll_alvo.saturating_sub(3);
                self.rolar_para(alvo);
            }
            MouseEventKind::ScrollDown => {
                // No fim, descer não precisa desligar o acompanhamento; fora
                // dele, o alvo parte da posição desejada (não da animada), para
                // vários giros seguidos não se perderem.
                if !self.seguir_fim {
                    let alvo = self.scroll_alvo.saturating_add(3);
                    self.rolar_para(alvo);
                }
            }
            MouseEventKind::Down(botao) => {
                // Clique no atalho "voltar para o último".
                if let Some(area) = self.area_voltar
                    && mouse.column >= area.x
                    && mouse.column < area.x + area.width
                    && mouse.row >= area.y
                    && mouse.row < area.y + area.height
                {
                    self.ir_para_o_fim();
                    return;
                }
                // Clique num bloco da conversa: abre ou fecha. Só o botão
                // esquerdo, e só sobre um bloco — clicar em texto não faz
                // nada, então não há risco de fechar sem querer.
                if botao == MouseButton::Left {
                    let alvo = self
                        .areas_blocos
                        .iter()
                        .find(|(_, area)| {
                            mouse.column >= area.x
                                && mouse.column < area.x + area.width
                                && mouse.row >= area.y
                                && mouse.row < area.y + area.height
                        })
                        .map(|(indice, _)| *indice);
                    if let Some(indice) = alvo {
                        self.alternar_bloco(indice);
                    }
                }
            }
            _ => {}
        }
    }

    /// Converte uma linha lógica para o deslocamento renderizado, quando o
    /// cache de quebra está disponível.
    fn app_scroll_da_linha(&self, linha: usize) -> usize {
        self.cache_alturas
            .as_ref()
            .and_then(|(_, _, prefixo)| prefixo.get(linha).copied())
            .unwrap_or(linha)
    }

    /// Executa uma ação configurável. Devolve `false` se a ação não existe.
    fn executar_acao(&mut self, acao: &str) -> bool {
        match acao {
            "buscar" => {
                self.abrir_busca();
                true
            }
            "copiar" => {
                self.copiar_ultima_resposta();
                true
            }
            "ajuda" => {
                self.abrir_modal(Modal::Ajuda {
                    linhas: texto_de_ajuda(),
                    scroll: 0,
                });
                true
            }
            "raciocinio" => {
                self.definir_raciocinio(!self.mostrar_raciocinio);
                let estado = if self.mostrar_raciocinio {
                    "raciocínio visível"
                } else {
                    "raciocínio oculto"
                };
                self.avisar_rapido(estado.to_string(), 20);
                true
            }
            "permissoes" => {
                self.ciclar_permissao();
                true
            }
            "rolar_cima" => {
                let alvo = self.scroll_alvo.saturating_sub(15);
                self.rolar_para(alvo);
                true
            }
            "rolar_baixo" => {
                if !self.seguir_fim {
                    let alvo = self.scroll_alvo.saturating_add(15);
                    self.rolar_para(alvo);
                }
                true
            }
            "interromper" => {
                if self.turno_ativo {
                    let _ = self.cmd_tx.send(UiCommand::Interromper);
                    self.empurrar_bloco(Bloco::Aviso(
                        "interrupção solicitada; as alterações já feitas serão preservadas".into(),
                    ));
                } else {
                    self.entrada.limpar();
                }
                true
            }
            _ => false,
        }
    }

    fn ciclar_permissao(&mut self) {
        self.nivel = self.nivel.cycle();
        self.config.definir_permissao(self.nivel);
        let _ = self.cmd_tx.send(UiCommand::DefinirPermissao(self.nivel));
        self.avisar_rapido(
            format!(
                "nível de permissão: {} — {}",
                self.nivel.label(),
                self.nivel.description()
            ),
            20,
        );
    }

    /// Copia a última resposta do assistente via OSC 52 (funciona em terminais
    /// e em SSH que aceitam a sequência; tmux pode exigir `set-clipboard on`).
    fn copiar_ultima_resposta(&mut self) {
        let texto = self.blocos.iter().rev().find_map(|bloco| match bloco {
            Bloco::Assistente(texto) if !texto.trim().is_empty() => Some(texto.clone()),
            _ => None,
        });
        let Some(texto) = texto else {
            self.avisar(self.idioma.t(crate::i18n::Chave::SemResposta).to_string());
            return;
        };
        let codificado = crate::util::base64(texto.as_bytes());
        use std::io::Write;
        let mut saida = std::io::stdout();
        if saida
            .write_all(format!("\x1b]52;c;{codificado}\x07").as_bytes())
            .is_ok()
        {
            self.avisar_rapido(self.idioma.t(crate::i18n::Chave::Copiada).to_string(), 20);
        } else {
            self.avisar("não foi possível enviar a cópia ao terminal".into());
        }
    }

    fn abrir_busca(&mut self) {
        self.abrir_modal(Modal::Busca {
            consulta: String::new(),
            indice: 0,
        });
    }

    pub(crate) fn linhas_com(&self, consulta: &str) -> Vec<usize> {
        let consulta = consulta.trim().to_lowercase();
        if consulta.is_empty() {
            return Vec::new();
        }
        self.linhas
            .iter()
            .enumerate()
            .filter(|(_, linha)| {
                let texto: String = linha
                    .spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
                    .to_lowercase();
                texto.contains(&consulta)
            })
            .map(|(indice, _)| indice)
            .collect()
    }

    fn buscar_saltar(&mut self, passo: isize) {
        let (consulta, indice_atual) = match &self.modal {
            Some(Modal::Busca { consulta, indice }) => (consulta.clone(), *indice),
            _ => return,
        };
        let achados = self.linhas_com(&consulta);
        if achados.is_empty() {
            return;
        }
        let novo = if passo == 0 {
            indice_atual.min(achados.len() - 1)
        } else if passo > 0 {
            (indice_atual + passo as usize) % achados.len()
        } else {
            let volta = (-passo) as usize % achados.len();
            (indice_atual + achados.len() - volta) % achados.len()
        };
        if let Some(Modal::Busca { indice, .. }) = &mut self.modal {
            *indice = novo;
        }
        let linha = achados[novo];
        // Converte a linha lógica para a posição renderizada usando o cache de
        // alturas (quando disponível): linhas longas quebram em várias e o
        // salto precisa cair na linha certa, não no índice lógico.
        let alvo_renderizado = self.app_scroll_da_linha(linha);
        self.seguir_fim = false;
        self.scroll_alvo = alvo_renderizado.min(u16::MAX as usize) as u16;
    }

    fn tratar_busca(&mut self, tecla: KeyEvent) {
        let controle = tecla.modifiers.contains(KeyModifiers::CONTROL);
        match tecla.code {
            KeyCode::Esc => self.fechar_modal(),
            KeyCode::Enter | KeyCode::Down => self.buscar_saltar(1),
            KeyCode::Up => self.buscar_saltar(-1),
            KeyCode::Backspace => {
                if let Some(Modal::Busca { consulta, indice }) = &mut self.modal {
                    consulta.pop();
                    *indice = 0;
                }
                self.buscar_saltar(0);
            }
            KeyCode::Char(c) if !controle => {
                if let Some(Modal::Busca { consulta, indice }) = &mut self.modal {
                    consulta.push(c);
                    *indice = 0;
                }
                self.buscar_saltar(0);
            }
            _ => {}
        }
    }

    /// Volta a acompanhar o estado mais recente da conversa.
    fn ir_para_o_fim(&mut self) {
        // O desenho posiciona `scroll` no máximo quando `seguir_fim` é true.
        // Um alvo em `u16::MAX` nunca convergia: o desenho clampava `scroll`
        // para `max_scroll`, `tiquetaquear` o puxava de volta para cima e a
        // interface redesenhanvava para sempre sem chegar ao fim.
        self.seguir_fim = true;
        self.scroll_alvo = self.scroll;
    }

    /// Rola até uma posição, com a escalada suave feita em `tiquetaquear`.
    fn rolar_para(&mut self, alvo: u16) {
        self.seguir_fim = false;
        self.scroll_alvo = alvo;
    }

    fn tratar_tecla(&mut self, tecla: KeyEvent) {
        let controle = tecla.modifiers.contains(KeyModifiers::CONTROL);
        // Ctrl+C é global: vale no rascunho, com modal aberto e durante o
        // turno. Antes ele morria no tratamento de modal (não interrompia nem
        // encerrava) e não negava a confirmação pendente.
        if tecla.code == KeyCode::Char('c') && controle {
            if self.modal.is_some() {
                if matches!(self.modal, Some(Modal::Confirmacao(_))) {
                    self.responder_confirmacao(Confirmacao::Negar);
                } else {
                    self.fechar_modal();
                    // Havia confirmações na fila: a próxima deve aparecer
                    // imediatamente, sem esperar outro evento.
                    self.proximo_confirmacao();
                }
                if self.turno_ativo {
                    let _ = self.cmd_tx.send(UiCommand::Interromper);
                    self.empurrar_bloco(Bloco::Aviso(
                        "interrupção solicitada (Ctrl+C novamente encerra o Kestrel)".into(),
                    ));
                } else {
                    self.avisar("modal fechado; Ctrl+C de novo encerra o Kestrel.".into());
                }
                return;
            }
            if self.turno_ativo {
                let _ = self.cmd_tx.send(UiCommand::Interromper);
                self.empurrar_bloco(Bloco::Aviso(
                    "interrupção solicitada (Ctrl+C novamente encerra o Kestrel)".into(),
                ));
            } else {
                self.iniciar_saida();
            }
            return;
        }
        if self.modal.is_some() {
            self.tratar_tecla_modal(tecla);
            return;
        }
        // Atalhos configuráveis (padrões ou `ui.keybindings`). Só sem modal;
        // dentro de um modal as teclas pertencem ao editor do modal.
        if tecla.kind == KeyEventKind::Press
            && let Some(acao) = self.keymap.acao(&tecla).map(str::to_string)
            && self.executar_acao(&acao)
        {
            return;
        }
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
            _ => {}
        }
    }

    fn tratar_tecla_modal(&mut self, tecla: KeyEvent) {
        if matches!(self.modal, Some(Modal::Busca { .. })) {
            self.tratar_busca(tecla);
            return;
        }
        let controle = tecla.modifiers.contains(KeyModifiers::CONTROL);
        let alt = tecla.modifiers.contains(KeyModifiers::ALT);
        enum Reacao {
            Nada,
            Fechar,
            Confirmar(Confirmacao),
            Escolher(ItemSeletor),
            SalvarComp,
            RemoverComp,
            Avisar(String),
            PedirConfirmacaoExcluir(Vec<String>),
            ConfirmarExcluir(Vec<String>),
        }
        let reacao = match self.modal.as_mut() {
            None => Reacao::Nada,
            Some(Modal::Busca { .. }) => Reacao::Nada,
            Some(Modal::Confirmacao(_)) if controle || alt => Reacao::Nada,
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
                    KeyCode::Char(c) if !controle && !alt => {
                        seletor.filtro.push(c);
                        seletor.sel = 0;
                    }
                    _ => {}
                }
                seletor.ajustar_selecao();
                reacao
            }
            // Histórico de sessões: além de retomar (enter), marca várias para
            // excluir de uma vez (espaço/X) e pede confirmação (d).
            Some(Modal::Sessoes { seletor, marcadas }) => {
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
                    // Espaço ou X marca/desmarca a sessão atual para exclusão.
                    KeyCode::Char(' ') | KeyCode::Char('x') | KeyCode::Char('X') => {
                        if let Some((indice, item)) = seletor.filtrados().get(seletor.sel)
                            && matches!(item.valor, ValorSeletor::Sessao(_))
                            && !marcadas.insert(*indice)
                        {
                            marcadas.remove(indice);
                        }
                    }
                    // D: exclui as marcadas (a sessão ativa nunca entra).
                    KeyCode::Char('d') | KeyCode::Char('D') => {
                        if marcadas.is_empty() {
                            reacao = Reacao::Avisar(
                                "marque sessões com espaço antes de excluir.".into(),
                            );
                        } else {
                            let ids: Vec<String> = seletor
                                .itens
                                .iter()
                                .enumerate()
                                .filter(|(i, _)| marcadas.contains(i))
                                .filter_map(|(_, item)| match &item.valor {
                                    ValorSeletor::Sessao(id) => Some(id.clone()),
                                    _ => None,
                                })
                                .filter(|id| *id != self.sessao_id)
                                .collect();
                            if ids.is_empty() {
                                reacao =
                                    Reacao::Avisar("a sessão atual não pode ser excluída.".into());
                            } else {
                                reacao = Reacao::PedirConfirmacaoExcluir(ids);
                            }
                        }
                    }
                    KeyCode::Char(c) if !controle && !alt => {
                        seletor.filtro.push(c);
                        seletor.sel = 0;
                    }
                    _ => {}
                }
                seletor.ajustar_selecao();
                reacao
            }
            Some(Modal::ConfirmarSessoes { ids }) => match tecla.code {
                KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Enter => {
                    Reacao::ConfirmarExcluir(ids.clone())
                }
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Reacao::Fechar,
                _ => Reacao::Nada,
            },
            Some(Modal::CompEditor {
                editor,
                erro,
                scroll,
                ..
            }) => {
                if controle {
                    match tecla.code {
                        KeyCode::Char('s') | KeyCode::Char('S') => Reacao::SalvarComp,
                        _ => Reacao::Nada,
                    }
                } else {
                    match tecla.code {
                        KeyCode::Esc => Reacao::Fechar,
                        KeyCode::Enter => {
                            editor.nova_linha();
                            *erro = None;
                            Reacao::Nada
                        }
                        KeyCode::Backspace => {
                            editor.backspace();
                            *erro = None;
                            Reacao::Nada
                        }
                        KeyCode::Delete => {
                            editor.apagar();
                            *erro = None;
                            Reacao::Nada
                        }
                        KeyCode::Left => {
                            editor.esquerda();
                            Reacao::Nada
                        }
                        KeyCode::Right => {
                            editor.direita();
                            Reacao::Nada
                        }
                        KeyCode::Home => {
                            editor.home();
                            Reacao::Nada
                        }
                        KeyCode::End => {
                            editor.fim();
                            Reacao::Nada
                        }
                        KeyCode::Up => {
                            editor.cima();
                            Reacao::Nada
                        }
                        KeyCode::Down => {
                            editor.baixo();
                            Reacao::Nada
                        }
                        KeyCode::PageUp => {
                            *scroll = scroll.saturating_sub(10);
                            Reacao::Nada
                        }
                        KeyCode::PageDown => {
                            *scroll = scroll.saturating_add(10);
                            Reacao::Nada
                        }
                        KeyCode::Char(c) if !alt => {
                            editor.inserir_char(c);
                            *erro = None;
                            Reacao::Nada
                        }
                        _ => Reacao::Nada,
                    }
                }
            }
            Some(Modal::ConfirmarComp { .. }) => match tecla.code {
                KeyCode::Char('s') | KeyCode::Char('S') | KeyCode::Enter => Reacao::RemoverComp,
                KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => Reacao::Fechar,
                _ => Reacao::Nada,
            },
            Some(Modal::Texto { scroll, .. }) | Some(Modal::Ajuda { scroll, .. }) => {
                match tecla.code {
                    KeyCode::Esc | KeyCode::Enter => Reacao::Fechar,
                    KeyCode::Char('q') if !controle && !alt => Reacao::Fechar,
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
            Reacao::Fechar => self.fechar_modal(),
            Reacao::Confirmar(resposta) => self.responder_confirmacao(resposta),
            Reacao::SalvarComp => self.salvar_comp_modal(),
            Reacao::RemoverComp => self.remover_comp_confirmado(),
            Reacao::Avisar(mensagem) => self.avisar(mensagem),
            Reacao::PedirConfirmacaoExcluir(ids) => {
                self.abrir_modal(Modal::ConfirmarSessoes { ids });
            }
            Reacao::ConfirmarExcluir(ids) => {
                self.excluir_sessoes(ids);
            }
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
        // Linha de atividade no fim da conversa (spinner + tempo + estado).
        if self.atualizar_atividade() {
            mudou = true;
        }
        // Aviso temporário: expira sozinho, sem ninguém precisar limpá-lo.
        if let Some((_, expira)) = &self.toast
            && self.tick >= *expira
        {
            self.toast = None;
            mudou = true;
        }
        // Medidor de contexto: caminha até o valor real em vez de pular. Um
        // salto de 40 pontos de uma vez parece erro de leitura.
        if self.contexto_exibido != self.contexto.0 {
            let distancia = self.contexto.0.abs_diff(self.contexto_exibido);
            let passo = (distancia / 8).max(1).min(distancia);
            self.contexto_exibido = if self.contexto.0 > self.contexto_exibido {
                self.contexto_exibido.saturating_add(passo)
            } else {
                self.contexto_exibido.saturating_sub(passo)
            };
            mudou = true;
        }
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

        // Animação do Pip: caminhada, respiração e piscada, quadro a quadro.
        // Só na recepção: numa conversa longa o mascote nem é desenhado, e
        // animá-lo forçaria um redesenho completo a cada quadro.
        if self.movimento_reduzido {
            if self.pip != AnimacaoPip::default() {
                self.pip = AnimacaoPip::default();
                mudou = true;
            }
        } else if self.pip_animado() {
            let anterior = self.pip;
            let mut novo = mascot::quadro_de(self.tick);
            // A piscada só acontece no repouso e precisa de quadros parados
            // suficientes: durante a caminhada não existem quadros piscando
            // (o olho fechado é gerado só na posição central).
            if novo.dx == 0 && self.tick >= self.pip_proxima_piscada {
                if mascot::quadro_de(self.tick + 3).dx == 0 {
                    novo.piscando = true;
                    self.pip_piscada_ate = self.tick + 3;
                    let intervalo = self.intervalo_de_piscada();
                    self.pip_proxima_piscada = self.tick + intervalo;
                } else {
                    // Perto do fim do repouso: espera o próximo.
                    self.pip_proxima_piscada = self.tick + 1;
                }
            }
            if self.pip.piscando && self.tick <= self.pip_piscada_ate {
                novo.piscando = true;
            } else if self.pip.piscando && self.tick > self.pip_piscada_ate {
                novo.piscando = false;
            }
            self.pip = novo;
            if self.pip != anterior {
                mudou = true;
            }
        }
        // Rolagem suave: o alvo é onde se quer chegar, `scroll` caminha até lá.
        if self.scroll != self.scroll_alvo {
            let distancia = self.scroll.abs_diff(self.scroll_alvo);
            let passo = (distancia / 2).max(1).min(distancia);
            self.scroll = if self.scroll_alvo > self.scroll {
                self.scroll.saturating_add(passo)
            } else {
                self.scroll.saturating_sub(passo)
            };
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
        // Na cadência da recepção (40 ms), 80–140 tiques dão 3,2–5,6 s.
        80 + (self.pip_sorteio >> 33) % 60
    }

    /// O Pip só é desenhado na recepção de sessão nova. Fora dela, animar
    /// gastaria redesenhos sem nada visível.
    pub fn pip_animado(&self) -> bool {
        self.config.ui.show_mascot && self.blocos.is_empty()
    }

    /// Persiste o histórico de entrada em disco.
    pub fn salvar_historico(&self) {
        let linhas = self
            .entrada
            .historico_recente(self.config.ui.max_input_history);
        if linhas.is_empty() {
            return;
        }
        // JSON, não uma entrada por linha: mensagens multilinha eram
        // fragmentadas ao reabrir o Kestrel.
        if let Ok(texto) = serde_json::to_string(&linhas) {
            let _ = std::fs::write(self.paths.history_file(), texto);
        }
    }
}

/// Primeira letra em maiúscula, preservando o resto (inclusive acentos).
/// Teto de memória dos blocos de texto em streaming.
const TETO_TEXTO: usize = 400_000;

/// Mantém o texto dentro do teto, descartando o **começo** (o mais antigo).
///
/// A verificação barata é por bytes — `len()` é O(1). Contar caracteres a cada
/// delta varria o texto inteiro e, numa resposta longa, custava mais que a
/// própria renderização. O corte por caracteres só acontece quando o teto é de
/// fato ultrapassado, para nunca cortar no meio de um caractere multibyte.
fn limitar_texto(texto: &mut String) {
    if texto.len() <= TETO_TEXTO {
        return;
    }
    // Corta por bytes em fronteira de caractere: comparar bytes mas subtrair
    // contagem de chars deixava texto multibyte acima do teto sem corte — e
    // ainda varria a string inteira a cada delta (O(n²)).
    let mut corte = texto.len() - TETO_TEXTO;
    while corte < texto.len() && !texto.is_char_boundary(corte) {
        corte += 1;
    }
    texto.drain(..corte);
}

fn com_maiuscula(texto: String) -> String {
    let mut caracteres = texto.chars();
    match caracteres.next() {
        Some(primeira) => primeira.to_uppercase().collect::<String>() + caracteres.as_str(),
        None => texto,
    }
}

fn itens_processos(lista: &[ProcessoView]) -> Vec<ItemSeletor> {
    lista
        .iter()
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
        .collect()
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
        "\n## Conversa\n\n\
         - Cada bloco tem uma **trilha colorida** à esquerda: leitura (azul), escrita (verde), \
comandos (âmbar), web (ciano), falha (vermelho) e subagente (violeta).\n\
         - **Clique com o botão esquerdo** sobre um bloco para recolher ou abrir: conversas longas ficam legíveis sem rolar o dia inteiro.\n\
         - Confirmações rápidas (modo, permissão, modelo, skin, thinking) aparecem como **aviso temporário** no topo e somem sozinhas, sem virar bloco permanente.\n\
         - O campo de mensagem mostra um **spinner** enquanto o agente trabalha.\n\
         - O rodapé traz o **uso de contexto** (usado/total + percentual) e um indicador discreto dos processos ativos.\n\n\
         ## Modos\n\n\
         - **build** (padrão): investiga, planeja, edita arquivos e executa comandos conforme o nível de permissão.\n\
         - **chat**: somente conversa e leitura; ferramentas que escrevem, executam ou planejam ficam indisponíveis.\n\
         - Troque com `/chat` e `/agent`; o modo atual aparece no topo da interface e é registrado na auditoria.\n\n\
         ## Rodapé do turno\n\n\
         - Ao fim de cada turno aparece só uma linha discreta: duração, tokens por segundo, modelo e modo.\n\
         - O rodapé inferior usa um círculo: apagado ocioso, piscando enquanto trabalha, branco ao concluir e vermelho em erro.\n\
         - O rodapé inferior não mostra skills nem “enter envia”; use `/skills` para ver as skills.\n\n\
         ## Nova sessão e progresso\n\n\
         - Em uma sessão nova, o Pip e instruções curtas aparecem no chat; ambos somem na primeira mensagem.\n\
         - Durante o turno, o trabalho aparece só no rodapé (estado discreto) e no spinner do campo de mensagem.\n\
         - Não aparece mais o aviso de “sessão anterior interrompida”; use `/retomar` quando quiser.\n\
         - `/historico` também **exclui sessões em lote**: espaço marca, `d` exclui as marcadas (com confirmação; a sessão atual nunca é excluída).\n\n\
         ## Comps e skins\n\n\
         - **Comps** são personalidades: um bloco de instruções que muda o tom e o estilo da resposta. \
`/person` lista, `/person new <nome>` cria com o editor dentro do Kestrel, `/person use <nome>` ativa, \
`/person edit` reabre, `/person see` mostra as instruções e o custo em tokens e `/person remove` apaga. \
Um Comp por sessão; as instruções ficam em `~/.config/kestrel/comps/<nome>.toml`.\n\
         - **Skins** são o visual, independentes do Comp. `/skin` lista e `/skin <nome>` troca na hora. \
Prontas: `mono` (padrão), `kestrel`, `daylight`; skins próprias em `~/.config/kestrel/skins/<nome>.toml` \
(chaves de cor `#rrggbb`) entram na lista. `KESTREL_SKIN=<nome>` força uma skin só nesta sessão.\n\
         - No editor de Comp: Enter quebra linha, Ctrl+S salva e Esc cancela.\n\n\
         ## Mouse\n\n\
         - Clique esquerdo sobre um bloco recolhe/abre\n\
         - Clique em `↓ voltar para o último` volta a acompanhar o fim da conversa\n\n\
         ## Teclado\n\n\
         - Enter envia; Alt+Enter (ou Shift+Enter) quebra linha\n\
         - Up/Down navega no histórico (ou nas sugestões de comando); Tab ou Enter completam um comando parcial\n\
         - Ctrl+C interrompe o turno; pressione de novo para sair\n\
         - Esc interrompe o turno\n\
         - Ctrl+F busca na conversa (Enter/↓ próximo, ↑ anterior)\n\
         - Ctrl+Y copia a última resposta via OSC 52\n\
         - Shift+Tab alterna o nível de permissão em ciclo\n\
         - F1 ajuda · F2 mostra/oculta o raciocínio do modelo (o mesmo que `/thinking`)\n\
         - PageUp/PageDown e a roda do mouse rolam a conversa\n\
         - Todos esses atalhos podem ser trocados em `[ui.keybindings]`; `/keys` mostra os atuais\n\
         - Rolando para cima você pode ler o histórico à vontade; o canto do campo de mensagem \
ganha o atalho `↓ voltar para o último` (clique ou role até o fim para voltar a acompanhar)\n\n\
         ## Tokens, fallback e sandbox\n\n\
         - A contagem de contexto usa tokenizer real por modelo (tiktoken) quando disponível e calibra com o `usage` do provedor.\n\
         - `provider.fallback_models` tenta o próximo modelo em indisponibilidade/limite/rede/5xx, sem trocar o modelo salvo.\n\
         - `agent.sandbox = \"auto\"` confina processos filhos com Landlock (escrita só no projeto/caches; rede opcional).\n\
         - `/branch [nome]` cria um ramo da sessão com histórico, plano, tarefas, decisões e alterações.\n\
         - `search_project` faz retrieval BM25; `apply_patch` aplica patches com validação de contexto.\n\n\
         ## MCP (servidores locais e remotos)\n\n\
         - Servidores declarados em `[[mcp]]` no `config.toml` entram na lista do modelo com o prefixo `mcp__<servidor>__<ferramenta>`.\n\
         - Transporte stdio (processo filho) ou HTTP/SSE com token/OAuth client-credentials.\n\
         - Recursos e prompts viram `list_resources`, `read_resource`, `list_prompts` e `get_prompt` quando anunciados.\n\
         - Servidores assinados exigem chave Ed25519 em `mcp_trusted_keys`; o manifesto inclui o SHA-256 do executável.\n\
         - Um servidor que falha não impede os outros; o aviso aparece na conversa.\n\
         - Ferramentas MCP passam pela mesma verificação de permissão e escopo das nativas.\n\n\
         ## Segurança\n\n\
         - Comandos catastróficos são bloqueados em qualquer nível de permissão.\n\
         - Em níveis mais restritos, ações sensíveis pedem confirmação.\n\
         - O sandbox Landlock (quando ativo) impede escrita fora das raízes autorizadas.\n\
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
    pip_grafico: Option<PipGrafico>,
) -> anyhow::Result<()> {
    crate::tokenizer::registrar_modelos(&config.provider.models);
    let handle: AgenteHandle = iniciar(config.clone(), paths.clone(), store.clone())?;
    let AgenteHandle {
        cmd_tx,
        mut eventos,
    } = handle;
    let projeto = Arc::new(project::analyze(&config.workdir()));
    let skills = Arc::new(SkillRegistry::discover(&paths, &config.workdir()));
    let mut app = App::novo(config, paths, store, cmd_tx, projeto, skills);
    app.pip_grafico = pip_grafico;
    let mut eventos_terminal = ui::canal_terminal();
    // Cadência adaptativa: 60 fps durante a rolagem, animação na recepção,
    // spinner no turno e ritmo econômico quando nada visível muda.
    let mut intervalo = Duration::from_millis(160);
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
        if app.saindo && tiques_encerrando > 80 {
            break;
        }
        // Cadência adaptativa: 60 fps enquanto a rolagem anima (resposta
        // imediata), ritmo de animação na recepção/modal, e econômica quando
        // nada visível muda.
        let novo = if app.scroll != app.scroll_alvo {
            Duration::from_millis(16)
        } else if app.turno_ativo {
            Duration::from_millis(80)
        } else if (!app.movimento_reduzido && app.pip_animado()) || app.modal.is_some() {
            Duration::from_millis(40)
        } else {
            Duration::from_millis(160)
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

    /// F2 e `/thinking` precisam ter o mesmo efeito: persistir a preferência
    /// e avisar o agente para parar de enviar deltas de raciocínio.
    #[test]
    fn f2_e_thinking_propagam_e_persistem_a_escolha() {
        let dir = std::env::temp_dir().join(format!("kestrel-thinking-{}", uuid::Uuid::new_v4()));
        let (mut app, mut rx) = app_de_teste();
        app.paths = Paths::with_root(&dir);

        app.entrada.definir_texto("/thinking off".into());
        app.enviar_entrada();
        assert!(!app.mostrar_raciocinio);
        assert!(!app.config.ui.show_reasoning);
        let mut viu = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, UiCommand::DefinirRaciocinio(false)) {
                viu = true;
            }
        }
        assert!(viu, "o agente não foi avisado sobre /thinking off");

        // F2 liga de volta e também propaga.
        app.tratar_tecla(KeyEvent::new(KeyCode::F(2), KeyModifiers::NONE));
        assert!(app.mostrar_raciocinio);
        assert!(app.config.ui.show_reasoning);
        let mut viu = false;
        while let Ok(cmd) = rx.try_recv() {
            if matches!(cmd, UiCommand::DefinirRaciocinio(true)) {
                viu = true;
            }
        }
        assert!(viu, "o agente não foi avisado sobre o F2");

        let _ = std::fs::remove_dir_all(&dir);
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
    /// `/undo` reverte de verdade: cria um arquivo, grava a alteração no banco
    /// (como as ferramentas fazem) e desfaz — o conteúdo tem de voltar ao
    /// original, sem depender de git.
    #[test]
    fn undo_reverte_o_arquivo_de_verdade() {
        let dir = std::env::temp_dir().join(format!("kestrel-undo-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut app, _rx) = app_de_teste();
        app.entrada.definir_texto("/undo".into());
        let alvo = dir.join("nota.txt");
        let antes = "linha um\nlinha dois\nlinha tres\n";
        std::fs::write(&alvo, antes).unwrap();
        // Simula a edição que a ferramenta `edit_file` faria.
        let depois = "linha um\nlinha DOIS\nlinha tres\n";
        let d = crate::diff::diff(antes, depois);
        app.store
            .record_file_change(
                &app.sessao_id,
                &alvo.display().to_string(),
                "editar",
                d.stats.additions,
                d.stats.deletions,
                Some(&d.text),
            )
            .unwrap();
        std::fs::write(&alvo, depois).unwrap();

        app.enviar_entrada();
        // O arquivo voltou ao conteúdo anterior.
        assert_eq!(std::fs::read_to_string(&alvo).unwrap(), antes);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Cada `/undo` avança para a alteração anterior da sessão. Antes, a
    /// alteração desfeita continuava sendo a "última", então o segundo
    /// comando reaplicava o mesmo diff (que já não batia) e travava ali.
    #[test]
    fn undo_avanca_para_alteracao_anterior() {
        let dir = std::env::temp_dir().join(format!("kestrel-undo3-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut app, _rx) = app_de_teste();
        let alvo = dir.join("nota.txt");
        let v0 = "versao zero\n";
        let v1 = "versao um\n";
        let v2 = "versao dois\n";
        std::fs::write(&alvo, v0).unwrap();
        let caminho = alvo.display().to_string();
        for (antes, depois) in [(v0, v1), (v1, v2)] {
            let d = crate::diff::diff(antes, depois);
            app.store
                .record_file_change(
                    &app.sessao_id,
                    &caminho,
                    "editar",
                    d.stats.additions,
                    d.stats.deletions,
                    Some(&d.text),
                )
                .unwrap();
            std::fs::write(&alvo, depois).unwrap();
        }

        app.entrada.definir_texto("/undo".into());
        app.enviar_entrada();
        assert_eq!(std::fs::read_to_string(&alvo).unwrap(), v1);

        app.entrada.definir_texto("/undo".into());
        app.enviar_entrada();
        assert_eq!(std::fs::read_to_string(&alvo).unwrap(), v0);

        // Não há mais nada para desfazer: avisa em vez de tentar de novo.
        app.entrada.definir_texto("/undo".into());
        app.enviar_entrada();
        assert_eq!(std::fs::read_to_string(&alvo).unwrap(), v0);
        match app.blocos.last() {
            Some(super::Bloco::Aviso(t)) => assert!(t.contains("Nada para desfazer")),
            outro => panic!("esperava aviso, veio {outro:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Quando o arquivo mudou desde a alteração registrada, o `/undo` avisa em
    /// vez de escrever um conteúdo errado.
    #[test]
    fn undo_avisa_quando_o_arquivo_mudou() {
        let dir = std::env::temp_dir().join(format!("kestrel-undo2-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut app, _rx) = app_de_teste();
        let alvo = dir.join("nota.txt");
        let antes = "a\nb\n";
        std::fs::write(&alvo, antes).unwrap();
        let depois = "a\nB\n";
        let d = crate::diff::diff(antes, depois);
        app.store
            .record_file_change(
                &app.sessao_id,
                &alvo.display().to_string(),
                "editar",
                d.stats.additions,
                d.stats.deletions,
                Some(&d.text),
            )
            .unwrap();
        // Alguém mudou o arquivo depois: o diff não bate mais.
        std::fs::write(&alvo, "completamente diferente\n").unwrap();

        app.entrada.definir_texto("/undo".into());
        app.enviar_entrada();
        // O conteúdo não foi tocado.
        assert_eq!(
            std::fs::read_to_string(&alvo).unwrap(),
            "completamente diferente\n"
        );
        // E o usuário foi avisado.
        let aviso = app
            .blocos
            .last()
            .and_then(|b| match b {
                super::Bloco::Aviso(t) => Some(t.clone()),
                _ => None,
            })
            .unwrap_or_default();
        assert!(aviso.contains("mudou"), "aviso pouco claro: {aviso}");
        let _ = std::fs::remove_dir_all(&dir);
    }

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

    /// `/person` precisa sustentar o ciclo completo do Anexo A: criar com o
    /// editor interno, ativar, gravar em disco e remover com confirmação —
    /// sem recompilar e sem escrever nada dentro do projeto.
    #[test]
    fn person_cria_ativa_e_remove_comp() {
        let dir = std::env::temp_dir().join(format!("kestrel-person-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let (mut app, mut rx) = app_de_teste();
        app.paths = crate::util::Paths::with_root(&dir);

        app.executar_comando("/person new Lazy");
        assert!(
            matches!(&app.modal, Some(Modal::CompEditor { nome, novo, .. }) if nome == "Lazy" && *novo)
        );
        if let Some(Modal::CompEditor { editor, .. }) = &mut app.modal {
            editor.definir_texto("Seja breve, direto e sem floreios.".into());
        }
        app.salvar_comp_modal();
        let arquivo = app.paths.comps_dir().join("lazy.toml");
        assert!(arquivo.exists(), "o Comp deveria ter sido gravado");
        assert!(app.modal.is_none(), "o editor deveria fechar ao salvar");

        app.executar_comando("/person use Lazy");
        assert_eq!(app.config.ui.active_comp.as_deref(), Some("Lazy"));
        match rx.try_recv().unwrap() {
            UiCommand::DefinirComp(ativo) => assert_eq!(ativo.as_deref(), Some("Lazy")),
            outro => panic!("comando inesperado: {outro:?}"),
        }

        app.executar_comando("/person remove Lazy");
        assert!(matches!(&app.modal, Some(Modal::ConfirmarComp { nome }) if nome == "Lazy"));
        app.remover_comp_confirmado();
        assert!(!arquivo.exists(), "o Comp deveria ter sido removido");
        assert_eq!(app.config.ui.active_comp, None);
        while let Ok(comando) = rx.try_recv() {
            if matches!(comando, UiCommand::DefinirComp(algum) if algum.is_none()) {
                return;
            }
        }
        panic!("a remoção deveria desativar o Comp ativo");
    }

    /// `/skin` troca a paleta na hora e mantém a cópia do agente alinhada —
    /// senão um `/model` posterior regravaria a skin antiga.
    #[test]
    fn skin_aplica_muda_a_configuracao_e_avisa_o_agente() {
        let (mut app, mut rx) = app_de_teste();
        app.executar_comando("/skin daylight");
        assert_eq!(app.config.ui.skin, "daylight");
        assert_eq!(crate::ui::theme::skin_atual(), "daylight");
        match rx.try_recv().unwrap() {
            UiCommand::DefinirSkin(skin) => assert_eq!(skin, "daylight"),
            outro => panic!("comando inesperado: {outro:?}"),
        }
        app.executar_comando("/skin inexistente");
        assert_eq!(
            app.config.ui.skin, "daylight",
            "skin inválida não deveria colar"
        );
        // Restaura o padrão para o restante dos testes de interface.
        app.executar_comando("/skin kestrel");
        assert_eq!(crate::ui::theme::skin_atual(), "kestrel");
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
            id: Some("call-1".into()),
            nome: "edit_file".into(),
            detalhe: "src/main.rs".into(),
            subagente: None,
        });
        app.tratar_evento(AgentEvent::FerramentaConcluida {
            id: Some("call-1".into()),
            nome: "edit_file".into(),
            sucesso: true,
            resumo: "3 linhas alteradas".into(),
            adicoes: 5,
            remocoes: 2,
            diff: "--- a/src/main.rs\n+++ b/src/main.rs\n@@ -1,2 +1,2 @@\n-fn velho() {}\n+fn novo() {}".into(),
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

    /// Duas chamadas de mesmo nome em paralelo precisam casar início e fim
    /// pelo id; antes a conclusão sempre marcava o primeiro bloco e o segundo
    /// ficava girando para sempre.
    #[test]
    fn conclusao_de_ferramenta_casa_pelo_id() {
        let (mut app, _rx) = app_de_teste();
        for (id, detalhe) in [("c1", "a.rs"), ("c2", "b.rs")] {
            app.tratar_evento(AgentEvent::FerramentaIniciada {
                id: Some(id.into()),
                nome: "read_file".into(),
                detalhe: detalhe.into(),
                subagente: None,
            });
        }
        app.tratar_evento(AgentEvent::FerramentaConcluida {
            id: Some("c2".into()),
            nome: "read_file".into(),
            sucesso: true,
            resumo: "b.rs lido".into(),
            adicoes: 0,
            remocoes: 0,
            diff: String::new(),
        });
        let blocos: Vec<(&str, bool, Option<&str>)> = app
            .blocos
            .iter()
            .filter_map(|b| match b {
                Bloco::Ferramenta {
                    detalhe,
                    rodando,
                    resumo,
                    ..
                } => Some((detalhe.as_str(), *rodando, resumo.as_deref())),
                _ => None,
            })
            .collect();
        assert_eq!(blocos.len(), 2);
        assert_eq!(blocos[0], ("a.rs", true, None));
        assert_eq!(blocos[1], ("b.rs", false, Some("b.rs lido")));
    }

    /// Se o modal de processos está aberto, um evento automático atualiza a
    /// lista no lugar em vez de deixar o modal mostrando dados velhos.
    #[test]
    fn processos_atualizados_refrescam_modal_aberto() {
        use crate::events::EstadoProcesso;
        let (mut app, _rx) = app_de_teste();
        let processo = |id: &str| ProcessoView {
            id: id.into(),
            comando: "sleep 1".into(),
            cwd: "/tmp".into(),
            estado: EstadoProcesso::Executando,
            codigo_saida: None,
            iniciado_em: "agora".into(),
            linhas: 0,
        };
        app.tratar_evento(AgentEvent::Processos {
            lista: vec![processo("proc1")],
        });
        assert!(matches!(app.modal, Some(Modal::Processos(_))));
        app.tratar_evento(AgentEvent::ProcessosAtualizados {
            lista: vec![processo("proc1"), processo("proc2")],
        });
        match &app.modal {
            Some(Modal::Processos(seletor)) => {
                assert_eq!(seletor.itens.len(), 2);
                assert!(seletor.itens[1].titulo.contains("proc2"));
            }
            _ => panic!("o modal de processos foi fechado"),
        }
    }

    /// Ctrl+C fechando um modal comum não pode deixar a próxima confirmação
    /// da fila parada até o próximo evento de teclado.
    #[test]
    fn ctrl_c_abre_a_proxima_confirmacao_da_fila() {
        let (mut app, _rx) = app_de_teste();
        let (tx1, _rx1) = tokio::sync::oneshot::channel();
        let (tx2, _rx2) = tokio::sync::oneshot::channel();
        app.fila_confirmacoes.push_back(ConfirmRequest {
            titulo: "primeira".into(),
            detalhe: "d1".into(),
            nivel: PermissionLevel::Semi,
            resposta: tx1,
        });
        app.fila_confirmacoes.push_back(ConfirmRequest {
            titulo: "segunda".into(),
            detalhe: "d2".into(),
            nivel: PermissionLevel::Semi,
            resposta: tx2,
        });
        app.modal = Some(Modal::Texto {
            titulo: "ajuda".into(),
            linhas: Vec::new(),
            scroll: 0,
        });
        app.tratar_tecla(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
        match &app.modal {
            Some(Modal::Confirmacao(pedido)) => assert_eq!(pedido.titulo, "primeira"),
            _ => panic!("a confirmação enfileirada não foi aberta"),
        }
    }

    /// O workdir informado pelo agente ao retomar uma sessão de outro projeto
    /// precisa ser refletido na configuração da interface.
    #[test]
    fn sessao_ativa_sincroniza_workdir_da_interface() {
        let dir =
            std::env::temp_dir().join(format!("kestrel-app-workdir-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let esperado = std::fs::canonicalize(&dir).unwrap();
        let (mut app, _rx) = app_de_teste();
        app.tratar_evento(AgentEvent::SessaoAtiva {
            id: "s2".into(),
            titulo: "outro projeto".into(),
            workdir: dir.display().to_string(),
        });
        assert_eq!(app.config.workdir(), esperado);
        assert!(app.config.agent.pin_workdir);
        let _ = std::fs::remove_dir_all(&dir);
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
        // Confirmações de modo são toasts: somem sozinhas, sem entulhar a
        // conversa (o histórico do modo fica no cabeçalho e na auditoria).
        match &app.toast {
            Some((t, _)) => assert!(t.contains("chat"), "veio: {t}"),
            outra => panic!("esperava toast de modo, veio {outra:?}"),
        }
        // Repetir o mesmo modo apenas informa, sem reenviar o comando.
        app.entrada.definir_texto("/chat".into());
        app.enviar_entrada();
        match &app.toast {
            Some((t, _)) => assert!(t.contains("já é chat"), "veio: {t}"),
            outra => panic!("esperava toast de modo repetido, veio {outra:?}"),
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
        match &app.toast {
            Some((t, _)) => {
                assert!(
                    t.contains("modelo atual: nvidia/nemotron-3-ultra-550b"),
                    "veio: {t}"
                );
            }
            outra => panic!("esperava toast de troca de modelo, veio {outra:?}"),
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
        match &app.toast {
            Some((t, _)) => {
                assert!(t.contains("esforço de raciocínio: high"), "veio: {t}");
            }
            outra => panic!("esperava toast de esforço, veio {outra:?}"),
        }
    }

    /// Eventos automáticos de processos (início/fim de comando em segundo
    /// plano) só atualizam o estado; não podem abrir um modal por cima do que
    /// o usuário está fazendo nem descartar uma confirmação pendente.
    #[test]
    fn atualizacao_automatica_de_processos_nao_abre_modal() {
        let (mut app, _rx) = app_de_teste();
        app.modal = Some(Modal::Ajuda {
            linhas: Vec::new(),
            scroll: 0,
        });
        app.tratar_evento(AgentEvent::ProcessosAtualizados {
            lista: vec![ProcessoView {
                id: "proc1".into(),
                comando: "sleep 10".into(),
                cwd: "/tmp".into(),
                estado: crate::events::EstadoProcesso::Executando,
                codigo_saida: None,
                iniciado_em: "agora".into(),
                linhas: 0,
            }],
        });
        assert_eq!(app.processos.len(), 1);
        assert!(
            matches!(app.modal, Some(Modal::Ajuda { .. })),
            "o modal atual foi substituído pelo seletor de processos"
        );
    }

    /// Cancelar a escolha de esforço cancela a troca de modelo; um `/effort`
    /// depois disso não pode aplicar o modelo que ficou pendente.
    #[test]
    fn cancelar_esforco_descarta_modelo_pendente() {
        use crate::config::Capability;
        let (mut app, mut rx) = app_de_teste();
        let mut entrada = crate::config::ModelEntry::new("nvidia/nemotron-3-ultra-550b");
        entrada.supports_reasoning = Capability::Sim;
        entrada.reasoning_efforts = vec!["high".into()];
        app.config.provider.models = vec![entrada.clone()];
        app.config.provider.model = entrada.id.clone();

        app.abrir_seletor_modelos(String::new());
        app.tratar_tecla_modal(KeyEvent::from(KeyCode::Enter));
        assert!(matches!(app.modal, Some(Modal::Esforco(_))));
        app.tratar_tecla_modal(KeyEvent::from(KeyCode::Esc));
        assert!(app.modal.is_none());
        assert!(
            app.modelo_pendente.is_none(),
            "o modelo cancelado ficou pendente"
        );
        assert!(rx.try_recv().is_err(), "nada deveria ter sido aplicado");

        // `/effort` posterior altera somente o esforço do modelo atual.
        app.executar_comando("/effort");
        assert!(matches!(app.modal, Some(Modal::Esforco(_))));
        app.tratar_tecla_modal(KeyEvent::from(KeyCode::Down));
        app.tratar_tecla_modal(KeyEvent::from(KeyCode::Enter));
        match rx.try_recv().unwrap() {
            UiCommand::DefinirEsforco(Some(nivel)) => assert_eq!(nivel, "high"),
            outro => panic!("comando inesperado: {outro:?}"),
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
            match app3.pip.dy {
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

    /// Blocos de subagente são agrupados: cabeçalho com a subtarefa, com as
    /// ferramentas dele marcadas abaixo, e cabeçalho saindo do "rodando".
    #[test]
    fn subagente_visivel_agrupa_suas_ferramentas() {
        let (mut app, _rx) = app_de_teste();
        app.tratar_evento(AgentEvent::SubagenteIniciado {
            tarefa: "investigar a API da lib X".into(),
        });
        app.tratar_evento(AgentEvent::FerramentaIniciada {
            id: None,
            nome: "subagente:read_file".into(),
            detalhe: "src/lib.rs".into(),
            subagente: Some("investigar a API da lib X".into()),
        });
        app.tratar_evento(AgentEvent::FerramentaConcluida {
            id: None,
            nome: "subagente:read_file".into(),
            sucesso: true,
            resumo: "leu".into(),
            adicoes: 0,
            remocoes: 0,
            diff: String::new(),
        });
        app.tratar_evento(AgentEvent::SubagenteConcluido { sucesso: true });

        let cabecalhos = app
            .blocos
            .iter()
            .filter(|b| matches!(b, Bloco::Subagente { .. }))
            .count();
        assert_eq!(cabecalhos, 1, "deveria haver um grupo de subagente");
        // A ferramenta do subagente carrega a subtarefa (fica recuada no chat).
        let marcada = app.blocos.iter().any(|b| {
            matches!(
                b,
                Bloco::Ferramenta {
                    subagente: Some(sub),
                    ..
                } if sub.contains("investigar")
            )
        });
        assert!(
            marcada,
            "a ferramenta deveria estar marcada com a subtarefa"
        );
        // O cabeçalho saiu do estado "rodando".
        let grupo = app
            .blocos
            .iter()
            .find(|b| matches!(b, Bloco::Subagente { .. }))
            .expect("grupo do subagente");
        assert!(
            matches!(
                grupo,
                Bloco::Subagente {
                    rodando: false,
                    sucesso: true,
                    ..
                }
            ),
            "veio: {grupo:?}"
        );
    }

    /// Clicar abre/fecha um bloco: o conteúdo recolhido vira uma linha só.
    #[test]
    fn clique_abre_e_fecha_bloco() {
        let (mut app, _rx) = app_de_teste();
        app.tratar_evento(AgentEvent::FerramentaIniciada {
            id: Some("c1".into()),
            nome: "read_file".into(),
            detalhe: "src/main.rs".into(),
            subagente: None,
        });
        // Saída com várias linhas: recolhido precisa ocupar bem menos.
        for i in 0..10 {
            app.tratar_evento(AgentEvent::FerramentaSaida {
                id: Some("c1".into()),
                nome: "read_file".into(),
                linha: format!("linha {i}"),
            });
        }
        let total_antes = app.linhas.len();
        assert!(total_antes > 5, "o bloco deveria ter várias linhas");
        // A saída é o último bloco: é ele que se recolhe.
        let alvo = app.blocos.len() - 1;
        app.alternar_bloco(alvo);
        assert!(app.recolhidos.contains(&alvo));
        let total_recolhido = app.linhas.len();
        assert!(
            total_recolhido < total_antes,
            "recolhido deveria ocupar menos linhas: {total_recolhido} vs {total_antes}"
        );
        app.alternar_bloco(alvo);
        assert!(!app.recolhidos.contains(&alvo));
        assert_eq!(app.linhas.len(), total_antes, "reabrir volta ao total");
    }

    /// Ao drenar blocos antigos, os índices recolhidos são deslocados: sem
    /// isso um bloco qualquer passaria a abrir/fechar no lugar errado.
    /// O estado recolhido é por índice posicional: `/clear` (ou troca de
    /// projeto) zerava `blocos` mas não `recolhidos`, e os índices antigos
    /// passavam a valer para a conversa nova — um bloco que o usuário nunca
    /// fechou aparecia com uma linha só, sem resumo nem diff.
    #[test]
    fn limpar_conversa_esquece_os_blocos_recolhidos() {
        let (mut app, _rx) = app_de_teste();
        // Uma saída de ferramenta com várias linhas: recolhida vira 1 linha.
        app.tratar_evento(AgentEvent::FerramentaIniciada {
            id: Some("c1".into()),
            nome: "read_file".into(),
            detalhe: "src/main.rs".into(),
            subagente: None,
        });
        for i in 0..8 {
            app.tratar_evento(AgentEvent::FerramentaSaida {
                id: Some("c1".into()),
                nome: "read_file".into(),
                linha: format!("linha {i}"),
            });
        }
        let cheio = app.linhas.len();
        assert!(cheio > 5);
        let recolhido = app.blocos.len() - 1;
        app.alternar_bloco(recolhido);
        let linha_unica = app.linhas.len();
        assert!(
            linha_unica < cheio,
            "recolhido deveria ocupar menos linhas: {linha_unica} vs {cheio}"
        );

        // Nova conversa: o bloco que cair no mesmo índice precisa nascer aberto.
        app.limpar_conversa();
        app.tratar_evento(AgentEvent::FerramentaIniciada {
            id: Some("c2".into()),
            nome: "read_file".into(),
            detalhe: "src/lib.rs".into(),
            subagente: None,
        });
        for i in 0..8 {
            app.tratar_evento(AgentEvent::FerramentaSaida {
                id: Some("c2".into()),
                nome: "read_file".into(),
                linha: format!("linha {i}"),
            });
        }
        assert_eq!(
            app.linhas.len(),
            cheio,
            "o recolhimento da conversa anterior não pode vazar para a nova"
        );
    }

    /// Giros seguidos da roda precisam acumular no alvo, não partir da
    /// posição animada (que ainda não se moveu entre os eventos).
    #[test]
    fn roda_acumula_no_alvo_da_rolagem() {
        let (mut app, _rx) = app_de_teste();
        app.seguir_fim = false;
        app.scroll = 10;
        app.scroll_alvo = 10;
        for _ in 0..3 {
            app.tratar_mouse(MouseEvent {
                kind: MouseEventKind::ScrollUp,
                column: 0,
                row: 0,
                modifiers: KeyModifiers::NONE,
            });
        }
        assert_eq!(app.scroll_alvo, 1, "cada giro deveria somar 3 ao alvo");
        assert_eq!(app.scroll, 10, "a posição animada só muda no tique");
        assert!(!app.seguir_fim);

        // Já no fim, PageDown não desliga o acompanhamento.
        app.seguir_fim = true;
        app.scroll = 0;
        app.scroll_alvo = 0;
        app.tratar_tecla(KeyEvent::from(KeyCode::PageDown));
        assert!(app.seguir_fim);
    }

    /// `ir_para_o_fim` precisa deixar scroll e alvo alinhados: um alvo em
    /// `u16::MAX` nunca convergia com o clamp do desenho e a interface
    /// redesenhanvava para sempre.
    #[test]
    fn atalhos_personalizados_substituem_os_padroes() {
        let mut overrides = std::collections::BTreeMap::new();
        overrides.insert("buscar".to_string(), "ctrl+g".to_string());
        overrides.insert("copiar".to_string(), "f8".to_string());
        let mapa = Keymap::novo(&overrides);
        assert_eq!(
            mapa.acao(&KeyEvent::new(KeyCode::Char('g'), KeyModifiers::CONTROL)),
            Some("buscar")
        );
        assert_eq!(
            mapa.acao(&KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            None,
            "o padrão substituído não deveria continuar ativo"
        );
        assert_eq!(
            mapa.acao(&KeyEvent::new(KeyCode::F(8), KeyModifiers::NONE)),
            Some("copiar")
        );
        let mut ruins = std::collections::BTreeMap::new();
        ruins.insert("buscar".to_string(), "ctrl+tecla-inexistente".to_string());
        let mapa = Keymap::novo(&ruins);
        assert_eq!(
            mapa.acao(&KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL)),
            None
        );
        assert_eq!(mapa.descricao("buscar"), "(desativado)");
    }

    #[test]
    fn busca_salta_entre_linhas_e_nao_liga_no_fim() {
        let (mut app, _rx) = app_de_teste();
        for i in 0..10 {
            app.empurrar_bloco(Bloco::Assistente(format!("linha com alvo {i}")));
        }
        app.modal = Some(Modal::Busca {
            consulta: "alvo".into(),
            indice: 0,
        });
        app.buscar_saltar(0);
        assert!(!app.seguir_fim);
        let primeiro = app.scroll_alvo;
        app.buscar_saltar(1);
        assert!(
            app.scroll_alvo > primeiro,
            "deveria avançar para outra linha"
        );
        app.modal = None;
    }

    #[test]
    fn ir_para_o_fim_alinha_o_alvo_da_rolagem() {
        let (mut app, _rx) = app_de_teste();
        for i in 0..30 {
            app.empurrar_bloco(Bloco::Assistente(format!("resposta {i}")));
        }
        app.rolar_para(4);
        assert_eq!(app.scroll_alvo, 4);
        app.ir_para_o_fim();
        assert_eq!(app.scroll, app.scroll_alvo);
        assert!(app.seguir_fim);
    }

    #[test]
    fn recolhidos_acompanham_a_drenagem() {
        let (mut app, _rx) = app_de_teste();
        for i in 0..620 {
            app.empurrar_bloco(Bloco::Assistente(format!("resposta {i}")));
        }
        assert!(app.blocos.len() <= 600, "blocos deveriam ser podados");
        app.recolhidos.insert(10);
        app.empurrar_bloco(Bloco::Assistente("mais uma".into()));
        // 10 - (620 - 500) = 10 - 120 => sai do conjunto (já foi drenado).
        assert!(!app.recolhidos.contains(&0));
        assert!(app.recolhidos.iter().all(|i| *i < app.blocos.len()));
    }

    /// O aviso temporário expira sozinho e some da conversa.
    #[test]
    fn toast_expira_sozinho() {
        let (mut app, _rx) = app_de_teste();
        app.entrada.definir_texto("/chat".into());
        app.enviar_entrada();
        assert!(app.toast.is_some(), "deveria haver um toast");
        let texto = app.toast.as_ref().unwrap().0.clone();
        assert!(texto.contains("chat"), "veio: {texto}");
        // Nenhum bloco de aviso permanente na conversa.
        assert!(
            !app.blocos.iter().any(|b| matches!(b, Bloco::Aviso(_))),
            "confirmação rápida não deve virar bloco permanente"
        );
        for _ in 0..30 {
            app.tiquetaquear();
        }
        assert!(app.toast.is_none(), "o toast deveria ter expirado");
    }

    /// O diff real da ferramenta é classificado linha a linha, para colorir
    /// somente o que entrou e saiu.
    #[test]
    fn diff_e_classificado_por_tipo() {
        let linhas =
            LinhaDiff::de_texto("diff --git a/f b/f\n@@ -1 +1 @@\n contexto\n-velho\n+novo");
        let tipos: Vec<TipoLinhaDiff> = linhas.iter().map(|l| l.tipo).collect();
        assert_eq!(
            tipos,
            vec![
                TipoLinhaDiff::Meta,
                TipoLinhaDiff::Meta,
                TipoLinhaDiff::Contexto,
                TipoLinhaDiff::Remocao,
                TipoLinhaDiff::Adicao,
            ]
        );
    }

    /// Cada turno do usuário abre com um fio de separação, dando costura à
    /// conversa.
    #[test]
    fn turno_abre_com_fio_de_separacao() {
        let (mut app, mut rx) = app_de_teste();
        app.entrada.definir_texto("primeira tarefa".into());
        app.enviar_entrada();
        assert!(matches!(app.blocos[0], Bloco::Separador), "fio primeiro");
        assert!(matches!(app.blocos[1], Bloco::Usuario(_)));
        assert!(!rx.try_recv().is_err());
    }

    /// A trilha identifica o tipo de bloco: leitura, escrita, shell, web,
    /// erro e subagente têm cor própria.
    #[test]
    fn trilha_identifica_o_tipo_do_bloco() {
        use crate::ui::theme::Trilha;
        assert_eq!(
            Bloco::Ferramenta {
                id: None,
                nome: "read_file".into(),
                detalhe: String::new(),
                rodando: true,
                sucesso: false,
                resumo: None,
                adicoes: 0,
                remocoes: 0,
                diff: Vec::new(),
                subagente: None,
            }
            .trilha(),
            Trilha::Leitura
        );
        assert_eq!(
            Bloco::Ferramenta {
                id: None,
                nome: "web_search".into(),
                detalhe: String::new(),
                rodando: true,
                sucesso: false,
                resumo: None,
                adicoes: 0,
                remocoes: 0,
                diff: Vec::new(),
                subagente: None,
            }
            .trilha(),
            Trilha::Web
        );
        assert_eq!(
            Bloco::Ferramenta {
                id: None,
                nome: "edit_file".into(),
                detalhe: String::new(),
                rodando: false,
                sucesso: false,
                resumo: None,
                adicoes: 1,
                remocoes: 1,
                diff: Vec::new(),
                subagente: None,
            }
            .trilha(),
            Trilha::Erro
        );
        // Marcado como subagente, a trilha é a do subagente — não a da
        // ferramenta em si.
        assert_eq!(
            Bloco::Ferramenta {
                id: None,
                nome: "read_file".into(),
                detalhe: String::new(),
                rodando: true,
                sucesso: false,
                resumo: None,
                adicoes: 0,
                remocoes: 0,
                diff: Vec::new(),
                subagente: Some("subtarefa".into()),
            }
            .trilha(),
            Trilha::Subagente
        );
    }

    /// O histórico marca várias sessões (espaço), pede confirmação (d) e
    /// exclui (s) — sem nunca tocar na sessão ativa.
    #[test]
    fn historico_marca_e_exclui_varias_sessoes() {
        let (mut app, _rx) = app_de_teste();
        for (id, titulo) in [("a", "zeta-um"), ("b", "zeta-dois"), ("c", "zeta-tres")] {
            app.store
                .create_session(id, "/p", titulo, "m", None, "semi", None)
                .unwrap();
        }
        app.sessao_id = "s".into();

        app.abrir_historico("");
        let Modal::Sessoes { seletor, .. } = app.modal.as_ref().unwrap() else {
            panic!("esperava modal de sessões");
        };
        assert_eq!(seletor.itens.len(), 4);

        // Marca direto pelas sessões "zeta-um" e "zeta-dois" (o atalho de
        // espaço é testado à parte; aqui o que importa é o fluxo de exclusão).
        {
            let Modal::Sessoes { seletor, marcadas } = app.modal.as_mut().unwrap() else {
                panic!("modal de sessões sumiu");
            };
            for titulo in ["zeta-um", "zeta-dois"] {
                seletor.filtro = titulo.to_string();
                seletor.ajustar_selecao();
                let (indice, item) = seletor.filtrados()[0];
                assert!(
                    matches!(&item.valor, ValorSeletor::Sessao(_)),
                    "item deveria ser sessão"
                );
                marcadas.insert(indice);
            }
            seletor.filtro.clear();
            seletor.ajustar_selecao();
        }
        let Modal::Sessoes { marcadas, .. } = app.modal.as_ref().unwrap() else {
            panic!("modal de sessões sumiu");
        };
        assert_eq!(marcadas.len(), 2, "duas sessões marcadas");

        // D pede confirmação com exatamente as duas marcadas.
        app.tratar_tecla(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        let Modal::ConfirmarSessoes { ids } = app.modal.as_ref().unwrap() else {
            panic!("esperava confirmação de exclusão");
        };
        assert_eq!(ids.len(), 2);

        // S confirma e exclui.
        app.tratar_tecla(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE));
        assert!(app.store.get_session("a").unwrap().is_none());
        assert!(app.store.get_session("b").unwrap().is_none());
        assert!(app.store.get_session("c").unwrap().is_some());
        assert!(app.store.get_session("s").unwrap().is_some());
        assert!(app.toast.is_some(), "deveria haver um toast de confirmação");
    }

    /// Cancelar a confirmação (N/Esc) não exclui nada.
    #[test]
    fn exclusao_de_sessoes_cancelada_nao_apaga() {
        let (mut app, _rx) = app_de_teste();
        app.store
            .create_session("a", "/p", "zeta-um", "m", None, "semi", None)
            .unwrap();
        app.abrir_historico("");
        for c in ['z', 'e', 't', 'a', '-', 'u', 'm'] {
            app.tratar_tecla(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
        }
        app.tratar_tecla(KeyEvent::new(KeyCode::Char(' '), KeyModifiers::NONE));
        app.tratar_tecla(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert!(matches!(app.modal, Some(Modal::ConfirmarSessoes { .. })));
        app.tratar_tecla(KeyEvent::new(KeyCode::Char('n'), KeyModifiers::NONE));
        assert!(app.modal.is_none(), "cancelar devia fechar o modal");
        assert!(app.store.get_session("a").unwrap().is_some());
    }

    /// O catálogo atualizado em segundo plano entra na configuração da
    /// interface, é persistido e avisa quando há modelos novos.
    #[test]
    fn catalogo_atualizado_entra_na_config() {
        let (mut app, _rx) = app_de_teste();
        app.tratar_evento(AgentEvent::CatalogoAtualizado {
            modelos: vec![crate::config::ModelEntry::new("novo/modelo")],
            novos: 1,
        });
        assert!(
            app.config
                .provider
                .models
                .iter()
                .any(|m| m.id == "novo/modelo"),
            "o modelo novo deveria estar no catálogo"
        );
        assert!(app.toast.is_some());
        // Sem modelos novos, nada de aviso.
        app.toast = None;
        app.tratar_evento(AgentEvent::CatalogoAtualizado {
            modelos: app.config.provider.models.clone(),
            novos: 0,
        });
        assert!(app.toast.is_none());
    }
}
