//! Pesquisa web, navegação, downloads e renderização com navegador.
//!
//! A infraestrutura é adaptativa: HTTP puro quando é suficiente; navegador
//! headless apenas quando a página realmente exige JavaScript.

use super::{
    Tool, ToolContext, ToolError, ToolOutput, arg_str, arg_str_opt, arg_u64, recortar_saida,
    resolver_caminho,
};
use crate::html::{html_para_texto, percent_decode};
use crate::provider::ToolSpec;
use futures_util::StreamExt;
use regex::Regex;
use serde_json::{Value, json};
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::OnceLock;

/// Tamanho máximo de corpo lido por `fetch_url` (o restante é descartado).
const LIMITE_CORPO_FETCH: usize = 8 * 1024 * 1024;

/// Endereços que nunca devem ser acessados pelas ferramentas web.
///
/// Link-local (`169.254.0.0/16`) inclui o serviço de metadados de nuvem
/// (169.254.169.254), alvo clássico de SSRF; CGNAT e multicast também não são
/// destinos legítimos de uma busca. Loopback e redes privadas continuam
/// liberados de propósito: rodar algo em `localhost:3000` e abrir é uso normal.
fn ip_interno(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_multicast()
                || v4.is_broadcast()
                // 100.64.0.0/10 (CGNAT; inclui o metadata do Alibaba)
                || (v4.octets()[0] == 100 && (64..=127).contains(&v4.octets()[1]))
        }
        IpAddr::V6(v6) => {
            v6.is_unspecified()
                || v6.is_multicast()
                // Link-local fe80::/10.
                || (v6.segments()[0] & 0xffc0) == 0xfe80
                // Metadata IPv6 da AWS (fd00:ec2::254).
                || v6 == std::net::Ipv6Addr::new(0xfd00, 0xec2, 0, 0, 0, 0, 0, 0x254)
        }
    }
}

/// Valida esquema, host literal e IPs resolvidos antes de uma requisição.
async fn validar_url_externa(url: &str) -> Result<reqwest::Url, ToolError> {
    let parsed = reqwest::Url::parse(url)
        .map_err(|e| ToolError::Argumentos(format!("URL inválida `{url}`: {e}")))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(ToolError::Argumentos(
            "a URL deve usar http:// ou https://".into(),
        ));
    }
    let Some(host) = parsed.host_str() else {
        return Err(ToolError::Argumentos("a URL não tem host".into()));
    };
    // `host_str` devolve IPv6 entre colchetes; `IpAddr` não aceita.
    let host_limpo = host.trim_start_matches('[').trim_end_matches(']');
    if let Ok(ip) = host_limpo.parse::<IpAddr>() {
        if ip_interno(ip) {
            return Err(ToolError::Permissao(format!(
                "acesso a endereço interno bloqueado: {host}"
            )));
        }
        return Ok(parsed);
    }
    // Nome (inclusive `metadata.google.internal`): resolve antes e confere
    // todos os endereços. Se a resolução falhar, deixa a requisição reportar.
    let porta = parsed.port_or_known_default().unwrap_or(443);
    if let Ok(enderecos) = tokio::net::lookup_host((host_limpo, porta)).await {
        for endereco in enderecos {
            if ip_interno(endereco.ip()) {
                return Err(ToolError::Permissao(format!(
                    "acesso a endereço interno bloqueado: {host} → {}",
                    endereco.ip()
                )));
            }
        }
    }
    Ok(parsed)
}

/// Navegadores conhecidos, na ordem de preferência.
const NAVEGADORES: &[&str] = &[
    "chromium",
    "chromium-browser",
    "google-chrome",
    "google-chrome-stable",
    "brave-browser",
    "microsoft-edge",
    "firefox",
];

pub fn detectar_navegador(preferido: Option<&str>) -> Option<PathBuf> {
    if let Some(bin) = preferido
        && let Some(caminho) = crate::util::which(bin)
    {
        return Some(caminho);
    }
    NAVEGADORES.iter().find_map(|nome| crate::util::which(nome))
}

