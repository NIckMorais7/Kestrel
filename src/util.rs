//! Utilitários gerais: caminhos, estimativa de tokens, redação de segredos e
//! pequenas funções de formatação usadas em toda a aplicação.

use regex::Regex;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

/// Caminhos persistentes da aplicação. Pode ser redirecionado em testes para
/// evitar tocar nos diretórios reais do usuário.
#[derive(Clone, Debug)]
pub struct Paths {
    pub config_dir: PathBuf,
    pub data_dir: PathBuf,
}

impl Paths {
    /// Descobre os caminhos reais, respeitando `KESTREL_CONFIG_DIR` e
    /// `KESTREL_DATA_DIR` e as convenções XDG.
    pub fn discover() -> Self {
        let config_dir = std::env::var_os("KESTREL_CONFIG_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::config_dir().map(|d| d.join("kestrel")))
            .unwrap_or_else(|| PathBuf::from(".kestrel/config"));
        let data_dir = std::env::var_os("KESTREL_DATA_DIR")
            .map(PathBuf::from)
            .or_else(|| dirs::data_dir().map(|d| d.join("kestrel")))
            .unwrap_or_else(|| PathBuf::from(".kestrel/data"));
        Self {
            config_dir,
            data_dir,
        }
    }

    /// Caminhos isolados sob uma raiz, para testes.
    #[cfg(test)]
    pub fn with_root(root: impl AsRef<Path>) -> Self {
        let root = root.as_ref();
        Self {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
        }
    }

    /// Caminhos com diretórios informados explicitamente (linha de comando),
    /// caindo para os caminhos descobertos do ambiente quando ausentes.
    pub fn custom(config_dir: Option<PathBuf>, data_dir: Option<PathBuf>) -> Self {
        let mut paths = Self::discover();
        if let Some(dir) = config_dir {
            paths.config_dir = crate::util::expand_tilde(&dir.to_string_lossy());
        }
        if let Some(dir) = data_dir {
            paths.data_dir = crate::util::expand_tilde(&dir.to_string_lossy());
        }
        paths
    }

    pub fn config_file(&self) -> PathBuf {
        self.config_dir.join("config.toml")
    }

    pub fn skills_dir(&self) -> PathBuf {
        self.config_dir.join("skills")
    }

    /// Personalidades (Comps) criadas pelo usuário.
    pub fn comps_dir(&self) -> PathBuf {
        self.config_dir.join("comps")
    }

    /// Skins personalizadas (`<nome>.toml`); as prontas vivem no código.
    pub fn skins_dir(&self) -> PathBuf {
        self.config_dir.join("skins")
    }

    pub fn db_file(&self) -> PathBuf {
        self.data_dir.join("kestrel.db")
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.data_dir.join("logs")
    }

    pub fn sessions_dir(&self) -> PathBuf {
        self.data_dir.join("sessions")
    }

    pub fn history_file(&self) -> PathBuf {
        self.data_dir.join("historico_entrada.txt")
    }

    pub fn ensure(&self) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.config_dir)?;
        std::fs::create_dir_all(self.skills_dir())?;
        std::fs::create_dir_all(self.comps_dir())?;
        std::fs::create_dir_all(self.skins_dir())?;
        std::fs::create_dir_all(&self.data_dir)?;
        std::fs::create_dir_all(self.logs_dir())?;
        std::fs::create_dir_all(self.sessions_dir())?;
        Ok(())
    }
}

/// Estimativa rápida de tokens (~4 caracteres por token, mínimo 1).
pub fn estimate_tokens(text: &str) -> u32 {
    let chars = text.chars().count();
    (chars.div_ceil(4)).max(1) as u32
}

/// Timestamp atual em RFC 3339 (UTC).
pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// Converte um timestamp RFC 3339 em string local legível.
pub fn format_timestamp(ts: &str) -> String {
    match chrono::DateTime::parse_from_rfc3339(ts) {
        Ok(dt) => dt
            .with_timezone(&chrono::Local)
            .format("%d/%m %H:%M")
            .to_string(),
        Err(_) => ts.chars().take(16).collect(),
    }
}

