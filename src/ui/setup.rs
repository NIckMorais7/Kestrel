//! Assistente de primeira execução: escolhe provedor, modelo, esforço de
//! raciocínio, nível de permissão e diretório de trabalho.
//!
//! Regras: a chave de API é lida apenas do ambiente e nunca é pedida na tela;
//! esforços de raciocínio só são oferecidos quando o suporte é conhecido;
//! o modelo nunca é trocado sem escolha explícita.

use crate::config::{Capability, Config, ModelEntry};
use crate::perm::PermissionLevel;
use crate::provider::catalog;
use crate::ui::input::Editor;
use crate::ui::mascot::{self, EstadoMascote};
use crate::ui::{self, theme};
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use std::time::Duration;

#[derive(Clone, PartialEq, Eq)]
enum Etapa {
    BoasVindas,
    Chave,
    Modelo,
    Esforco,
    Permissao,
    Diretorio,
    Resumo,
}

impl Etapa {
    fn numero(&self) -> usize {
        match self {
            Etapa::BoasVindas => 1,
            Etapa::Chave => 2,
            Etapa::Modelo => 3,
            Etapa::Esforco => 4,
            Etapa::Permissao => 5,
            Etapa::Diretorio => 6,
            Etapa::Resumo => 7,
        }
    }

    fn titulo(&self) -> &'static str {
        match self {
            Etapa::BoasVindas => "boas-vindas",
            Etapa::Chave => "chave de API",
            Etapa::Modelo => "modelo",
            Etapa::Esforco => "esforço de raciocínio",
            Etapa::Permissao => "permissões",
            Etapa::Diretorio => "diretório de trabalho",
            Etapa::Resumo => "resumo",
        }
    }

    fn anterior(&self) -> Etapa {
        match self {
            Etapa::BoasVindas => Etapa::BoasVindas,
            Etapa::Chave => Etapa::BoasVindas,
            Etapa::Modelo => Etapa::Chave,
            Etapa::Esforco => Etapa::Modelo,
            Etapa::Permissao => Etapa::Esforco,
            Etapa::Diretorio => Etapa::Permissao,
            Etapa::Resumo => Etapa::Diretorio,
        }
    }
}

enum EstadoModelos {
    Ocioso,
    Carregando,
    Prontos(Vec<ModelEntry>),
    Falha(String),
}

pub struct Assistente {
    config: Config,
    etapa: Etapa,
    editor: Editor,
    sel: usize,
    filtro: String,
    modelos: EstadoModelos,
    manual: bool,
    erro: Option<String>,
    tick: u64,
    concluido: bool,
}

impl Assistente {
    pub fn novo(config: Config) -> Self {
        let mut editor = Editor::novo(20);
        if let Some(dir) = &config.agent.workdir {
            editor.definir_texto(dir.display().to_string());
        }
        Self {
            config,
            etapa: Etapa::BoasVindas,
            editor,
            sel: 0,
            filtro: String::new(),
            modelos: EstadoModelos::Ocioso,
            manual: false,
            erro: None,
            tick: 0,
            concluido: false,
        }
    }

    fn avancar(&mut self) {
        self.erro = None;
        self.sel = 0;
        self.filtro.clear();
        self.etapa = match self.etapa {
            Etapa::BoasVindas => Etapa::Chave,
            Etapa::Chave => Etapa::Modelo,
            Etapa::Modelo => {
                // Pula o esforço quando o modelo não tem suporte conhecido.
                let suporta = self
                    .config
                    .model_entry(&self.config.provider.model)
                    .map(|m| m.supports_reasoning == Capability::Sim)
                    .unwrap_or(false);
                if suporta {
                    Etapa::Esforco
                } else {
                    self.config.provider.reasoning_effort = None;
                    Etapa::Permissao
                }
            }
            Etapa::Esforco => Etapa::Permissao,
            Etapa::Permissao => {
                self.editor = Editor::novo(20);
                self.editor
                    .definir_texto(self.config.workdir().display().to_string());
                Etapa::Diretorio
            }
            Etapa::Diretorio => Etapa::Resumo,
            Etapa::Resumo => {
                self.config.setup_complete = true;
                self.concluido = true;
                Etapa::Resumo
            }
        };
    }

