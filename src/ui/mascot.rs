//! Mascote oficial do Kestrel: o Pip.
//!
//! O PNG original (`assets/pip.png`) é desenhado de verdade no terminal com
//! `ratatui-image`: Sixel, Kitty ou iTerm2 quando o terminal oferece um desses
//! protocolos; halfblocks do próprio `ratatui-image` quando não oferece.
//!
//! O espaço é fixo: **6 colunas × 2 linhas**. A animação não muda a altura da
//! caixa — ela desloca a imagem dentro dela. O PNG é recortado na caixa opaca,
//! redimensionado uma vez para a resolução de pixels da área e só então
//! deslocado; assim a caminhada é visível mesmo em 48×32 pixels.
//!
//! A animação é determinística e tem um ciclo de 128 tiques:
//! repouso (respirando), caminhada leve para a esquerda e volta, caminhada
//! leve para a direita e volta, repouso longo. A piscada é sorteada pela
//! interface e só acontece no repouso.
//!
//! Os buracos dos olhos são transparentes no PNG original. Em terminal claro
//! eles mostravam o fundo claro e pareciam “olho branco”. Agora eles são
//! pintados com a cor de fundo da aplicação; a piscada cobre os mesmos
//! buracos com a cor do pelo.

use image::{DynamicImage, Rgba, RgbaImage, imageops};
use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui::style::Color;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Image, Resize};
use std::collections::HashMap;

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

/// Colunas de terminal ocupadas pelo Pip.
pub const COLUNAS: u16 = 6;
/// Linhas de terminal ocupadas pelo Pip.
pub const LINHAS: u16 = 2;

/// Quadro atual da animação. `dx`/`dy` são deslocamentos em pixels da imagem
/// já redimensionada (não em pixels do PNG original).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AnimacaoPip {
    pub dx: i8,
    pub dy: i8,
    pub piscando: bool,
}

impl AnimacaoPip {
    fn chave(self) -> Quadro {
        Quadro {
            dx: self.dx.clamp(-4, 4),
            dy: self.dy.clamp(0, 1),
            piscando: self.piscando,
        }
    }
}

/// Deslocamento da respiração para um tique: ciclo lento de 16 tiques,
/// com subida e retorno suaves.
fn respiracao(tique: u64) -> i8 {
    i8::from((4..12).contains(&(tique % 16)))
}

/// Quadro do ciclo de animação para um tique.
///
/// O ciclo completo tem 128 tiques. Com a cadência de 120 ms do turno ativo,
/// isso dá cerca de 15 segundos. A caminhada usa 32 tiques para cada lado
/// (ida e volta), com passos de 1 px na imagem redimensionada.
pub fn quadro_de(tique: u64) -> AnimacaoPip {
    let fase = (tique % 128) as u32;
    let (dx, dy) = match fase {
        32..=39 => (-2, 0),
        40..=47 => (-4, 0),
        48..=55 => (-2, 0),
        56..=63 => (0, 0),
        64..=71 => (2, 0),
        72..=79 => (4, 0),
        80..=87 => (2, 0),
        88..=95 => (0, 0),
        // Repouso antes e depois da caminhada.
        _ => (0, respiracao(tique)),
    };
    AnimacaoPip {
        dx,
        dy,
        piscando: false,
    }
}

/// Chave de um quadro já codificado.
#[derive(Clone, Copy, Debug, Hash, PartialEq, Eq)]
struct Quadro {
    dx: i8,
    dy: i8,
    piscando: bool,
}

/// Pip em PNG, com os quadros da animação já codificados no protocolo do
/// terminal. É criado uma vez no início da sessão e só desenha.
#[derive(Clone)]
pub struct PipGrafico {
    quadros: HashMap<Quadro, Protocol>,
}

const PNG: &[u8] = include_bytes!("../../assets/pip.png");

// Caixa opaca do PNG original e os dois furos dos olhos, em coordenadas
// relativas a essa caixa. Foram detectados pela transparência da imagem
// (flood-fill a partir da borda, restando só os buracos internos);
// os testes conferem que continuam dentro da caixa.
const CAIXA_X: u32 = 299;
const CAIXA_Y: u32 = 235;
const CAIXA_LARGURA: u32 = 1802;
const CAIXA_ALTURA: u32 = 1174;
const OLHOS: [(u32, u32, u32, u32); 2] = [(485, 212, 597, 478), (1204, 213, 1317, 478)];

/// Margem horizontal/vertical, em pixels de destino, para a caminhada não
/// cortar a silhueta nas bordas da caixa de 6×2.
const MARGEM: u32 = 2;

