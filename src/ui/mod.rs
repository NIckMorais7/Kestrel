//! Interface do Kestrel (ratatui): desenho da conversa, painéis, entrada e modais.
//!
//! Todo o texto da interface está em português brasileiro. Os números de
//! alterações exibidos vêm dos diffs reais devolvidos pelas ferramentas.

pub mod input;
pub mod markdown;
pub mod mascot;
pub mod setup;
pub mod theme;

use crate::app::{App, Modal, Seletor};
use crate::events::EstadoProcesso;
use crate::ui::mascot::EstadoMascote;
use crossterm::event as ct;
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
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
    // Rodapé compacto: só uma linha. O mascote não fica mais aqui.
    let altura_status = 1;
    // Com o rodapé vazio, o campo de mensagem pode descer: três linhas de
    // respiro em telas com folga deixam a conversa mais arejada.
    let respiro = if area.height >= 20 {
        3
    } else if area.height >= 16 {
        2
    } else {
        1
    };
    // Três faixas, como o Claude Code/Hermes: cabeçalho, conversa e rodapé.
    // O campo de mensagem fica logo acima do rodapé; o respiro separa a
    // conversa da caixa de entrada sem desenhar mais uma borda.
    let [cabecalho, corpo, _respiro, area_entrada, status] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Min(5),
        Constraint::Length(respiro),
        Constraint::Length(altura_entrada),
        Constraint::Length(altura_status),
    ])
    .areas(area);

    // Durante o turno, uma linha do chat vira linha de progresso: a estrela
    // branca aparece ali, acima do campo, sem cobrir o texto da conversa.
    let (corpo, linha_progresso) = if app.turno_ativo && corpo.height >= 3 {
        let [conversa, linha] =
            Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(corpo);
        (conversa, Some(linha))
    } else {
        (corpo, None)
    };

    desenhar_cabecalho(f, app, cabecalho);
    desenhar_conversa(f, app, corpo);
    desenhar_progresso_estrela(f, app, linha_progresso);
    desenhar_entrada(f, app, area_entrada);
    desenhar_sugestoes(f, app, area_entrada);
    desenhar_status(f, app, status);
    desenhar_modal(f, app, area);
}

/// Cabeçalho em duas linhas, sem caixa e sem provedor:
///
/// ```text
///  ◆ Kestrel v0.5.0   modelo/teste · high
///  ~/Projetos/Kestrel (master) · build · Semi
/// ```
fn desenhar_cabecalho(f: &mut Frame, app: &App, area: Rect) {
    let mut titulo = vec![
        Span::styled(" ◆ ", theme::titulo()),
        Span::styled("Kestrel", theme::titulo()),
        Span::styled(format!(" v{}", env!("CARGO_PKG_VERSION")), theme::apagado()),
        Span::styled("   ", theme::apagado()),
        Span::styled(crate::util::truncate(&app.modelo, 42), theme::texto()),
    ];
    if let Some(esforco) = &app.esforco {
        titulo.push(Span::styled(format!(" · {esforco}"), theme::apagado()));
    }
    // Provedor não fica no cabeçalho: `nvidia` é detalhe de configuração e
    // poluía o topo. Ele continua disponível em `/config` e `/model`.

    let projeto = crate::util::caminho_curto(&app.config.workdir(), 52);
    let mut contexto = vec![Span::styled(format!(" {projeto}"), theme::apagado())];
    if let Some(branch) = &app.projeto.git_branch {
        contexto.push(Span::styled(format!(" ({branch})"), theme::apagado()));
    }
    contexto.push(Span::styled(" · modo ", theme::apagado()));
    contexto.push(Span::styled(
        app.modo.label().to_string(),
        if app.modo == crate::config::ModoOperacao::Chat {
            theme::apagado()
        } else {
            theme::acento()
        },
    ));
    contexto.push(Span::styled(" · permissão ", theme::apagado()));
    contexto.push(Span::styled(
        app.nivel.label().to_string(),
        Style::default().fg(theme::cor_nivel(app.nivel)),
    ));
    if let Some(comp) = &app.config.ui.active_comp {
        contexto.push(Span::styled(" · comp ", theme::apagado()));
        contexto.push(Span::styled(comp.clone(), theme::titulo()));
    }

    f.render_widget(
        Paragraph::new(Text::from(vec![Line::from(titulo), Line::from(contexto)])),
        area,
    );
}

