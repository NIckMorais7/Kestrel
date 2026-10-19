//! Tema visual do Kestrel: paleta, estilos e helpers de apresentação.
//!
//! A aparência segue a reformulação da v0.4: fundo **preto**, acento **branco**,
//! cinzas e poucas bordas — direção estilo Claude Code pedida pelo usuário.
//! A skin é uma preferência de sessão (`/skin`) e pode ser trocada em runtime;
//! os estilos consultam a paleta atual, então nenhum widget precisa saber qual
//! skin está ativa.
//!
//! Além das três prontas, o Kestrel lê skins próprias de
//! `~/.config/kestrel/skins/<nome>.toml`. Cada chave é uma cor hexadecimal
//! (`#rrggbb`); as chaves ausentes herdam a skin `kestrel`. O nome da skin é o
//! nome do arquivo (sem `.toml`), o que mantém o `/skin` estável em qualquer
//! idioma.

use crate::events::StatusPasso;
use crate::perm::PermissionLevel;
use ratatui::style::{Color, Modifier, Style};
use ratatui::widgets::{Block, BorderType, Borders};
use std::path::Path;
use std::sync::{OnceLock, RwLock};

/// Conjunto completo de cores de uma skin.
#[derive(Clone, Debug)]
pub struct Paleta {
    pub nome: String,
    pub fundo: Color,
    /// Superfície levemente elevada: campo de mensagem e modais.
    pub superficie: Color,
    pub texto: Color,
    pub apagado: Color,
    pub acento: Color,
    /// Acento mais escuro para prefixos e metadados.
    pub acento_fraco: Color,
    pub borda: Color,
    pub borda_ativa: Color,
    pub usuario: Color,
    pub raciocinio: Color,
    pub sucesso: Color,
    pub falha: Color,
    pub aviso: Color,
    pub info: Color,
    pub execucao: Color,
    pub codigo_fundo: Color,
    pub selecao_fundo: Color,
    pub selecao_texto: Color,
}

impl Paleta {
    /// Padrão `kestrel`: preto/branco, no estilo Claude Code — fundo preto,
    /// texto branco, acento branco e o resto em cinzas. O ciano saiu de cena.
    fn kestrel() -> Self {
        Self {
            nome: "kestrel".to_string(),
            fundo: Color::Rgb(14, 14, 14),
            superficie: Color::Rgb(24, 24, 24),
            texto: Color::Rgb(219, 219, 219),
            apagado: Color::Rgb(128, 128, 128),
            acento: Color::Rgb(238, 238, 238),
            acento_fraco: Color::Rgb(150, 150, 150),
            borda: Color::Rgb(52, 52, 52),
            borda_ativa: Color::Rgb(180, 180, 180),
            usuario: Color::Rgb(238, 238, 238),
            raciocinio: Color::Rgb(150, 150, 150),
            sucesso: Color::Rgb(190, 190, 190),
            falha: Color::Rgb(160, 160, 160),
            aviso: Color::Rgb(210, 210, 210),
            info: Color::Rgb(180, 180, 180),
            execucao: Color::Rgb(210, 210, 210),
            codigo_fundo: Color::Rgb(32, 32, 32),
            selecao_fundo: Color::Rgb(225, 225, 225),
            selecao_texto: Color::Rgb(14, 14, 14),
        }
    }

    fn mono() -> Self {
        Self {
            nome: "mono".to_string(),
            fundo: Color::Rgb(0, 0, 0),
            superficie: Color::Rgb(10, 10, 10),
            texto: Color::Rgb(255, 255, 255),
            apagado: Color::Rgb(178, 178, 178),
            acento: Color::Rgb(255, 255, 255),
            acento_fraco: Color::Rgb(208, 208, 208),
            borda: Color::Rgb(46, 46, 46),
            borda_ativa: Color::Rgb(255, 255, 255),
            usuario: Color::Rgb(255, 255, 255),
            raciocinio: Color::Rgb(168, 168, 168),
            sucesso: Color::Rgb(224, 224, 224),
            falha: Color::Rgb(199, 74, 70),
            aviso: Color::Rgb(196, 148, 70),
            info: Color::Rgb(214, 214, 214),
            execucao: Color::Rgb(224, 224, 224),
            codigo_fundo: Color::Rgb(18, 18, 18),
            selecao_fundo: Color::Rgb(255, 255, 255),
            selecao_texto: Color::Rgb(0, 0, 0),
        }
    }

