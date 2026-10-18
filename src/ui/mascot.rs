//! Mascote oficial do Kestrel: o Pip.
//!
//! O Pip é renderizado a partir dos pixels da imagem original
//! (`/home/nick/Assets/Pip/pip.png`, cópia fiel em `assets/pip.png`; a textura
//! em [`crate::ui::pip_textura`] é derivada dela pelo script
//! `scripts/gerar_textura_pip.py`). Nenhum desenho é refeito à mão: cada texel
//! vem da imagem, reamostrado por área.
//!
//! A arte original é geométrica: uma massa branca com a tinta (contorno e
//! olhos) vazada como transparência. No terminal os furos deixam aparecer o
//! fundo quase preto do aplicativo — ou seja, a própria tinta do desenho.
//!
//! Estratégia: cada célula do terminal mostra dois texels verticais com o
//! caractere `▀` (cor de frente = texel de cima, cor de fundo = texel de baixo).
//! Isso preserva a proporção do desenho em terminais comuns (célula ~1:2) e não
//! depende de protocolos gráficos (sixel/kitty/iterm), que não são assumidos.
//! Terminais sem cor verdadeira aproximam a paleta.
//!
//! Na interface o Pip aparece como indicador de trabalho junto ao campo de
//! mensagem e no assistente de configuração, sempre no tamanho
//! [`TamanhoMascote::Mini`] — exatamente duas linhas do terminal, o menor
//! tamanho em que o rosto e as patinhas do desenho original continuam
//! reconhecíveis. Nada de versão grande: o mascote é discreto por decisão de
//! projeto.
//!
//! A animação é manual e determinística, com apenas dois movimentos:
//! respiração (deslocamento vertical de um texel, sem mudar a altura ocupada)
//! e piscada (máscara derivada dos olhos localizados na imagem original).
//! Nenhuma deformação do rosto é aplicada: a piscada apenas cobre os furos dos
//! olhos com a cor do pelo e a respiração apenas desloca o desenho.

use crate::ui::pip_textura as tex;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Estados de atividade exibidos no indicador inferior.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EstadoMascote {
    Ocioso,
    Pensando,
    Trabalhando,
    Ferramentas,
    Concluindo,
    Erro,
}

impl EstadoMascote {
    pub fn label(&self) -> &'static str {
        match self {
            EstadoMascote::Ocioso => "ocioso",
            EstadoMascote::Pensando => "pensando",
            EstadoMascote::Trabalhando => "trabalhando",
            EstadoMascote::Ferramentas => "executando ferramenta",
            EstadoMascote::Concluindo => "concluindo",
            EstadoMascote::Erro => "erro",
        }
    }
}

/// Resolução da textura usada na interface: uma só, de duas linhas.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TamanhoMascote {
    /// Indicador de trabalho exibido junto ao campo de mensagem enquanto o
    /// modelo pensa, executa ferramentas ou redige a resposta — e também o
    /// mascote do assistente de configuração. Ocupa exatamente uma linha do
    /// terminal: a metade do tamanho anterior (que já era a metade de outro),
    /// o menor tamanho em que o desenho original continua reconhecível.
    Mini,
}

impl TamanhoMascote {
    fn largura(self) -> usize {
        tex::LARGURA_MINI
    }

    fn altura(self) -> usize {
        tex::ALTURA_MINI
    }

    fn pixels(self) -> &'static [u32] {
        &tex::PIXELS_MINI
    }

    fn olhos(self) -> &'static [tex::OlhoTexel; 2] {
        &tex::OLHOS_MINI
    }
}

/// Quantas colunas do terminal o mascote ocupa.
pub fn colunas(tamanho: TamanhoMascote) -> u16 {
    tamanho.largura() as u16
}

/// Quantas linhas do terminal o mascote ocupa.
pub fn linhas_ocupadas(tamanho: TamanhoMascote) -> u16 {
    // Cada célula cobre dois texels. A respiração desloca o desenho um texel
    // para baixo, mas nunca muda a altura da caixa: a última fileira de texels
    // fica coberta enquanto o ar está inspirado.
    tamanho.altura().div_ceil(2) as u16
}

/// Estado da animação em um quadro.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnimacaoPip {
    /// 0 = posição de repouso; 1 = inspirado (um texel abaixo).
    pub deslocamento: u8,
    /// Olhos fechados neste quadro.
    pub piscando: bool,
}

/// Deslocamento da respiração para um tique: ciclo lento de 16 quadros
/// (4s na cadência ociosa), com subida e retorno suaves.
pub fn deslocamento_de(tique: u64) -> u8 {
    u8::from((4..12).contains(&(tique % 16)))
}