fn desenhar_conversa(f: &mut Frame, app: &mut App, area: Rect) {
    // A conversa não tem caixa: espaço vazio separa melhor que mais uma borda.
    // O padding de 2 células alinha o texto com o cabeçalho e o rodapé.
    let interno = Rect {
        x: area.x + 2,
        y: area.y,
        width: area.width.saturating_sub(4).max(1),
        height: area.height,
    };
    let largura = interno.width.max(1) as usize;

    // Estado vazio: recepção de nova sessão, no estilo Claude Code. O mascote
    // aparece aqui (e em nenhum outro lugar da interface), com instruções que
    // somem junto com ele na primeira mensagem.
    if app.blocos.is_empty() {
        let com_pip = app.config.ui.show_mascot && app.pip_grafico.is_some();
        let texto_area = if com_pip && interno.width > mascot::COLUNAS + 4 {
            Rect {
                x: interno.x + mascot::COLUNAS + 2,
                y: interno.y,
                width: interno.width.saturating_sub(mascot::COLUNAS + 2),
                height: interno.height,
            }
        } else {
            interno
        };
        let linhas: Vec<Line<'static>> = vec![
            Line::default(),
            Line::from(vec![
                Span::styled("◆ ", theme::titulo()),
                Span::styled("Kestrel", theme::titulo()),
                Span::styled(
                    format!("  v{}", env!("CARGO_PKG_VERSION")),
                    theme::apagado(),
                ),
            ]),
            Line::default(),
            Line::from(Span::styled(
                "Nova sessão. Descreva a tarefa em linguagem natural.",
                theme::texto(),
            )),
            Line::default(),
            Line::from(vec![
                Span::styled("/help", theme::rotulo()),
                Span::styled("  comandos", theme::apagado()),
                Span::styled("     /model", theme::rotulo()),
                Span::styled("  troca o modelo", theme::apagado()),
                Span::styled("     /person", theme::rotulo()),
                Span::styled("  personalidades", theme::apagado()),
            ]),
            Line::from(vec![
                Span::styled("/agent", theme::rotulo()),
                Span::styled("  modo build", theme::apagado()),
                Span::styled("   /chat", theme::rotulo()),
                Span::styled("  modo conversa", theme::apagado()),
                Span::styled("   /skin", theme::rotulo()),
                Span::styled("  troca a aparência", theme::apagado()),
            ]),
        ];
        f.render_widget(
            Paragraph::new(Text::from(linhas)).wrap(Wrap { trim: false }),
            texto_area,
        );
        if com_pip && interno.height >= mascot::LINHAS {
            // Por último: a imagem ocupa 6×2 e não é coberta pelo texto.
            if let Some(pip) = &app.pip_grafico {
                pip.desenhar(
                    f,
                    Rect {
                        x: interno.x,
                        y: interno.y,
                        width: mascot::COLUNAS.min(interno.width),
                        height: mascot::LINHAS.min(interno.height),
                    },
                    app.pip,
                );
            }
        }
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
        .wrap(Wrap { trim: false })
        .scroll((scroll, 0));
    f.render_widget(paragrafo, interno);

    // Indicador de rolagem quando o usuário saiu do fim.
    if !app.seguir_fim && max_scroll > 0 {
        let indicador = format!(" ↑ {scroll}/{max_scroll} ");
        let largura_ind = unicode_width::UnicodeWidthStr::width(indicador.as_str()) as u16;
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

/// Fases da estrela: pequena → grande → pequena. O ciclo é rápido o bastante
/// para ficar fluido na cadência de 80 ms, sem piscar.
fn estrela_progresso(tique: u64) -> &'static str {
    const FASES: [&str; 10] = ["✧", "✶", "✦", "✪", "✰", "★", "✰", "✪", "✦", "✶"];
    let fase = ((tique / 2) % FASES.len() as u64) as usize;
    FASES[fase]
}

fn desenhar_progresso_estrela(f: &mut Frame, app: &App, area: Option<Rect>) {
    let Some(area) = area else { return };
    if !app.turno_ativo || area.height == 0 || area.width < 4 {
        return;
    }
    let linha = Line::from(vec![
        Span::styled(estrela_progresso(app.tick), theme::acento()),
        Span::styled(format!("  {}", app.estado_texto), theme::apagado()),
    ]);
    f.render_widget(
        Paragraph::new(linha),
        Rect {
            x: area.x + 2,
            y: area.y,
            width: area.width.saturating_sub(2),
            height: 1,
        },
    );
}

fn desenhar_status(f: &mut Frame, app: &mut App, area: Rect) {
    // Rodapé compacto no estilo Claude Code: estado à esquerda, atalhos à
    // direita. O mascote saiu daqui; o progresso agora acontece no chat.
    let linha_texto = area.y + area.height.saturating_sub(1);
    let deslocamento_texto = 1;

    let estilo_estado = match app.estado {
        EstadoMascote::Erro => theme::falha(),
        EstadoMascote::Concluindo => theme::sucesso(),
        EstadoMascote::Ocioso => theme::apagado(),
        _ => theme::em_execucao(),
    };
    let mut spans = Vec::new();
    // O marcador pulsa enquanto há trabalho; parado, fica no acento fixo.
    spans.push(Span::styled(
        "● ".to_string(),
        if app.turno_ativo {
            theme::pulso(app.tick, true)
        } else {
            estilo_estado
        },
    ));
    spans.push(Span::styled(app.estado_texto.clone(), estilo_estado));
    if app.token_total.0 + app.token_total.1 > 0 {
        let total = app.token_total.0 + app.token_total.1;
        spans.push(Span::styled(
            format!(" · {} tok", crate::util::formatar_num(total)),
            theme::apagado(),
        ));
    }
    if app.contexto.1 > 0 {
        let fracao = app.contexto.0 as f32 / app.contexto.1.max(1) as f32;
        if fracao >= 0.7 {
            spans.push(Span::styled(" · contexto ", theme::apagado()));
            spans.push(Span::styled(
                format!("{:.0}%", fracao * 100.0),
                Style::default().fg(theme::cor_contexto(fracao)),
            ));
        }
    }
    if app.mostrar_raciocinio {
        spans.push(Span::styled(" · thinking ", theme::apagado()));
        spans.push(Span::styled("on", theme::acento()));
    }
    let ativos = app
        .processos
        .iter()
        .filter(|p| p.estado == EstadoProcesso::Executando)
        .count();
    // Skills saíram do rodapé: para conferir skills existe `/skills`. Só um
    // aviso de processo ativo continua, porque indica trabalho real em curso.
    if ativos > 0 {
        spans.push(Span::styled(
            format!(" · processos {ativos}"),
            theme::apagado(),
        ));
    }

    // Atalhos à direita. Quando o usuário saiu do fim, o único atalho é o
    // clique para voltar — o mesmo comportamento de antes.
    let indicadores: String;
    let estilo_indicadores: Style;
    let mut area_botao: Option<Rect> = None;
    if !app.seguir_fim {
        indicadores = " ↓ voltar para o último ".to_string();
        estilo_indicadores = theme::titulo();
        let largura = crate::util::largura_texto(&indicadores) as u16;
        area_botao = Some(Rect {
            x: area.x + area.width.saturating_sub(largura + 1),
            y: linha_texto,
            width: largura + 1,
            height: 1,
        });
    } else if app.turno_ativo {
        indicadores = " Ctrl+C interrompe ".to_string();
        estilo_indicadores = theme::apagado();
    } else {
        // Sem “enter envia”: o campo já é evidente. O canto fica vazio.
        indicadores = String::new();
        estilo_indicadores = theme::apagado();
    }
    let largura_indicadores = crate::util::largura_texto(&indicadores) as u16;
    let largura_esquerda: u16 = spans
        .iter()
        .map(|s| crate::util::largura_texto(&s.content) as u16)
        .sum::<u16>()
        + deslocamento_texto;
    let destino_texto = Rect {
        x: area.x + deslocamento_texto,
        y: linha_texto,
        width: area.width.saturating_sub(deslocamento_texto),
        height: 1,
    };
    f.render_widget(Paragraph::new(Line::from(spans)), destino_texto);
    if !indicadores.is_empty() && largura_esquerda + largura_indicadores + 2 < area.width {
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
    // Campo de mensagem no estilo Claude Code: caixa arredondada, superfície
    // levemente elevada e o glifo `❯` no acento. O texto começa duas células
    // depois da borda: uma para o glifo e uma de padding.
    let bloco = theme::bloco()
        .border_style(theme::borda_ativa())
        .style(theme::superficie());
    let interno = bloco.inner(area);
    f.render_widget(bloco, area);
    if interno.width < 3 || interno.height == 0 {
        return;
    }

    let texto_area = Rect {
        x: interno.x + 2,
        y: interno.y,
        width: interno.width.saturating_sub(2),
        height: interno.height,
    };
    let (linha_cursor, coluna_cursor) = app.entrada.posicao_cursor();
    let linhas_texto: Vec<&str> = app.entrada.texto().split('\n').collect();
    let (scroll_v, scroll_h) = calcular_rolagem(
        &linhas_texto,
        linha_cursor,
        coluna_cursor,
        texto_area.width as usize,
        texto_area.height as usize,
    );

    let mut linhas_render: Vec<Line<'static>> = Vec::new();
    if app.entrada.vazio() {
        linhas_render.push(Line::from(Span::styled(
            "descreva a tarefa em linguagem natural…",
            theme::apagado(),
        )));
    } else {
        for linha in &linhas_texto {
            let recorte: String = linha
                .chars()
                .skip(scroll_h)
                .take(texto_area.width as usize)
                .collect();
            linhas_render.push(Line::from(Span::styled(recorte, theme::texto())));
        }
    }
    f.render_widget(
        Paragraph::new(Text::from(linhas_render)).scroll((scroll_v, 0)),
        texto_area,
    );
    let estilo_prompt = if app.turno_ativo {
        theme::pulso(app.tick, true).add_modifier(Modifier::BOLD)
    } else {
        theme::acento().add_modifier(Modifier::BOLD)
    };
    f.render_widget(
        Paragraph::new(Span::styled("❯", estilo_prompt)),
        Rect {
            x: interno.x,
            y: interno.y,
            width: 1,
            height: 1,
        },
    );

    // Posiciona o cursor real do terminal.
    let x = texto_area.x
        + (coluna_cursor.saturating_sub(scroll_h)).min(texto_area.width.saturating_sub(1) as usize)
            as u16;
    let y = texto_area.y
        + (linha_cursor.saturating_sub(scroll_v as usize))
            .min(texto_area.height.saturating_sub(1) as usize) as u16;
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
                .fg(theme::selecao_texto())
                .bg(theme::selecao_fundo())
        } else {
            theme::texto()
        };
        let argumentos = if comando.argumentos.is_empty() {
            String::new()
        } else {
            format!(" {}", comando.argumentos)
        };
        let marcador = if selecionado { "❯" } else { " " };
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
        .style(theme::superficie())
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
                .style(theme::superficie())
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
            desenhar_seletor(f, seletor, area_modal, app.tick);
        }
        Modal::CompEditor {
            nome,
            novo,
            editor,
            erro,
            scroll,
            ..
        } => {
            desenhar_comp_editor(
                f,
                nome,
                *novo,
                editor,
                erro.as_deref(),
                *scroll,
                retangulo_central(area, 90, 80),
            );
        }
        Modal::ConfirmarComp { nome } => {
            desenhar_confirmar_comp(f, nome, retangulo_central(area, 54, 32));
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

/// Editor de instruções de um Comp, dentro do próprio Kestrel.
fn desenhar_comp_editor(
    f: &mut Frame,
    nome: &str,
    novo: bool,
    editor: &crate::ui::input::Editor,
    erro: Option<&str>,
    scroll: u16,
    area: Rect,
) {
    f.render_widget(Clear, area);
    let titulo = if novo {
        format!(" novo Comp — {nome} ")
    } else {
        format!(" Comp — {nome} ")
    };
    let bloco = theme::bloco()
        .border_style(theme::modal_borda())
        .style(theme::superficie())
        .title(Span::styled(titulo, theme::titulo()))
        .title_bottom(Span::styled(
            " Ctrl+S salva · Esc cancela · Enter quebra linha ",
            theme::apagado(),
        ));
    let interno = bloco.inner(area);
    f.render_widget(bloco, area);
    if interno.height == 0 {
        return;
    }

    let altura_erro = if erro.is_some() { 2 } else { 0 };
    let altura_texto = interno.height.saturating_sub(altura_erro).max(1);
    let largura = interno.width.max(1) as usize;
    let (linha_cursor, coluna_cursor) = editor.posicao_cursor();

    // Mantém o cursor visível mesmo quando o texto passa da altura do modal.
    let rolagem = if linha_cursor < scroll as usize {
        linha_cursor as u16
    } else if linha_cursor >= scroll as usize + altura_texto as usize {
        (linha_cursor + 1 - altura_texto as usize) as u16
    } else {
        scroll
    };
    let mut linhas = Vec::new();
    if editor.vazio() {
        linhas.push(Line::from(Span::styled(
            "descreva como o agente deve falar e se comportar…",
            theme::apagado(),
        )));
    } else {
        for linha in editor.texto().split('\n') {
            let recorte: String = linha.chars().take(largura.saturating_sub(1)).collect();
            linhas.push(Line::from(Span::styled(recorte, theme::texto())));
        }
    }
    f.render_widget(
        Paragraph::new(Text::from(linhas)).scroll((rolagem, 0)),
        Rect {
            x: interno.x,
            y: interno.y,
            width: interno.width,
            height: altura_texto,
        },
    );

    if let Some(mensagem) = erro {
        let y = interno.y + interno.height.saturating_sub(2);
        f.render_widget(
            Paragraph::new(Span::styled(
                format!("erro: {}", crate::util::truncate(mensagem, largura)),
                theme::falha(),
            )),
            Rect {
                x: interno.x,
                y,
                width: interno.width,
                height: 1,
            },
        );
        f.render_widget(
            Paragraph::new(Span::styled(
                "corrija o texto e tente Ctrl+S de novo",
                theme::apagado(),
            )),
            Rect {
                x: interno.x,
                y: y + 1,
                width: interno.width,
                height: 1,
            },
        );
    }

    let x = interno.x + (coluna_cursor).min(largura.saturating_sub(1)) as u16;
    let y = interno.y
        + (linha_cursor.saturating_sub(rolagem as usize))
            .min(altura_texto.saturating_sub(1) as usize) as u16;
    f.set_cursor_position((x, y));
}

/// Confirmação de remoção de Comp: separada do fluxo de ferramentas porque a
/// decisão é do usuário na interface, não uma permissão de execução.
fn desenhar_confirmar_comp(f: &mut Frame, nome: &str, area: Rect) {
    f.render_widget(Clear, area);
    let bloco = theme::bloco()
        .border_style(theme::modal_borda())
        .style(theme::superficie())
        .title(Span::styled(" remover Comp ", theme::titulo()));
    let interno = bloco.inner(area);
    f.render_widget(bloco, area);
    let linhas = vec![
        Line::default(),
        Line::from(vec![
            Span::styled("Remover o Comp ", theme::texto()),
            Span::styled(nome.to_string(), theme::titulo()),
            Span::styled("?", theme::texto()),
        ]),
        Line::default(),
        Line::from(Span::styled(
            "O arquivo será apagado. Se este for o Comp ativo, o tom padrão volta.",
            theme::apagado(),
        )),
        Line::default(),
        Line::from(vec![
            Span::styled(" S ", theme::botao_negar()),
            Span::styled(" remover   ", theme::texto()),
            Span::styled(" N ", theme::botao_permitir()),
            Span::styled(" cancelar", theme::texto()),
        ]),
    ];
    f.render_widget(
        Paragraph::new(Text::from(linhas)).wrap(Wrap { trim: false }),
        interno,
    );
}

fn desenhar_seletor(f: &mut Frame, seletor: &Seletor, area: Rect, tique: u64) {
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
        .style(theme::superficie())
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
        let estilo_titulo = if selecionado {
            theme::pulso(tique, true).add_modifier(Modifier::BOLD)
        } else {
            theme::texto()
        };
        // O glifo pula levemente entre dois estados (sem mover texto) para o
        // item selecionado ser perceptível sem piscar a linha inteira.
        let marcador = if selecionado {
            if (tique / 2) % 4 < 2 { "▶ " } else { "▮ " }
        } else {
            "  "
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
            .style(theme::superficie()),
        interno,
    );
}

fn desenhar_texto(f: &mut Frame, titulo: &str, linhas: &[Line<'static>], scroll: u16, area: Rect) {
    f.render_widget(Clear, area);
    let bloco = theme::bloco()
        .border_style(theme::modal_borda())
        .style(theme::superficie())
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
        assert!(tela.contains("Nova sessão"));
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
        // O provedor não é informação de cabeçalho: saiu do topo a pedido do
        // usuário e continua disponível em /config.
        assert!(!tela.contains("nvidia"));
        assert!(tela.contains("build"));
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
        assert!(!tela.contains("skills"));
        assert!(!tela.contains("enter envia"));
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
        assert!(tela.contains("processos 1"));
        assert!(!tela.contains("skills"));
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
    fn pip_e_instrucoes_somem_depois_da_primeira_mensagem() {
        let mut app = app_de_teste();
        // Sem Pip configurado, nada de meias-células na tela.
        let parado = desenhar_em(&mut app, 120, 36);
        assert!(!parado.contains('▀') && !parado.contains('▄'));

        app.pip_grafico = Some(
            crate::ui::mascot::PipGrafico::novo(&ratatui_image::picker::Picker::halfblocks())
                .expect("Pip de teste"),
        );
        app.tick = 10; // fase grande da estrela de progresso.
        app.tratar_evento(crate::events::AgentEvent::TurnoIniciado);
        let nova = desenhar_em(&mut app, 120, 36);
        // Nova sessão: mascote + instruções + progresso com estrela no chat.
        assert!(nova.contains("Nova sessão"));
        assert!(nova.contains("/help"));
        assert!(nova.contains("★"), "estrela de progresso deveria aparecer");
        assert!(
            nova.matches('▀').count() + nova.matches('▄').count() >= 6,
            "o Pip deveria aparecer na recepção de nova sessão"
        );

        // Depois da primeira mensagem, mascote e instruções saem juntos.
        app.tratar_evento(crate::events::AgentEvent::Texto {
            delta: "linha da conversa".into(),
        });
        let depois = desenhar_em(&mut app, 120, 36);
        assert!(depois.contains("linha da conversa"));
        assert!(!depois.contains("Nova sessão"));
        assert!(!depois.contains("▀") && !depois.contains("▄"));
        // O progresso continua, mas agora é só a estrela no chat.
        assert!(depois.contains("★"));
    }

    #[test]
    fn animacao_do_pip_muda_o_quadro_sem_quebrar_o_resto() {
        let mut app = app_de_teste();
        app.tratar_evento(crate::events::AgentEvent::Texto {
            delta: "linha da conversa".into(),
        });
        app.tratar_evento(crate::events::AgentEvent::TurnoIniciado);
        app.pip_grafico = Some(
            crate::ui::mascot::PipGrafico::novo(&ratatui_image::picker::Picker::halfblocks())
                .expect("Pip de teste"),
        );
        let parado = desenhar_em(&mut app, 120, 36);
        app.pip = crate::ui::mascot::AnimacaoPip {
            dx: 1,
            dy: 0,
            piscando: true,
        };
        let animado = desenhar_em(&mut app, 120, 36);
        // O desenho não quebra a conversa. A mudança real de quadro é testada
        // em `mascot`/`app`; no TestBackend o protocolo de halfblocks expõe os
        // mesmos glifos e varia apenas nas cores, que não entram no snapshot.
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