    fn daylight() -> Self {
        Self {
            nome: "daylight".to_string(),
            fundo: Color::Rgb(250, 249, 245),
            superficie: Color::Rgb(255, 255, 252),
            texto: Color::Rgb(33, 31, 28),
            apagado: Color::Rgb(112, 106, 98),
            acento: Color::Rgb(28, 27, 25),
            acento_fraco: Color::Rgb(118, 110, 101),
            borda: Color::Rgb(216, 211, 202),
            borda_ativa: Color::Rgb(28, 27, 25),
            usuario: Color::Rgb(33, 31, 28),
            raciocinio: Color::Rgb(118, 111, 103),
            sucesso: Color::Rgb(56, 126, 78),
            falha: Color::Rgb(174, 62, 58),
            aviso: Color::Rgb(146, 108, 37),
            info: Color::Rgb(58, 104, 145),
            execucao: Color::Rgb(146, 108, 37),
            codigo_fundo: Color::Rgb(241, 238, 231),
            selecao_fundo: Color::Rgb(28, 27, 25),
            selecao_texto: Color::Rgb(255, 255, 252),
        }
    }

    #[cfg(test)]
    fn por_nome(nome: &str) -> Option<Self> {
        match nome.trim().to_lowercase().as_str() {
            "kestrel" | "padrao" | "padrão" => Some(Self::kestrel()),
            "mono" | "monocromatico" | "monocromático" => Some(Self::mono()),
            "daylight" | "claro" | "light" => Some(Self::daylight()),
            _ => None,
        }
    }
}

static PALETA: OnceLock<RwLock<Paleta>> = OnceLock::new();
static REGISTRO: OnceLock<RwLock<Vec<Paleta>>> = OnceLock::new();

fn com<T>(f: impl FnOnce(&Paleta) -> T) -> T {
    let guard = PALETA
        .get_or_init(|| RwLock::new(Paleta::kestrel()))
        .read()
        .unwrap_or_else(|envenenado| envenenado.into_inner());
    f(&guard)
}

fn com_registro<T>(f: impl FnOnce(&Vec<Paleta>) -> T) -> T {
    let guard = REGISTRO
        .get_or_init(|| RwLock::new(vec![Paleta::kestrel(), Paleta::mono(), Paleta::daylight()]))
        .read()
        .unwrap_or_else(|envenenado| envenenado.into_inner());
    f(&guard)
}

/// Nomes de skin aceitos: `[a-z0-9_-]`, no máximo 32 caracteres. É assim que o
/// nome do arquivo vira um nome de comando sem surpresas.
fn nome_valido(nome: &str) -> bool {
    !nome.is_empty()
        && nome.chars().count() <= 32
        && nome
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
}

/// Converte `#rrggbb` (ou `rrggbb`) em cor. Não aceita nomes: a skin precisa
/// ser explícita para renderizar igual em qualquer terminal.
fn cor_hex(valor: &str) -> Result<Color, String> {
    let texto = valor.trim().trim_start_matches('#');
    if texto.len() != 6 || !texto.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!(
            "cor `{valor}` inválida: use `#rrggbb` (ex.: `#0d0f13`)"
        ));
    }
    let canal = |fatia: &str| -> Result<u8, String> {
        u8::from_str_radix(fatia, 16).map_err(|_| format!("cor `{valor}` inválida"))
    };
    let r = canal(&texto[0..2])?;
    let g = canal(&texto[2..4])?;
    let b = canal(&texto[4..6])?;
    Ok(Color::Rgb(r, g, b))
}

fn cor_toml(valor: &toml::Value, chave: &str) -> Result<Color, String> {
    match valor {
        toml::Value::String(s) => cor_hex(s),
        outro => Err(format!(
            "a chave `{chave}` deve ser uma cor em texto (`#rrggbb`), não `{outro}`"
        )),
    }
}