fn regexes() -> &'static Regexes {
    static R: OnceLock<Regexes> = OnceLock::new();
    R.get_or_init(|| Regexes {
        ddg_resultado: Regex::new(
            r#"(?is)<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="([^"]+)"[^>]*>(.*?)</a>"#,
        )
        .expect("regex"),
        ddg_snippet: Regex::new(r#"(?is)<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(.*?)</a>"#)
            .expect("regex"),
        tags: Regex::new(r"(?s)<[^>]+>").expect("regex"),
        bloqueio: Regex::new(r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>")
            .expect("regex"),
    })
}

struct Regexes {
    ddg_resultado: Regex,
    ddg_snippet: Regex,
    tags: Regex,
    bloqueio: Regex,
}

fn limpar(fragmento: &str) -> String {
    let r = regexes();
    let sem_blocos = r.bloqueio.replace_all(fragmento, "");
    let sem_tags = r.tags.replace_all(&sem_blocos, "");
    crate::html::decodificar_entidades(&sem_tags)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Extrai a URL real de um link do DuckDuckGo (`/l/?uddg=...`).
fn url_real(href: &str) -> String {
    if let Some(pos) = href.find("uddg=") {
        let resto = &href[pos + 5..];
        let fim = resto.find('&').unwrap_or(resto.len());
        return percent_decode(&resto[..fim]);
    }
    if href.starts_with("//") {
        return format!("https:{href}");
    }
    href.to_string()
}

pub struct WebSearch;

#[async_trait::async_trait]
impl Tool for WebSearch {
    fn nome(&self) -> &'static str {
        "web_search"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Pesquisa na web e devolve título, URL e resumo dos resultados. Use \
                depois fetch_url para ler o conteúdo relevante. O mecanismo padrão é o DuckDuckGo \
                (sem necessidade de chave)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "consulta": {"type": "string"},
                    "max_resultados": {"type": "integer", "description": "Padrão 8"},
                    "idioma": {"type": "string", "description": "Código de idioma, ex.: pt-br"}
                },
                "required": ["consulta"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        crate::util::truncate(&arg_str_opt(args, "consulta").unwrap_or_default(), 60)
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let consulta = arg_str(&args, "consulta")?;
        let max = arg_u64(&args, "max_resultados").unwrap_or(8) as usize;
        let idioma = arg_str_opt(&args, "idioma").unwrap_or_else(|| "pt-br".into());

        if ctx.web.engine == "searx"
            && let Some(base) = &ctx.web.searx_url
        {
            return buscar_searx(ctx, base, &consulta, max).await;
        }
        buscar_duckduckgo(ctx, &consulta, &idioma, max).await
    }
}

async fn buscar_duckduckgo(
    ctx: &ToolContext,
    consulta: &str,
    idioma: &str,
    max: usize,
) -> Result<ToolOutput, ToolError> {
    let resposta = ctx
        .http
        .post("https://html.duckduckgo.com/html/")
        .header("Accept", "text/html")
        .form(&[("q", consulta), ("kl", idioma)])
        .send()
        .await
        .map_err(|e| {
            ToolError::msg(format!(
                "falha na pesquisa (rede): {e}. Verifique sua conexão ou use fetch_url diretamente."
            ))
        })?;
    if !resposta.status().is_success() {
        return Err(ToolError::msg(format!(
            "o mecanismo de pesquisa respondeu HTTP {}",
            resposta.status()
        )));
    }
    let corpo = resposta.text().await.unwrap_or_default();
    let r = regexes();
    let titulos: Vec<(String, String)> = r
        .ddg_resultado
        .captures_iter(&corpo)
        .map(|c| {
            (
                limpar(c.get(2).map(|m| m.as_str()).unwrap_or_default()),
                url_real(c.get(1).map(|m| m.as_str()).unwrap_or_default()),
            )
        })
        .filter(|(t, _)| !t.is_empty())
        .collect();
    let snippets: Vec<String> = r
        .ddg_snippet
        .captures_iter(&corpo)
        .map(|c| limpar(c.get(1).map(|m| m.as_str()).unwrap_or_default()))
        .collect();

    if titulos.is_empty() {
        return Ok(ToolOutput::texto(format!(
            "Nenhum resultado encontrado para `{consulta}`. \
             O mecanismo pode ter solicitado captcha; tente reformular a consulta ou use fetch_url em uma URL conhecida."
        )));
    }
    let mut linhas = Vec::new();
    for (i, (titulo, url)) in titulos.iter().take(max).enumerate() {
        linhas.push(format!(
            "{}. {}\n   {}\n   {}",
            i + 1,
            titulo,
            url,
            snippets.get(i).cloned().unwrap_or_default()
        ));
    }
    Ok(ToolOutput::texto(format!(
        "Resultados para `{consulta}` ({}):\n\n{}",
        ctx.web.engine,
        linhas.join("\n")
    ))
    .com_resumo(format!(
        "{} resultados para \"{}\"",
        linhas.len(),
        crate::util::truncate(consulta, 40)
    )))
}

async fn buscar_searx(
    ctx: &ToolContext,
    base: &str,
    consulta: &str,
    max: usize,
) -> Result<ToolOutput, ToolError> {
    let url = format!("{}/search", base.trim_end_matches('/'));
    let resposta = ctx
        .http
        .get(&url)
        .query(&[("q", consulta), ("format", "json")])
        .send()
        .await
        .map_err(|e| ToolError::msg(format!("falha ao consultar o SearxNG: {e}")))?;
    if !resposta.status().is_success() {
        return Err(ToolError::msg(format!(
            "o SearxNG respondeu HTTP {}",
            resposta.status()
        )));
    }
    let valor: Value = resposta
        .json()
        .await
        .map_err(|e| ToolError::msg(format!("resposta inválida do SearxNG: {e}")))?;
    let mut linhas = Vec::new();
    if let Some(resultados) = valor.get("results").and_then(|r| r.as_array()) {
        for (i, item) in resultados.iter().take(max).enumerate() {
            linhas.push(format!(
                "{}. {}\n   {}\n   {}",
                i + 1,
                item.get("title").and_then(|t| t.as_str()).unwrap_or(""),
                item.get("url").and_then(|u| u.as_str()).unwrap_or(""),
                item.get("content").and_then(|c| c.as_str()).unwrap_or("")
            ));
        }
    }
    if linhas.is_empty() {
        return Ok(ToolOutput::texto(format!(
            "Nenhum resultado no SearxNG para `{consulta}`."
        )));
    }
    Ok(ToolOutput::texto(format!(
        "Resultados para `{consulta}` (searx):\n\n{}",
        linhas.join("\n")
    )))
}

pub struct FetchUrl;

#[async_trait::async_trait]
impl Tool for FetchUrl {
    fn nome(&self) -> &'static str {
        "fetch_url"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Abre uma URL por HTTP e devolve o conteúdo em texto legível (HTML \
                convertido), com título, links principais e limites de tamanho. Use `formato: \
                \"html\"` para receber o HTML bruto."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "max_caracteres": {"type": "integer", "description": "Limite de texto (padrão 20000)"},
                    "formato": {"type": "string", "description": "texto | html"},
                    "incluir_links": {"type": "boolean", "description": "Lista links encontrados (padrão true)"}
                },
                "required": ["url"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        crate::util::truncate(&arg_str_opt(args, "url").unwrap_or_default(), 80)
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let url = arg_str(&args, "url")?;
        let url = validar_url_externa(&url).await?;
        let max = arg_u64(&args, "max_caracteres").unwrap_or(20_000) as usize;
        let formato = arg_str_opt(&args, "formato").unwrap_or_else(|| "texto".into());
        let incluir_links = args
            .get("incluir_links")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let resposta = ctx
            .http
            .get(url.clone())
            .header(
                "Accept",
                "text/html,application/xhtml+xml,application/json;q=0.9,text/plain;q=0.8,*/*;q=0.5",
            )
            .send()
            .await
            .map_err(|e| ToolError::msg(format!("falha ao abrir {url}: {e}")))?;
        let status = resposta.status();
        let tipo = resposta
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        if !status.is_success() {
            let dica = match status.as_u16() {
                401 | 403 => {
                    "A página pode exigir autenticação; o Kestrel não envia credenciais automaticamente."
                }
                404 => "A URL não existe (404).",
                429 => "O site limitou as requisições (429); aguarde e tente novamente.",
                _ => "Erro HTTP do servidor.",
            };
            return Err(ToolError::msg(format!(
                "{url} respondeu HTTP {status}. {dica}"
            )));
        }

        // Lê com teto: `.text()` carregava a resposta inteira na memória.
        let corpo = ler_corpo_limitado(resposta, LIMITE_CORPO_FETCH)
            .await
            .map_err(|e| ToolError::msg(format!("falha ao ler o corpo de {url}: {e}")))?;

        if formato == "html"
            || (!tipo.contains("html") && !tipo.is_empty() && !tipo.contains("text"))
        {
            return Ok(ToolOutput::texto(format!(
                "Conteúdo de {url} (content-type: {tipo}):\n\n{}",
                recortar_saida(&corpo, max)
            ))
            .com_resumo(format!("abriu {url}")));
        }

        let pagina = html_para_texto(&corpo);
        let mut partes = Vec::new();
        if !pagina.titulo.is_empty() {
            partes.push(format!("Título: {}", pagina.titulo));
        }
        partes.push(format!("URL: {url}"));
        partes.push(String::new());
        partes.push(recortar_saida(&pagina.texto, max));
        if incluir_links && !pagina.links.is_empty() {
            let links: Vec<String> = pagina
                .links
                .iter()
                .take(40)
                .map(|(texto, href)| format!("- {texto}: {href}"))
                .collect();
            partes.push(format!(
                "\nLinks encontrados ({}):\n{}",
                pagina.links.len(),
                links.join("\n")
            ));
        }
        let texto = partes.join("\n");
        Ok(ToolOutput::texto(texto).com_resumo(format!(
            "abriu {} ({} caracteres)",
            crate::util::truncate(url.as_str(), 50),
            pagina.texto.chars().count()
        )))
    }
}

