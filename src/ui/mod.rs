//! Interface do Kestrel (ratatui): desenho da conversa, painéis, entrada e modais.
//!
//! Todo o texto da interface está em português brasileiro. Os números de
//! alterações exibidos vêm dos diffs reais devolvidos pelas ferramentas.

pub mod input;
pub mod markdown;
pub mod mascot;
pub mod pip_textura;
pub mod setup;
pub mod theme;

use crate::app::{App, Modal, Seletor};
use crate::events::EstadoProcesso;
use crate::ui::mascot::{EstadoMascote, TamanhoMascote};
use crossterm::event as ct;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Clear, Paragraph, Wrap};
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Duration;
use tokio::sync::mpsc;

/// Assinantes do fluxo de eventos do terminal (uma única thread de leitura).
static ASSINANTES: Mutex<Vec<mpsc::UnboundedSender<ct::Event>>> = Mutex::new(Vec::new());
static LEITURA_INICIADA: OnceLock<()> = OnceLock::new();

/// Devolve um receptor de eventos do terminal (teclado, mouse, redimensionamento).
///
/// A thread de leitura é criada apenas uma vez por processo; chamadas
/// subsequentes (assistente de configuração, aplicação) recebem novos
/// receptores do mesmo fluxo.
pub fn canal_terminal() -> mpsc::UnboundedReceiver<ct::Event> {
    LEITURA_INICIADA.get_or_init(|| {
        std::thread::spawn(|| {
            loop {
                match ct::read() {
                    Ok(evento) => {
                        let mut guard = ASSINANTES.lock().expect("assinantes do terminal");
                        guard.retain(|tx| tx.send(evento.clone()).is_ok());
                    }
                    Err(_) => std::thread::sleep(Duration::from_millis(30)),
                }
            }
        });
    });
    let (tx, rx) = mpsc::unbounded_channel();
    ASSINANTES.lock().expect("assinantes do terminal").push(tx);
    rx
}

/// Desenha a interface completa.
pub fn desenhar(f: &mut Frame, app: &mut App) {
    let area = f.area();
    // Fundo quase preto do aplicativo: os componentes desenhados depois
    // herdam esta cor (estilos sem `bg` não a sobrescrevem).
    f.render_widget(Block::default().style(theme::fundo()), area);
    if area.width < 40 || area.height < 12 {
        let aviso = Paragraph::new(
            "Terminal pequeno demais para a interface do Kestrel.\nAumente a janela.",
        )
        .wrap(Wrap { trim: true });
        f.render_widget(aviso, area);
        return;
    }

    let altura_entrada = (app.entrada.total_linhas().min(8) as u16) + 2;
    // Enquanto o modelo trabalha, o Pip ocupa o lugar dos antigos pontinhos:
    // o indicador cresce acima do campo de mensagem (duas linhas) e volta ao
    // normal quando o turno termina. Em terminais baixos ele dá lugar ao
    // status compacto.
    let mostrar_pip = app.turno_ativo && app.config.ui.show_mascot && area.height >= 14;
    let altura_status = if mostrar_pip {
        mascot::linhas_ocupadas(TamanhoMascote::Mini)
    } else {
        1
    };
    let [cabecalho, corpo, barra, status, area_entrada] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(5),
        Constraint::Length(1),
        Constraint::Length(altura_status),
        Constraint::Length(altura_entrada),
    ])
    .areas(area);

    desenhar_cabecalho(f, app, cabecalho);
    desenhar_conversa(f, app, corpo);
    desenhar_barra_estado(f, app, barra);
    desenhar_status(f, app, status, mostrar_pip);
    desenhar_entrada(f, app, area_entrada);
    desenhar_sugestoes(f, app, area_entrada);
    desenhar_modal(f, app, area);
}

