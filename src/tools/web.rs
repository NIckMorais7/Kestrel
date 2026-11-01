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
use std::time::Duration;
use tokio::io::AsyncReadExt;

/// Tamanho máximo de corpo lido por `fetch_url` (o restante é descartado).
const LIMITE_CORPO_FETCH: usize = 8 * 1024 * 1024;

/// Tamanho máximo de corpo lido de mecanismos de busca.
///
/// Antes `resposta.text()`/`.json()` carregavam tudo: um SearxNG mal
/// configurado (ou um servidor hostil) podia devolver gigabytes e esgotar a
/// memória do Kestrel.
const LIMITE_CORPO_BUSCA: usize = 4 * 1024 * 1024;

/// Quantos redirecionamentos seguimos manualmente antes de desistir.
const MAX_REDIRECIONAMENTOS: usize = 8;

/// Remove o arquivo parcial de um download em qualquer saída, inclusive
/// cancelamento/timeout que descarta o future.
struct LimparDownload(Option<PathBuf>);

impl LimparDownload {
    fn esquecer(&mut self) {
        self.0 = None;
    }
}

impl Drop for LimparDownload {
    fn drop(&mut self) {
        if let Some(caminho) = &self.0 {
            let _ = std::fs::remove_file(caminho);
        }
    }
}

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
            // Endereços IPv4 mapeados (`::ffff:169.254.169.254`) passavam
            // direto pela checagem e alcançavam o metadata da nuvem.
            if let Some(v4) = v6.to_ipv4() {
                return ip_interno(IpAddr::V4(v4));
            }
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

/// Cliente HTTP das ferramentas web com redirecionamento **manual**: o
/// automático do reqwest seguia um `Location` para `169.254.169.254` sem
/// passar pela validação de SSRF.
fn cliente_web(ctx: &ToolContext) -> Result<reqwest::Client, ToolError> {
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        // Revalida cada endereço no momento da conexão. A checagem em
        // `validar_url_externa` resolve o nome antes, mas o `reqwest` resolveria
        // de novo no `send` — uma resposta DNS diferente podia apontar para o
        // metadata da nuvem depois da validação (DNS rebinding).
        .dns_resolver(std::sync::Arc::new(ResolvedorSeguro))
        .connect_timeout(Duration::from_secs(20))
        .timeout(Duration::from_secs(ctx.web.request_timeout_secs.max(10)))
        .user_agent(ctx.web.user_agent.clone())
        .build()
        .map_err(|e| ToolError::msg(format!("falha ao preparar o cliente HTTP: {e}")))
}

/// Resolvedor que recusa qualquer endereço interno, inclusive os obtidos numa
/// segunda resolução feita pelo próprio cliente HTTP.
struct ResolvedorSeguro;

impl reqwest::dns::Resolve for ResolvedorSeguro {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        Box::pin(async move {
            let host = name.as_str().to_string();
            let enderecos = tokio::net::lookup_host((host.as_str(), 0))
                .await
                .map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)?;
            let mut seguros = Vec::new();
            for endereco in enderecos {
                if ip_interno(endereco.ip()) {
                    let erro = std::io::Error::new(
                        std::io::ErrorKind::PermissionDenied,
                        format!(
                            "acesso a endereço interno bloqueado: {host} → {}",
                            endereco.ip()
                        ),
                    );
                    return Err(Box::new(erro) as Box<dyn std::error::Error + Send + Sync>);
                }
                seguros.push(endereco);
            }
            if seguros.is_empty() {
                let erro = std::io::Error::new(
                    std::io::ErrorKind::AddrNotAvailable,
                    format!("o host {host} não resolveu para nenhum endereço"),
                );
                return Err(Box::new(erro) as Box<dyn std::error::Error + Send + Sync>);
            }
            Ok(Box::new(seguros.into_iter()) as reqwest::dns::Addrs)
        })
    }
}

