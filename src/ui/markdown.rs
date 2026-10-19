//! Renderizador de Markdown para a conversa.
//!
//! Próprio, sem depender de widgets externos: converte os eventos do
//! `pulldown-cmark` em linhas e estilos do ratatui. Cobre parágrafos, títulos,
//! listas, citações, código (inline e blocos), negrito, itálico, riscado,
//! links, réguas e tabelas simples.

use crate::ui::theme;
use pulldown_cmark::{Event, HeadingLevel, Options, Parser, Tag, TagEnd};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

#[derive(Clone, Copy, Default)]
struct EstiloInline {
    forte: bool,
    enfase: bool,
    riscado: bool,
    codigo: bool,
    link: bool,
}

#[derive(Clone)]
struct OpcaoLista {
    ordenada: Option<u64>,
    proximo: u64,
}

struct Renderizador {
    linhas: Vec<Line<'static>>,
    atual: Vec<Span<'static>>,
    estilo: EstiloInline,
    titulo: Option<HeadingLevel>,
    listas: Vec<OpcaoLista>,
    citacao: u16,
    em_codigo: bool,
    ultima_vazia: bool,
    tabela: Option<Tabela>,
    link_pendente: Option<String>,
}

#[derive(Default)]
struct Tabela {
    linhas: Vec<Vec<String>>,
    celula: String,
    cabecalho: bool,
}

impl Renderizador {
    fn novo() -> Self {
        Self {
            linhas: Vec::new(),
            atual: Vec::new(),
            estilo: EstiloInline::default(),
            titulo: None,
            listas: Vec::new(),
            citacao: 0,
            em_codigo: false,
            ultima_vazia: true,
            tabela: None,
            link_pendente: None,
        }
    }

    /// Garante uma linha em branco antes de um novo bloco.
    fn separacao(&mut self) {
        if !self.ultima_vazia && !self.linhas.is_empty() {
            self.linhas.push(Line::default());
            self.ultima_vazia = true;
        }
        if self.linhas.is_empty() {
            self.ultima_vazia = true;
        }
    }

    /// Prefixos de citação e lista para o começo de uma linha.
    fn prefixos(&mut self) {
        if self.citacao > 0 {
            for _ in 0..self.citacao {
                self.atual.push(Span::styled("▏ ", theme::apagado()));
            }
        }
        let profundidade = self.listas.len().saturating_sub(1);
        if let Some(lista) = self.listas.last_mut() {
            if profundidade > 0 {
                self.atual.push(Span::raw("  ".repeat(profundidade)));
            }
            let marcador = match lista.ordenada {
                Some(_) => {
                    let numero = lista.proximo;
                    lista.proximo += 1;
                    format!("{numero}. ")
                }
                None => "• ".to_string(),
            };
            self.atual.push(Span::styled(marcador, theme::rotulo()));
        }
    }

    fn fechar_linha(&mut self) {
        while let Some(span) = self.atual.last() {
            if span.content.trim().is_empty() {
                self.atual.pop();
            } else {
                break;
            }
        }
        if self.atual.is_empty() {
            if !self.ultima_vazia {
                self.linhas.push(Line::default());
                self.ultima_vazia = true;
            }
        } else {
            self.linhas
                .push(Line::from(std::mem::take(&mut self.atual)));
            self.ultima_vazia = false;
        }
    }

    fn estilo_span(&self) -> Style {
        if self.em_codigo {
            return Style::default()
                .fg(Color::Rgb(226, 232, 240))
                .bg(theme::codigo_fundo());
        }
        let mut s = if let Some(nivel) = self.titulo {
            match nivel {
                HeadingLevel::H1 => theme::titulo().add_modifier(Modifier::UNDERLINED),
                HeadingLevel::H2 => theme::titulo(),
                _ => theme::rotulo(),
            }
        } else if self.citacao > 0 {
            theme::raciocinio()
        } else {
            theme::texto()
        };
        if self.estilo.forte {
            s = s.add_modifier(Modifier::BOLD);
        }
        if self.estilo.enfase {
            s = s.add_modifier(Modifier::ITALIC);
        }
        if self.estilo.riscado {
            s = s.add_modifier(Modifier::CROSSED_OUT);
        }
        if self.estilo.link {
            s = s
                .fg(Color::Rgb(130, 180, 255))
                .add_modifier(Modifier::UNDERLINED);
        }
        if self.estilo.codigo {
            s = s.bg(theme::codigo_fundo()).fg(Color::Rgb(235, 205, 150));
        }
        s
    }