/// Lê o corpo de uma resposta com teto de bytes, em streaming.
async fn ler_corpo_limitado(resposta: reqwest::Response, limite: usize) -> Result<String, String> {
    if let Some(tamanho) = resposta.content_length()
        && tamanho as usize > limite
    {
        return Err(format!(
            "o corpo tem {} e excede o limite de {}",
            crate::util::human_bytes(tamanho),
            crate::util::human_bytes(limite as u64)
        ));
    }
    let mut bytes: Vec<u8> = Vec::new();
    let mut stream = resposta.bytes_stream();
    while let Some(pedaco) = stream.next().await {
        let pedaco = pedaco.map_err(|e| e.to_string())?;
        let restante = limite.saturating_sub(bytes.len());
        if pedaco.len() >= restante {
            bytes.extend_from_slice(&pedaco[..restante]);
            break;
        }
        bytes.extend_from_slice(&pedaco);
    }
    Ok(String::from_utf8_lossy(&bytes).to_string())
}

pub struct DownloadFile;

#[async_trait::async_trait]
impl Tool for DownloadFile {
    fn nome(&self) -> &'static str {
        "download_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Baixa um recurso da web para um arquivo local, com limite de tamanho. \
                O conteúdo baixado não é executado automaticamente: inspecione-o antes de usar."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "caminho": {"type": "string", "description": "Destino relativo ao projeto"},
                    "max_bytes": {"type": "integer", "description": "Limite de tamanho (padrão: configuração, 50MB)"}
                },
                "required": ["url", "caminho"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "caminho")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    fn detalhe(&self, args: &Value) -> String {
        format!(
            "{} → {}",
            crate::util::truncate(&arg_str_opt(args, "url").unwrap_or_default(), 50),
            arg_str_opt(args, "caminho").unwrap_or_default()
        )
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(600)
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let url = arg_str(&args, "url")?;
        let url = validar_url_externa(&url).await?;
        let destino = resolver_caminho(ctx, &arg_str(&args, "caminho")?);
        // O pedido do modelo não pode furar o limite da configuração (antes
        // `max_bytes` era honrado sem teto e enchia o disco).
        let limite = arg_u64(&args, "max_bytes")
            .unwrap_or(ctx.web.max_download_bytes)
            .min(ctx.web.max_download_bytes);

        let resposta = ctx
            .http
            .get(url.clone())
            .send()
            .await
            .map_err(|e| ToolError::msg(format!("falha ao baixar {url}: {e}")))?;
        if !resposta.status().is_success() {
            return Err(ToolError::msg(format!(
                "{url} respondeu HTTP {}",
                resposta.status()
            )));
        }
        if let Some(tamanho) = resposta.content_length()
            && tamanho > limite
        {
            return Err(ToolError::msg(format!(
                "o recurso tem {} e excede o limite de {}",
                crate::util::human_bytes(tamanho),
                crate::util::human_bytes(limite)
            )));
        }

        if let Some(pai) = destino.parent() {
            std::fs::create_dir_all(pai)
                .map_err(|e| ToolError::msg(format!("falha ao criar {}: {e}", pai.display())))?;
        }
        use tokio::io::AsyncWriteExt;
        let temporario = destino.with_extension("kestrel-download");
        let mut arquivo = tokio::fs::File::create(&temporario)
            .await
            .map_err(|e| ToolError::msg(format!("falha ao criar {}: {e}", temporario.display())))?;

        let mut total: u64 = 0;
        let mut stream = resposta.bytes_stream();
        while let Some(pedaco) = stream.next().await {
            let bytes = match pedaco {
                Ok(bytes) => bytes,
                Err(e) => {
                    drop(arquivo);
                    let _ = std::fs::remove_file(&temporario);
                    return Err(ToolError::msg(format!("falha durante o download: {e}")));
                }
            };
            total += bytes.len() as u64;
            if total > limite {
                drop(arquivo);
                let _ = std::fs::remove_file(&temporario);
                return Err(ToolError::msg(format!(
                    "download interrompido: excedeu o limite de {}",
                    crate::util::human_bytes(limite)
                )));
            }
            if let Err(e) = arquivo.write_all(&bytes).await {
                drop(arquivo);
                let _ = std::fs::remove_file(&temporario);
                return Err(ToolError::msg(format!("falha ao gravar o arquivo: {e}")));
            }
        }
        if let Err(e) = arquivo.flush().await {
            drop(arquivo);
            let _ = std::fs::remove_file(&temporario);
            return Err(ToolError::msg(format!("falha ao finalizar o arquivo: {e}")));
        }
        drop(arquivo);
        if let Err(e) = std::fs::rename(&temporario, &destino) {
            let _ = std::fs::remove_file(&temporario);
            return Err(ToolError::msg(format!(
                "falha ao finalizar {}: {e}",
                destino.display()
            )));
        }

        // Identificação básica do conteúdo (sem executar nada).
        let amostra = std::fs::read(&destino)
            .map(|b| b.into_iter().take(8).collect::<Vec<u8>>())
            .unwrap_or_default();
        let tipo = if amostra.starts_with(b"\x7fELF") {
            "executável ELF"
        } else if amostra.starts_with(b"PK\x03\x04") {
            "arquivo ZIP (ou formato baseado em ZIP)"
        } else if amostra.starts_with(b"%PDF") {
            "documento PDF"
        } else if amostra.starts_with(b"\x1f\x8b") {
            "arquivo gzip"
        } else if amostra
            .iter()
            .all(|b| !b.is_ascii_control() || *b == b'\n' || *b == b'\t')
        {
            "texto"
        } else {
            "binário"
        };
        ctx.store
            .record_decision(
                &ctx.session_id,
                "download",
                &format!(
                    "baixou {url} para {}",
                    crate::util::display_path(&ctx.workdir, &destino)
                ),
                Some(&format!("{} bytes", total)),
            )
            .ok();
        Ok(ToolOutput::texto(format!(
            "Baixado: {url} → {} ({}, {tipo}).\nO conteúdo NÃO foi executado. Inspecione o arquivo antes de usá-lo.",
            destino.display(),
            crate::util::human_bytes(total)
        ))
        .com_resumo(format!("baixou {} ({})", crate::util::display_path(&ctx.workdir, &destino), crate::util::human_bytes(total))))
    }
}

