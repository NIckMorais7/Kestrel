//! Ícone oficial do Kestrel para o cabeçalho.
//!
//! São duas versões do mesmo ícone: branco para fundo escuro e preto para fundo
//! claro. A escolha é feita no desenho, então `/skin` troca o ícone junto com o
//! restante da interface sem precisar recriar o PNG.

use crate::ui::theme;
use image::{DynamicImage, Rgba, RgbaImage, imageops};
use ratatui::Frame;
use ratatui::layout::{Rect, Size};
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{FilterType, Image, Resize};

/// Colunas ocupadas pelo ícone no cabeçalho.
pub const COLUNAS: u16 = 2;
/// Linhas ocupadas pelo ícone.
pub const LINHAS: u16 = 1;

/// Ícone já codificado nas duas cores.
#[derive(Clone)]
pub struct IconeKestrel {
    claro: Protocol,
    escuro: Protocol,
}

const ICONE_BRANCO: &[u8] = include_bytes!("../../assets/white_icon_kestrel.png");
const ICONE_PRETO: &[u8] = include_bytes!("../../assets/icon_kestrel.png");

impl IconeKestrel {
    /// Decodifica os dois PNGs e codifica no tamanho de 2×1 células.
    pub fn novo(picker: &Picker) -> anyhow::Result<Self> {
        let fonte = picker.font_size();
        let largura = COLUNAS as u32 * u32::from(fonte.width.max(1));
        let altura = LINHAS as u32 * u32::from(fonte.height.max(1));
        let branco = preparar(ICONE_BRANCO, largura, altura)?;
        let preto = preparar(ICONE_PRETO, largura, altura)?;
        Ok(Self {
            claro: codificar(picker, branco)?,
            escuro: codificar(picker, preto)?,
        })
    }

    /// Desenha o ícone que contrasta com o fundo atual.
    pub fn desenhar(&self, f: &mut Frame, area: Rect) {
        let protocol = if theme::fundo_e_claro() {
            &self.escuro
        } else {
            &self.claro
        };
        f.render_widget(Image::new(protocol), area);
    }
}

fn codificar(picker: &Picker, imagem: RgbaImage) -> anyhow::Result<Protocol> {
    picker
        .new_protocol(
            DynamicImage::ImageRgba8(imagem),
            Size::new(COLUNAS, LINHAS),
            Resize::Fit(None),
        )
        .map_err(|e| anyhow::anyhow!("falha ao codificar o ícone do Kestrel: {e}"))
}

/// Redimensiona o ícone para caber exatamente na área de células, mantendo a
/// proporção e centralizando. Não pinta fundo: a transparência deixa o tema
/// aparecer atrás.
fn preparar(png: &[u8], largura: u32, altura: u32) -> anyhow::Result<RgbaImage> {
    let original = image::load_from_memory(png)
        .map_err(|e| anyhow::anyhow!("falha ao decodificar o ícone do Kestrel: {e}"))?
        .to_rgba8();
    let escala = (largura as f32 / original.width() as f32)
        .min(altura as f32 / original.height() as f32)
        .clamp(0.01, 1.0);
    // O `clamp` de escala pode arredondar para cima num terminal com fonte
    // minúscula; sem o `.min`, `largura - redimensionada_largura` estourava
    // (panic em debug) antes de o ícone ser desenhado.
    let redimensionada_largura = ((original.width() as f32 * escala).round().max(1.0) as u32)
        .min(largura)
        .max(1);
    let redimensionada_altura = ((original.height() as f32 * escala).round().max(1.0) as u32)
        .min(altura)
        .max(1);
    let redimensionada = imageops::resize(
        &original,
        redimensionada_largura,
        redimensionada_altura,
        FilterType::Lanczos3,
    );
    let mut canvas = RgbaImage::from_pixel(largura, altura, Rgba([0, 0, 0, 0]));
    let deslocamento_x = ((largura - redimensionada_largura) / 2) as i64;
    let deslocamento_y = ((altura - redimensionada_altura) / 2) as i64;
    for y in 0..redimensionada_altura {
        for x in 0..redimensionada_largura {
            if let Some(pixel) = canvas.get_pixel_mut_checked(
                (deslocamento_x + i64::from(x)) as u32,
                (deslocamento_y + i64::from(y)) as u32,
            ) {
                *pixel = *redimensionada.get_pixel(x, y);
            }
        }
    }
    Ok(canvas)
}

#[cfg(test)]
mod testes {
    use super::*;

    /// Fonte minúscula (2×1 células) não pode estourar a subtração ao
    /// centralizar: a escala mínima do `clamp` podia deixar o ícone maior que
    /// a área.
    #[test]
    fn icone_em_area_minuscula_nao_estoura() {
        let imagem = preparar(ICONE_BRANCO, 2, 1).expect("ícone");
        assert_eq!(imagem.dimensions(), (2, 1));
    }

    #[test]
    fn icone_cabe_na_area_do_cabecalho() {
        let picker = ratatui_image::picker::Picker::halfblocks();
        let icone = IconeKestrel::novo(&picker).expect("ícone");
        // Os dois protocolos têm que existir e ocupar a mesma área.
        assert_eq!(icone.claro.size(), Size::new(COLUNAS, LINHAS));
        assert_eq!(icone.escuro.size(), Size::new(COLUNAS, LINHAS));
    }
}