    fn voltar(&mut self) {
        self.erro = None;
        self.etapa = self.etapa.anterior();
    }

    async fn buscar_modelos(&mut self) {
        if !self.config.api_key_presente() {
            self.modelos = EstadoModelos::Falha(format!(
                "a variável {} não está definida; informe o identificador do modelo manualmente \
                 (pressione M) ou saia, configure a chave e execute o Kestrel novamente",
                self.config.provider.api_key_env
            ));
            return;
        }
        let provider = match crate::provider::nvidia::create_provider(&self.config) {
            Ok(p) => p,
            Err(e) => {
                self.modelos = EstadoModelos::Falha(format!("falha ao criar o provedor: {e}"));
                return;
            }
        };
        self.modelos = EstadoModelos::Carregando;
        match provider.list_models().await {
            Ok(descobertos) => {
                let mesclados =
                    catalog::merge_discovered(&self.config.provider.models, descobertos);
                self.config.provider.models = mesclados;
                let itens = self.ordenar_modelos(self.config.provider.models.clone());
                self.modelos = EstadoModelos::Prontos(itens);
            }
            Err(e) => {
                self.modelos = EstadoModelos::Falha(format!(
                    "não foi possível listar os modelos do provedor: {e}. \
                     Verifique a rede e a chave, ou informe o modelo manualmente (M)."
                ));
            }
        }
    }

    fn ordenar_modelos(&self, mut modelos: Vec<ModelEntry>) -> Vec<ModelEntry> {
        // Ferramentas primeiro, depois nome: os utilizáveis aparecem no topo.
        modelos.sort_by(|a, b| {
            let peso = |m: &ModelEntry| match m.supports_tools {
                Capability::Sim => 0,
                Capability::Desconhecido => 1,
                Capability::Nao => 2,
            };
            peso(a).cmp(&peso(b)).then_with(|| a.id.cmp(&b.id))
        });
        modelos
    }

    fn modelos_filtrados(&self) -> Vec<ModelEntry> {
        match &self.modelos {
            EstadoModelos::Prontos(lista) => {
                let filtro = self.filtro.to_lowercase();
                lista
                    .iter()
                    .filter(|m| filtro.is_empty() || m.id.to_lowercase().contains(&filtro))
                    .cloned()
                    .collect()
            }
            _ => Vec::new(),
        }
    }

    fn selecionar_modelo(&mut self) {
        let filtrados = self.modelos_filtrados();
        if let Some(entrada) = filtrados.get(self.sel).cloned() {
            self.config.provider.model = entrada.id.clone();
            self.config.upsert_model(entrada);
            self.avancar();
        }
    }