/// Converte um pixel da textura em cor do terminal.
fn cor(pixel: u32) -> Option<Color> {
    let alfa = pixel & 0xFF;
    if alfa < 100 {
        return None;
    }
    Some(Color::Rgb(
        ((pixel >> 24) & 0xFF) as u8,
        ((pixel >> 16) & 0xFF) as u8,
        ((pixel >> 8) & 0xFF) as u8,
    ))
}

/// Cor de um texel já considerando a piscada (máscara sobre os olhos).
fn texel(tamanho: TamanhoMascote, x: usize, y: usize, piscando: bool) -> Option<Color> {
    if piscando {
        for olho in tamanho.olhos() {
            if (olho.x0..=olho.x1).contains(&x) && (olho.y0..=olho.y1).contains(&y) {
                let meio = (olho.y0 + olho.y1) / 2;
                let cor_olho = if y == meio {
                    olho.palpebra
                } else {
                    olho.pelagem
                };
                return cor(cor_olho);
            }
        }
    }
    cor(tamanho.pixels()[y * tamanho.largura() + x])
}

/// Renderiza o Pip em linhas de texto prontas para o `Paragraph`.
///
/// O desenho fica alinhado à esquerda na área informada; as células
/// transparentes são preenchidas com espaços para não deslocar o sprite. A
/// respiração desce o desenho um texel; como a caixa nunca muda de altura, a
/// última fileira de texels do sprite fica oculta enquanto o ar está inspirado.
pub fn linhas(tamanho: TamanhoMascote, animacao: AnimacaoPip) -> Vec<Line<'static>> {
    let largura = tamanho.largura();
    let altura = tamanho.altura() as isize;
    let deslocamento = animacao.deslocamento.min(1) as isize;
    let total_celulas = linhas_ocupadas(tamanho) as usize;
    let mut saida = Vec::with_capacity(total_celulas);

    for celula in 0..total_celulas {
        let superior = (2 * celula) as isize - deslocamento;
        let inferior = superior + 1;
        let mut spans: Vec<Span<'static>> = Vec::with_capacity(largura);
        for x in 0..largura {
            let em_cima = if (0..altura).contains(&superior) {
                texel(tamanho, x, superior as usize, animacao.piscando)
            } else {
                None
            };
            let embaixo = if (0..altura).contains(&inferior) {
                texel(tamanho, x, inferior as usize, animacao.piscando)
            } else {
                None
            };
            let span = match (em_cima, embaixo) {
                (Some(cima), Some(baixo)) => Span::styled("▀", Style::default().fg(cima).bg(baixo)),
                (Some(cima), None) => Span::styled("▀", Style::default().fg(cima)),
                (None, Some(baixo)) => Span::styled("▄", Style::default().fg(baixo)),
                (None, None) => Span::raw(" "),
            };
            spans.push(span);
        }
        // Remove espaços transparentes à direita para não pintar a coluna.
        while spans.last().is_some_and(|s| s.content == " ") {
            spans.pop();
        }
        saida.push(Line::from(spans));
    }
    saida
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn textura_tem_o_tamanho_declarado() {
        assert_eq!(tex::PIXELS_MINI.len(), tex::LARGURA_MINI * tex::ALTURA_MINI);
    }

    #[test]
    fn olhos_ficam_dentro_da_textura() {
        for tamanho in [TamanhoMascote::Mini] {
            for olho in tamanho.olhos() {
                assert!(olho.x1 < tamanho.largura());
                assert!(olho.y1 < tamanho.altura());
                assert!(olho.x0 <= olho.x1 && olho.y0 <= olho.y1);
            }
        }
    }

    #[test]
    fn piscada_troca_os_texels_dos_olhos() {
        let tamanho = TamanhoMascote::Mini;
        let olho = tex::OLHOS_MINI[0];
        let centro_x = olho.x0;
        let centro_y = olho.y0;
        let aberto = texel(tamanho, centro_x, centro_y, false);
        let fechado = texel(tamanho, centro_x, centro_y, true);
        assert_ne!(aberto, fechado);
        assert_eq!(fechado, cor(olho.pelagem));
        let meio = (olho.y0 + olho.y1) / 2;
        assert_eq!(texel(tamanho, centro_x, meio, true), cor(olho.palpebra));
    }

    #[test]
    fn mini_tem_os_olhos_vazados_e_ocupa_uma_linha() {
        let desenhado = linhas(TamanhoMascote::Mini, AnimacaoPip::default());
        // Uma linha: o Pip foi reduzido à metade (era 6x4 texels = 2 linhas).
        assert_eq!(desenhado.len(), 1);
        for olho in tex::OLHOS_MINI {
            // O olho é um furo na arte: em repouso o texel não tem cor própria
            // (aparece o fundo escuro do aplicativo) e na piscada ele desaparece
            // no branco do pelo.
            assert_eq!(texel(TamanhoMascote::Mini, olho.x0, olho.y0, false), None);
            assert_eq!(
                texel(TamanhoMascote::Mini, olho.x0, olho.y0, true),
                cor(olho.pelagem)
            );
        }
    }

    /// Reconstrói as fileiras de texels da caixa desenhada: cada linha do
    /// terminal vira dois texels verticais por coluna (`None` = vazio). Colunas
    /// removidas à direita são recompostas para comparar fileiras inteiras.
    fn caixa(ls: &[Line<'static>], largura: usize) -> Vec<Vec<Option<Color>>> {
        let mut saida = Vec::new();
        for linha in ls {
            let mut cima: Vec<Option<Color>> = Vec::new();
            let mut baixo: Vec<Option<Color>> = Vec::new();
            for span in &linha.spans {
                match span.content.as_ref() {
                    "▀" => {
                        cima.push(Some(span.style.fg.expect("cor do texel de cima")));
                        baixo.push(span.style.bg);
                    }
                    "▄" => {
                        cima.push(None);
                        baixo.push(Some(span.style.fg.expect("cor do texel de baixo")));
                    }
                    " " => {
                        cima.push(None);
                        baixo.push(None);
                    }
                    outro => panic!("glifo inesperado no mascote: {outro:?}"),
                }
            }
            while cima.len() < largura {
                cima.push(None);
                baixo.push(None);
            }
            saida.push(cima);
            saida.push(baixo);
        }
        saida
    }

    #[test]
    fn respiracao_desloca_a_imagem_um_texel_sem_deformar() {
        for tamanho in [TamanhoMascote::Mini] {
            let largura = tamanho.largura();
            let parado = caixa(
                &linhas(
                    tamanho,
                    AnimacaoPip {
                        deslocamento: 0,
                        piscando: false,
                    },
                ),
                largura,
            );
            let inspirado = caixa(
                &linhas(
                    tamanho,
                    AnimacaoPip {
                        deslocamento: 1,
                        piscando: false,
                    },
                ),
                largura,
            );
            // A caixa nunca muda de altura entre os dois quadros.
            assert_eq!(inspirado.len(), parado.len());
            // O quadro inspirado é o de repouso um texel abaixo, sem deformar:
            // cada fileira repete exatamente a anterior.
            let total = parado.len();
            for y in 0..total - 1 {
                assert_eq!(inspirado[y + 1], parado[y], "texel fora do lugar: y={y}");
            }
            // A primeira fileira abre espaço e a última sai da caixa.
            assert!(inspirado[0].iter().all(Option::is_none));
            assert_ne!(parado[0], inspirado[0]);
        }
    }

    #[test]
    fn animacao_e_deterministica_e_ciclica() {
        assert_eq!(deslocamento_de(0), 0);
        assert_eq!(deslocamento_de(5), 1);
        assert_eq!(deslocamento_de(16), deslocamento_de(0));
        let a = linhas(TamanhoMascote::Mini, AnimacaoPip::default());
        let b = linhas(TamanhoMascote::Mini, AnimacaoPip::default());
        assert_eq!(a.len(), b.len());
        for (l1, l2) in a.iter().zip(b.iter()) {
            let t1: String = l1.spans.iter().map(|s| s.content.as_ref()).collect();
            let t2: String = l2.spans.iter().map(|s| s.content.as_ref()).collect();
            assert_eq!(t1, t2);
        }
    }

    #[test]
    fn altura_em_linhas_nao_muda_com_a_respiracao() {
        // Uma linha, sempre: o Pip não pode crescer nem encolher.
        assert_eq!(linhas_ocupadas(TamanhoMascote::Mini), 1);
        for tamanho in [TamanhoMascote::Mini] {
            for deslocamento in [0, 1] {
                let desenhado = linhas(
                    tamanho,
                    AnimacaoPip {
                        deslocamento,
                        piscando: true,
                    },
                );
                assert_eq!(desenhado.len(), linhas_ocupadas(tamanho) as usize);
            }
        }
    }

    #[test]
    fn mascote_nunca_passou_de_duas_linhas() {
        // Regressão: o Pip já foi desenhado com 17 linhas (28x34 texels) como
        // "companheiro da conversa", com 7 linhas no assistente e com 2 linhas
        // no indicador. Agora ele cabe em uma única linha, em qualquer uso.
        assert_eq!(tex::LARGURA_MINI, 3);
        assert_eq!(tex::ALTURA_MINI, 2);
        assert_eq!(
            linhas(TamanhoMascote::Mini, AnimacaoPip::default()).len(),
            1
        );
    }
}