/// Lê uma skin própria do disco. Campos ausentes herdam a `kestrel`.
fn paleta_de_arquivo(caminho: &Path) -> Result<Paleta, String> {
    let nome = caminho
        .file_stem()
        .and_then(|n| n.to_str())
        .map(|n| n.to_lowercase())
        .ok_or_else(|| format!("nome de arquivo inválido: {}", caminho.display()))?;
    if !nome_valido(&nome) {
        return Err(format!(
            "nome de skin inválido em {}: use letras minúsculas, números, `-` ou `_` (até 32)",
            caminho.display()
        ));
    }
    let texto = std::fs::read_to_string(caminho).map_err(|e| format!("falha ao ler skin: {e}"))?;
    let valores: toml::Value = toml::from_str(&texto)
        .map_err(|e| format!("skin `{nome}` inválida em {}: {e}", caminho.display()))?;

    let mut paleta = Paleta::kestrel();
    paleta.nome = nome.clone();
    macro_rules! cor {
        ($chave:literal, $campo:ident) => {
            if let Some(valor) = valores.get($chave) {
                paleta.$campo = cor_toml(valor, $chave)?;
            }
        };
    }
    cor!("fundo", fundo);
    cor!("superficie", superficie);
    cor!("texto", texto);
    cor!("apagado", apagado);
    cor!("acento", acento);
    cor!("acento_fraco", acento_fraco);
    cor!("borda", borda);
    cor!("borda_ativa", borda_ativa);
    cor!("usuario", usuario);
    cor!("raciocinio", raciocinio);
    cor!("sucesso", sucesso);
    cor!("falha", falha);
    cor!("aviso", aviso);
    cor!("info", info);
    cor!("execucao", execucao);
    cor!("codigo_fundo", codigo_fundo);
    cor!("selecao_fundo", selecao_fundo);
    cor!("selecao_texto", selecao_texto);
    Ok(paleta)
}

/// Carrega todas as skins próprias válidas do diretório. Arquivos inválidos são
/// ignorados: uma skin quebrada não pode impedir o Kestrel de abrir.
pub fn carregar_paletas(dir: &Path) -> Vec<Paleta> {
    let Ok(entradas) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut paletas = Vec::new();
    for entrada in entradas.flatten() {
        let caminho = entrada.path();
        if caminho.extension().and_then(|e| e.to_str()) != Some("toml") || !caminho.is_file() {
            continue;
        }
        if let Ok(paleta) = paleta_de_arquivo(&caminho) {
            paletas.push(paleta);
        }
    }
    paletas.sort_by_key(|p| p.nome.clone());
    paletas
}

/// Instala a lista completa de skins (prontas + próprias) no registro.
fn instalar_registro(mut paletas: Vec<Paleta>) {
    let mut builtins = vec![Paleta::kestrel(), Paleta::mono(), Paleta::daylight()];
    builtins.append(&mut paletas);
    let mut guard = REGISTRO
        .get_or_init(|| RwLock::new(builtins.clone()))
        .write()
        .unwrap_or_else(|envenenado| envenenado.into_inner());
    *guard = builtins;
}

/// Decide a skin quando nenhuma foi configurada (ou a configurada não existe).
/// `COLORFGBG` é o único sinal lido: os terminais que não o exportam seguem no
/// escuro, que é o padrão do Kestrel. `KESTREL_SKIN` continua mandando mais que
/// tudo e é resolvido por quem chama `inicializar`.
fn skin_automatica() -> &'static str {
    if fundo_claro_em(std::env::var("COLORFGBG").ok().as_deref()) {
        "daylight"
    } else {
        "mono"
    }
}

/// Interpreta o valor de `COLORFGBG` (`foreground;background`). Fundos claros
/// (7/15/white) pedem a skin clara; o resto segue no escuro.
fn fundo_claro_em(valor: Option<&str>) -> bool {
    let Some(valor) = valor else {
        return false;
    };
    let fundo = valor.rsplit(';').next().unwrap_or("").trim();
    matches!(
        fundo,
        "7" | "15" | "white" | "light" | "lightgray" | "lightgrey"
    )
}

/// Inicializa o tema no começo do processo. Devolve o nome da skin aplicada.
/// Uma preferência inválida cai na detecção automática em vez de travar a
/// interface — o usuário sempre pode corrigir com `/skin`.
pub fn inicializar(dir: &Path, preferida: &str) -> String {
    instalar_registro(carregar_paletas(dir));
    let escolhida = preferida.trim().to_lowercase();
    if !escolhida.is_empty() && aplicar_skin(&escolhida).is_ok() {
        return skin_atual();
    }
    let auto = skin_automatica();
    let _ = aplicar_skin(auto);
    auto.to_string()
}

