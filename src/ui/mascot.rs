//! Mascote oficial do Kestrel: o Pip.
//!
//! O PNG original (`assets/pip.png`) é desenhado de verdade no terminal com
//! `ratatui-image`, usando o protocolo gráfico do terminal quando disponível
//! (Sixel/Kitty/iTerm2) e halfblocks apenas como último recurso. O PNG é
//! redimensionado uma única vez para a resolução de pixels de 6×2 e o quadro é
//! codificado já nessa resolução, sem segundo resize.
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

/// Deslocamento da respiração: ciclo lento de 32 tiques. Na cadência da
/// recepção (40 ms) são ~1,3 s por ciclo; o peito sobe e desce sem saltos
/// bruscos porque a troca acontece nos zeros da onda.
fn respiracao(tique: u64) -> i8 {
    let angulo = 2.0 * std::f32::consts::PI * ((tique % 32) as f32) / 32.0;
    i8::from(angulo.sin() > 0.0)
}

/// Um passeio de 20 quadros: sai do centro (5), segura na ponta (2), volta
/// (4) e acomoda (5). Cada quadro move no máximo 1 pixel, então a 40 ms o
/// deslocamento é contínuo em vez de saltar a cada 300 ms.
fn passeio(indice: u64, direcao: i8) -> (i8, i8) {
    let posicao = match indice {
        0..=4 => indice as i8,
        5..=6 => 4,
        7..=10 => 10 - indice as i8,
        _ => 0,
    };
    // Balanço do passo: 0/1 alternando a cada 3 quadros (~120 ms).
    let passo = ((indice / 3) % 2) as i8;
    (posicao * direcao, passo)
}