    fn empurrar_texto(&mut self, texto: &str) {
        if texto.is_empty() {
            return;
        }
        let estilo = self.estilo_span();
        self.atual.push(Span::styled(texto.to_string(), estilo));
        self.ultima_vazia = false;
    }

    fn empurrar_texto_codigo(&mut self, texto: &str) {
        // Blocos de código: cada linha vira uma linha própria, com fundo.
        let limpo = crate::util::strip_ansi(texto);
        for (i, pedaco) in limpo.split('\n').enumerate() {
            if i > 0 {
                self.fechar_linha();
            }
            if !pedaco.is_empty() {
                self.atual.push(Span::styled(
                    pedaco.to_string(),
                    Style::default()
                        .fg(Color::Rgb(214, 222, 235))
                        .bg(theme::codigo_fundo()),
                ));
                self.ultima_vazia = false;
            }
        }
    }

    fn anexar_url(&mut self, url: &str) {
        if !url.is_empty() {
            let mostrado = url.strip_prefix("mailto:").unwrap_or(url);
            self.atual.push(Span::styled(
                format!(" ({mostrado})"),
                theme::apagado().add_modifier(Modifier::ITALIC),
            ));
        }
    }

    fn evento(&mut self, evento: Event<'_>) {
        match evento {
            Event::Start(tag) => self.iniciar_tag(tag),
            Event::End(tag) => self.encerrar_tag(tag),
            Event::Text(texto) => {
                if self.em_codigo {
                    self.empurrar_texto_codigo(&texto);
                } else if self.tabela.is_some() {
                    if let Some(t) = &mut self.tabela {
                        t.celula
                            .push_str(&crate::html::decodificar_entidades(&texto));
                    }
                } else {
                    self.empurrar_texto(&texto);
                }
            }
            Event::Code(texto) => {
                self.estilo.codigo = true;
                self.empurrar_texto(&texto);
                self.estilo.codigo = false;
            }
            Event::SoftBreak => self.empurrar_texto(" "),
            Event::HardBreak => self.fechar_linha(),
            Event::Rule => {
                self.separacao();
                self.linhas
                    .push(Line::from(Span::styled("─".repeat(40), theme::apagado())));
                self.ultima_vazia = false;
            }
            Event::TaskListMarker(marcado) => {
                self.atual.push(Span::styled(
                    if marcado { "[x] " } else { "[ ] " },
                    theme::rotulo(),
                ));
            }
            Event::Html(html) | Event::InlineHtml(html) => {
                let texto = crate::html::html_para_texto(&html).texto;
                if !texto.is_empty() {
                    if self.em_codigo {
                        self.empurrar_texto_codigo(&texto);
                    } else {
                        self.empurrar_texto(&texto);
                    }
                }
            }
            Event::FootnoteReference(nome) => {
                self.empurrar_texto(&format!("[{nome}]"));
            }
            Event::InlineMath(f) | Event::DisplayMath(f) => {
                self.estilo.codigo = true;
                self.empurrar_texto(&format!("${f}$"));
                self.estilo.codigo = false;
            }
        }
    }

