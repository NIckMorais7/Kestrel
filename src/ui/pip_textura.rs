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
pub const LARGURA_MINI: usize = 3;
/// Altura da textura mini em texels.
pub const ALTURA_MINI: usize = 2;

/// Pixels (0xAARRGGBB; alfa 0 = vazado) da textura mini.
pub static PIXELS_MINI: [u32; 6] = [
    0x00000000, 0xFFFFFFFF, 0x00000000, 0xFFFFFFFF, 0xFFFFFFFF, 0xFFFFFFFF,
];

/// Olhos da textura mini, na proporção da imagem original.
pub const OLHOS_MINI: [OlhoTexel; 2] = [
    OlhoTexel {
        x0: 0,
        y0: 0,
        x1: 0,
        y1: 0,
        pelagem: 0xFFFFFFFF,
        palpebra: 0xFFFFFFFF,
    },
    OlhoTexel {
        x0: 2,
        y0: 0,
        x1: 2,
        y1: 0,
        pelagem: 0xFFFFFFFF,
        palpebra: 0xFFFFFFFF,
    },
];