/// Procura um binário no PATH (ou um caminho absoluto/relativo direto).
///
/// O arquivo precisa ser executável: só `is_file()` aceitava qualquer binário
/// do PATH que por acaso fosse só leitura.
pub fn which(bin: &str) -> Option<PathBuf> {
    fn executavel(p: &Path) -> bool {
        if !p.is_file() {
            return false;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            p.metadata()
                .map(|m| m.permissions().mode() & 0o111 != 0)
                .unwrap_or(false)
        }
        #[cfg(not(unix))]
        {
            true
        }
    }

    if bin.contains('/') {
        let p = PathBuf::from(bin);
        return executavel(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(bin))
        .find(|candidate| executavel(candidate))
}

/// Trunca um texto em `max` caracteres **no total**, reticências incluídas.
pub fn truncate(text: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Largura exibida de um texto no terminal (caracteres largos contam 2).
pub fn largura_texto(texto: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(texto)
}

/// Remove sequências de escape ANSI de um texto.
///
/// Cobre CSI (`\x1b[…m`), OSC (`\x1b]…\x07`, inclui OSC 52 de clipboard),
/// DCS (`\x1bP…\x1b\\`), escapes de dois bytes, seleção de charset e a
/// variante de 8 bits do CSI (`\x9b`). Antes só CSI era removido e um título
/// OSC continuava injetando texto/escape no terminal.
pub fn strip_ansi(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(concat!(
            r"\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)",
            r"|\x1bP[^\x1b]*(?:\x1b\\|\x07)",
            r"|\x1b\[[0-9;?]*[ -/]*[@-~]",
            r"|\x1b[()][0-9A-Za-z]",
            r"|\x1b[@-Z\\-_]",
            r"|\x9b[0-9;?]*[ -/]*[@-~]",
        ))
        .expect("regex ANSI válida")
    });
    re.replace_all(input, "").to_string()
}

/// Redige segredos conhecidos (chaves de API, tokens, cabeçalhos Bearer e
/// atribuições do tipo `api_key = ...`).
///
/// O README promete que chaves nunca são gravadas: esta função é aplicada a
/// tudo que vai para o banco (auditoria, `tool_runs`, diffs).
pub fn redact_secrets(input: &str) -> String {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(
            r#"(?ix)(
                nvapi-[A-Za-z0-9_\-]{4,}
              | sk-[A-Za-z0-9_\-]{4,}
              | gh[pousr]_[A-Za-z0-9]{16,}
              | github_pat_[A-Za-z0-9_]{20,}
              | AIza[0-9A-Za-z_\-]{20,}
              | xox[baprs]-[A-Za-z0-9\-]{10,}
              | glpat-[A-Za-z0-9_\-]{20,}
              | AKIA[0-9A-Z]{16}
              | bearer\s+[A-Za-z0-9._\-]{4,}
              | -----BEGIN[A-Z\ ]*PRIVATE\ KEY-----
              | \b(?:api[_-]?key|apikey|secret[_-]?key|access[_-]?token
                   |auth[_-]?token|refresh[_-]?token|client[_-]?secret
                   |senha|password|passwd|pwd|authorization)\b
                \s*[:=]\s*['"]?[^\s'"]{6,}
            )"#,
        )
        .expect("regex de redação válida")
    });
    re.replace_all(input, "[SEGREDO-REDIGIDO]").to_string()
}

/// Formata um número grande com separador de milhar (12.4k, 1,2M).
pub fn formatar_num(n: u64) -> String {
    if n < 1000 {
        return n.to_string();
    }
    if n < 1_000_000 {
        return format!("{:.1}k", n as f64 / 1000.0);
    }
    format!("{:.1}M", n as f64 / 1_000_000.0)
}

/// Formata bytes de forma legível.
pub fn human_bytes(bytes: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut value = bytes as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit < UNITS.len() - 1 {
        value /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{value:.1} {}", UNITS[unit])
    }
}

/// Formata uma duração em milissegundos.
pub fn format_duration_ms(ms: u64) -> String {
    if ms < 1000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1000.0)
    } else {
        let mins = ms / 60_000;
        let secs = (ms % 60_000) / 1000;
        format!("{mins}m{secs:02}s")
    }
}

/// Caminho relativo a `base` quando possível, senão o caminho completo.
pub fn display_path(base: &Path, path: &Path) -> String {
    match path.strip_prefix(base) {
        Ok(rel) => {
            let texto = rel.display().to_string();
            if texto.is_empty() {
                ".".to_string()
            } else {
                texto
            }
        }
        Err(_) => path.display().to_string(),
    }
}

/// Caminho curto para exibição no cabeçalho: `$HOME` vira `~` e, se ainda
/// ficar grande, o começo é cortado para preservar o nome do projeto.
pub fn caminho_curto(path: &Path, max: usize) -> String {
    let texto = match std::env::var("HOME") {
        Ok(home) if !home.is_empty() => match path.strip_prefix(&home) {
            Ok(resto) => format!("~/{}", resto.display()),
            Err(_) => path.display().to_string(),
        },
        _ => path.display().to_string(),
    };
    if texto.chars().count() <= max {
        return texto;
    }
    let sufixo: String = texto
        .chars()
        .rev()
        .take(max.saturating_sub(1))
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    format!("…{sufixo}")
}