    fn tratar_tecla(&mut self, tecla: KeyEvent) {
        match self.etapa.clone() {
            Etapa::BoasVindas | Etapa::Chave | Etapa::Resumo => match tecla.code {
                KeyCode::Enter => self.avancar(),
                KeyCode::Esc => self.voltar(),
                _ => {}
            },
            Etapa::Modelo => self.tratar_tecla_modelo(tecla),
            Etapa::Esforco => {
                let opcoes = self.esforcos();
                match tecla.code {
                    KeyCode::Enter => {
                        if let Some(nivel) = opcoes.get(self.sel).cloned() {
                            self.config.provider.reasoning_effort = nivel;
                            self.avancar();
                        }
                    }
                    KeyCode::Esc => {
                        self.config.provider.reasoning_effort = None;
                        self.voltar();
                    }
                    KeyCode::Up => self.sel = self.sel.saturating_sub(1),
                    KeyCode::Down if !opcoes.is_empty() => {
                        self.sel = (self.sel + 1).min(opcoes.len() - 1);
                    }
                    _ => {}
                }
            }
            Etapa::Permissao => {
                let opcoes = [
                    PermissionLevel::Read,
                    PermissionLevel::Write,
                    PermissionLevel::Semi,
                    PermissionLevel::Full,
                ];
                match tecla.code {
                    KeyCode::Enter => {
                        self.config.agent.permission = opcoes[self.sel.min(opcoes.len() - 1)];
                        self.avancar();
                    }
                    KeyCode::Esc => self.voltar(),
                    KeyCode::Up => self.sel = self.sel.saturating_sub(1),
                    KeyCode::Down => self.sel = (self.sel + 1).min(opcoes.len() - 1),
                    _ => {}
                }
            }
            Etapa::Diretorio => match tecla.code {
                KeyCode::Enter => {
                    let texto = self.editor.texto().trim().to_string();
                    if texto.is_empty() {
                        self.config.agent.workdir = None;
                        self.avancar();
                        return;
                    }
                    let caminho = crate::util::expand_tilde(&texto);
                    if caminho.is_dir() {
                        // Aceitar o diretório atual (o que vem pré-preenchido)
                        // não fixa nada: o Kestrel continua abrindo onde for
                        // chamado. Digitar outro diretório é escolha explícita
                        // e passa a ser fixo entre execuções.
                        let atual = std::env::current_dir().unwrap_or_default();
                        let mesmo = caminho == atual
                            || std::fs::canonicalize(&caminho).ok()
                                == std::fs::canonicalize(&atual).ok();
                        self.config.agent.workdir = Some(caminho);
                        self.config.fixar_diretorio(!mesmo);
                        self.avancar();
                    } else {
                        self.erro =
                            Some(format!("`{}` não é um diretório válido", caminho.display()));
                    }
                }
                KeyCode::Esc => self.voltar(),
                KeyCode::Char(c) => self.editor.inserir_char(c),
                KeyCode::Backspace => self.editor.backspace(),
                KeyCode::Left => self.editor.esquerda(),
                KeyCode::Right => self.editor.direita(),
                KeyCode::Home => self.editor.home(),
                KeyCode::End => self.editor.fim(),
                _ => {}
            },
        }
    }

    fn tratar_tecla_modelo(&mut self, tecla: KeyEvent) {
        if !matches!(self.modelos, EstadoModelos::Prontos(_)) {
            match tecla.code {
                KeyCode::Esc => self.voltar(),
                KeyCode::Char('m') | KeyCode::Char('M') => {
                    // Liga o modo manual e encerra o evento: sem o `return`, o
                    // fluxo continuava e o próprio 'M' era inserido no campo.
                    self.manual = true;
                    self.erro = None;
                    self.editor.limpar();
                    return;
                }
                _ => {}
            }
            if self.manual {
                match tecla.code {
                    KeyCode::Enter => {
                        let id = self.editor.texto().trim().to_string();
                        if id.is_empty() {
                            self.erro = Some("informe o identificador do modelo, ex.: meta/llama-3.1-70b-instruct".into());
                        } else {
                            let entrada = catalog::enrich(ModelEntry::new(id.clone()));
                            self.config.provider.model = id;
                            self.config.upsert_model(entrada);
                            self.manual = false;
                            self.avancar();
                        }
                    }
                    // Esc desliga o modo manual (o campo manual é descartado).
                    KeyCode::Esc => {
                        self.manual = false;
                    }
                    KeyCode::Char(c) => self.editor.inserir_char(c),
                    KeyCode::Backspace => self.editor.backspace(),
                    KeyCode::Left => self.editor.esquerda(),
                    KeyCode::Right => self.editor.direita(),
                    KeyCode::Home => self.editor.home(),
                    KeyCode::End => self.editor.fim(),
                    _ => {}
                }
            } else if tecla.code == KeyCode::Enter {
                self.avancar();
            }
            return;
        }
        let filtrados = self.modelos_filtrados();
        match tecla.code {
            KeyCode::Enter => self.selecionar_modelo(),
            KeyCode::Esc => self.voltar(),
            KeyCode::Up => self.sel = self.sel.saturating_sub(1),
            KeyCode::Down => {
                if !filtrados.is_empty() {
                    self.sel = (self.sel + 1).min(filtrados.len() - 1);
                }
            }
            KeyCode::Backspace => {
                self.filtro.pop();
                self.sel = 0;
            }
            KeyCode::Char(c) => {
                self.filtro.push(c);
                self.sel = 0;
            }
            _ => {}
        }
        // Garante que a seleção não passe do tamanho filtrado.
        let total = self.modelos_filtrados().len();
        if total > 0 && self.sel >= total {
            self.sel = total - 1;
        }
    }