/// Recarrega as skins próprias do disco sem sair da interface. Preserva a
/// skin atual se ela ainda existir; se foi removida, cai na automática.
/// Devolve o nome aplicado.
pub fn recarregar(dir: &Path) -> String {
    let atual = skin_atual();
    instalar_registro(carregar_paletas(dir));
    if aplicar_skin(&atual).is_ok() {
        return atual;
    }
    let auto = skin_automatica();
    let _ = aplicar_skin(auto);
    auto.to_string()
}

/// Nomes das skins disponíveis (prontas + próprias carregadas).
pub fn skins() -> Vec<String> {
    com_registro(|paletas| paletas.iter().map(|p| p.nome.clone()).collect())
}

/// Aplica uma skin do registro. Devolve erro claro quando o nome não existe.
pub fn aplicar_skin(nome: &str) -> Result<(), String> {
    let procurado = nome.trim().to_lowercase();
    let paleta = com_registro(|paletas| paletas.iter().find(|p| p.nome == procurado).cloned())
        .ok_or_else(|| {
            format!(
                "skin `{nome}` não existe. Disponíveis: {}.",
                skins().join(", ")
            )
        })?;
    let mut guard = PALETA
        .get_or_init(|| RwLock::new(Paleta::kestrel()))
        .write()
        .unwrap_or_else(|envenenado| envenenado.into_inner());
    *guard = paleta;
    Ok(())
}

/// Nome da skin ativa.
pub fn skin_atual() -> String {
    com(|p| p.nome.clone())
}

/// Cor de fundo da aplicação.
pub fn fundo_cor() -> Color {
    com(|p| p.fundo)
}

/// Cor da superfície elevada.
pub fn superficie_cor() -> Color {
    com(|p| p.superficie)
}

/// Estilo com o fundo do aplicativo.
pub fn fundo() -> Style {
    Style::default().bg(fundo_cor())
}

/// Estilo com a superfície elevada (campo de mensagem, modais).
pub fn superficie() -> Style {
    Style::default().bg(superficie_cor())
}

/// Bloco padrão do Kestrel: contorno arredondado. Use com parcimônia — a
/// conversa não tem borda; o campo de mensagem e os modais têm.
pub fn bloco() -> Block<'static> {
    Block::default()
        .borders(Borders::ALL)
        .border_type(BorderType::Rounded)
}

/// Texto principal.
pub fn texto() -> Style {
    Style::default().fg(com(|p| p.texto))
}

/// Texto secundário/apagado.
pub fn apagado() -> Style {
    Style::default().fg(com(|p| p.apagado))
}

/// Título de seção.
pub fn titulo() -> Style {
    Style::default()
        .fg(com(|p| p.acento))
        .add_modifier(Modifier::BOLD)
}

/// Rótulos e cabeçalhos.
pub fn rotulo() -> Style {
    Style::default()
        .fg(com(|p| p.acento_fraco))
        .add_modifier(Modifier::BOLD)
}

/// Acento do prompt e de links.
pub fn acento() -> Style {
    Style::default().fg(com(|p| p.acento))
}

pub fn acento_fraco() -> Style {
    Style::default().fg(com(|p| p.acento_fraco))
}

pub fn borda() -> Style {
    Style::default().fg(com(|p| p.borda))
}

pub fn borda_ativa() -> Style {
    Style::default().fg(com(|p| p.borda_ativa))
}

pub fn selecao_fundo() -> Color {
    com(|p| p.selecao_fundo)
}

pub fn selecao_texto() -> Color {
    com(|p| p.selecao_texto)
}

/// Mensagens do usuário.
pub fn usuario() -> Style {
    Style::default()
        .fg(com(|p| p.usuario))
        .add_modifier(Modifier::BOLD)
}

/// Raciocínio do modelo (mostrado como nota discreta).
pub fn raciocinio() -> Style {
    Style::default()
        .fg(com(|p| p.raciocinio))
        .add_modifier(Modifier::ITALIC)
}

pub fn sucesso() -> Style {
    Style::default().fg(com(|p| p.sucesso))
}

pub fn falha() -> Style {
    Style::default().fg(com(|p| p.falha))
}

pub fn aviso() -> Style {
    Style::default().fg(com(|p| p.aviso))
}

pub fn info() -> Style {
    Style::default().fg(com(|p| p.info))
}

pub fn em_execucao() -> Style {
    Style::default().fg(com(|p| p.execucao))
}