/// Linha superior: só o projeto (e o branch), sem o rótulo "Kestrel".
///
/// Modelo, modo, permissão e contexto foram para a barra acima do campo de
/// mensagem (`desenhar_barra_estado`), onde ficam perto de onde o usuário
/// digita — o topo fica limpo para a conversa.
fn desenhar_cabecalho(f: &mut Frame, app: &App, area: Rect) {
    let projeto = crate::util::caminho_curto(&app.config.workdir(), 48);
    let mut spans = vec![Span::styled(format!(" {projeto}"), theme::apagado())];
    if let Some(branch) = &app.projeto.git_branch {
        spans.push(Span::styled(format!(" ({branch})"), theme::apagado()));
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

/// Barra com modelo, provedor, modo, permissão e contexto, logo acima do campo
/// de mensagem.
fn desenhar_barra_estado(f: &mut Frame, app: &App, area: Rect) {
    let mut spans = vec![Span::styled(
        crate::util::truncate(&app.modelo, 44),
        theme::texto(),
    )];
    if let Some(esforco) = &app.esforco {
        spans.push(Span::styled(format!(" · {esforco}"), theme::apagado()));
    }
    spans.push(Span::styled(
        format!(" · {}", app.config.provider.id),
        theme::apagado(),
    ));
    spans.push(Span::styled("  │  modo ", theme::borda()));
    spans.push(Span::styled(
        app.modo.label().to_string(),
        if app.modo == crate::config::ModoOperacao::Chat {
            theme::rotulo()
        } else {
            Style::default()
                .fg(Color::Rgb(140, 220, 170))
                .add_modifier(Modifier::BOLD)
        },
    ));
    spans.push(Span::styled("  │  permissão ", theme::borda()));
    spans.push(Span::styled(
        app.nivel.label().to_string(),
        Style::default()
            .fg(theme::cor_nivel(app.nivel))
            .add_modifier(Modifier::BOLD),
    ));
    // O raciocínio só é mencionado quando está visível.
    if app.mostrar_raciocinio {
        spans.push(Span::styled("  │  thinking ", theme::borda()));
        spans.push(Span::styled("on", theme::rotulo()));
    }
    // Contexto só aparece quando de fato importa (perto do limite).
    if app.contexto.1 > 0 {
        let fracao = app.contexto.0 as f32 / app.contexto.1.max(1) as f32;
        if fracao >= 0.7 {
            spans.push(Span::styled("  │  contexto ", theme::borda()));
            spans.push(Span::styled(
                format!("{:.0}%", fracao * 100.0),
                Style::default().fg(theme::cor_contexto(fracao)),
            ));
        }
    }
    f.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn desenhar_conversa(f: &mut Frame, app: &mut App, area: Rect) {
    let titulo = if app.sessao_titulo.is_empty() {
        " conversa ".to_string()
    } else {
        format!(
            " conversa — {} ",
            crate::util::truncate(&app.sessao_titulo, 50)
        )
    };
    let bloco = theme::bloco()
        .border_style(theme::borda())
        .title(Span::styled(titulo, theme::rotulo()));
    let interno = bloco.inner(area);
    let largura = interno.width.max(1) as usize;

    // Estado vazio: uma recepção curta em vez de uma tela sem nada.
    if app.blocos.is_empty() {
        let linhas: Vec<Line<'static>> = vec![
            Line::default(),
            Line::from(vec![
                Span::styled("  Bem-vindo ao Kestrel. ", theme::titulo()),
                Span::styled("Descreva a tarefa em linguagem natural.", theme::texto()),
            ]),
            Line::default(),
            Line::from(vec![
                Span::styled("  /help", theme::rotulo()),
                Span::styled(" lista os comandos · ", theme::apagado()),
                Span::styled("/model", theme::rotulo()),
                Span::styled(" troca o modelo · ", theme::apagado()),
                Span::styled("/chat", theme::rotulo()),
                Span::styled(" e ", theme::apagado()),
                Span::styled("/agente", theme::rotulo()),
                Span::styled(" trocam o modo · ", theme::apagado()),
                Span::styled("/permissions", theme::rotulo()),
                Span::styled(" ajusta o nível", theme::apagado()),
            ]),
        ];
        f.render_widget(
            Paragraph::new(Text::from(linhas))
                .block(bloco)
                .wrap(Wrap { trim: false }),
            area,
        );
        return;
    }

    let total = contar_altura(&app.linhas, largura) as u16;
    let visivel = interno.height;
    let max_scroll = total.saturating_sub(visivel);
    if app.seguir_fim {
        app.scroll = max_scroll;
    } else {
        app.scroll = app.scroll.min(max_scroll);
        if app.scroll >= max_scroll {
            app.seguir_fim = true;
        }
    }
    let scroll = app.scroll;

    let paragrafo = Paragraph::new(app.linhas.as_slice())
        .block(bloco)
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(paragrafo, area);

    // Indicador de rolagem quando o usuário saiu do fim.
    if !app.seguir_fim && max_scroll > 0 {
        let indicador = format!(" ↑ {scroll}/{max_scroll} ");
        let largura_ind = indicador.chars().count() as u16;
        if area.width > largura_ind + 2 {
            let destino = Rect {
                x: area.x + area.width - largura_ind - 1,
                y: area.y,
                width: largura_ind,
                height: 1,
            };
            f.render_widget(
                Paragraph::new(Span::styled(indicador, theme::aviso())),
                destino,
            );
        }
    }
}

fn desenhar_status(f: &mut Frame, app: &mut App, area: Rect, mostrar_pip: bool) {
    // Enquanto o modelo trabalha, o Pip animado fica à esquerda do texto de
    // estado, na própria linha (ele tem uma linha de altura). Sem o `>= 1`,
    // com o Pip de 1 linha a guarda `area.height > 1` Nunca o desenhava e o
    // indicador voltava a ser o antigo "●".
    let desenhando_pip =
        mostrar_pip && area.height >= 1 && area.width > mascot::colunas(TamanhoMascote::Mini) + 20;
    let linha_texto = area.y + area.height.saturating_sub(1);
    if desenhando_pip {
        let linhas_pip = mascot::linhas(TamanhoMascote::Mini, app.pip);
        let largura_pip = mascot::colunas(TamanhoMascote::Mini);
        let destino = Rect {
            x: area.x + 1,
            y: linha_texto.saturating_sub(linhas_pip.len().saturating_sub(1) as u16),
            width: largura_pip.min(area.width.saturating_sub(2)),
            height: (linhas_pip.len() as u16).min(area.height),
        };
        f.render_widget(Paragraph::new(Text::from(linhas_pip)), destino);
    }
    let mut spans = Vec::new();
    let deslocamento_texto = if desenhando_pip {
        mascot::colunas(TamanhoMascote::Mini) + 3
    } else {
        0
    };
    if !desenhando_pip {
        let estilo_icone = match app.estado {
            EstadoMascote::Erro => theme::falha(),
            EstadoMascote::Concluindo => theme::sucesso(),
            EstadoMascote::Ocioso => theme::apagado(),
            _ => theme::em_execucao(),
        };
        spans.push(Span::styled(" ● ".to_string(), estilo_icone));
    }
    spans.push(Span::styled(
        app.estado_texto.clone(),
        if desenhando_pip {
            theme::em_execucao()
        } else {
            theme::texto()
        },
    ));
    if app.turno_ativo {
        spans.push(Span::styled(
            "   Ctrl+C ou Esc interrompe",
            theme::apagado(),
        ));
    } else if !app.saindo {
        // Enquanto encerra, o próprio estado já diz o que está acontecendo.
        spans.push(Span::styled("   enter envia · /help", theme::apagado()));
    }

    // Canto superior direito do campo de mensagem: quando o usuário se afasta
    // do fim, o espaço que antes mostrava a contagem de skills vira o atalho
    // clicável para voltar ao estado mais recente.
    let indicadores: String;
    let estilo_indicadores: Style;
    let mut area_botao: Option<Rect> = None;
    if !app.seguir_fim {
        indicadores = " ↓ voltar para o último ".to_string();
        estilo_indicadores = theme::rotulo();
        let largura = crate::util::largura_texto(&indicadores) as u16;
        area_botao = Some(Rect {
            x: area.x + area.width.saturating_sub(largura + 1),
            y: linha_texto,
            width: largura + 1,
            height: 1,
        });
    } else {
        let skills = app.skills.all().len();
        let ativos = app
            .processos
            .iter()
            .filter(|p| p.estado == EstadoProcesso::Executando)
            .count();
        let mut texto = format!("skills {skills} ");
        if ativos > 0 {
            texto = format!("skills {skills} · processos {ativos} ");
        }
        indicadores = texto;
        estilo_indicadores = theme::apagado();
    }
    let largura_indicadores = crate::util::largura_texto(&indicadores) as u16;
    let largura_esquerda: u16 = spans
        .iter()
        .map(|s| crate::util::largura_texto(&s.content) as u16)
        .sum::<u16>()
        + deslocamento_texto;
    let linha = Line::from(spans);
    let destino_texto = Rect {
        x: area.x + deslocamento_texto,
        y: linha_texto,
        width: area.width.saturating_sub(deslocamento_texto),
        height: 1,
    };
    f.render_widget(Paragraph::new(linha), destino_texto);
    if largura_esquerda + largura_indicadores + 2 < area.width {
        let destino = Rect {
            x: area.x + area.width - largura_indicadores - 1,
            y: linha_texto,
            width: largura_indicadores,
            height: 1,
        };
        f.render_widget(
            Paragraph::new(Span::styled(indicadores, estilo_indicadores)),
            destino,
        );
    }
    // A interface usa a mesma área para decidir o clique.
    app.area_voltar = area_botao;
}

fn desenhar_entrada(f: &mut Frame, app: &mut App, area: Rect) {
    // Sem título "mensagem" e sem dica de redirecionamento: o campo é
    // evidente por si e a dica só ocupava uma linha da conversa.
    let bloco = theme::bloco().border_style(theme::borda_ativa());
    let interno = bloco.inner(area);

    let (linha_cursor, coluna_cursor) = app.entrada.posicao_cursor();
    let linhas_texto: Vec<&str> = app.entrada.texto().split('\n').collect();
    let (scroll_v, scroll_h) = calcular_rolagem(
        &linhas_texto,
        linha_cursor,
        coluna_cursor,
        interno.width as usize,
        interno.height as usize,
    );

    let mut linhas_render: Vec<Line<'static>> = Vec::new();
    if app.entrada.vazio() {
        linhas_render.push(Line::from(Span::styled(
            "ex.: adicione testes para o módulo de configuração e rode cargo test",
            theme::apagado(),
        )));
    } else {
        for linha in &linhas_texto {
            let recorte: String = linha
                .chars()
                .skip(scroll_h)
                .take(interno.width as usize)
                .collect();
            linhas_render.push(Line::from(Span::styled(recorte, theme::texto())));
        }
    }
    f.render_widget(
        Paragraph::new(Text::from(linhas_render))
            .block(bloco)
            .scroll((scroll_v, 0)),
        area,
    );

    // Posiciona o cursor real do terminal.
    let x = interno.x
        + (coluna_cursor.saturating_sub(scroll_h)).min(interno.width.saturating_sub(1) as usize)
            as u16;
    let y = interno.y
        + (linha_cursor.saturating_sub(scroll_v as usize))
            .min(interno.height.saturating_sub(1) as usize) as u16;
    f.set_cursor_position((x, y));
}

fn calcular_rolagem(
    _linhas: &[&str],
    linha_cursor: usize,
    coluna_cursor: usize,
    largura: usize,
    altura: usize,
) -> (u16, usize) {
    let largura = largura.max(1);
    let altura = altura.max(1);
    let scroll_v = if linha_cursor >= altura {
        linha_cursor - altura + 1
    } else {
        0
    };
    let scroll_h = if coluna_cursor >= largura {
        coluna_cursor - largura + 1
    } else {
        0
    };
    (scroll_v as u16, scroll_h)
}

fn desenhar_sugestoes(f: &mut Frame, app: &App, area_entrada: Rect) {
    if app.modal.is_some() {
        return;
    }
    let sugestoes = app.entrada.sugestoes();
    if sugestoes.is_empty() {
        return;
    }
    let quantidade = sugestoes.len().min(7) as u16;
    let altura = quantidade + 2;
    let largura = (area_entrada.width.saturating_sub(4)).min(72);
    if area_entrada.y < altura + 1 {
        return;
    }
    let area = Rect {
        x: area_entrada.x + 2,
        y: area_entrada.y - altura,
        width: largura,
        height: altura,
    };
    f.render_widget(Clear, area);
    // A caixa tem altura fixa; a lista rola para manter o item escolhido
    // visível. Sem esta janela, com mais de ~7 comandos o marcador desaparecia
    // e a lista não acompanhava a setinha.
    let visivel = quantidade as usize;
    let total = sugestoes.len();
    let mut linhas = Vec::new();
    for (i, comando) in sugestoes.iter().enumerate() {
        let selecionado = i == app.sugestao_sel.min(total - 1);
        let estilo = if selecionado {
            Style::default()
                .fg(Color::Rgb(20, 22, 28))
                .bg(theme::borda_ativa().fg.unwrap_or(Color::Rgb(120, 180, 255)))
        } else {
            theme::texto()
        };
        let argumentos = if comando.argumentos.is_empty() {
            String::new()
        } else {
            format!(" {}", comando.argumentos)
        };
        let marcador = if selecionado { "▶" } else { " " };
        linhas.push(Line::from(vec![
            Span::styled(
                format!("{marcador} {}{} ", comando.nome, argumentos),
                estilo,
            ),
            Span::styled(
                crate::util::truncate(comando.descricao, largura.saturating_sub(2) as usize),
                if selecionado {
                    estilo
                } else {
                    theme::apagado()
                },
            ),
        ]));
    }
    // Mantém o escolhido no meio da janela quando der; nas pontas, encosta.
    let centro = visivel / 2;
    let janela = app
        .sugestao_sel
        .saturating_sub(centro)
        .min(total.saturating_sub(visivel));
    let posicao = if total > 1 {
        format!("{}/{} · ", app.sugestao_sel.min(total - 1) + 1, total)
    } else {
        String::new()
    };
    let bloco = theme::bloco()
        .border_style(theme::borda_ativa())
        .style(theme::fundo())
        .title(Span::styled(" comandos ", theme::rotulo()))
        .title_bottom(Span::styled(
            format!(" {posicao}↑↓ escolhe · tab/enter completa "),
            theme::apagado(),
        ));
    f.render_widget(
        Paragraph::new(Text::from(linhas))
            .scroll((janela as u16, 0))
            .block(bloco),
        area,
    );
}

fn desenhar_modal(f: &mut Frame, app: &mut App, area: Rect) {
    let Some(modal) = app.modal.as_ref() else {
        return;
    };
    let area_modal = retangulo_central(area, 84, 70);
    match modal {
        Modal::Confirmacao(pedido) => {
            f.render_widget(Clear, area_modal);
            let bloco = theme::bloco()
                .border_style(theme::modal_borda())
                .style(theme::fundo())
                .title(Span::styled(" confirmação necessária ", theme::titulo()));
            let interno = bloco.inner(area_modal);
            f.render_widget(bloco, area_modal);
            let linhas = vec![
                Line::default(),
                Line::from(Span::styled(
                    pedido.titulo.clone(),
                    theme::texto().add_modifier(Modifier::BOLD),
                )),
                Line::default(),
                Line::from(Span::styled(pedido.detalhe.clone(), theme::apagado())),
                Line::default(),
                Line::from(Span::styled(
                    format!("nível de permissão atual: {}", pedido.nivel.label()),
                    theme::apagado(),
                )),
                Line::default(),
                Line::from(vec![
                    Span::styled(" S ", theme::botao_permitir()),
                    Span::styled(" permitir   ", theme::texto()),
                    Span::styled(" A ", theme::botao_sempre()),
                    Span::styled(" permitir sempre nesta sessão   ", theme::texto()),
                    Span::styled(" N ", theme::botao_negar()),
                    Span::styled(" negar", theme::texto()),
                ]),
                Line::default(),
                Line::from(Span::styled(
                    "Esc nega · a decisão fica registrada na auditoria",
                    theme::apagado(),
                )),
            ];
            f.render_widget(
                Paragraph::new(Text::from(linhas)).wrap(Wrap { trim: false }),
                interno,
            );
        }
        Modal::Permissoes(seletor)
        | Modal::Modelos(seletor)
        | Modal::Esforco(seletor)
        | Modal::Sessoes(seletor)
        | Modal::Projetos(seletor)
        | Modal::Processos(seletor) => {
            desenhar_seletor(f, seletor, area_modal);
        }
        Modal::Texto {
            titulo,
            linhas,
            scroll,
        } => {
            desenhar_texto(f, titulo, linhas, *scroll, area_modal);
        }
        Modal::Ajuda { linhas, scroll } => {
            desenhar_texto(f, "ajuda", linhas, *scroll, area_modal);
        }
    }
}

fn desenhar_seletor(f: &mut Frame, seletor: &Seletor, area: Rect) {
    f.render_widget(Clear, area);
    let filtrados = seletor.filtrados();
    let total = filtrados.len();
    let posicao = if total == 0 {
        0
    } else {
        seletor.sel.min(total - 1) + 1
    };
    let rolagem = if total > 1 {
        format!("{posicao}/{total} · ")
    } else {
        String::new()
    };
    let bloco = theme::bloco()
        .border_style(theme::modal_borda())
        .style(theme::fundo())
        .title(Span::styled(
            format!(" {} ", seletor.titulo),
            theme::titulo(),
        ))
        .title_bottom(Span::styled(
            format!(
                " {rolagem}{}{} ",
                seletor.dica,
                if seletor.filtro.is_empty() {
                    String::new()
                } else {
                    format!(" · filtro: {}", seletor.filtro)
                }
            ),
            theme::apagado(),
        ));
    let interno = bloco.inner(area);
    f.render_widget(bloco, area);

    if filtrados.is_empty() {
        f.render_widget(
            Paragraph::new(Span::styled(
                "nenhum item corresponde ao filtro",
                theme::aviso(),
            )),
            interno,
        );
        return;
    }

    // Monta todas as linhas anotando a faixa (início/altura) de cada item, e
    // desloca a janela para que o item selecionado fique sempre visível — a
    // lista inteira acompanha a setinha, sem itens "escondidos" abaixo do box.
    let mut linhas: Vec<Line<'static>> = Vec::new();
    let mut faixas: Vec<(usize, usize)> = Vec::with_capacity(filtrados.len());
    for (indice, (_, item)) in filtrados.iter().enumerate() {
        let selecionado = indice == seletor.sel;
        let inicio = linhas.len();
        let marcador = if selecionado { "▶ " } else { "  " };
        let estilo_titulo = if selecionado {
            Style::default()
                .fg(Color::Rgb(255, 220, 160))
                .add_modifier(Modifier::BOLD)
        } else {
            theme::texto()
        };
        linhas.push(Line::from(Span::styled(
            format!("{marcador}{}", item.titulo),
            estilo_titulo,
        )));
        for (i, parte) in item.detalhe.lines().enumerate() {
            if i >= 2 {
                break;
            }
            linhas.push(Line::from(Span::styled(
                format!(
                    "  {}",
                    crate::util::truncate(parte, interno.width as usize * 2)
                ),
                theme::apagado(),
            )));
        }
        faixas.push((inicio, linhas.len() - inicio));
    }

    let visivel = interno.height as usize;
    let (inicio_selecionado, altura_selecionado) = faixas[seletor.sel.min(faixas.len() - 1)];
    // Mantém o item selecionado na mesma altura da janela quando possível.
    let janela = if altura_selecionado >= visivel {
        inicio_selecionado
    } else {
        let margem = (visivel - altura_selecionado) / 2;
        inicio_selecionado
            .saturating_sub(margem)
            .min(linhas.len().saturating_sub(visivel))
    };
    let janela = janela.min(linhas.len().saturating_sub(visivel));
    f.render_widget(
        Paragraph::new(Text::from(linhas))
            .scroll((janela as u16, 0))
            .style(theme::fundo()),
        interno,
    );
}

fn desenhar_texto(f: &mut Frame, titulo: &str, linhas: &[Line<'static>], scroll: u16, area: Rect) {
    f.render_widget(Clear, area);
    let bloco = theme::bloco()
        .border_style(theme::modal_borda())
        .style(theme::fundo())
        .title(Span::styled(format!(" {titulo} "), theme::titulo()))
        .title_bottom(Span::styled(" esc fecha · ↑↓ rola ", theme::apagado()));
    let interno = bloco.inner(area);
    f.render_widget(bloco, area);
    let paragrafo = Paragraph::new(Text::from(linhas.to_vec()))
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(paragrafo, interno);
}

fn retangulo_central(area: Rect, largura_pct: u16, altura_pct: u16) -> Rect {
    let largura = (area.width.saturating_sub(4))
        .min(area.width * largura_pct / 100)
        .max(30);
    let altura = (area.height.saturating_sub(2))
        .min(area.height * altura_pct / 100)
        .max(10);
    Rect {
        x: area.x + (area.width.saturating_sub(largura)) / 2,
        y: area.y + (area.height.saturating_sub(altura)) / 2,
        width: largura,
        height: altura,
    }
}

/// Estimativa de altura renderizada, considerando embrulho de linha.
pub fn contar_altura(linhas: &[Line<'static>], largura: usize) -> usize {
    let largura = largura.max(1);
    let mut total = 0usize;
    for linha in linhas {
        let texto: String = linha.spans.iter().map(|s| s.content.as_ref()).collect();
        total += quebrar_em_linhas(&texto, largura);
    }
    total.max(1)
}

fn quebrar_em_linhas(texto: &str, largura: usize) -> usize {
    if texto.is_empty() {
        return 1;
    }
    use unicode_width::UnicodeWidthStr;
    let mut linhas = 1usize;
    let mut atual = 0usize;
    for (i, palavra) in texto.split(' ').enumerate() {
        let mut restante = UnicodeWidthStr::width(palavra);
        if i > 0 {
            if atual + 1 + restante <= largura {
                atual += 1 + restante;
                continue;
            }
            linhas += 1;
        }
        if restante > largura {
            let extra = restante.div_ceil(largura) - 1;
            linhas += extra;
            restante -= extra * largura;
        }
        atual = restante;
    }
    linhas
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn altura_considera_embrulho() {
        let linha = Line::from(Span::raw("palavra ".repeat(10)));
        assert_eq!(contar_altura(std::slice::from_ref(&linha), 100), 1);
        assert!(contar_altura(std::slice::from_ref(&linha), 20) > 1);
    }

    #[test]
    fn palavra_maior_que_a_linha_quebra_em_pedacos() {
        let linha = Line::from(Span::raw("x".repeat(25)));
        assert_eq!(contar_altura(std::slice::from_ref(&linha), 10), 3);
    }

    #[test]
    fn retangulo_central_cabe_na_area() {
        let area = Rect::new(0, 0, 120, 40);
        let r = retangulo_central(area, 84, 70);
        assert!(r.width <= area.width && r.height <= area.height);
        assert!(r.x >= area.x && r.y >= area.y);
    }

    // ------------------------------------------------ interface (TestBackend)

    use crate::config::Config;
    use crate::db::Store;
    use crate::events::ProcessoView;
    use crate::project::ProjectInfo;
    use crate::skills::SkillRegistry;
    use crate::util::Paths;
    use std::sync::Arc;

    fn app_de_teste() -> App {
        let store = Store::open_in_memory().unwrap();
        store
            .create_session("s", "/p", "t", "modelo/teste", None, "semi", None)
            .unwrap();
        let (cmd_tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let mut config = Config::default();
        config.provider.model = "modelo/teste".into();
        App::novo(
            config,
            Paths::with_root("/tmp/kestrel-ui-teste"),
            store,
            cmd_tx,
            Arc::new(ProjectInfo::default()),
            Arc::new(SkillRegistry::vazio()),
        )
    }

    fn desenhar_em(app: &mut App, largura: u16, altura: u16) -> String {
        let backend = ratatui::backend::TestBackend::new(largura, altura);
        let mut terminal = ratatui::Terminal::new(backend).unwrap();
        terminal.draw(|f| desenhar(f, app)).unwrap();
        let buffer = terminal.backend().buffer();
        let linhas: Vec<String> = buffer
            .content()
            .chunks(buffer.area.width as usize)
            .map(|celulas| celulas.iter().map(|c| c.symbol()).collect())
            .collect();
        assert_eq!(linhas.len(), altura as usize);
        linhas.join("\n")
    }

    #[test]
    fn terminal_pequeno_mostra_apenas_o_aviso() {
        let mut app = app_de_teste();
        let tela = desenhar_em(&mut app, 30, 8);
        assert!(tela.contains("Terminal pequeno demais"));
        assert!(!tela.contains("Bem-vindo"));
    }

    #[test]
    fn interface_limpa_sem_painel_de_sessao() {
        let mut app = app_de_teste();
        let tela = desenhar_em(&mut app, 120, 36);
        assert!(tela.contains("Bem-vindo ao Kestrel"));
        for antigo in [
            "Plano",
            "Subtarefas",
            "Ambiente",
            "/plan detalha",
            "sem plano registrado",
            "nenhuma subtarefa",
        ] {
            assert!(
                !tela.contains(antigo),
                "painel antigo ainda aparece: {antigo}"
            );
        }
    }

    #[test]
    fn cabecalho_mostra_modelo_modo_e_permissao() {
        let mut app = app_de_teste();
        let tela = desenhar_em(&mut app, 120, 30);
        assert!(tela.contains("modelo/teste"));
        assert!(tela.contains("nvidia"));
        assert!(tela.contains("agente"));
        assert!(tela.contains("Semi"));
        // Contexto distante do limite não ocupa o cabeçalho.
        app.contexto = (100, 10_000);
        let tela = desenhar_em(&mut app, 120, 30);
        assert!(!tela.contains("contexto"));
        // Perto do limite, o aviso aparece.
        app.contexto = (9_000, 10_000);
        let tela = desenhar_em(&mut app, 120, 30);
        assert!(tela.contains("contexto"));
    }

    #[test]
    fn indicadores_inferiores_usam_dados_reais() {
        let mut app = app_de_teste();
        let tela = desenhar_em(&mut app, 120, 30);
        assert!(tela.contains("skills 0"));
        assert!(!tela.contains("processos"));
        app.processos.push(ProcessoView {
            id: "p1".into(),
            comando: "cargo test".into(),
            cwd: "/p".into(),
            estado: crate::events::EstadoProcesso::Executando,
            codigo_saida: None,
            iniciado_em: "agora".into(),
            linhas: 0,
        });
        let tela = desenhar_em(&mut app, 120, 30);
        assert!(tela.contains("skills 0 · processos 1"));
    }

    #[test]
    fn contornos_sao_arredondados() {
        let mut app = app_de_teste();
        let tela = desenhar_em(&mut app, 120, 36);
        assert!(tela.contains('╭'), "cantos superiores arredondados");
        assert!(tela.contains('╮'), "cantos superiores arredondados");
        assert!(tela.contains('╰'), "cantos inferiores arredondados");
        assert!(tela.contains('╯'), "cantos inferiores arredondados");
        for reto in ['┌', '┐', '└', '┘'] {
            assert!(!tela.contains(reto), "canto reto ainda aparece: {reto}");
        }
        // Os modais também são arredondados.
        let itens = vec![crate::app::ItemSeletor {
            titulo: "modelo de exemplo".into(),
            detalhe: "org/modelo".into(),
            valor: crate::app::ValorSeletor::Nada,
        }];
        app.modal = Some(crate::app::Modal::Modelos(crate::app::Seletor::novo(
            "Modelos", "dica", itens,
        )));
        let com_modal = desenhar_em(&mut app, 120, 36);
        assert!(com_modal.contains('╭') && com_modal.contains('╰'));
        for reto in ['┌', '┐', '└', '┘'] {
            assert!(!com_modal.contains(reto), "canto reto no modal: {reto}");
        }
    }

    #[test]
    fn pip_substitui_o_spinner_so_quando_o_modelo_trabalha() {
        let mut app = app_de_teste();
        // Parado: nenhum bloco do Pip aparece na tela (nada de gato fixo).
        let parado = desenhar_em(&mut app, 120, 36);
        assert!(!parado.contains('▀') && !parado.contains('▄'));
        assert!(parado.contains('●'));
        // Trabalhando: o Pip aparece junto ao campo de mensagem, no lugar dos
        // antigos pontinhos (nenhum glifo de braille no rodapé).
        app.tratar_evento(crate::events::AgentEvent::TurnoIniciado);
        let trabalhando = desenhar_em(&mut app, 120, 36);
        // O Pip tem 3 colunas x 1 linha: tres meias-células desenhadas.
        assert!(
            trabalhando.matches('▀').count() + trabalhando.matches('▄').count() >= 3,
            "o Pip deveria estar desenhado enquanto o modelo trabalha"
        );
        for glifo in ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'] {
            assert!(
                !trabalhando.contains(glifo),
                "o spinner antigo ainda aparece: {glifo}"
            );
        }
        // A conversa continua completa e o texto de estado fica ao lado do Pip.
        assert!(trabalhando.contains("Bem-vindo ao Kestrel"));
        assert!(trabalhando.contains("pensando"));
        // O Pip fica acima do campo de mensagem (não no canto do chat).
        let linha_pip = trabalhando
            .lines()
            .position(|l| l.contains('▀') || l.contains('▄'))
            .unwrap();
        // O campo de mensagem não tem mais título: a linha dele é a do
        // exemplo/placeholder que ocupa quando está vazio.
        let linha_mensagem = trabalhando
            .lines()
            .position(|l| l.contains("adicione testes para o módulo"))
            .unwrap();
        assert!(linha_pip < linha_mensagem);
        // Terminal baixo usa o status compacto (sem Pip) e não quebra.
        let baixo = desenhar_em(&mut app, 120, 13);
        assert!(!baixo.contains('▀') && !baixo.contains('▄'));
    }

    #[test]
    fn animacao_do_pip_muda_o_quadro_sem_quebrar_o_resto() {
        let mut app = app_de_teste();
        app.tratar_evento(crate::events::AgentEvent::Texto {
            delta: "linha da conversa".into(),
        });
        app.tratar_evento(crate::events::AgentEvent::TurnoIniciado);
        let parado = desenhar_em(&mut app, 120, 36);
        app.pip = crate::ui::mascot::AnimacaoPip {
            deslocamento: 1,
            piscando: true,
        };
        let animado = desenhar_em(&mut app, 120, 36);
        // O quadro muda (animação real) e o texto da conversa permanece.
        assert_ne!(parado, animado);
        assert!(animado.contains("linha da conversa"));
        assert!(parado.contains("linha da conversa"));
        // Mesmo quadro desenhado duas vezes é idêntico (sem tremor).
        let repetido = desenhar_em(&mut app, 120, 36);
        assert_eq!(animado, repetido);
    }

    #[test]
    fn seletor_acompanha_a_setinha_e_mostra_a_posicao() {
        let mut app = app_de_teste();
        let itens: Vec<crate::app::ItemSeletor> = (0..60)
            .map(|i| crate::app::ItemSeletor {
                titulo: format!("modelo-{i:02}"),
                detalhe: format!(
                    "org/modelo-{i:02}\ncontexto 512.0 KB · ferramentas: sim · raciocínio: sim"
                ),
                valor: crate::app::ValorSeletor::Nada,
            })
            .collect();
        let mut seletor = crate::app::Seletor::novo("Modelos", "digite para filtrar", itens);
        seletor.sel = 50;
        app.modal = Some(crate::app::Modal::Modelos(seletor));
        let tela = desenhar_em(&mut app, 100, 30);
        // O item selecionado e o marcador ficam visíveis dentro do box.
        assert!(
            tela.contains("modelo-50"),
            "o item selecionado deveria estar visível"
        );
        assert!(tela.contains('▶'));
        assert!(tela.contains("51/60"));
        let marcador = tela.lines().position(|l| l.contains('▶')).unwrap();
        let rodape = tela.lines().position(|l| l.contains("51/60")).unwrap();
        assert!(marcador < rodape, "o marcador precisa ficar dentro do box");
        // Perto do fim da lista o marcador continua visível (sem cortar itens).
        if let Some(crate::app::Modal::Modelos(seletor)) = app.modal.as_mut() {
            seletor.sel = 59;
        }
        let tela = desenhar_em(&mut app, 100, 30);
        assert!(tela.contains("modelo-59"));
        assert!(tela.contains('▶'));
        assert!(tela.contains("60/60"));
        let marcador = tela.lines().position(|l| l.contains('▶')).unwrap();
        let rodape = tela.lines().position(|l| l.contains("60/60")).unwrap();
        assert!(marcador < rodape);
    }

    #[test]
    fn redimensionamento_e_conversas_longas_nao_quebram() {
        let mut app = app_de_teste();
        for i in 0..120 {
            app.tratar_evento(crate::events::AgentEvent::Texto {
                delta: format!(
                    "linha {i} do histórico da conversa, com texto suficiente para embrulhar.\n\n"
                ),
            });
        }
        for (largura, altura) in [
            (40u16, 12u16),
            (41, 13),
            (60, 18),
            (80, 24),
            (100, 30),
            (120, 36),
            (160, 50),
            (200, 60),
        ] {
            let _tela = desenhar_em(&mut app, largura, altura);
        }
        // O usuário está no fim da conversa: a rolagem acompanha.
        assert!(app.seguir_fim);
        assert!(app.scroll > 0);
    }
}