    fn iniciar_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Paragraph => {
                self.separacao();
                self.prefixos();
            }
            Tag::Heading { level, .. } => {
                self.separacao();
                self.titulo = Some(level);
                if level == HeadingLevel::H1 {
                    self.atual.push(Span::styled("▌ ", theme::titulo()));
                }
            }
            Tag::BlockQuote(_) => {
                self.separacao();
                self.citacao += 1;
            }
            Tag::CodeBlock(_) => {
                self.separacao();
                self.em_codigo = true;
            }
            Tag::List(inicio) => {
                if self.listas.is_empty() {
                    self.separacao();
                }
                self.listas.push(OpcaoLista {
                    ordenada: inicio,
                    proximo: inicio.unwrap_or(1),
                });
            }
            Tag::Item => {
                if self.citacao > 0 {
                    for _ in 0..self.citacao {
                        self.atual.push(Span::styled("▏ ", theme::apagado()));
                    }
                }
                self.prefixos();
            }
            Tag::Emphasis => self.estilo.enfase = true,
            Tag::Strong => self.estilo.forte = true,
            Tag::Strikethrough => self.estilo.riscado = true,
            Tag::Link { dest_url, .. } => {
                self.estilo.link = true;
                self.link_pendente = Some(dest_url.to_string());
            }
            Tag::Image { dest_url, .. } => {
                self.estilo.link = true;
                self.atual.push(Span::styled("[imagem: ", theme::apagado()));
                self.link_pendente = Some(dest_url.to_string());
            }
            Tag::Table(_) => {
                self.separacao();
                self.tabela = Some(Tabela::default());
            }
            Tag::TableHead => {
                if let Some(t) = &mut self.tabela {
                    t.cabecalho = true;
                }
            }
            Tag::TableRow => {
                if let Some(t) = &mut self.tabela {
                    t.linhas.push(Vec::new());
                }
            }
            Tag::TableCell => {
                if let Some(t) = &mut self.tabela {
                    t.celula.clear();
                }
            }
            _ => {}
        }
    }

    fn encerrar_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Paragraph => {
                self.fechar_linha();
            }
            TagEnd::Heading(_) => {
                self.titulo = None;
                self.fechar_linha();
            }
            TagEnd::BlockQuote(_) => {
                self.citacao = self.citacao.saturating_sub(1);
                self.fechar_linha();
            }
            TagEnd::CodeBlock => {
                self.em_codigo = false;
                self.fechar_linha();
            }
            TagEnd::List(_) => {
                self.listas.pop();
                if self.listas.is_empty() {
                    self.fechar_linha();
                }
            }
            TagEnd::Item => {
                self.fechar_linha();
            }
            TagEnd::Emphasis => self.estilo.enfase = false,
            TagEnd::Strong => self.estilo.forte = false,
            TagEnd::Strikethrough => self.estilo.riscado = false,
            TagEnd::Link => {
                if let Some(url) = self.link_pendente.take() {
                    self.anexar_url(&url);
                }
                self.estilo.link = false;
            }
            TagEnd::Image => {
                self.atual.push(Span::styled("]", theme::apagado()));
                if let Some(url) = self.link_pendente.take() {
                    self.anexar_url(&url);
                }
                self.estilo.link = false;
            }
            TagEnd::TableCell => {
                if let Some(t) = &mut self.tabela {
                    let celula = std::mem::take(&mut t.celula);
                    if let Some(linha) = t.linhas.last_mut() {
                        linha.push(celula.trim().to_string());
                    }
                }
            }
            TagEnd::TableHead => {
                if let Some(t) = &mut self.tabela {
                    t.cabecalho = false;
                }
            }
            TagEnd::TableRow => {}
            TagEnd::Table => {
                if let Some(t) = self.tabela.take() {
                    self.renderizar_tabela(t);
                }
            }
            _ => {}
        }
    }

    fn renderizar_tabela(&mut self, tabela: Tabela) {
        if tabela.linhas.is_empty() {
            return;
        }
        let colunas = tabela.linhas.iter().map(Vec::len).max().unwrap_or(0);
        let mut larguras = vec![0usize; colunas];
        for linha in &tabela.linhas {
            for (i, celula) in linha.iter().enumerate() {
                // Largura de terminal, não número de `char`s: CJK/emoji ocupam
                // duas colunas e desalinhavam a tabela inteira.
                larguras[i] = larguras[i].max(celula.width());
            }
        }
        let mut primeira = true;
        for linha in &tabela.linhas {
            let mut spans = Vec::new();
            for (i, largura) in larguras.iter().enumerate() {
                let celula = linha.get(i).map(String::as_str).unwrap_or("");
                let mut conteudo = celula.to_string();
                let extra = largura.saturating_sub(conteudo.width());
                conteudo.push_str(&" ".repeat(extra));
                let estilo = if primeira {
                    theme::rotulo().add_modifier(Modifier::BOLD)
                } else {
                    theme::texto()
                };
                spans.push(Span::styled(format!(" {conteudo} "), estilo));
                if i + 1 < larguras.len() {
                    spans.push(Span::styled("│", theme::borda()));
                }
            }
            self.linhas.push(Line::from(spans));
            self.ultima_vazia = false;
            if primeira {
                let total: usize = larguras.iter().map(|l| l + 2).sum::<usize>()
                    + larguras.len().saturating_sub(1);
                let linha = "─".repeat(total.min(120));
                self.linhas
                    .push(Line::from(Span::styled(linha, theme::borda())));
                primeira = false;
            }
        }
        self.separacao();
    }

    fn finalizar(mut self) -> Vec<Line<'static>> {
        self.fechar_linha();
        while self.linhas.last().is_some_and(|l| l.spans.is_empty()) {
            self.linhas.pop();
        }
        self.linhas
    }
}