/// Expande `~` no início de um caminho.
pub fn expand_tilde(input: &str) -> PathBuf {
    if let Some(rest) = input.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    if input == "~"
        && let Some(home) = dirs::home_dir()
    {
        return home;
    }
    PathBuf::from(input)
}

/// Normaliza um caminho removendo componentes `.` e `..` sem exigir que exista.
pub fn normalize_path(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Conta linhas de um texto (0 para texto vazio).
pub fn count_lines(text: &str) -> usize {
    if text.is_empty() {
        0
    } else {
        text.lines().count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn estimativa_de_tokens_cresce_com_texto() {
        assert!(estimate_tokens("a") >= 1);
        assert!(estimate_tokens(&"palavra ".repeat(100)) > estimate_tokens("palavra"));
    }

    #[test]
    fn formata_numeros_grandes() {
        assert_eq!(formatar_num(0), "0");
        assert_eq!(formatar_num(999), "999");
        assert_eq!(formatar_num(12400), "12.4k");
        assert_eq!(formatar_num(2_470_000), "2.5M");
    }

    #[test]
    fn redige_chaves_de_api() {
        let redigido = redact_secrets("chave nvapi-1234567890abcdef e Bearer abc.def.ghi");
        assert!(!redigido.contains("nvapi-1234567890abcdef"));
        assert!(!redigido.contains("abc.def.ghi"));
        // Formatos conhecidos de outros provedores.
        for caso in [
            "token ghp_abcdefghijklmnopqrstuvxz",
            "github_pat_abcdefghijklmnopqrstuvwxyz012345",
            "AIzaSyA1234567890abcdefghij_klm",
            "slack xoxb-1234567890-abcdefghij",
            "aws AKIAIOSFODNN7EXAMPLE",
            "glpat-abcdefghijklmnopqrstuvwx",
        ] {
            assert!(
                redact_secrets(caso).contains("[SEGREDO-REDIGIDO]"),
                "{caso}"
            );
        }
        // Atribuições explícitas: o nome fica, o valor some.
        for caso in [
            "api_key = \"valor-super-secreto\"",
            "API_KEY: nvapi-abcdef123456",
            "password = 'senha12345'",
            "senha=segredo123",
            "authorization: Bearer abcdef1234567890",
        ] {
            assert!(
                redact_secrets(caso).contains("[SEGREDO-REDIGIDO]"),
                "{caso}"
            );
        }
        // Texto comum não é alterado.
        assert_eq!(
            redact_secrets("cargo build --release"),
            "cargo build --release"
        );
    }

    #[test]
    fn normaliza_caminhos() {
        assert_eq!(
            normalize_path(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
    }

    #[test]
    fn trunca_com_reticencias() {
        assert_eq!(truncate("abcdef", 3), "ab…");
        assert_eq!(truncate("ab", 5), "ab");
        assert_eq!(truncate("", 0), "");
        // O total nunca passa de `max`.
        for max in [1, 2, 5] {
            assert!(truncate("abcdefghij", max).chars().count() <= max);
        }
    }

    #[test]
    fn remove_escapes_ansi_variados() {
        assert_eq!(strip_ansi("\x1b[31mvermelho\x1b[0m"), "vermelho");
        // OSC de título e OSC 52 (clipboard), terminados por BEL ou ST.
        assert_eq!(strip_ansi("\x1b]0;titulo\x07texto"), "texto");
        assert_eq!(strip_ansi("\x1b]52;c;cGF5bG9hZA==\x1b\\fim"), "fim");
        // DCS e CSI de 8 bits também somem.
        assert_eq!(strip_ansi("\x1bPq#0;2;0;0;0\x1b\\ok"), "ok");
        assert_eq!(strip_ansi("\u{9b}31mcor\u{9b}0m"), "cor");
    }

    #[test]
    fn caminho_curto_guarda_o_nome_do_projeto() {
        // Caminho igual à base não vira vazio (aparecia um buraco no cabeçalho).
        assert_eq!(display_path(Path::new("/a/b"), Path::new("/a/b")), ".");
        assert_eq!(display_path(Path::new("/a"), Path::new("/a/b/c")), "b/c");
        // Caminhos curtos ficam intactos; longos perdem o começo, não o fim.
        assert_eq!(caminho_curto(Path::new("/tmp/projeto"), 40), "/tmp/projeto");
        let longo = caminho_curto(Path::new("/um/caminho/muito/longo/para/o/projeto"), 12);
        assert!(longo.ends_with("projeto"), "veio {longo}");
        assert_eq!(longo.chars().count(), 12);
    }

    #[test]
    fn formata_bytes_e_duracoes() {
        assert_eq!(human_bytes(512), "512 B");
        assert_eq!(human_bytes(2048), "2.0 KB");
        assert_eq!(format_duration_ms(500), "500ms");
        assert_eq!(format_duration_ms(1500), "1.5s");
    }
}