impl PipGrafico {
    /// Decodifica o PNG e codifica todos os quadros da animação.
    pub fn novo(picker: &Picker) -> anyhow::Result<Self> {
        let fonte = picker.font_size();
        let largura = COLUNAS as u32 * u32::from(fonte.width.max(1));
        let altura = LINHAS as u32 * u32::from(fonte.height.max(1));

        let original = image::load_from_memory(PNG)
            .map_err(|e| anyhow::anyhow!("falha ao decodificar o PNG do Pip: {e}"))?
            .to_rgba8();
        let recorte =
            imageops::crop_imm(&original, CAIXA_X, CAIXA_Y, CAIXA_LARGURA, CAIXA_ALTURA).to_image();

        let util_largura = largura.saturating_sub(2 * MARGEM).max(1);
        let util_altura = altura.saturating_sub(2 * MARGEM).max(1);
        let escala = (util_largura as f32 / CAIXA_LARGURA as f32)
            .min(util_altura as f32 / CAIXA_ALTURA as f32);
        let redimensionada_largura = (CAIXA_LARGURA as f32 * escala).round().max(1.0) as u32;
        let redimensionada_altura = (CAIXA_ALTURA as f32 * escala).round().max(1.0) as u32;
        let redimensionada = imageops::resize(
            &recorte,
            redimensionada_largura,
            redimensionada_altura,
            FilterType::Lanczos3,
        );

        let deslocamento_x = ((largura - redimensionada_largura) / 2) as i64;
        let deslocamento_y = ((altura - redimensionada_altura) / 2) as i64;
        let mut base = RgbaImage::from_pixel(largura, altura, Rgba([0, 0, 0, 0]));
        for y in 0..redimensionada_altura {
            for x in 0..redimensionada_largura {
                base.put_pixel(
                    (deslocamento_x + i64::from(x)) as u32,
                    (deslocamento_y + i64::from(y)) as u32,
                    *redimensionada.get_pixel(x, y),
                );
            }
        }

        let fechado = olhos_fechados(&base, escala, deslocamento_x, deslocamento_y);
        // Sem isso, o buraco transparente do olho mostra o fundo do terminal:
        // em terminal claro o Pip ficava com “olho branco”.
        pintar_olhos(
            &mut base,
            escala,
            deslocamento_x,
            deslocamento_y,
            cor_de_fundo(),
        );
        let mut quadros = HashMap::new();
        for dx in -4..=4 {
            for dy in 0..=1 {
                quadros.insert(
                    Quadro {
                        dx,
                        dy,
                        piscando: false,
                    },
                    codificar(picker, &deslocar(&base, dx, dy))?,
                );
            }
        }
        for dy in 0..=1 {
            quadros.insert(
                Quadro {
                    dx: 0,
                    dy,
                    piscando: true,
                },
                codificar(picker, &deslocar(&fechado, 0, dy))?,
            );
        }

        Ok(Self { quadros })
    }

    /// Desenha o quadro atual na área reservada. A área deve ter pelo menos
    /// [`COLUNAS`] × [`LINHAS`] células; a imagem é desenhada exatamente nesse
    /// tamanho.
    pub fn desenhar(&self, f: &mut Frame, area: Rect, animacao: AnimacaoPip) {
        let chave = animacao.chave();
        let protocol = self
            .quadros
            .get(&chave)
            .or_else(|| {
                self.quadros.get(&Quadro {
                    dx: 0,
                    dy: 0,
                    piscando: false,
                })
            })
            .expect("quadros do Pip sempre incluem o repouso");
        f.render_widget(Image::new(protocol), area);
    }
}

fn codificar(picker: &Picker, imagem: &RgbaImage) -> anyhow::Result<Protocol> {
    picker
        .new_protocol(
            DynamicImage::ImageRgba8(imagem.clone()),
            Size::new(COLUNAS, LINHAS),
            Resize::Fit(Some(FilterType::Nearest)),
        )
        .map_err(|e| anyhow::anyhow!("falha ao codificar o Pip: {e}"))
}

/// Cor de fundo da aplicação como pixel opaco. O Pip usa o tema já
/// inicializado; em skins claras os olhos acompanham o fundo claro.
fn cor_de_fundo() -> Rgba<u8> {
    match crate::ui::theme::fundo_cor() {
        Color::Rgb(r, g, b) => Rgba([r, g, b, 255]),
        _ => Rgba([0, 0, 0, 255]),
    }
}