/// Quadro do ciclo de animação para um tique.
///
/// O ciclo tem 96 quadros: repouso, passeio à esquerda, pausa, passeio à
/// direita e repouso. A 40 ms são ~3,8 s por ciclo; todos os quadros usam
/// deslocamentos de 1 pixel (`-4..=4`), que é a menor unidade da imagem
/// redimensionada.
pub fn quadro_de(tique: u64) -> AnimacaoPip {
    let fase = tique % 96;
    let (dx, dy) = match fase {
        0..=19 => (0, respiracao(tique)),
        20..=39 => passeio(fase - 20, -1),
        40..=51 => (0, respiracao(tique)),
        52..=71 => passeio(fase - 52, 1),
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
    /// Guardado para recodificar quando a skin troca: o fundo da moldura
    /// acompanha a cor do aplicativo.
    picker: Picker,
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
        let mut pip = Self {
            quadros: HashMap::new(),
            picker: picker.clone(),
        };
        pip.recodificar()?;
        Ok(pip)
    }

    /// Recodifica todos os quadros com a cor de fundo atual.
    ///
    /// A moldura é opaca (evita rastro entre quadros), então trocar de skin
    /// sem refazer os quadros deixaria um retângulo com a cor antiga ao redor
    /// do Pip.
    pub fn atualizar_tema(&mut self) -> anyhow::Result<()> {
        self.recodificar()
    }

    fn recodificar(&mut self) -> anyhow::Result<()> {
        let picker = &self.picker;
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
        let redimensionada_largura = ((CAIXA_LARGURA as f32 * escala).round().max(1.0) as u32)
            .min(largura)
            .max(1);
        let redimensionada_altura = ((CAIXA_ALTURA as f32 * escala).round().max(1.0) as u32)
            .min(altura)
            .max(1);
        let redimensionada = imageops::resize(
            &recorte,
            redimensionada_largura,
            redimensionada_altura,
            FilterType::Lanczos3,
        );

        let deslocamento_x = ((largura - redimensionada_largura) / 2) as i64;
        let deslocamento_y = ((altura - redimensionada_altura) / 2) as i64;
        let fundo = cor_de_fundo();
        let mut base = RgbaImage::from_pixel(largura, altura, fundo);
        for y in 0..redimensionada_altura {
            for x in 0..redimensionada_largura {
                // Variante checada, como no `icone.rs`: a crate `image` faz
                // bounds-check com pânico, e um cast de `i64` negativo daria
                // uma coordenada gigante. Fora da caixa, o pixel é ignorado.
                if let Some(pixel) = base.get_pixel_mut_checked(
                    (deslocamento_x + i64::from(x)) as u32,
                    (deslocamento_y + i64::from(y)) as u32,
                ) {
                    // Compõe sobre o fundo em vez de copiar o alpha: cópias
                    // transparentes deixavam a moldura com buracos que alguns
                    // protocolos gráficos pintavam de branco.
                    *pixel = misturar(*pixel, *redimensionada.get_pixel(x, y));
                }
            }
        }

        // A cor do pelo vem da imagem redimensionada (antes de qualquer pintura
        // de olho): amostra o anel ao redor de cada buraco, então a piscada
        // fecha o olho com o tom *real* do pelo — não branco puro fixo, que
        // aparecia como um “olho branco” estranho quando o pelo é sombreado.
        let cor_pelo = [
            cor_do_pelo(&redimensionada, escala, OLHOS[0]),
            cor_do_pelo(&redimensionada, escala, OLHOS[1]),
        ];
        // Olhos: tinta escura fixa (quase preta), em contraste com o pelo
        // branco — amostrar o contorno resultava num cinza estranho, e o
        // fundo deixava o olho branco demais em skins claras.
        let cor_tinta = [COR_TINTA; 2];
        let fechado = olhos_fechados(
            &base,
            escala,
            deslocamento_x,
            deslocamento_y,
            &cor_pelo,
            &cor_tinta,
        );
        // Os furos são pintados com a tinta escura fixa, não com o fundo: em
        // skin clara o fundo deixava o olho claro e ele sumia no pelo branco.
        pintar_olhos(
            &mut base,
            escala,
            deslocamento_x,
            deslocamento_y,
            &cor_tinta,
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

        self.quadros = quadros;
        Ok(())
    }

    /// Desenha o quadro atual na área reservada. A área deve ter pelo menos
    /// [`COLUNAS`] × [`LINHAS`] células; a imagem é desenhada exatamente nesse
    /// tamanho.
    pub fn desenhar(&self, f: &mut Frame, area: Rect, animacao: AnimacaoPip) {
        // O `clear_area` do Sixel apaga as células com a cor de fundo corrente
        // antes de desenhar a imagem. Fixar o fundo do tema na área evita uma
        // faixa clara quando a célula ainda estava no estilo padrão do
        // terminal (qualquer sobra da limpeza fica na cor do app).
        f.render_widget(
            ratatui::widgets::Block::default().style(crate::ui::theme::fundo()),
            area,
        );
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
    // A imagem já foi redimensionada para a resolução de pixels da área
    // (`new_protocol` receberia 6×2 células). `Fit(None)` evita um segundo
    // reescalonamento, que era o que deixava o Pip borrado/pixelado.
    picker
        .new_protocol(
            DynamicImage::ImageRgba8(imagem.clone()),
            Size::new(COLUNAS, LINHAS),
            Resize::Fit(None),
        )
        .map_err(|e| anyhow::anyhow!("falha ao codificar o Pip: {e}"))
}

/// Tinta dos olhos (abertos e pálpebra fechada): quase preta, para contrastar
/// com o pelo branco em qualquer skin. Amostrar o contorno resultava num cinza
/// estranho; o fundo deixava o olho branco demais em skins claras.
const COR_TINTA: Rgba<u8> = Rgba([0x13, 0x15, 0x1a, 255]);

/// Cor de fundo da aplicação como pixel opaco. O Pip usa o tema já
/// inicializado; em skins claras os olhos acompanham o fundo claro.
fn cor_de_fundo() -> Rgba<u8> {
    match crate::ui::theme::fundo_cor() {
        Color::Rgb(r, g, b) => Rgba([r, g, b, 255]),
        _ => Rgba([0, 0, 0, 255]),
    }
}

/// Pinta os furos dos olhos com a cor de tinta de cada um.
fn pintar_olhos(imagem: &mut RgbaImage, escala: f32, ox: i64, oy: i64, cores: &[Rgba<u8>; 2]) {
    for (i, olho) in OLHOS.iter().enumerate() {
        pintar_olhos_um(imagem, escala, ox, oy, *olho, cores[i]);
    }
}

/// Fecha os olhos: cobre o buraco com o pelo e desenha uma pálpebra escura.
///
/// Preencher o buraco só com o pelo fazia o olho "sumir" no rosto branco — e
/// o bloco branco da piscada chamava mais atenção que o olho aberto. A linha
/// de tinta dá a leitura de olho fechado em qualquer skin.
fn olhos_fechados(
    base: &RgbaImage,
    escala: f32,
    ox: i64,
    oy: i64,
    cor_pelo: &[Rgba<u8>; 2],
    cor_tinta: &[Rgba<u8>; 2],
) -> RgbaImage {
    let mut fechado = base.clone();
    for (i, olho) in OLHOS.iter().enumerate() {
        pintar_olhos_um(&mut fechado, escala, ox, oy, *olho, cor_pelo[i]);
        desenhar_pestana(&mut fechado, escala, ox, oy, *olho, cor_tinta[i]);
    }
    fechado
}

/// Retângulo do olho em pixels de destino, já recortado à imagem.
fn retangulo_do_olho(
    largura: i64,
    altura: i64,
    escala: f32,
    ox: i64,
    oy: i64,
    (x0, y0, x1, y1): (u32, u32, u32, u32),
) -> (i64, i64, i64, i64) {
    let inicio_x = (ox + (x0 as f32 * escala).round() as i64).clamp(0, largura - 1);
    let inicio_y = (oy + (y0 as f32 * escala).round() as i64).clamp(0, altura - 1);
    let fim_x = (ox + (x1 as f32 * escala).round() as i64).clamp(0, largura - 1);
    let fim_y = (oy + (y1 as f32 * escala).round() as i64).clamp(0, altura - 1);
    (inicio_x, inicio_y, fim_x, fim_y)
}

/// Pinta um único furo de olho.
fn pintar_olhos_um(
    imagem: &mut RgbaImage,
    escala: f32,
    ox: i64,
    oy: i64,
    olho: (u32, u32, u32, u32),
    cor: Rgba<u8>,
) {
    let largura = imagem.width() as i64;
    let altura = imagem.height() as i64;
    let (inicio_x, inicio_y, fim_x, fim_y) =
        retangulo_do_olho(largura, altura, escala, ox, oy, olho);
    for y in inicio_y..=fim_y {
        for x in inicio_x..=fim_x {
            imagem.put_pixel(x as u32, y as u32, cor);
        }
    }
}

/// Desenha a pálpebra fechada: barra horizontal de tinta no meio do olho.
fn desenhar_pestana(
    imagem: &mut RgbaImage,
    escala: f32,
    ox: i64,
    oy: i64,
    olho: (u32, u32, u32, u32),
    cor: Rgba<u8>,
) {
    let largura = imagem.width() as i64;
    let altura = imagem.height() as i64;
    let (inicio_x, inicio_y, fim_x, fim_y) =
        retangulo_do_olho(largura, altura, escala, ox, oy, olho);
    let altura_olho = (fim_y - inicio_y + 1).max(1);
    let espessura = (altura_olho / 4).clamp(1, 3);
    let meio = (inicio_y + fim_y) / 2;
    let topo = (meio - espessura / 2).max(inicio_y);
    let base = (topo + espessura - 1).min(fim_y);
    // Um pixel além da caixa em cada lado dá a impressão de cílio/pálpebra,
    // sem tocar no contorno do rosto.
    let x0 = (inicio_x - 1).max(0);
    let x1 = (fim_x + 1).min(largura - 1);
    for y in topo..=base {
        for x in x0..=x1 {
            imagem.put_pixel(x as u32, y as u32, cor);
        }
    }
}

/// Cor média do pelo ao redor de um buraco de olho, na imagem redimensionada
/// (pré-pintura). O anel externo à caixa do olho é amostrado; sem nada opaco
/// por perto, cai num branco neutro.
fn cor_do_pelo(imagem: &RgbaImage, escala: f32, olho: (u32, u32, u32, u32)) -> Rgba<u8> {
    let (x0, y0, x1, y1) = olho;
    let e0 = (x0 as f32 * escala).round() as i64;
    let e1 = (x1 as f32 * escala).round() as i64;
    let f0 = (y0 as f32 * escala).round() as i64;
    let f1 = (y1 as f32 * escala).round() as i64;
    let margem = 3i64;
    let inicio_x = (e0 - margem).max(0);
    let inicio_y = (f0 - margem).max(0);
    let fim_x = (e1 + margem).min(imagem.width() as i64 - 1);
    let fim_y = (f1 + margem).min(imagem.height() as i64 - 1);
    let mut soma = (0u64, 0u64, 0u64);
    let mut n = 0u64;
    for y in inicio_y..=fim_y {
        for x in inicio_x..=fim_x {
            if x >= e0 && x <= e1 && y >= f0 && y <= f1 {
                continue; // só o anel externo
            }
            let px = imagem.get_pixel(x as u32, y as u32);
            if px[3] > 0 {
                soma.0 += u64::from(px[0]);
                soma.1 += u64::from(px[1]);
                soma.2 += u64::from(px[2]);
                n += 1;
            }
        }
    }
    if n == 0 {
        return Rgba([245, 245, 245, 255]);
    }
    Rgba([
        (soma.0 / n) as u8,
        (soma.1 / n) as u8,
        (soma.2 / n) as u8,
        255,
    ])
}

/// Compõe um pixel RGBA sobre um fundo opaco. O resultado é sempre opaco:
/// transparência sobrando na moldura virava bloco branco em alguns protocolos.
fn misturar(fundo: Rgba<u8>, frente: Rgba<u8>) -> Rgba<u8> {
    let alpha = u32::from(frente[3]);
    if alpha == 0 {
        return fundo;
    }
    if alpha == 255 {
        return frente;
    }
    let inverso = 255 - alpha;
    let canal =
        |i: usize| ((u32::from(frente[i]) * alpha + u32::from(fundo[i]) * inverso) / 255) as u8;
    Rgba([canal(0), canal(1), canal(2), 255])
}

/// Move a imagem dentro da mesma caixa, preenchendo o espaço vazio com a cor
/// de fundo do aplicativo.
fn deslocar(base: &RgbaImage, dx: i8, dy: i8) -> RgbaImage {
    let (largura, altura) = base.dimensions();
    // Fundo opaco no quadro inteiro: sem transparência sobrando onde a silhueta
    // passou antes, o que em protocolos gráficos causava “Pip duplicado”.
    let mut saida = RgbaImage::from_pixel(largura, altura, cor_de_fundo());
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
    fn ciclo_tem_repouso_passeio_e_volta() {
        assert_eq!(
            quadro_de(0),
            AnimacaoPip {
                dx: 0,
                dy: 0,
                piscando: false
            }
        );
        // Passeio à esquerda: 0 → -4 um pixel por quadro, pausa na ponta e
        // volta. Nada de saltos de vários pixels no mesmo quadro.
        assert_eq!(quadro_de(20).dx, 0);
        assert_eq!(quadro_de(21).dx, -1);
        assert_eq!(quadro_de(22).dx, -2);
        assert_eq!(quadro_de(23).dx, -3);
        assert_eq!(quadro_de(24).dx, -4);
        assert_eq!(quadro_de(25).dx, -4);
        assert_eq!(quadro_de(26).dx, -4);
        assert_eq!(quadro_de(27).dx, -3);
        assert_eq!(quadro_de(28).dx, -2);
        assert_eq!(quadro_de(29).dx, -1);
        assert_eq!(quadro_de(30).dx, 0);
        assert_eq!(quadro_de(39).dx, 0);
        // Pausa e passeio simétrico à direita.
        assert_eq!(quadro_de(40).dx, 0);
        assert_eq!(quadro_de(52).dx, 0);
        assert_eq!(quadro_de(53).dx, 1);
        assert_eq!(quadro_de(56).dx, 4);
        assert_eq!(quadro_de(62).dx, 0);
        assert_eq!(quadro_de(96), quadro_de(0));
    }

    #[test]
    fn caminhada_e_simetrica_e_continua() {
        // A ida à esquerda é o espelho da ida à direita (mesma onda).
        for p in 0..20 {
            let esquerda = quadro_de(20 + p);
            let direita = quadro_de(52 + p);
            assert_eq!(esquerda.dx, -direita.dx, "passo {p}");
            assert_eq!(esquerda.dy, direita.dy, "passo {p}");
        }
        // Nenhum salto maior que 1 px entre quadros consecutivos.
        let mut anterior = quadro_de(19).dx;
        for tique in 20..=71 {
            let atual = quadro_de(tique).dx;
            assert!(
                (atual - anterior).abs() <= 1,
                "salto em {tique}: {anterior} -> {atual}"
            );
            anterior = atual;
        }
        // A passada sobe em algum momento e desce em outro (dy varia).
        let mut viu_zero = false;
        let mut viu_um = false;
        for tique in 20..=71 {
            match quadro_de(tique).dy {
                0 => viu_zero = true,
                _ => viu_um = true,
            }
        }
        assert!(viu_zero && viu_um, "a caminhada precisa ter passada");
    }

    #[test]
    fn mistura_alpha_sobre_o_fundo_e_sempre_opaca() {
        let fundo = Rgba([10, 20, 30, 255]);
        assert_eq!(misturar(fundo, Rgba([0, 0, 0, 0])), fundo);
        let opaca = Rgba([200, 100, 50, 255]);
        assert_eq!(misturar(fundo, opaca), opaca);
        let meio = misturar(fundo, Rgba([200, 100, 50, 128]));
        assert_eq!(meio[3], 255);
        assert!(meio[0] > fundo[0] && meio[0] < opaca[0]);
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

    /// O olho usa uma tinta escura fixa (quase preta) para contrastar com o
    /// pelo branco em qualquer skin — nem cinza amostrado do contorno, nem o
    /// branco do fundo.
    #[test]
    fn olho_usa_tinta_escura_fixa() {
        assert_eq!(COR_TINTA, Rgba([0x13, 0x15, 0x1a, 255]));
        assert!(COR_TINTA[0] < 40 && COR_TINTA[1] < 40, "tinta clara demais");
        assert_eq!(COR_TINTA[3], 255);
    }

    #[test]
    fn pestana_escura_fica_no_meio_do_olho() {
        let mut imagem = RgbaImage::from_pixel(30, 30, Rgba([255, 255, 255, 255]));
        let tinta = Rgba([30, 32, 38, 255]);
        desenhar_pestana(&mut imagem, 1.0, 0, 0, (10, 10, 20, 20), tinta);
        // Meio do olho recebe a pálpebra; topo continua com o pelo.
        assert_eq!(*imagem.get_pixel(15, 15), tinta);
        assert_eq!(*imagem.get_pixel(15, 10), Rgba([255, 255, 255, 255]));
    }

    #[test]
    fn cor_do_pelo_amostra_o_anel_externo() {
        let mut imagem = RgbaImage::from_pixel(40, 40, Rgba([0, 0, 0, 0]));
        // Pelo cinza-claro opaco ao redor de um buraco retangular transparente
        // (10,10)-(20,20): o anel externo tem a cor do pelo.
        for y in 0..40 {
            for x in 0..40 {
                let dentro = (10..=20).contains(&x) && (10..=20).contains(&y);
                if !dentro {
                    imagem.put_pixel(x, y, Rgba([220, 220, 220, 255]));
                }
            }
        }
        let cor = cor_do_pelo(&imagem, 1.0, (10, 10, 20, 20));
        assert!(cor[0] > 200 && cor[0] < 240, "veio: {cor:?}");
        assert_eq!(cor[3], 255);
    }

    /// Sem pelo opaco por perto (tudo transparente), cai num branco neutro —
    /// nunca pânico.
    #[test]
    fn cor_do_pelo_vazia_usa_branco_neutro() {
        let imagem = RgbaImage::from_pixel(20, 20, Rgba([0, 0, 0, 0]));
        let cor = cor_do_pelo(&imagem, 1.0, (4, 4, 8, 8));
        assert_eq!(cor, Rgba([245, 245, 245, 255]));
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