    fn esforcos(&self) -> Vec<Option<String>> {
        let mut opcoes = vec![None];
        if let Some(entrada) = self.config.model_entry(&self.config.provider.model)
            && entrada.supports_reasoning == Capability::Sim
        {
            let lista = if entrada.reasoning_efforts.is_empty() {
                vec!["low".to_string(), "medium".to_string(), "high".to_string()]
            } else {
                entrada.reasoning_efforts.clone()
            };
            opcoes.extend(lista.into_iter().map(Some));
        }
        opcoes
    }

    fn desenhar(&self, f: &mut Frame) {
        let area = f.area();
        f.render_widget(Clear, area);
        // Mesmo fundo quase preto da interface principal.
        f.render_widget(Block::default().style(theme::fundo()), area);
        let [cabecalho, corpo, rodape] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Min(10),
            Constraint::Length(2),
        ])
        .areas(area);

        let passo = format!("passo {} de 7", self.etapa.numero());
        let titulo = format!(" Kestrel — configuração inicial · {} ", self.etapa.titulo());
        f.render_widget(
            Paragraph::new(vec![Line::from(vec![
                Span::styled(
                    " Kestrel ",
                    Style::default()
                        .fg(Color::Rgb(30, 32, 40))
                        .bg(Color::Rgb(255, 196, 110))
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(format!("  {titulo}"), theme::titulo()),
                Span::styled(format!("  {passo}"), theme::apagado()),
            ])]),
            cabecalho,
        );

        let bloco = theme::bloco().border_style(theme::modal_borda());
        let interno = bloco.inner(corpo);
        f.render_widget(bloco, corpo);

        let [texto, lateral] =
            Layout::horizontal([Constraint::Min(40), Constraint::Length(20)]).areas(interno);
        let linhas = self.linhas_etapa();
        f.render_widget(
            Paragraph::new(Text::from(linhas)).wrap(Wrap { trim: false }),
            texto,
        );

        let estado = if matches!(self.modelos, EstadoModelos::Carregando) {
            EstadoMascote::Pensando
        } else if self.erro.is_some() {
            EstadoMascote::Erro
        } else {
            EstadoMascote::Ocioso
        };
        if self.config.ui.show_mascot {
            let animacao = mascot::AnimacaoPip {
                deslocamento: mascot::deslocamento_de(self.tick / 3),
                piscando: self.tick % 143 == 142,
            };
            // Duas linhas de Pip com o estado ao pé — o mesmo tamanho usado no
            // indicador de trabalho: nada de mascote grande no assistente.
            let linhas_pip = mascot::linhas(mascot::TamanhoMascote::Mini, animacao);
            let altura = linhas_pip.len() as u16 + 1;
            if lateral.height >= altura && lateral.width > 0 {
                let rotulo =
                    crate::util::truncate(&format!(" {}", estado.label()), lateral.width as usize);
                let mut conteudo = Text::from(linhas_pip);
                conteudo.push_line(Line::from(Span::styled(rotulo, theme::apagado())));
                f.render_widget(
                    Paragraph::new(conteudo),
                    Rect {
                        x: lateral.x,
                        y: lateral.y,
                        width: lateral.width,
                        height: altura,
                    },
                );
            }
        }

        let mut linhas_rodape = vec![Line::from(Span::styled(
            " enter avança · esc volta · a chave de API é lida apenas de variável de ambiente ",
            theme::apagado(),
        ))];
        if let Some(erro) = &self.erro {
            linhas_rodape.push(Line::from(Span::styled(
                format!(" {erro} "),
                theme::falha(),
            )));
        }
        f.render_widget(
            Paragraph::new(Text::from(linhas_rodape)).wrap(Wrap { trim: true }),
            rodape,
        );
    }

    fn linhas_etapa(&self) -> Vec<Line<'static>> {
        let mut linhas: Vec<Line<'static>> = Vec::new();
        let mut titulo = |texto: &str| {
            linhas.push(Line::default());
            linhas.push(Line::from(Span::styled(texto.to_string(), theme::titulo())));
        };
        match &self.etapa {
            Etapa::BoasVindas => {
                titulo("Bem-vindo ao Kestrel");
                for linha in [
                    "O Kestrel é um agente autônomo de codificação que trabalha no seu terminal:",
                    "ele analisa o projeto, planeja, executa comandos reais, edita arquivos e",
                    "valida o resultado com testes. Esta configuração leva menos de um minuto.",
                    "",
                    "Você poderá mudar tudo depois com /model, /effort e /permissions.",
                ] {
                    linhas.push(paragrafo(linha));
                }
            }
            Etapa::Chave => {
                titulo("Chave de API");
                let presente = self.config.api_key_presente();
                linhas.push(Line::from(vec![
                    Span::styled("variável ", theme::texto()),
                    Span::styled(self.config.provider.api_key_env.clone(), theme::rotulo()),
                    Span::styled(": ", theme::texto()),
                    Span::styled(
                        if presente {
                            "presente no ambiente"
                        } else {
                            "ausente"
                        },
                        if presente {
                            theme::sucesso()
                        } else {
                            theme::falha()
                        },
                    ),
                ]));
                linhas.push(paragrafo(""));
                linhas.push(paragrafo(
                    "A chave é lida somente do ambiente e nunca é gravada em disco nem exibida.",
                ));
                linhas.push(paragrafo(
                    "No seu terminal, defina: export NVIDIA_API_KEY=\"sua-chave\"",
                ));
                if !presente {
                    linhas.push(Line::from(Span::styled(
                        "sem chave: você pode continuar, mas o agente não conseguirá falar com o modelo",
                        theme::aviso(),
                    )));
                }
                linhas.push(paragrafo(
                    "Endpoint padrão: https://integrate.api.nvidia.com/v1",
                ));
            }
            Etapa::Modelo => {
                titulo("Modelo");
                if self.manual {
                    linhas.push(paragrafo(
                        "Digite o identificador do modelo exatamente como no catálogo do provedor:",
                    ));
                    linhas.push(Line::from(vec![
                        Span::styled("  ❯ ", theme::usuario()),
                        Span::styled(self.editor.texto().to_string(), theme::texto()),
                    ]));
                    linhas.push(paragrafo(
                        "ex.: meta/llama-3.3-70b-instruct · deepseek-ai/deepseek-r1",
                    ));
                    return linhas;
                }
                match &self.modelos {
                    EstadoModelos::Ocioso | EstadoModelos::Carregando => {
                        linhas.push(paragrafo("Consultando o catálogo do provedor…"));
                    }
                    EstadoModelos::Falha(mensagem) => {
                        for parte in mensagem.lines() {
                            linhas
                                .push(Line::from(Span::styled(parte.to_string(), theme::aviso())));
                        }
                        linhas.push(paragrafo(""));
                        linhas.push(paragrafo(
                            "Pressione M para informar o identificador do modelo manualmente;",
                        ));
                        linhas.push(paragrafo("os demais passos continuam funcionando."));
                    }
                    EstadoModelos::Prontos(_) => {
                        linhas.push(paragrafo(
                            "Escolha um modelo (ferramentas: sim aparecem primeiro). Digite para filtrar:",
                        ));
                        if !self.filtro.is_empty() {
                            linhas.push(Line::from(vec![
                                Span::styled("filtro: ", theme::apagado()),
                                Span::styled(self.filtro.clone(), theme::texto()),
                            ]));
                        }
                        let filtrados = self.modelos_filtrados();
                        if filtrados.is_empty() {
                            linhas.push(Line::from(Span::styled(
                                "  nenhum modelo corresponde ao filtro",
                                theme::aviso(),
                            )));
                        }
                        for (i, modelo) in filtrados
                            .iter()
                            .enumerate()
                            .skip(self.sel.saturating_sub(5))
                            .take(14)
                        {
                            let selecionado = i == self.sel;
                            let marcador = if selecionado { "  ▶ " } else { "    " };
                            let estilo = if selecionado {
                                theme::titulo()
                            } else {
                                theme::texto()
                            };
                            let capacidades = format!(
                                "ctx {} · ferramentas {} · raciocínio {}",
                                modelo
                                    .context_window
                                    .map(|c| crate::util::human_bytes((c as u64) * 4))
                                    .unwrap_or_else(|| "?".into()),
                                modelo.supports_tools.label(),
                                modelo.supports_reasoning.label()
                            );
                            linhas.push(Line::from(vec![
                                Span::styled(marcador, theme::rotulo()),
                                Span::styled(modelo.id.clone(), estilo),
                                Span::styled(format!("  [{capacidades}]"), theme::apagado()),
                            ]));
                        }
                        linhas.push(paragrafo(
                            "capacidades vêm do catálogo interno/documentação; capacidades desconhecidas não são presumidas",
                        ));
                    }
                }
            }
            Etapa::Esforco => {
                titulo("Esforço de raciocínio");
                linhas.push(paragrafo(
                    "Este modelo declara suporte a raciocínio. Escolha o esforço a enviar:",
                ));
                for (i, opcao) in self.esforcos().iter().enumerate() {
                    let selecionado = i == self.sel;
                    let marcador = if selecionado { "  ▶ " } else { "    " };
                    let rotulo = opcao
                        .clone()
                        .unwrap_or_else(|| "(não especificar)".to_string());
                    linhas.push(Line::from(vec![
                        Span::styled(marcador, theme::rotulo()),
                        Span::styled(
                            rotulo,
                            if selecionado {
                                theme::titulo()
                            } else {
                                theme::texto()
                            },
                        ),
                    ]));
                }
                linhas.push(paragrafo(""));
                linhas.push(paragrafo(
                    "Somente níveis comprovadamente suportados são oferecidos.",
                ));
            }
            Etapa::Permissao => {
                titulo("Nível de permissão");
                linhas.push(paragrafo(
                    "Define o que o agente pode fazer sem pedir confirmação:",
                ));
                let opcoes = [
                    PermissionLevel::Read,
                    PermissionLevel::Write,
                    PermissionLevel::Semi,
                    PermissionLevel::Full,
                ];
                for (i, nivel) in opcoes.iter().enumerate() {
                    let selecionado = i == self.sel;
                    let marcador = if selecionado { "  ▶ " } else { "    " };
                    linhas.push(Line::from(vec![
                        Span::styled(marcador, theme::rotulo()),
                        Span::styled(
                            format!("{} — ", nivel.label()),
                            Style::default()
                                .fg(theme::cor_nivel(*nivel))
                                .add_modifier(Modifier::BOLD),
                        ),
                        Span::styled(nivel.description().to_string(), theme::texto()),
                    ]));
                }
                linhas.push(paragrafo(""));
                linhas.push(paragrafo(
                    "Comandos catastróficos continuam bloqueados em qualquer nível.",
                ));
            }
            Etapa::Diretorio => {
                titulo("Diretório de trabalho");
                linhas.push(paragrafo(
                    "O agente trabalha dentro deste diretório. Enter vazio usa o diretório atual:",
                ));
                linhas.push(Line::from(vec![
                    Span::styled("  ❯ ", theme::usuario()),
                    Span::styled(self.editor.texto().to_string(), theme::texto()),
                ]));
                linhas.push(paragrafo(format!(
                    "diretório atual: {}",
                    std::env::current_dir()
                        .map(|d| d.display().to_string())
                        .unwrap_or_else(|_| "?".into())
                )));
            }
            Etapa::Resumo => {
                titulo("Tudo pronto");
                let modelo = if self.config.provider.model.is_empty() {
                    "(nenhum — o agente avisará ao usar)".to_string()
                } else {
                    self.config.provider.model.clone()
                };
                linhas.push(Line::from(vec![
                    Span::styled("modelo: ", theme::apagado()),
                    Span::styled(modelo, theme::texto()),
                ]));
                linhas.push(Line::from(vec![
                    Span::styled("esforço de raciocínio: ", theme::apagado()),
                    Span::styled(
                        self.config
                            .provider
                            .reasoning_effort
                            .clone()
                            .unwrap_or_else(|| "(não especificado)".into()),
                        theme::texto(),
                    ),
                ]));
                linhas.push(Line::from(vec![
                    Span::styled("permissão: ", theme::apagado()),
                    Span::styled(
                        format!(
                            "{} — {}",
                            self.config.agent.permission.label(),
                            self.config.agent.permission.description()
                        ),
                        Style::default().fg(theme::cor_nivel(self.config.agent.permission)),
                    ),
                ]));
                linhas.push(Line::from(vec![
                    Span::styled("diretório: ", theme::apagado()),
                    Span::styled(self.config.workdir().display().to_string(), theme::texto()),
                ]));
                linhas.push(paragrafo(""));
                linhas.push(paragrafo(
                    "Enter salva a configuração e abre o Kestrel. Digite uma tarefa e o agente começa.",
                ));
            }
        }
        linhas
    }

    fn tiquetaquear(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }
}