/// Faz um GET validando cada salto de redirecionamento.
async fn get_com_redirecionamentos(
    cliente: &reqwest::Client,
    url_inicial: &str,
    accept: Option<&str>,
) -> Result<(reqwest::Url, reqwest::Response), ToolError> {
    let mut url = validar_url_externa(url_inicial).await?;
    for _ in 0..=MAX_REDIRECIONAMENTOS {
        let mut pedido = cliente.get(url.clone());
        if let Some(accept) = accept {
            pedido = pedido.header("Accept", accept);
        }
        let resposta = pedido
            .send()
            .await
            .map_err(|e| ToolError::msg(format!("falha ao abrir {url}: {e}")))?;
        if !resposta.status().is_redirection() {
            return Ok((url, resposta));
        }
        let Some(local) = resposta
            .headers()
            .get(reqwest::header::LOCATION)
            .and_then(|v| v.to_str().ok())
        else {
            return Ok((url, resposta));
        };
        let proxima = url
            .join(local)
            .map_err(|e| ToolError::msg(format!("redirecionamento inválido de {url}: {e}")))?;
        url = validar_url_externa(proxima.as_str()).await?;
    }
    Err(ToolError::msg(format!(
        "a URL {url_inicial} redirecionou mais de {MAX_REDIRECIONAMENTOS} vezes"
    )))
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
        ddg_snippet_div: Regex::new(
            r#"(?is)<(?:a|div|span)[^>]*class=["'][^"']*result__snippet[^"']*["'][^>]*>(.*?)</(?:a|div|span)>"#,
        )
        .expect("regex"),
        a_tag: Regex::new(r#"(?is)<a([^>]*)>(.*?)</a>"#).expect("regex"),
        href_attr: Regex::new(r#"(?is)href=["']([^"']+)["']"#).expect("regex"),
        lite_snippet: Regex::new(
            r#"(?is)<td[^>]*class=["'][^"']*result-snippet[^"']*["'][^>]*>(.*?)</td>"#,
        )
        .expect("regex"),
        tags: Regex::new(r"(?s)<[^>]+>").expect("regex"),
        bloqueio: Regex::new(r"(?is)<script[^>]*>.*?</script>|<style[^>]*>.*?</style>")
            .expect("regex"),
    })
}

struct Regexes {
    ddg_resultado: Regex,
    ddg_snippet: Regex,
    ddg_snippet_div: Regex,
    a_tag: Regex,
    href_attr: Regex,
    lite_snippet: Regex,
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
                depois fetch_url para ler o conteúdo relevante. O padrão é o DuckDuckGo; se ele \
                falhar ou não trouxer resultados, tenta DuckDuckGo Lite, DuckDuckGo HTML e Exa \
                (chave opcional em EXA_API_KEY)."
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
        let max = (arg_u64(&args, "max_resultados").unwrap_or(8) as usize).clamp(1, 20);
        let idioma = arg_str_opt(&args, "idioma").unwrap_or_else(|| "pt-br".into());
        buscar_com_fallback(ctx, &consulta, &idioma, max).await
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum MotorBusca {
    Searx,
    DuckDuckGo,
    DuckDuckGoLite,
    DuckDuckGoHtml,
    Exa,
}

impl MotorBusca {
    fn nome(self) -> &'static str {
        match self {
            MotorBusca::Searx => "searx",
            MotorBusca::DuckDuckGo => "duckduckgo",
            MotorBusca::DuckDuckGoLite => "duckduckgo lite",
            MotorBusca::DuckDuckGoHtml => "duckduckgo html",
            MotorBusca::Exa => "exa",
        }
    }

    fn url_ddg(self) -> Option<&'static str> {
        match self {
            MotorBusca::DuckDuckGo => Some("https://duckduckgo.com/html/"),
            MotorBusca::DuckDuckGoLite => Some("https://lite.duckduckgo.com/lite/"),
            MotorBusca::DuckDuckGoHtml => Some("https://html.duckduckgo.com/html/"),
            _ => None,
        }
    }
}

#[derive(Clone, Debug)]
struct ResultadoBusca {
    titulo: String,
    url: String,
    trecho: String,
}

/// Ordem da busca: Searx (se configurado), DuckDuckGo, DuckDuckGo Lite,
/// DuckDuckGo HTML e, por último, Exa (quando a chave está disponível).
fn planejar_motores(web: &crate::config::WebConfig) -> Vec<MotorBusca> {
    let mut motores = Vec::new();
    if web.engine.trim().eq_ignore_ascii_case("searx")
        && web
            .searx_url
            .as_deref()
            .is_some_and(|u| !u.trim().is_empty())
    {
        motores.push(MotorBusca::Searx);
    }
    motores.extend([
        MotorBusca::DuckDuckGo,
        MotorBusca::DuckDuckGoLite,
        MotorBusca::DuckDuckGoHtml,
        MotorBusca::Exa,
    ]);
    motores
}

