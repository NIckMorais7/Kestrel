//! Conversão de HTML em texto simples e extração de links, sem depender de um
//! motor de renderização. Usado pela navegação web e pelos downloads.

use regex::Regex;
use std::sync::OnceLock;

/// Resultado da conversão.
#[derive(Clone, Debug, Default)]
pub struct PaginaTexto {
    pub titulo: String,
    pub texto: String,
    pub links: Vec<(String, String)>,
}

fn regex_compilada() -> &'static Regexes {
    static R: OnceLock<Regexes> = OnceLock::new();
    R.get_or_init(|| Regexes {
        titulo: Regex::new(r"(?is)<title[^>]*>(.*?)</title>").expect("regex"),
        blocos_ignorados: Regex::new(
            r"(?is)<head[^>]*>.*?</head>|<script[^>]*>.*?</script>|<style[^>]*>.*?</style>|<noscript[^>]*>.*?</noscript>|<svg[^>]*>.*?</svg>|<template[^>]*>.*?</template>|<iframe[^>]*>.*?</iframe>",
        )
        .expect("regex"),
        comentarios: Regex::new(r"(?s)<!--.*?-->").expect("regex"),
        quebras: Regex::new(r"(?i)<(br|/p|/div|/li|/tr|/h[1-6]|/section|/article|/pre|/table)[^>]*>")
            .expect("regex"),
        itens: Regex::new(r"(?i)<li[^>]*>").expect("regex"),
        tags: Regex::new(r"(?s)<[^>]+>").expect("regex"),
        links: Regex::new(r#"(?is)<a\s+[^>]*href\s*=\s*["']([^"']+)["'][^>]*>(.*?)</a>"#)
            .expect("regex"),
        espacos: Regex::new(r"[ \t\u{00a0}]+").expect("regex"),
        linhas_vazias: Regex::new(r"\n{3,}").expect("regex"),
    })
}

struct Regexes {
    titulo: Regex,
    blocos_ignorados: Regex,
    comentarios: Regex,
    quebras: Regex,
    itens: Regex,
    tags: Regex,
    links: Regex,
    espacos: Regex,
    linhas_vazias: Regex,
}

/// Converte HTML em texto legível.
pub fn html_para_texto(html: &str) -> PaginaTexto {
    let r = regex_compilada();
    // O título também pode conter tags (`<title>Olá <b>mundo</b></title>`) e
    // títulos dentro de comentários não são o título real da página.
    let sem_comentarios = r.comentarios.replace_all(html, " ");
    let titulo = r
        .titulo
        .captures(&sem_comentarios)
        .and_then(|c| c.get(1))
        .map(|m| {
            let sem_tags = r.tags.replace_all(m.as_str(), "");
            limpar_texto(&decodificar_entidades(&sem_tags))
        })
        .unwrap_or_default();

    let mut links = Vec::new();
    for cap in r.links.captures_iter(html) {
        let href = cap.get(1).map(|m| m.as_str()).unwrap_or_default();
        let texto = cap
            .get(2)
            .map(|m| limpar_texto(&decodificar_entidades(&r.tags.replace_all(m.as_str(), ""))))
            .unwrap_or_default();
        if href.starts_with("http") && !texto.is_empty() && links.len() < 200 {
            links.push((texto, href.to_string()));
        }
    }

    let mut corpo = r.comentarios.replace_all(html, " ").to_string();
    corpo = r.blocos_ignorados.replace_all(&corpo, " ").to_string();
    corpo = r.itens.replace_all(&corpo, "\n- ").to_string();
    corpo = r.quebras.replace_all(&corpo, "\n").to_string();
    corpo = r.tags.replace_all(&corpo, "").to_string();
    corpo = decodificar_entidades(&corpo);
    corpo = r.espacos.replace_all(&corpo, " ").to_string();
    let mut texto = corpo
        .lines()
        .map(|l| l.trim().to_string())
        .collect::<Vec<_>>()
        .join("\n");
    texto = r.linhas_vazias.replace_all(&texto, "\n\n").to_string();

    PaginaTexto {
        titulo,
        texto: texto.trim().to_string(),
        links,
    }
}

fn limpar_texto(t: &str) -> String {
    regex_compilada()
        .espacos
        .replace_all(t, " ")
        .trim()
        .to_string()
}

