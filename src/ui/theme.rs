//! Tema visual do Kestrel: cores, estilos e helpers de apresentação.
//!
//! Tudo centralizado aqui para manter a interface coerente e legível em
//! terminais escuros e claros.

use crate::events::StatusPasso;
use crate::perm::PermissionLevel;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders};

/// Cor de fundo do aplicativo: azul-acinzentado escuro, não preto.
///
/// O preto absoluto (7,8,11) parecia OLED/alto contrate e cansava a vista em
/// sessões longas. É aplicado em toda a interface (inclusive modais), então os
/// componentes desenhados por cima mantêm esse fundo.
pub const FUNDO: Color = Color::Rgb(24, 27, 34);

/// Estilo com o fundo do aplicativo.
pub fn fundo() -> Style {
    Style::default().bg(FUNDO)
}

/// Bloco padrão do Kestrel: contorno arredondado em toda a interface (nada de
/// retângulos duros). Os chamadores só precisam definir estilo e títulos.
pub fn bloco() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
}

/// Texto principal.
pub fn texto() -> Style {
    Style::default().fg(Color::Rgb(226, 228, 233))
}

/// Texto secundário/apagado.
pub fn apagado() -> Style {
    Style::default().fg(Color::Rgb(128, 134, 148))
}

/// Título de seção.
pub fn titulo() -> Style {
    Style::default()
        .fg(Color::Rgb(255, 196, 110))
        .add_modifier(Modifier::BOLD)
}

/// Rótulos e cabeçalhos.
pub fn rotulo() -> Style {
    Style::default()
        .fg(Color::Rgb(150, 180, 230))
        .add_modifier(Modifier::BOLD)
}

pub fn borda() -> Style {
    Style::default().fg(Color::Rgb(84, 92, 112))
}

pub fn borda_ativa() -> Style {
    Style::default().fg(Color::Rgb(120, 180, 255))
}

/// Mensagens do usuário.
pub fn usuario() -> Style {
    Style::default()
        .fg(Color::Rgb(140, 220, 255))
        .add_modifier(Modifier::BOLD)
}

/// Raciocínio do modelo (mostrado como nota discreta).
pub fn raciocinio() -> Style {
    Style::default()
        .fg(Color::Rgb(138, 132, 168))
        .add_modifier(Modifier::ITALIC)
}

pub fn sucesso() -> Style {
    Style::default().fg(Color::Rgb(126, 214, 152))
}

pub fn falha() -> Style {
    Style::default().fg(Color::Rgb(240, 128, 128))
}

pub fn aviso() -> Style {
    Style::default().fg(Color::Rgb(240, 200, 120))
}

pub fn info() -> Style {
    Style::default().fg(Color::Rgb(140, 190, 240))
}

pub fn em_execucao() -> Style {
    Style::default().fg(Color::Rgb(255, 196, 110))
}

/// Cor associada ao estado da mascote/turno.
pub fn cor_nivel(nivel: PermissionLevel) -> Color {
    match nivel {
        PermissionLevel::Read => Color::Rgb(126, 200, 240),
        PermissionLevel::Write => Color::Rgb(150, 210, 160),
        PermissionLevel::Semi => Color::Rgb(240, 200, 120),
        PermissionLevel::Full => Color::Rgb(240, 130, 130),
    }
}

/// Cor associada a um passo de plano/subtarefa.
pub fn cor_status(status: StatusPasso) -> Color {
    match status {
        StatusPasso::Pendente => Color::Rgb(150, 160, 175),
        StatusPasso::EmAndamento => Color::Rgb(255, 196, 110),
        StatusPasso::Concluido => Color::Rgb(126, 214, 152),
        StatusPasso::Falhou => Color::Rgb(240, 128, 128),
        StatusPasso::Pulado => Color::Rgb(128, 134, 148),
    }
}

/// Cor do indicador de uso do contexto.
pub fn cor_contexto(fracao: f32) -> Color {
    if fracao >= 0.9 {
        Color::Rgb(240, 128, 128)
    } else if fracao >= 0.7 {
        Color::Rgb(240, 200, 120)
    } else {
        Color::Rgb(126, 214, 152)
    }
}

/// Estilo de título de modal.
pub fn modal_borda() -> Style {
    Style::default().fg(Color::Rgb(120, 180, 255))
}

/// Tecla de ação em modais (pílulas coloridas, sem colchetes quadrados).
pub fn botao_permitir() -> Style {
    Style::default()
        .fg(Color::Rgb(14, 26, 18))
        .bg(Color::Rgb(150, 210, 160))
        .add_modifier(Modifier::BOLD)
}

pub fn botao_sempre() -> Style {
    Style::default()
        .fg(Color::Rgb(14, 22, 32))
        .bg(Color::Rgb(150, 190, 240))
        .add_modifier(Modifier::BOLD)
}

pub fn botao_negar() -> Style {
    Style::default()
        .fg(Color::Rgb(34, 14, 16))
        .bg(Color::Rgb(232, 150, 150))
        .add_modifier(Modifier::BOLD)
}

/// Fundo destacado (código inline, blocos de código).
pub fn codigo_fundo() -> Color {
    Color::Rgb(38, 42, 54)
}