async fn buscar_com_fallback(
    ctx: &ToolContext,
    consulta: &str,
    idioma: &str,
    max: usize,
) -> Result<ToolOutput, ToolError> {
    let motores = planejar_motores(&ctx.web);
    // Cada mecanismo tem um teto curto: um provedor pendurado não pode somar
    // minutos de espera aos demais fallbacks.
    let limite = std::time::Duration::from_secs(ctx.web.request_timeout_secs.clamp(5, 30));
    let mut obtidos = Vec::with_capacity(motores.len());
    for motor in motores {
        let resultado =
            match tokio::time::timeout(limite, executar_motor(ctx, motor, consulta, idioma, max))
                .await
            {
                Ok(resultado) => resultado,
                Err(_) => Err(format!("tempo limite de {}s", limite.as_secs())),
            };
        obtidos.push((motor, resultado));
    }
    consolidar(consulta, obtidos)
}

/// Aplica a ordem de fallback sobre os resultados já coletados.
fn consolidar(
    consulta: &str,
    obtidos: Vec<(MotorBusca, Result<Vec<ResultadoBusca>, String>)>,
) -> Result<ToolOutput, ToolError> {
    let mut falhas: Vec<String> = Vec::new();
    let mut respondeu_vazio = false;
    for (motor, resultado) in obtidos {
        match resultado {
            Ok(resultados) if !resultados.is_empty() => {
                return Ok(formatar_resultados(consulta, motor, &resultados, &falhas));
            }
            Ok(_) => {
                respondeu_vazio = true;
                falhas.push(format!("{}: sem resultados", motor.nome()));
            }
            Err(e) => falhas.push(format!("{}: {e}", motor.nome())),
        }
    }
    let detalhe = falhas.join("; ");
    if respondeu_vazio {
        Ok(ToolOutput::texto(format!(
            "Nenhum resultado para `{consulta}` em nenhum mecanismo. Tentativas: {detalhe}. \
             Reformule a consulta ou use fetch_url em uma URL conhecida."
        )))
    } else {
        Err(ToolError::msg(format!(
            "pesquisa web falhou em todos os mecanismos. Tentativas: {detalhe}. \
             Verifique a conexão ou configure a chave do Exa."
        )))
    }
}

async fn executar_motor(
    ctx: &ToolContext,
    motor: MotorBusca,
    consulta: &str,
    idioma: &str,
    max: usize,
) -> Result<Vec<ResultadoBusca>, String> {
    match motor {
        MotorBusca::Searx => {
            let base = ctx
                .web
                .searx_url
                .as_deref()
                .ok_or_else(|| "web.searx_url não definido".to_string())?;
            buscar_searx_resultados(ctx, base, consulta, max).await
        }
        MotorBusca::DuckDuckGo | MotorBusca::DuckDuckGoLite | MotorBusca::DuckDuckGoHtml => {
            let base = motor
                .url_ddg()
                .ok_or_else(|| "URL do DuckDuckGo ausente".to_string())?;
            buscar_ddg(
                ctx,
                base,
                consulta,
                idioma,
                max,
                motor == MotorBusca::DuckDuckGoLite,
            )
            .await
        }
        MotorBusca::Exa => buscar_exa(ctx, consulta, max).await,
    }
}

async fn buscar_ddg(
    ctx: &ToolContext,
    base: &str,
    consulta: &str,
    idioma: &str,
    max: usize,
    lite: bool,
) -> Result<Vec<ResultadoBusca>, String> {
    let cliente = cliente_web(ctx).map_err(|e| e.to_string())?;
    let mut url = reqwest::Url::parse(base).map_err(|e| format!("URL do DDG inválida: {e}"))?;
    url.query_pairs_mut()
        .append_pair("q", consulta)
        .append_pair("kl", idioma);
    let (_final, resposta) = get_com_redirecionamentos(&cliente, url.as_str(), Some("text/html"))
        .await
        .map_err(|e| e.to_string())?;
    if !resposta.status().is_success() {
        return Err(format!("HTTP {}", resposta.status()));
    }
    let corpo = ler_corpo_limitado(resposta, LIMITE_CORPO_BUSCA).await?;
    Ok(if lite {
        parse_lite(&corpo, max)
    } else {
        parse_ddg(&corpo, max)
    })
}

