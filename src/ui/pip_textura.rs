//! Textura do Pip, derivada da imagem original (assets/pip.png).
//!
//! Este arquivo é GERADO por scripts/gerar_textura_pip.py — não edite à mão.
//! Os pixels vêm da imagem oficial em /home/nick/Assets/Pip/pip.png; nada
//! é redesenhado aqui. Cada texel ocupa meia célula do terminal (blocos
//! ▀/▄ com cor verdadeira) e o fundo escuro do aplicativo aparece pelos
//! furos da arte (contorno e olhos).

/// Um olho do Pip em coordenadas de texel, com as cores do fechamento.
#[derive(Clone, Copy, Debug)]
pub struct OlhoTexel {
    pub x0: usize,
    pub y0: usize,
    pub x1: usize,
    pub y1: usize,
    /// Cor do pelo que cobre o olho durante a piscada.
    pub pelagem: u32,
    /// Cor dos texels da pálpebra fechada.
    pub palpebra: u32,
}

/// Largura da textura mini em texels.
pub const LARGURA_MINI: usize = 6;
/// Altura da textura mini em texels.
pub const ALTURA_MINI: usize = 4;

/// Pixels (0xAARRGGBB; alfa 0 = vazado) da textura mini.
pub static PIXELS_MINI: [u32; 24] = [
    0x00000000, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF, 0x00000000, 0xFFFFFFFF, 0x00000000,
    0xFFFFFFFF, 0xFFFFFFFF, 0x00000000, 0xFFFFFFFF, 0x00000000, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF,
    0xFFFFFFFF, 0x00000000, 0x00000000, 0xFFFFFFFF, 0x00000000, 0xFFFFFFFF, 0xFFFFFFFF, 0x00000000,
];

/// Olhos da textura mini, na proporção da imagem original.
pub const OLHOS_MINI: [OlhoTexel; 2] = [
    OlhoTexel {
        x0: 1,
        y0: 1,
        x1: 1,
        y1: 1,
        pelagem: 0xFFFFFFFF,
        palpebra: 0xFFFFFFFF,
    },
    OlhoTexel {
        x0: 4,
        y0: 1,
        x1: 4,
        y1: 1,
        pelagem: 0xFFFFFFFF,
        palpebra: 0xFFFFFFFF,
    },
];