/// Decodifica entidades HTML nomeadas e numéricas.
pub fn decodificar_entidades(texto: &str) -> String {
    let mut saida = String::with_capacity(texto.len());
    let mut chars = texto.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '&' {
            saida.push(c);
            continue;
        }
        let mut entidade = String::new();
        let mut fechou = false;
        for _ in 0..12 {
            match chars.peek() {
                Some(';') => {
                    chars.next();
                    fechou = true;
                    break;
                }
                Some(ch) if ch.is_alphanumeric() || *ch == '#' => {
                    entidade.push(*ch);
                    chars.next();
                }
                _ => break,
            }
        }
        if !fechou {
            saida.push('&');
            saida.push_str(&entidade);
            continue;
        }
        let decodificado = if let Some(hex) = entidade
            .strip_prefix("#x")
            .or_else(|| entidade.strip_prefix("#X"))
        {
            u32::from_str_radix(hex, 16).ok().and_then(char::from_u32)
        } else if let Some(num) = entidade.strip_prefix('#') {
            num.parse::<u32>().ok().and_then(char::from_u32)
        } else {
            match entidade.as_str() {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                "nbsp" => Some(' '),
                "mdash" => Some('—'),
                "ndash" => Some('–'),
                "hellip" => Some('…'),
                "lsquo" => Some('‘'),
                "rsquo" => Some('’'),
                "ldquo" => Some('“'),
                "rdquo" => Some('”'),
                "bull" => Some('•'),
                "middot" => Some('·'),
                "copy" => Some('©'),
                "reg" => Some('®'),
                "trade" => Some('™'),
                "times" => Some('×'),
                "hellipsis" => Some('…'),
                "euro" => Some('€'),
                "pound" => Some('£'),
                "deg" => Some('°'),
                _ => None,
            }
        };
        match decodificado {
            Some(ch) => saida.push(ch),
            None => {
                saida.push('&');
                saida.push_str(&entidade);
                saida.push(';');
            }
        }
    }
    saida
}

/// Decodifica percent-encoding de URLs (ex.: parâmetros `uddg=`).
pub fn percent_decode(input: &str) -> String {
    let bytes = input.as_bytes();
    let mut saida = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // `+` só significa espaço quando aparece literalmente na URL. Trocar
        // todo `+` depois de decodificar transformava `%2B` (um mais literal)
        // em espaço.
        if bytes[i] == b'+' {
            saida.push(b' ');
            i += 1;
            continue;
        }
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = std::str::from_utf8(&bytes[i + 1..i + 3]).unwrap_or("");
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                saida.push(v);
                i += 3;
                continue;
            }
        }
        saida.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&saida).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrai_titulo_e_texto() {
        let html = r#"<html><head><title>Exemplo &amp; Teste</title></head>
        <body><script>var x = 1;</script><h1>Olá</h1><p>Primeiro parágrafo.</p>
        <ul><li>um</li><li>dois</li></ul></body></html>"#;
        let pagina = html_para_texto(html);
        assert_eq!(pagina.titulo, "Exemplo & Teste");
        assert!(pagina.texto.contains("Olá"));
        assert!(!pagina.texto.contains("var x"), "script deve ser removido");
        assert!(pagina.texto.contains("- um"));
    }

    #[test]
    fn extrai_links() {
        let html = r#"<a href="https://exemplo.com/a">Um</a> <a href="/relativo">Rel</a>"#;
        let pagina = html_para_texto(html);
        assert_eq!(pagina.links.len(), 1);
        assert_eq!(pagina.links[0].1, "https://exemplo.com/a");
    }

    #[test]
    fn decodifica_entidades() {
        assert_eq!(
            decodificar_entidades("a &amp; b &lt;c&gt; &#65; &#x42;"),
            "a & b <c> A B"
        );
        assert_eq!(decodificar_entidades("&naoexiste;"), "&naoexiste;");
    }

    #[test]
    fn decodifica_percent_encoding() {
        assert_eq!(
            percent_decode("https%3A%2F%2Fexemplo.com%2F%3Fq%3D1"),
            "https://exemplo.com/?q=1"
        );
        assert_eq!(percent_decode("a+b"), "a b");
        // `%2B` é um mais literal, não o `+` de formulário.
        assert_eq!(percent_decode("a%2Bb"), "a+b");
        assert_eq!(percent_decode("100%25"), "100%");
    }

    #[test]
    fn titulo_remove_tags_e_ignora_comentarios() {
        let html = r#"<!-- <title>falso</title> -->
            <html><head><title>Olá <b>mundo</b></title></head>
            <body><p>corpo</p></body></html>"#;
        let pagina = html_para_texto(html);
        assert_eq!(pagina.titulo, "Olá mundo");
        assert!(!pagina.texto.contains("Olá mundo"), "cabeçalho duplicado");
    }

    #[test]
    fn remove_comentarios() {
        let pagina = html_para_texto("<p>a</p><!-- comentário --><p>b</p>");
        assert!(!pagina.texto.contains("comentário"));
        assert!(pagina.texto.contains('a') && pagina.texto.contains('b'));
    }
}