fn parse_ddg(html: &str, limite: usize) -> Vec<ResultadoBusca> {
    let r = regexes();
    let mut resultados: Vec<ResultadoBusca> = r
        .ddg_resultado
        .captures_iter(html)
        .filter_map(|c| {
            let titulo = limpar(c.get(2).map(|m| m.as_str()).unwrap_or_default());
            let url = url_real(c.get(1).map(|m| m.as_str()).unwrap_or_default());
            (!titulo.is_empty() && !url.is_empty()).then_some(ResultadoBusca {
                titulo,
                url,
                trecho: String::new(),
            })
        })
        .collect();
    if resultados.is_empty() {
        resultados = extrair_links(html, "result__a", limite)
            .into_iter()
            .map(|(url, titulo)| ResultadoBusca {
                titulo,
                url,
                trecho: String::new(),
            })
            .collect();
    }
    let mut trechos: Vec<String> = r
        .ddg_snippet
        .captures_iter(html)
        .map(|c| limpar(c.get(1).map(|m| m.as_str()).unwrap_or_default()))
        .collect();
    if trechos.is_empty() {
        trechos = r
            .ddg_snippet_div
            .captures_iter(html)
            .map(|c| limpar(c.get(1).map(|m| m.as_str()).unwrap_or_default()))
            .collect();
    }
    for (i, resultado) in resultados.iter_mut().enumerate() {
        resultado.trecho = trechos.get(i).cloned().unwrap_or_default();
    }
    resultados.truncate(limite);
    resultados
}

fn parse_lite(html: &str, limite: usize) -> Vec<ResultadoBusca> {
    let r = regexes();
    let trechos: Vec<String> = r
        .lite_snippet
        .captures_iter(html)
        .map(|c| limpar(c.get(1).map(|m| m.as_str()).unwrap_or_default()))
        .collect();
    extrair_links(html, "result-link", limite)
        .into_iter()
        .enumerate()
        .map(|(i, (url, titulo))| ResultadoBusca {
            titulo,
            url,
            trecho: trechos.get(i).cloned().unwrap_or_default(),
        })
        .collect()
}

/// Extrai links `<a>` cujo atributo de classe contém `classe`, sem depender da
/// ordem dos atributos (`href` antes ou depois de `class`).
fn extrair_links(html: &str, classe: &str, limite: usize) -> Vec<(String, String)> {
    let r = regexes();
    r.a_tag
        .captures_iter(html)
        .filter_map(|c| {
            let attrs = c.get(1)?.as_str().to_lowercase();
            if !attrs.contains(classe) {
                return None;
            }
            let href = r.href_attr.captures(c.get(1)?.as_str())?.get(1)?.as_str();
            if href.starts_with("javascript:") || href.starts_with('#') {
                return None;
            }
            let titulo = limpar(c.get(2)?.as_str());
            (!titulo.is_empty()).then_some((url_real(href), titulo))
        })
        .take(limite)
        .collect()
}

async fn buscar_searx_resultados(
    ctx: &ToolContext,
    base: &str,
    consulta: &str,
    max: usize,
) -> Result<Vec<ResultadoBusca>, String> {
    let url = format!("{}/search", base.trim_end_matches('/'));
    let cliente = cliente_web(ctx).map_err(|e| e.to_string())?;
    let resposta = cliente
        .get(&url)
        .query(&[("q", consulta), ("format", "json")])
        .send()
        .await
        .map_err(|e| format!("falha ao consultar o SearxNG: {e}"))?;
    if !resposta.status().is_success() {
        return Err(format!("HTTP {}", resposta.status()));
    }
    let corpo = ler_corpo_limitado(resposta, LIMITE_CORPO_BUSCA).await?;
    let valor: Value =
        serde_json::from_str(&corpo).map_err(|e| format!("resposta inválida do SearxNG: {e}"))?;
    Ok(valor
        .get("results")
        .and_then(|r| r.as_array())
        .map(|resultados| {
            resultados
                .iter()
                .take(max)
                .filter_map(|item| {
                    let url = item.get("url").and_then(|u| u.as_str())?;
                    Some(ResultadoBusca {
                        titulo: limpar(item.get("title").and_then(|t| t.as_str()).unwrap_or(url)),
                        url: url.to_string(),
                        trecho: limpar(item.get("content").and_then(|c| c.as_str()).unwrap_or("")),
                    })
                })
                .collect()
        })
        .unwrap_or_default())
}