/// Cor associada ao nível de permissão.
pub fn cor_nivel(nivel: PermissionLevel) -> Color {
    match nivel {
        PermissionLevel::Read => com(|p| p.info),
        PermissionLevel::Write => com(|p| p.sucesso),
        PermissionLevel::Semi => com(|p| p.aviso),
        PermissionLevel::Full => com(|p| p.falha),
    }
}

/// Cor associada a um passo de plano/subtarefa.
pub fn cor_status(status: StatusPasso) -> Color {
    match status {
        StatusPasso::Pendente => com(|p| p.apagado),
        StatusPasso::EmAndamento => com(|p| p.execucao),
        StatusPasso::Concluido => com(|p| p.sucesso),
        StatusPasso::Falhou => com(|p| p.falha),
        StatusPasso::Pulado => com(|p| p.apagado),
    }
}

/// Cor do indicador de uso do contexto.
pub fn cor_contexto(fracao: f32) -> Color {
    if fracao >= 0.9 {
        com(|p| p.falha)
    } else if fracao >= 0.7 {
        com(|p| p.aviso)
    } else {
        com(|p| p.sucesso)
    }
}

/// Pulso curto e leve usado em glifos de atividade.
///
/// Não é cor de fundo nem brilho em texto: só o símbolo alterna entre acento e
/// apagado. Rápido o suficiente para perceber, lento o suficiente para não
/// piscar. Quando não há atividade, fica fixo no acento.
pub fn pulso(tique: u64, ativo: bool) -> Style {
    if !ativo {
        return acento();
    }
    // Dois tiques por fase: ~240 ms durante um turno (cadência de 120 ms) e
    // ~500 ms quando parado (250 ms).
    let fase = (tique / 2) % 4;
    if fase < 2 { acento() } else { acento_fraco() }
}

/// Estilo de borda de modal.
pub fn modal_borda() -> Style {
    Style::default().fg(com(|p| p.acento_fraco))
}

/// Tecla de ação em modais (pílulas, sem colchetes quadrados).
pub fn botao_permitir() -> Style {
    Style::default()
        .fg(com(|p| p.fundo))
        .bg(com(|p| p.sucesso))
        .add_modifier(Modifier::BOLD)
}

pub fn botao_sempre() -> Style {
    Style::default()
        .fg(com(|p| p.fundo))
        .bg(com(|p| p.info))
        .add_modifier(Modifier::BOLD)
}

pub fn botao_negar() -> Style {
    Style::default()
        .fg(com(|p| p.fundo))
        .bg(com(|p| p.falha))
        .add_modifier(Modifier::BOLD)
}

/// Fundo destacado (código inline, blocos de código).
pub fn codigo_fundo() -> Color {
    com(|p| p.codigo_fundo)
}

#[cfg(test)]
mod testes {
    use super::*;

    #[test]
    fn skins_tem_contraste_e_nomes_estaveis() {
        // Registro limpo: só as prontas (sem skins do diretório do usuário).
        instalar_registro(Vec::new());
        for nome in skins() {
            let paleta = Paleta::por_nome(&nome).expect("skin pronta");
            assert_eq!(paleta.nome, nome);
            assert_ne!(paleta.fundo, paleta.texto);
            assert_ne!(paleta.acento, paleta.fundo);
        }
        assert!(Paleta::por_nome("nao-existe").is_none());
        // Depois da troca pedida, `mono` é o padrão: fundo preto, acento
        // branco. A `kestrel` ficou como a cinza antiga.
        let padrao = Paleta::por_nome("mono").expect("skin padrão");
        assert_eq!(padrao.fundo, Color::Rgb(0, 0, 0));
        assert_eq!(padrao.acento, Color::Rgb(255, 255, 255));
        assert_eq!(
            Paleta::por_nome("kestrel").unwrap().fundo,
            Color::Rgb(14, 14, 14)
        );
    }

    #[test]
    fn aplicar_skin_desconhecida_nao_quebra() {
        instalar_registro(Vec::new());
        let erro = aplicar_skin("nao-existe").unwrap_err();
        assert!(erro.contains("kestrel"));
        assert!(erro.contains("daylight"));
    }