pub struct BrowserRender;

#[async_trait::async_trait]
impl Tool for BrowserRender {
    fn nome(&self) -> &'static str {
        "browser_render"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Renderiza uma página com JavaScript usando um navegador headless \
                instalado no sistema (necessário para sites que dependem de JS). Se nenhum \
                navegador estiver disponível, a ferramenta informa e você deve usar fetch_url."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string"},
                    "espera_ms": {"type": "integer", "description": "Tempo virtual de espera para JS (padrão 6000)"},
                    "max_caracteres": {"type": "integer", "description": "Padrão 20000"}
                },
                "required": ["url"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        crate::util::truncate(&arg_str_opt(args, "url").unwrap_or_default(), 80)
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(180)
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let url = arg_str(&args, "url")?;
        let url = validar_url_externa(&url).await?;
        let espera = arg_u64(&args, "espera_ms").unwrap_or(6000);
        let max = arg_u64(&args, "max_caracteres").unwrap_or(20_000) as usize;
        let navegador = detectar_navegador(ctx.web.browser_bin.as_deref()).ok_or_else(|| {
            ToolError::msg(format!(
                "nenhum navegador headless foi encontrado (procurei por: {}). \
                 Instale o chromium ou defina `web.browser_bin` na configuração. \
                 Enquanto isso, use fetch_url para páginas sem JavaScript.",
                NAVEGADORES.join(", ")
            ))
        })?;

        let espera_texto = espera.to_string();
        let mut comando = tokio::process::Command::new(&navegador);
        comando
            .args([
                "--headless=new",
                "--disable-gpu",
                // Sem `--no-sandbox`: o padrão do navegador é a sandbox; abrir
                // mão dela por padrão transformava qualquer página renderizada
                // em execução de JS sem isolamento.
                "--hide-scrollbars",
                "--virtual-time-budget",
                &espera_texto,
                "--dump-dom",
                url.as_str(),
            ])
            .kill_on_drop(true);
        let saida = comando.output().await.map_err(|e| {
            ToolError::msg(format!("falha ao executar {}: {e}", navegador.display()))
        })?;
        if !saida.status.success() {
            return Err(ToolError::msg(format!(
                "o navegador terminou com código {:?}: {}",
                saida.status.code(),
                crate::util::truncate(&String::from_utf8_lossy(&saida.stderr), 400)
            )));
        }
        let dom = String::from_utf8_lossy(&saida.stdout).to_string();
        if dom.trim().is_empty() {
            return Err(ToolError::msg(
                "o navegador não devolveu conteúdo; tente aumentar `espera_ms` ou use fetch_url",
            ));
        }
        let pagina = html_para_texto(&dom);
        Ok(ToolOutput::texto(format!(
            "Renderizado com {} (JavaScript executado).\nTítulo: {}\nURL: {url}\n\n{}",
            navegador
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default(),
            pagina.titulo,
            recortar_saida(&pagina.texto, max)
        ))
        .com_resumo(format!("renderizou {url}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extrai_url_real_do_redirect() {
        let href = "/l/?uddg=https%3A%2F%2Fdoc.rust-lang.org%2Fbook%2F&rut=abc";
        assert_eq!(url_real(href), "https://doc.rust-lang.org/book/");
        assert_eq!(url_real("https://exemplo.com"), "https://exemplo.com");
        assert_eq!(url_real("//exemplo.com/x"), "https://exemplo.com/x");
    }

    #[test]
    fn limpa_tags_e_entidades() {
        assert_eq!(limpar("<b>Título</b> &amp; <i>mais</i>"), "Título & mais");
    }

    #[test]
    fn detecta_navegador_sem_configuracao() {
        // Não garante presença, mas não deve entrar em pânico.
        let _ = detectar_navegador(Some("binario-inexistente-xyz"));
    }

    #[tokio::test]
    async fn bloqueia_enderecos_internos_e_esquemas_invalidos() {
        for url in [
            "http://169.254.169.254/latest/meta-data/",
            "http://100.100.100.200/latest/meta-data/",
            "http://[fe80::1]/",
            "http://0.0.0.0/",
            "ftp://exemplo.com/x",
            "file:///etc/passwd",
        ] {
            assert!(
                validar_url_externa(url).await.is_err(),
                "deveria bloquear: {url}"
            );
        }
        // Loopback (servidor local de desenvolvimento) continua permitido.
        assert!(validar_url_externa("http://127.0.0.1:3000").await.is_ok());
        assert!(validar_url_externa("http://localhost:8080").await.is_ok());
    }

    #[test]
    fn ip_interno_cobre_metadados_de_nuvem() {
        use std::net::IpAddr;
        for ip in ["169.254.169.254", "100.100.100.200", "0.0.0.0", "fe80::1"] {
            assert!(ip_interno(ip.parse::<IpAddr>().unwrap()), "{ip}");
        }
        for ip in ["127.0.0.1", "192.168.1.10", "10.0.0.1", "8.8.8.8", "::1"] {
            assert!(!ip_interno(ip.parse::<IpAddr>().unwrap()), "{ip}");
        }
    }
}