async fn buscar_exa(
    ctx: &ToolContext,
    consulta: &str,
    max: usize,
) -> Result<Vec<ResultadoBusca>, String> {
    let env = ctx.web.exa_api_key_env.trim();
    if env.is_empty() {
        return Err("web.exa_api_key_env vazio".into());
    }
    let chave = std::env::var(env)
        .ok()
        .filter(|v| !v.trim().is_empty())
        .ok_or_else(|| format!("{env} não definida"))?;
    let endpoint = ctx
        .web
        .exa_url
        .as_deref()
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .unwrap_or("https://api.exa.ai/search");
    let cliente = cliente_web(ctx).map_err(|e| e.to_string())?;
    let resposta = cliente
        .post(endpoint)
        .header("x-api-key", chave)
        .header("Accept", "application/json")
        .json(&json!({
            "query": consulta,
            "numResults": max,
            "type": "auto",
            "contents": {"text": {"maxCharacters": 1200}}
        }))
        .send()
        .await
        .map_err(|e| format!("falha ao consultar o Exa: {e}"))?;
    let status = resposta.status();
    if !status.is_success() {
        return Err(format!(
            "HTTP {status}{}",
            if status.as_u16() == 401 || status.as_u16() == 403 {
                " (verifique a chave do Exa)"
            } else {
                ""
            }
        ));
    }
    let corpo = ler_corpo_limitado(resposta, LIMITE_CORPO_BUSCA).await?;
    let valor: Value =
        serde_json::from_str(&corpo).map_err(|e| format!("resposta inválida do Exa: {e}"))?;
    if let Some(erro) = valor.get("error") {
        return Err(format!(
            "Exa: {}",
            crate::util::truncate(&erro.to_string(), 300)
        ));
    }
    let resultados = parse_exa(&valor, max)?;
    Ok(resultados)
}