fn paragrafo(texto: impl Into<String>) -> Line<'static> {
    Line::from(Span::styled(texto.into(), theme::texto()))
}

/// Executa o assistente até o fim, devolvendo a configuração pronta.
pub async fn executar(
    terminal: &mut ratatui::DefaultTerminal,
    config: Config,
) -> anyhow::Result<Config> {
    let mut assistente = Assistente::novo(config);
    let mut eventos = ui::canal_terminal();
    let mut tique = tokio::time::interval(Duration::from_millis(120));
    tique.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        if assistente_precisa_carregar(&assistente) {
            assistente.desenhar_carregando(terminal)?;
            assistente.buscar_modelos().await;
        }
        terminal.draw(|f| assistente.desenhar(f))?;
        tokio::select! {
            Some(evento) = eventos.recv() => {
                match evento {
                    Event::Key(tecla) if tecla.kind == KeyEventKind::Press => {
                        assistente.tratar_tecla(tecla);
                    }
                    Event::Resize(_, _) => {}
                    _ => {}
                }
            }
            _ = tique.tick() => assistente.tiquetaquear(),
        }
        if assistente.concluido {
            break;
        }
    }
    Ok(assistente.config)
}

fn assistente_precisa_carregar(assistente: &Assistente) -> bool {
    assistente.etapa == Etapa::Modelo && matches!(assistente.modelos, EstadoModelos::Ocioso)
}

impl Assistente {
    fn desenhar_carregando(&self, terminal: &mut ratatui::DefaultTerminal) -> anyhow::Result<()> {
        terminal.draw(|f| {
            let area = f.area();
            f.render_widget(
                Paragraph::new(Span::styled(
                    " consultando o catálogo de modelos do provedor… ",
                    theme::titulo(),
                ))
                .block(theme::bloco().border_style(theme::modal_borda())),
                area,
            );
        })?;
        Ok(())
    }
}