    #[test]
    fn troca_de_skin_muda_a_paleta_e_volta_ao_padrao() {
        // Este teste mexe no estado global; restaura no fim para não afetar os
        // testes de interface que rodam em paralelo.
        instalar_registro(Vec::new());
        aplicar_skin("mono").unwrap();
        assert_eq!(skin_atual(), "mono");
        assert_eq!(fundo_cor(), Color::Rgb(0, 0, 0));
        aplicar_skin("kestrel").unwrap();
        assert_eq!(skin_atual(), "kestrel");
        assert_eq!(fundo_cor(), Color::Rgb(14, 14, 14));
    }

    #[test]
    fn cor_hex_aceita_formato_curto_e_recusa_lixo() {
        assert_eq!(cor_hex("#0d0f13").unwrap(), Color::Rgb(13, 15, 19));
        assert_eq!(cor_hex("6cb6c4").unwrap(), Color::Rgb(108, 182, 196));
        assert!(cor_hex("#123").is_err());
        assert!(cor_hex("vermelho").is_err());
    }

    #[test]
    fn skin_propria_parcial_herda_a_kestrel() {
        let dir = std::env::temp_dir().join(format!("kestrel-skin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("azul.toml"),
            "fundo = \"#000010\"\nacento = \"#2040ff\"\n",
        )
        .unwrap();
        let paleta = paleta_de_arquivo(&dir.join("azul.toml")).unwrap();
        assert_eq!(paleta.nome, "azul");
        assert_eq!(paleta.fundo, Color::Rgb(0, 0, 16));
        assert_eq!(paleta.acento, Color::Rgb(32, 64, 255));
        // Sem `texto`, herda a cor de texto da `kestrel`.
        assert_eq!(paleta.texto, Paleta::kestrel().texto);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn skin_propria_invalida_nao_entra_na_lista() {
        let dir = std::env::temp_dir().join(format!("kestrel-skin-bad-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("ruim.toml"), "fundo = \"#xyzxyz\"\n").unwrap();
        std::fs::write(dir.join("nao-e-toml.txt"), "ignorado").unwrap();
        assert!(carregar_paletas(&dir).is_empty());
        assert!(paleta_de_arquivo(&dir.join("ruim.toml")).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn inicializar_carrega_skin_propria_e_cai_para_o_automatico() {
        let dir = std::env::temp_dir().join(format!("kestrel-skin-init-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("azul.toml"), "fundo = \"#000018\"\n").unwrap();
        assert_eq!(inicializar(&dir, "azul"), "azul");
        assert_eq!(skin_atual(), "azul");
        // Preferência inválida não trava: cai no automático (escuro aqui).
        let aplicada = inicializar(&dir, "nao-existe");
        assert_eq!(aplicada, skin_automatica());
        assert_eq!(skin_atual(), aplicada);
        // Um diretório vazio também funciona (só as prontas).
        let vazio =
            std::env::temp_dir().join(format!("kestrel-skin-vazio-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&vazio).unwrap();
        assert_eq!(inicializar(&vazio, "daylight"), "daylight");
        assert_eq!(skin_atual(), "daylight");
        // Restaura o padrão para o restante dos testes.
        assert_eq!(inicializar(&vazio, "kestrel"), "kestrel");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&vazio);
    }

    #[test]
    fn recarregar_pega_skins_criadas_depois_e_perde_as_removidas() {
        let dir = std::env::temp_dir().join(format!("kestrel-skin-live-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(inicializar(&dir, "kestrel"), "kestrel");
        std::fs::write(dir.join("azul.toml"), "fundo = \"#000018\"\n").unwrap();
        assert_eq!(recarregar(&dir), "kestrel");
        assert!(
            skins().iter().any(|s| s == "azul"),
            "/skin deveria ver a nova skin"
        );
        aplicar_skin("azul").unwrap();
        let _ = std::fs::remove_file(dir.join("azul.toml"));
        let aplicada = recarregar(&dir);
        assert_ne!(aplicada, "azul", "skin removida não pode continuar ativa");
        assert!(!skins().iter().any(|s| s == "azul"));
        // Restaura o padrão para o restante dos testes.
        assert_eq!(inicializar(&dir, "kestrel"), "kestrel");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fundo_colorfgbg_claro_e_escuro() {
        assert!(fundo_claro_em(Some("15;7")));
        assert!(fundo_claro_em(Some("white;15")));
        assert!(!fundo_claro_em(Some("15;0")));
        assert!(!fundo_claro_em(Some("15;default")));
        assert!(!fundo_claro_em(None));
    }
}