fn parse_exa(valor: &Value, max: usize) -> Result<Vec<ResultadoBusca>, String> {
    let resultados = valor
        .get("results")
        .and_then(|r| r.as_array())
        .ok_or_else(|| "resposta do Exa sem `results`".to_string())?;
    Ok(resultados
        .iter()
        .take(max)
        .filter_map(|item| {
            let url = item.get("url").and_then(|u| u.as_str())?;
            let titulo = item.get("title").and_then(|t| t.as_str()).unwrap_or(url);
            let trecho = item
                .get("text")
                .or_else(|| item.get("summary"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            Some(ResultadoBusca {
                titulo: limpar(titulo),
                url: url.to_string(),
                trecho: crate::util::truncate(&limpar(trecho), 500),
            })
        })
        .collect())
}

fn formatar_resultados(
    consulta: &str,
    motor: MotorBusca,
    resultados: &[ResultadoBusca],
    tentativas: &[String],
) -> ToolOutput {
    let mut linhas = Vec::new();
    for (i, resultado) in resultados.iter().enumerate() {
        linhas.push(format!(
            "{}. {}\n   {}\n   {}",
            i + 1,
            resultado.titulo,
            resultado.url,
            resultado.trecho
        ));
    }
    let fallback = if tentativas.is_empty() {
        String::new()
    } else {
        format!("\nFallback: {}.", tentativas.join("; "))
    };
    ToolOutput::texto(format!(
        "Resultados para `{consulta}` ({}):{fallback}\n\n{}",
        motor.nome(),
        linhas.join("\n")
    ))
    .com_resumo(format!(
        "{} resultados ({})",
        resultados.len(),
        motor.nome()
    ))
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
        let url_bruta = arg_str(&args, "url")?;
        let max = arg_u64(&args, "max_caracteres").unwrap_or(20_000) as usize;
        let formato = arg_str_opt(&args, "formato").unwrap_or_else(|| "texto".into());
        let incluir_links = args
            .get("incluir_links")
            .and_then(|v| v.as_bool())
            .unwrap_or(true);

        let cliente = cliente_web(ctx)?;
        let (url, resposta) = get_com_redirecionamentos(
            &cliente,
            &url_bruta,
            Some(
                "text/html,application/xhtml+xml,application/json;q=0.9,text/plain;q=0.8,*/*;q=0.5",
            ),
        )
        .await?;
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
    let mut truncado = false;
    let mut stream = resposta.bytes_stream();
    while let Some(pedaco) = stream.next().await {
        let pedaco = pedaco.map_err(|e| e.to_string())?;
        let restante = limite.saturating_sub(bytes.len());
        if pedaco.len() > restante {
            bytes.extend_from_slice(&pedaco[..restante]);
            truncado = true;
            break;
        }
        bytes.extend_from_slice(&pedaco);
    }
    let mut texto = String::from_utf8_lossy(&bytes).to_string();
    if truncado {
        // Sem este aviso o modelo recebia HTML cortado no meio (efeito comum
        // em resposta chunked, sem Content-Length) sem saber que faltava corpo.
        texto.push_str(&format!(
            "\n\n[conteúdo truncado: o corpo excedeu o limite de {} bytes e foi entregue parcialmente; \
             apenas o início foi lido]",
            crate::util::human_bytes(limite as u64)
        ));
    }
    Ok(texto)
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
        let url_bruta = arg_str(&args, "url")?;
        let destino = resolver_caminho(ctx, &arg_str(&args, "caminho")?);
        // O pedido do modelo não pode furar o limite da configuração (antes
        // `max_bytes` era honrado sem teto e enchia o disco).
        let limite = arg_u64(&args, "max_bytes")
            .unwrap_or(ctx.web.max_download_bytes)
            .min(ctx.web.max_download_bytes);

        let cliente = cliente_web(ctx)?;
        let (url, resposta) = get_com_redirecionamentos(&cliente, &url_bruta, None).await?;
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
        // Nome único e criado com `create_new`: dois downloads de arquivos com
        // a mesma extensão não podem compartilhar o temporário, e um symlink
        // plantado no destino não deve ser seguido.
        let nome = destino
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "download".into());
        let temporario = destino.with_file_name(format!(
            ".{nome}.{}.kestrel-download",
            uuid::Uuid::new_v4().simple()
        ));
        let mut arquivo = tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporario)
            .await
            .map_err(|e| ToolError::msg(format!("falha ao criar {}: {e}", temporario.display())))?;
        // Mesmo que a chamada seja cancelada ou o future seja descartado no
        // meio, o arquivo parcial não fica no projeto.
        let mut limpeza = LimparDownload(Some(temporario.clone()));

        let mut total: u64 = 0;
        let mut stream = resposta.bytes_stream();
        loop {
            let pedaco = tokio::select! {
                _ = ctx.cancel.cancelled() => {
                    drop(arquivo);
                    return Err(ToolError::Cancelado);
                }
                pedaco = stream.next() => pedaco,
            };
            let Some(pedaco) = pedaco else { break };
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
        limpeza.esquecer();

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

        // Pré-valida os redirecionamentos HTTP estáticos: o navegador seguiria
        // um `Location` para um endereço interno sem passar pela checagem de
        // SSRF. Redirecionamentos dinâmicos (JS/meta refresh) continuam sendo
        // uma limitação do navegador headless — por isso o Kestrel não executa
        // o conteúdo baixado e mantém a sandbox padrão do navegador.
        let cliente = cliente_web(ctx)?;
        let (url_final, preflight) = get_com_redirecionamentos(
            &cliente,
            url.as_str(),
            Some("text/html,application/xhtml+xml,*/*;q=0.5"),
        )
        .await?;
        drop(preflight);
        let url = url_final;

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
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);
        let mut filho = comando.spawn().map_err(|e| {
            ToolError::msg(format!("falha ao executar {}: {e}", navegador.display()))
        })?;
        let stdout = filho
            .stdout
            .take()
            .ok_or_else(|| ToolError::msg("não foi possível capturar a saída do navegador"))?;
        let stderr = filho
            .stderr
            .take()
            .ok_or_else(|| ToolError::msg("não foi possível capturar os erros do navegador"))?;
        // O DOM de uma página hostil podia crescer sem limite na memória.
        const LIMITE_DOM: usize = 8 * 1024 * 1024;
        let mut dom_bytes = Vec::new();
        let mut erro_bytes = Vec::new();
        let mut stdout_limitado = stdout.take(LIMITE_DOM as u64 + 1);
        let mut stderr_limitado = stderr.take(64 * 1024);
        let (leitura_saida, leitura_erro) = tokio::join!(
            tokio::io::AsyncReadExt::read_to_end(&mut stdout_limitado, &mut dom_bytes),
            tokio::io::AsyncReadExt::read_to_end(&mut stderr_limitado, &mut erro_bytes),
        );
        leitura_saida
            .map_err(|e| ToolError::msg(format!("falha ao ler a saída do navegador: {e}")))?;
        leitura_erro
            .map_err(|e| ToolError::msg(format!("falha ao ler os erros do navegador: {e}")))?;
        // O `Take` no stdout para de drenar ao atingir LIMITE_DOM+1: se o DOM
        // for maior, o chromium fica bloqueado escrevendo no pipe e um `wait()`
        // sem tempo limite nunca completaria (deadlock). Detectamos o estouro,
        // matamos o filho e sempre emolduramos o `wait()` num tempo limite.
        let estourou_dom = dom_bytes.len() > LIMITE_DOM;
        if estourou_dom {
            let _ = filho.start_kill();
        }
        let status = match tokio::time::timeout(Duration::from_secs(10), filho.wait()).await {
            Ok(Ok(s)) => Some(s),
            // Estouro do tempo limite ou falha ao aguardar: ambos ficam sem
            // status; o caminho de estouro do DOM já trata o caso principal.
            Ok(Err(_)) => None,
            Err(_) => None,
        };
        if estourou_dom {
            return Err(ToolError::msg(format!(
                "o navegador produziu um DOM maior que o limite de {}",
                crate::util::human_bytes(LIMITE_DOM as u64)
            )));
        }
        let status = status.ok_or_else(|| {
            ToolError::msg("o navegador não pôde ser aguardado (travado ou encerrado a tempo)")
        })?;
        if !status.success() {
            return Err(ToolError::msg(format!(
                "o navegador terminou com código {:?}: {}",
                status.code(),
                crate::util::truncate(&String::from_utf8_lossy(&erro_bytes), 400)
            )));
        }
        let dom = String::from_utf8_lossy(&dom_bytes).to_string();
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
        for ip in [
            "169.254.169.254",
            "100.100.100.200",
            "0.0.0.0",
            "fe80::1",
            "::ffff:169.254.169.254",
            "::ffff:100.100.100.200",
            "::ffff:0.0.0.0",
        ] {
            assert!(ip_interno(ip.parse::<IpAddr>().unwrap()), "{ip}");
        }
        for ip in ["127.0.0.1", "192.168.1.10", "10.0.0.1", "8.8.8.8", "::1"] {
            assert!(!ip_interno(ip.parse::<IpAddr>().unwrap()), "{ip}");
        }
    }

    /// O reqwest seguia redirecionamentos sozinho e a validação de SSRF só
    /// cobria a URL inicial. Cada salto precisa passar por `validar_url_externa`.
    #[tokio::test]
    async fn redirecionamento_para_metadados_e_bloqueado() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endereco = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut socket, _)) = listener.accept().await {
                let mut buffer = [0u8; 1024];
                let _ = socket.read(&mut buffer).await;
                let _ = socket
                    .write_all(
                        b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\n\r\n",
                    )
                    .await;
            }
        });
        let cliente = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let url = format!("http://{endereco}/");
        let resultado = get_com_redirecionamentos(&cliente, &url, None).await;
        match resultado {
            Ok(_) => panic!("o redirecionamento para metadados deveria ser bloqueado"),
            Err(erro) => assert!(
                erro.to_string().contains("interno"),
                "erro inesperado: {erro}"
            ),
        }
    }

    #[test]
    fn cadeia_de_busca_usa_ddg_e_fallbacks_na_ordem() {
        let web = crate::config::WebConfig::default();
        assert_eq!(
            planejar_motores(&web),
            vec![
                MotorBusca::DuckDuckGo,
                MotorBusca::DuckDuckGoLite,
                MotorBusca::DuckDuckGoHtml,
                MotorBusca::Exa,
            ]
        );
        let mut web = web;
        web.engine = "searx".into();
        web.searx_url = Some("http://localhost:8888".into());
        let motores = planejar_motores(&web);
        assert_eq!(motores[0], MotorBusca::Searx);
        assert_eq!(motores.len(), 5);
    }

    #[test]
    fn parser_do_ddg_html_extrai_titulo_url_e_trecho() {
        let html = r#"
            <a rel="nofollow" class="result__a" href="//duckduckgo.com/l/?uddg=https%3A%2F%2Fexample.com%2Fpagina&rut=1">Título <b>um</b></a>
            <a class="result__snippet" href="https://example.com/pagina">Resumo <b>importante</b></a>
            <a rel="nofollow" class="result__a" href="https://example.org/dois">Segundo</a>
        "#;
        let resultados = parse_ddg(html, 10);
        assert_eq!(resultados.len(), 2);
        assert_eq!(resultados[0].titulo, "Título um");
        assert_eq!(resultados[0].url, "https://example.com/pagina");
        assert_eq!(resultados[0].trecho, "Resumo importante");
        assert_eq!(resultados[1].url, "https://example.org/dois");
    }

    #[test]
    fn parser_do_ddg_lite_aceita_href_antes_da_classe() {
        let html = r#"
            <a rel="nofollow" href="https://lite.example/1" class='result-link'>Lite 1</a>
            <td class='result-snippet'>Resumo lite</td>
            <a class='result-link' rel="nofollow" href="//lite.example/2">Lite 2</a>
        "#;
        let resultados = parse_lite(html, 10);
        assert_eq!(resultados.len(), 2);
        assert_eq!(resultados[0].url, "https://lite.example/1");
        assert_eq!(resultados[0].trecho, "Resumo lite");
        assert_eq!(resultados[1].url, "https://lite.example/2");
    }

    #[test]
    fn parser_do_exa_limpa_html_e_respeita_limite() {
        let valor = json!({
            "results": [
                {"title": "Exa <b>Title</b>", "url": "https://exa.example/1", "text": "Texto <i>limpo</i>"},
                {"title": "Segundo", "url": "https://exa.example/2", "summary": "resumo"}
            ]
        });
        let resultados = parse_exa(&valor, 1).unwrap();
        assert_eq!(resultados.len(), 1);
        assert_eq!(resultados[0].titulo, "Exa Title");
        assert_eq!(resultados[0].trecho, "Texto limpo");
    }

    #[test]
    fn fallback_continua_apos_vazio_ou_erro() {
        let resultado = consolidar(
            "consulta",
            vec![
                (MotorBusca::DuckDuckGo, Ok(Vec::new())),
                (MotorBusca::DuckDuckGoLite, Err("HTTP 202".into())),
                (
                    MotorBusca::DuckDuckGoHtml,
                    Ok(vec![ResultadoBusca {
                        titulo: "Achado".into(),
                        url: "https://exemplo/achado".into(),
                        trecho: "trecho".into(),
                    }]),
                ),
            ],
        )
        .unwrap();
        assert!(resultado.conteudo.contains("duckduckgo html"));
        assert!(resultado.conteudo.contains("Achado"));
        assert!(resultado.conteudo.contains("Fallback"));
        assert!(resultado.conteudo.contains("duckduckgo: sem resultados"));

        let vazio = consolidar("consulta", vec![(MotorBusca::DuckDuckGo, Ok(Vec::new()))]).unwrap();
        assert!(vazio.conteudo.contains("Nenhum resultado"));

        let erro = consolidar(
            "consulta",
            vec![(MotorBusca::DuckDuckGo, Err("rede caiu".into()))],
        )
        .expect_err("todos os mecanismos falharam");
        assert!(erro.to_string().contains("todos os mecanismos"));
    }

    /// Corpo truncado ao atingir o limite precisa de um marcador visível: sem
    /// ele o modelo recebia HTML cortado no meio (resposta chunked, sem
    /// Content-Length) sem saber que faltava conteúdo.
    #[tokio::test]
    async fn corpo_truncado_ganha_marcador_e_corpo_pequeno_nao() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        async fn servir_e_ler(corpo: &'static str, limite: usize) -> String {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let endereco = listener.local_addr().unwrap();
            tokio::spawn(async move {
                if let Ok((mut socket, _)) = listener.accept().await {
                    let mut buffer = [0u8; 2048];
                    let _ = socket.read(&mut buffer).await;
                    // Resposta chunked, sem Content-Length: é o caso em que o
                    // reqwest entrega o corpo via `bytes_stream`, exatamente onde
                    // o corte precisava ser sinalizado.
                    let resposta = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{corpo}\r\n0\r\n\r\n",
                        corpo.len()
                    );
                    let _ = socket.write_all(resposta.as_bytes()).await;
                }
            });
            let cliente = reqwest::Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .build()
                .unwrap();
            let resposta = cliente
                .get(format!("http://{endereco}/"))
                .send()
                .await
                .unwrap();
            ler_corpo_limitado(resposta, limite).await.unwrap()
        }

        // Corpo pequeno, dentro do limite: nada é truncado.
        let completo = servir_e_ler("abc", 100).await;
        assert_eq!(completo, "abc");
        assert!(!completo.contains("truncad"));

        // Corpo maior que o limite: aparece o marcador de truncamento.
        let corpo = "0123456789ABCDEFGHIJ"; // 20 bytes
        let cortado = servir_e_ler(corpo, 5).await;
        assert!(
            cortado.starts_with("01234"),
            "início preservado: {cortado:?}"
        );
        assert!(
            cortado.contains("truncad"),
            "deve avisar do corte: {cortado:?}"
        );
    }
}