/// Converte Markdown em linhas prontas para exibição.
pub fn renderizar(texto: &str) -> Vec<Line<'static>> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TASKLISTS);
    let parser = Parser::new_ext(texto, opts);
    let mut r = Renderizador::novo();
    for evento in parser {
        r.evento(evento);
    }
    r.finalizar()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texto_de(linhas: &[Line<'static>]) -> String {
        linhas
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.to_string())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn renderiza_titulo_negrito_e_codigo() {
        let linhas = renderizar("# Título\n\nTexto **forte** e `código`.");
        let texto = texto_de(&linhas);
        assert!(texto.contains("Título"));
        assert!(texto.contains("forte"));
        assert!(texto.contains("código"));
        let negrito = linhas
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("forte"))
            .unwrap();
        assert!(negrito.style.add_modifier.contains(Modifier::BOLD));
        let codigo = linhas
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("código"))
            .unwrap();
        assert_eq!(codigo.style.bg, Some(theme::codigo_fundo()));
    }

    #[test]
    fn renderiza_listas_e_citacoes() {
        let linhas = renderizar("- primeiro\n- segundo\n\n> citado\n\n1. um\n2. dois");
        let texto = texto_de(&linhas);
        assert!(texto.contains("• primeiro"));
        assert!(texto.contains("• segundo"));
        assert!(texto.contains("▏ citado"));
        assert!(texto.contains("1. um"));
        assert!(texto.contains("2. dois"));
    }

    #[test]
    fn renderiza_bloco_de_codigo_com_fundo() {
        let linhas = renderizar("```rust\nfn main() {}\n```");
        let fundo = linhas
            .iter()
            .flat_map(|l| l.spans.iter())
            .find(|s| s.content.contains("fn main"))
            .unwrap();
        assert_eq!(fundo.style.bg, Some(theme::codigo_fundo()));
    }

    #[test]
    fn renderiza_links_e_tabelas() {
        let linhas =
            renderizar("veja [docs](https://exemplo.com/x)\n\n| a | b |\n|---|---|\n| 1 | 2 |");
        let texto = texto_de(&linhas);
        assert!(texto.contains("docs"));
        assert!(texto.contains("https://exemplo.com/x"));
        assert!(texto.contains("a"));
        assert!(texto.contains("1"));
    }

    #[test]
    fn nao_quebra_com_entrada_vazia() {
        assert!(renderizar("").is_empty());
        // Linhas em branco não têm conteúdo renderizável.
        assert!(renderizar("\n\n\n").is_empty());
    }
}