/// Pinta os furos dos olhos, já na escala de destino, com uma cor opaca.
fn pintar_olhos(imagem: &mut RgbaImage, escala: f32, ox: i64, oy: i64, cor: Rgba<u8>) {
    let largura = imagem.width() as i64;
    let altura = imagem.height() as i64;
    for (x0, y0, x1, y1) in OLHOS {
        let inicio_x = (ox + (x0 as f32 * escala).round() as i64).clamp(0, largura - 1);
        let inicio_y = (oy + (y0 as f32 * escala).round() as i64).clamp(0, altura - 1);
        let fim_x = (ox + (x1 as f32 * escala).round() as i64).clamp(0, largura - 1);
        let fim_y = (oy + (y1 as f32 * escala).round() as i64).clamp(0, altura - 1);
        for y in inicio_y..=fim_y {
            for x in inicio_x..=fim_x {
                imagem.put_pixel(x as u32, y as u32, cor);
            }
        }
    }
}

/// Cobre os furos dos olhos com a cor do pelo. A caixa dos olhos é retangular,
/// mas fica inteiramente dentro da região branca; preenchê-la só fecha o
/// buraco e não altera o contorno. O olho some por alguns quadros sem depender
/// do fundo do terminal.
fn olhos_fechados(base: &RgbaImage, escala: f32, ox: i64, oy: i64) -> RgbaImage {
    let mut fechado = base.clone();
    pintar_olhos(&mut fechado, escala, ox, oy, Rgba([255, 255, 255, 255]));
    fechado
}

/// Move a imagem dentro da mesma caixa, preenchendo o espaço vazio com
/// transparência (o fundo do status aparece atrás).
fn deslocar(base: &RgbaImage, dx: i8, dy: i8) -> RgbaImage {
    let (largura, altura) = base.dimensions();
    let mut saida = RgbaImage::from_pixel(largura, altura, Rgba([0, 0, 0, 0]));
    for y in 0..i64::from(altura) {
        for x in 0..i64::from(largura) {
            let origem_x = x - i64::from(dx);
            let origem_y = y - i64::from(dy);
            if origem_x >= 0
                && origem_y >= 0
                && origem_x < i64::from(largura)
                && origem_y < i64::from(altura)
            {
                saida.put_pixel(
                    x as u32,
                    y as u32,
                    *base.get_pixel(origem_x as u32, origem_y as u32),
                );
            }
        }
    }
    saida
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ciclo_tem_repouso_caminhada_e_volta() {
        assert_eq!(
            quadro_de(0),
            AnimacaoPip {
                dx: 0,
                dy: 0,
                piscando: false
            }
        );
        assert_eq!(quadro_de(36).dx, -2);
        assert_eq!(quadro_de(44).dx, -4);
        assert_eq!(quadro_de(60).dx, 0);
        assert_eq!(quadro_de(68).dx, 2);
        assert_eq!(quadro_de(76).dx, 4);
        assert_eq!(quadro_de(92).dx, 0);
        assert_eq!(quadro_de(100).dx, 0);
        assert_eq!(quadro_de(128), quadro_de(0));
    }

    #[test]
    fn respiracao_nao_muda_a_altura_da_caixa() {
        for tique in 0..128 {
            let quadro = quadro_de(tique);
            assert!(quadro.dx >= -4 && quadro.dx <= 4);
            assert!(quadro.dy == 0 || quadro.dy == 1);
        }
    }

    #[test]
    fn caixa_e_olhos_cabem_na_imagem_original() {
        let original = image::load_from_memory(PNG).unwrap().to_rgba8();
        assert!(CAIXA_X + CAIXA_LARGURA <= original.width());
        assert!(CAIXA_Y + CAIXA_ALTURA <= original.height());
        for (x0, y0, x1, y1) in OLHOS {
            assert!(x0 < x1 && y0 < y1);
            assert!(x1 < CAIXA_LARGURA && y1 < CAIXA_ALTURA);
        }
    }

    #[test]
    fn deslocamento_move_a_imagem_sem_mudar_o_tamanho() {
        let mut base = RgbaImage::from_pixel(8, 8, Rgba([0, 0, 0, 0]));
        base.put_pixel(4, 4, Rgba([255, 255, 255, 255]));
        let esquerda = deslocar(&base, -2, 0);
        assert_eq!(esquerda.dimensions(), (8, 8));
        assert_eq!(*esquerda.get_pixel(2, 4), Rgba([255, 255, 255, 255]));
        let baixo = deslocar(&base, 0, 1);
        assert_eq!(*baixo.get_pixel(4, 5), Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn quadros_sao_codificados_para_o_tamanho_fixo() {
        let picker = Picker::halfblocks();
        let pip = PipGrafico::novo(&picker).expect("Pip");
        assert_eq!(pip.quadros.len(), 20);
        for (quadro, protocol) in &pip.quadros {
            assert_eq!(protocol.size(), Size::new(COLUNAS, LINHAS));
            assert!(quadro.dx >= -4 && quadro.dx <= 4);
            assert!(quadro.dy == 0 || quadro.dy == 1);
        }
    }
}
