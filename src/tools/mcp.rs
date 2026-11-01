//! Cliente MCP (Model Context Protocol) para servidores locais e remotos.
//!
//! Fala JSON-RPC 2.0 por stdio (processo filho) ou HTTP (Streamable HTTP/SSE),
//! com `initialize`, `tools/list`, `resources/list`, `prompts/list` e as
//! chamadas correspondentes. Servidores assinados podem ser verificados com
//! Ed25519 antes de conectar.
//!
//! Decisões de segurança:
//! - stdio: o servidor é filho do Kestrel e morre com ele (`kill_on_drop`);
//! - HTTP: URL obrigatória, headers/OAuth explícitos e timeout por chamada;
//! - resposta com teto de bytes e tempo limite por chamada;
//! - caminhos declarados nos argumentos entram na verificação de escopo, então
//!   um servidor MCP não burla a regra de "fora do projeto" por não ser nativo.

use super::{Tool, ToolContext, ToolError, ToolOutput, recortar_saida};
use crate::config::{McpOAuth, McpServidor, McpTransporte};
use crate::provider::ToolSpec;
use futures_util::StreamExt;
use ring::signature::UnparsedPublicKey;
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

/// Tempo limite por chamada de ferramenta MCP.
const TIMEOUT_CHAMADA: Duration = Duration::from_secs(120);

/// Tempo limite do aperto de mão (`initialize` + listagens).
const TIMEOUT_APERTO: Duration = Duration::from_secs(30);

/// Forma de invocação de uma ferramenta MCP exposta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetodoMcp {
    Ferramenta,
    ListarRecursos,
    LerRecurso,
    ListarPrompts,
    ObterPrompt,
}

/// Ferramenta exposta por um servidor MCP.
#[derive(Clone, Debug)]
pub struct FerramentaMcp {
    /// Nome como o modelo chama: `mcp__<servidor>__<ferramenta>`.
    pub nome: String,
    pub descricao: String,
    pub esquema: Value,
    /// Nome do servidor que a expõe (para o usuário saber de onde veio).
    pub servidor: String,
    /// Nome original, usado no `tools/call`.
    pub original: String,
    pub metodo: MetodoMcp,
}

/// Recurso exposto por um servidor MCP.
#[derive(Clone, Debug)]
pub struct RecursoMcp {
    pub uri: String,
    pub nome: String,
    pub descricao: Option<String>,
    pub mime: Option<String>,
}

/// Prompt exposto por um servidor MCP.
#[derive(Clone, Debug)]
pub struct PromptMcp {
    pub nome: String,
    pub descricao: Option<String>,
    pub argumentos: Vec<String>,
}

impl FerramentaMcp {
    /// Nome exposto ao modelo. O prefixo `mcp__` deixa claro que a ferramenta
    /// não é nativa e o nome do servidor evita colisão entre dois servidores.
    pub fn nome_exposto(servidor: &str, original: &str) -> String {
        let srv = sanitizar(servidor);
        // Sem nome de servidor (config incompleta), não sobra `__` duplicado.
        if srv.is_empty() {
            return format!("mcp__{}", sanitizar(original));
        }
        format!("mcp__{srv}__{}", sanitizar(original))
    }
}

/// Mantém o nome utilizável num identificador: `[a-z0-9_-]`, no máximo 64.
fn sanitizar(texto: &str) -> String {
    let mut saida: String = texto
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' {
                c.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect();
    while saida.contains("__") {
        saida = saida.replace("__", "_");
    }
    saida.trim_matches('_').chars().take(60).collect()
}

/// Cliente de um servidor MCP: stdio ou HTTP.
pub struct ClienteMcp {
    nome: String,
    child: Mutex<Option<Child>>,
    stdin: Mutex<Option<ChildStdin>>,
    http: Option<HttpMcp>,
    proximo_id: AtomicU64,
    /// Pedidos pendentes, casados pelo `id` do JSON-RPC (transporte stdio).
    pendentes: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
    ferramentas: std::sync::OnceLock<Vec<FerramentaMcp>>,
    recursos: std::sync::OnceLock<Vec<RecursoMcp>>,
    prompts: std::sync::OnceLock<Vec<PromptMcp>>,
    /// Última notificação recebida (diagnóstico) e aviso de listagem alterada.
    #[allow(dead_code)]
    ultima_notificacao: Mutex<Option<String>>,
    #[allow(dead_code)]
    listas_mudaram: AtomicBool,
}

struct HttpMcp {
    cliente: reqwest::Client,
    url: String,
    headers: BTreeMap<String, String>,
    oauth: Option<McpOAuth>,
    token_env: Option<String>,
    token: Mutex<Option<CacheToken>>,
    sessao: Mutex<Option<String>>,
}

struct CacheToken {
    valor: String,
    expira: Option<Instant>,
}

impl ClienteMcp {
    /// Conecta ao servidor configurado (stdio ou HTTP), verificando a
    /// assinatura quando configurada.
    pub async fn conectar(
        cfg: &McpServidor,
        confiaveis: &BTreeMap<String, String>,
        sandbox: &crate::sandbox::SandboxSettings,
    ) -> Result<Arc<Self>, String> {
        verificar_assinatura(cfg, confiaveis)?;
        match cfg.transporte {
            McpTransporte::Http => Self::conectar_http(cfg).await,
            McpTransporte::Stdio => Self::conectar_stdio(cfg, sandbox).await,
        }
    }

    /// Sobe o processo filho e faz o aperto de mão.
    async fn conectar_stdio(
        cfg: &McpServidor,
        sandbox: &crate::sandbox::SandboxSettings,
    ) -> Result<Arc<Self>, String> {
        let mut cmd = Command::new(&cfg.comando);
        cmd.args(&cfg.args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .kill_on_drop(true);
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        if let Some(dir) = &cfg.cwd {
            cmd.current_dir(dir);
        }
        // Servidores MCP são plugins: aplica o mesmo confinamento de escrita
        // dos comandos (quando ligado). `cwd` e o diretório atual entram como
        // raízes graváveis; caches e /tmp já são adicionados pelo sandbox.
        let cwd = cfg
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_default();
        let mut raizes = vec![cwd.clone()];
        if let Ok(atual) = std::env::current_dir() {
            raizes.push(atual);
        }
        if let Err(e) = crate::sandbox::aplicar_no_comando(&mut cmd, sandbox, &cwd, &raizes) {
            return Err(format!(
                "não foi possível preparar o sandbox do MCP `{}`: {e}",
                cfg.nome
            ));
        }
        let mut child = cmd.spawn().map_err(|e| {
            format!(
                "não foi possível iniciar o servidor MCP `{}`: {e}",
                cfg.nome
            )
        })?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| format!("o servidor MCP `{}` não devolveu stdout", cfg.nome))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| format!("o servidor MCP `{}` não aceitou stdin", cfg.nome))?;

        let cliente = Arc::new(Self {
            nome: cfg.nome.clone(),
            child: Mutex::new(Some(child)),
            stdin: Mutex::new(Some(stdin)),
            http: None,
            proximo_id: AtomicU64::new(1),
            pendentes: Arc::new(Mutex::new(HashMap::new())),
            ferramentas: std::sync::OnceLock::new(),
            recursos: std::sync::OnceLock::new(),
            prompts: std::sync::OnceLock::new(),
            ultima_notificacao: Mutex::new(None),
            listas_mudaram: AtomicBool::new(false),
        });

        // Leitor: respostas casam pelo `id`; notificações do servidor são
        // registradas (inclusive `*/list_changed`).
        {
            let leitor = Arc::downgrade(&cliente);
            tokio::spawn(async move {
                let mut linhas = BufReader::new(stdout).lines();
                loop {
                    let Ok(alguma) = linhas.next_line().await else {
                        break;
                    };
                    let Some(linha) = alguma else {
                        break;
                    };
                    if linha.trim().is_empty() {
                        continue;
                    }
                    let Ok(valor) = serde_json::from_str::<Value>(&linha) else {
                        continue;
                    };
                    let Some(cli) = leitor.upgrade() else {
                        return;
                    };
                    if let Some(metodo) = valor.get("method").and_then(|m| m.as_str()) {
                        cli.registrar_notificacao(metodo, &valor).await;
                        continue;
                    }
                    let Some(id) = valor.get("id").and_then(|i| i.as_u64()) else {
                        continue;
                    };
                    if let Some(tx) = cli.pendentes.lock().await.remove(&id) {
                        let _ = tx.send(valor);
                    }
                }
                if let Some(cli) = leitor.upgrade() {
                    cli.pendentes.lock().await.clear();
                }
            });
        }

        cliente.aperto_de_mao().await?;
        Ok(cliente)
    }

    /// Conecta por HTTP (Streamable HTTP/SSE).
    async fn conectar_http(cfg: &McpServidor) -> Result<Arc<Self>, String> {
        let url = cfg
            .url
            .as_deref()
            .map(str::trim)
            .filter(|u| !u.is_empty())
            .ok_or_else(|| format!("servidor MCP `{}` HTTP sem `url`", cfg.nome))?
            .to_string();
        let cliente_http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(20))
            .timeout(TIMEOUT_CHAMADA)
            .user_agent(format!("Kestrel/{} (MCP)", crate::VERSAO))
            .build()
            .map_err(|e| format!("cliente HTTP do MCP `{}`: {e}", cfg.nome))?;
        let cliente = Arc::new(Self {
            nome: cfg.nome.clone(),
            child: Mutex::new(None),
            stdin: Mutex::new(None),
            http: Some(HttpMcp {
                cliente: cliente_http,
                url,
                headers: cfg.headers.clone(),
                oauth: cfg.oauth.clone(),
                token_env: cfg.token_env.clone(),
                token: Mutex::new(None),
                sessao: Mutex::new(None),
            }),
            proximo_id: AtomicU64::new(1),
            pendentes: Arc::new(Mutex::new(HashMap::new())),
            ferramentas: std::sync::OnceLock::new(),
            recursos: std::sync::OnceLock::new(),
            prompts: std::sync::OnceLock::new(),
            ultima_notificacao: Mutex::new(None),
            listas_mudaram: AtomicBool::new(false),
        });
        cliente.aperto_de_mao().await?;
        Ok(cliente)
    }

    /// Handshake comum: initialize, tools/list e extensões (resources/prompts).
    async fn aperto_de_mao(&self) -> Result<(), String> {
        let init = self
            .pedido(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {"roots": {"listChanged": false}},
                    "clientInfo": { "name": "kestrel", "version": crate::VERSAO }
                }),
                TIMEOUT_APERTO,
            )
            .await?;
        self.notificar("notifications/initialized", json!({})).await;
        let lista = self.pedido("tools/list", json!({}), TIMEOUT_APERTO).await?;
        let mut ferramentas = extrair_ferramentas(&self.nome, &lista);
        let capabilities = init.get("capabilities");

        if capabilities.and_then(|c| c.get("resources")).is_some()
            && let Ok(valor) = self
                .pedido("resources/list", json!({}), TIMEOUT_APERTO)
                .await
        {
            let recursos = extrair_recursos(&valor);
            if !recursos.is_empty() {
                ferramentas.push(ferramenta_listar_recursos(&self.nome));
                ferramentas.push(ferramenta_ler_recurso(&self.nome));
            }
            let _ = self.recursos.set(recursos);
        }
        if capabilities.and_then(|c| c.get("prompts")).is_some()
            && let Ok(valor) = self.pedido("prompts/list", json!({}), TIMEOUT_APERTO).await
        {
            let prompts = extrair_prompts(&valor);
            if !prompts.is_empty() {
                ferramentas.push(ferramenta_listar_prompts(&self.nome));
                ferramentas.push(ferramenta_obter_prompt(&self.nome));
            }
            let _ = self.prompts.set(prompts);
        }
        let _ = self.ferramentas.set(ferramentas);
        Ok(())
    }

    /// Ferramentas expostas pelo servidor.
    pub fn ferramentas(&self) -> &[FerramentaMcp] {
        self.ferramentas.get().map(Vec::as_slice).unwrap_or(&[])
    }

    #[cfg(test)]
    pub fn recursos(&self) -> &[RecursoMcp] {
        self.recursos.get().map(Vec::as_slice).unwrap_or(&[])
    }

    #[cfg(test)]
    pub fn prompts(&self) -> &[PromptMcp] {
        self.prompts.get().map(Vec::as_slice).unwrap_or(&[])
    }

    async fn registrar_notificacao(&self, metodo: &str, valor: &Value) {
        if metodo.contains("list_changed") {
            self.listas_mudaram.store(true, Ordering::Relaxed);
        }
        let resumo = json!({
            "metodo": metodo,
            "params": valor.get("params").cloned().unwrap_or(Value::Null),
        });
        *self.ultima_notificacao.lock().await =
            Some(crate::util::truncate(&resumo.to_string(), 300));
    }

    /// Notificação (sem resposta esperada).
    async fn notificar(&self, metodo: &str, params: Value) {
        if let Some(http) = &self.http {
            self.notificar_http(http, metodo, params).await;
            return;
        }
        let linha = json!({"jsonrpc": "2.0", "method": metodo, "params": params});
        let Ok(mut texto) = serde_json::to_string(&linha) else {
            return;
        };
        texto.push('\n');
        let mut stdin = self.stdin.lock().await;
        if let Some(stdin) = stdin.as_mut() {
            let _ = stdin.write_all(texto.as_bytes()).await;
            let _ = stdin.flush().await;
        }
    }

    /// Pedido com resposta, casado pelo `id`.
    async fn pedido(&self, metodo: &str, params: Value, tempo: Duration) -> Result<Value, String> {
        if let Some(http) = &self.http {
            return self.pedido_http(http, metodo, params, tempo).await;
        }
        let id = self.proximo_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        let pendentes = self.pendentes.clone();
        pendentes.lock().await.insert(id, tx);
        // Remove o pedido em qualquer saída (timeout, erro de escrita ou
        // cancelamento do future): sem isso um servidor lento vazava um
        // `Sender` por chamada indefinidamente.
        let _limpeza = LimparPendente {
            id,
            pendentes: pendentes.clone(),
        };
        let linha = json!({"jsonrpc": "2.0", "id": id, "method": metodo, "params": params});
        let mut texto = serde_json::to_string(&linha).map_err(|e| e.to_string())?;
        texto.push('\n');
        let prazo = tokio::time::Instant::now() + tempo;
        let escrita = tokio::time::timeout_at(prazo, async {
            let mut stdin = self.stdin.lock().await;
            let stdin = stdin
                .as_mut()
                .ok_or_else(|| std::io::Error::other("stdin do MCP fechado"))?;
            stdin.write_all(texto.as_bytes()).await?;
            stdin.flush().await
        })
        .await;
        match escrita {
            Ok(Ok(())) => {}
            Ok(Err(_)) => {
                return Err(format!("falha ao escrever no servidor MCP `{}`", self.nome));
            }
            Err(_) => {
                return Err(format!(
                    "o servidor MCP `{}` não aceitou o pedido `{metodo}` a tempo",
                    self.nome
                ));
            }
        }
        let resposta = tokio::time::timeout_at(prazo, rx)
            .await
            .map_err(|_| format!("o servidor MCP `{}` não respondeu a `{metodo}`", self.nome))?
            .map_err(|_| format!("o servidor MCP `{}` fechou a conexão", self.nome))?;
        if let Some(erro) = resposta.get("error") {
            return Err(format!(
                "o servidor MCP `{}` respondeu com erro: {}",
                self.nome,
                crate::util::truncate(&erro.to_string(), 300)
            ));
        }
        Ok(resposta.get("result").cloned().unwrap_or(Value::Null))
    }

    // ----------------------------------------------------------- HTTP

    async fn pedido_http(
        &self,
        http: &HttpMcp,
        metodo: &str,
        params: Value,
        tempo: Duration,
    ) -> Result<Value, String> {
        let id = self.proximo_id.fetch_add(1, Ordering::SeqCst);
        let corpo = json!({"jsonrpc": "2.0", "id": id, "method": metodo, "params": params});
        let resposta = self.enviar_http(http, corpo, tempo, "pedido").await?;
        let status = resposta.status();
        if !status.is_success() {
            let corpo = resposta.text().await.unwrap_or_default();
            return Err(format!(
                "o servidor MCP `{}` respondeu HTTP {status}: {}",
                self.nome,
                crate::util::truncate(corpo.trim(), 300)
            ));
        }
        if let Some(sessao) = resposta
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
            .filter(|s| !s.is_empty())
        {
            *http.sessao.lock().await = Some(sessao.to_string());
        }
        let e_sse = resposta
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v.contains("text/event-stream"));
        if e_sse {
            return self.ler_sse_http(resposta, id, metodo).await;
        }
        let texto = resposta
            .text()
            .await
            .map_err(|e| format!("falha ao ler a resposta HTTP do MCP: {e}"))?;
        let valor: Value = serde_json::from_str(&texto)
            .map_err(|e| format!("resposta HTTP do MCP não é JSON: {e}"))?;
        self.resultado_http(&valor, id, metodo)
    }

    async fn enviar_http(
        &self,
        http: &HttpMcp,
        corpo: Value,
        tempo: Duration,
        rotulo: &str,
    ) -> Result<reqwest::Response, String> {
        let mut pedido = http
            .cliente
            .post(&http.url)
            .header(
                reqwest::header::ACCEPT,
                "application/json, text/event-stream",
            )
            .json(&corpo);
        if let Some(token) = http.autorizacao().await? {
            pedido = pedido.bearer_auth(token);
        }
        for (chave, valor) in &http.headers {
            pedido = pedido.header(chave, valor);
        }
        if let Some(sessao) = http.sessao.lock().await.clone() {
            pedido = pedido.header("Mcp-Session-Id", sessao);
        }
        tokio::time::timeout(tempo, pedido.send())
            .await
            .map_err(|_| {
                format!(
                    "o servidor MCP `{}` não respondeu ao {rotulo} a tempo",
                    self.nome
                )
            })?
            .map_err(|e| format!("falha HTTP no servidor MCP `{}`: {e}", self.nome))
    }

    async fn notificar_http(&self, http: &HttpMcp, metodo: &str, params: Value) {
        let corpo = json!({"jsonrpc": "2.0", "method": metodo, "params": params});
        let _ = self
            .enviar_http(http, corpo, Duration::from_secs(15), "aviso")
            .await;
    }

    /// Tenta interpretar o payload completo de um evento SSE (já com as linhas
    /// `data:` acumuladas). Devolve `Ok(Some(valor))` quando é a resposta da
    /// chamada; `Ok(None)` para notificações, `[DONE]` ou payloads
    /// irrelevantes (comentários/keep-alive não-JSON).
    async fn processar_dados_sse(
        &self,
        dados: &str,
        id: u64,
        metodo: &str,
    ) -> Result<Option<Value>, String> {
        let dados = dados.trim();
        if dados.is_empty() || dados == "[DONE]" {
            return Ok(None);
        }
        let Ok(valor) = serde_json::from_str::<Value>(dados) else {
            return Ok(None);
        };
        if let Some(notificacao) = valor.get("method").and_then(|m| m.as_str()) {
            self.registrar_notificacao(notificacao, &valor).await;
            return Ok(None);
        }
        self.resultado_http(&valor, id, metodo).map(Some)
    }

    /// Consome uma linha do stream SSE: acumula linhas `data:` consecutivas no
    /// evento atual e, quando uma linha em branco encerra o evento, interpreta
    /// o payload completo. Devolve `Ok(Some(valor))` quando é a resposta.
    async fn consumir_linha_sse(
        &self,
        linha: &str,
        dados_evento: &mut Option<String>,
        id: u64,
        metodo: &str,
    ) -> Result<Option<Value>, String> {
        let linha = linha.trim_end_matches('\r');
        if let Some(dados) = linha.strip_prefix("data:") {
            let entrada = dados.trim();
            match dados_evento.as_mut() {
                Some(atual) => {
                    atual.push('\n');
                    atual.push_str(entrada);
                }
                None => *dados_evento = Some(entrada.to_string()),
            }
            return Ok(None);
        }
        if linha.is_empty() {
            // Linha em branco encerra o evento SSE.
            if let Some(corpo) = dados_evento.take() {
                return self.processar_dados_sse(&corpo, id, metodo).await;
            }
        }
        // Demais campos SSE (id:, event:, retry:) não afetam o payload.
        Ok(None)
    }

    async fn ler_sse_http(
        &self,
        resposta: reqwest::Response,
        id: u64,
        metodo: &str,
    ) -> Result<Value, String> {
        let mut fluxo = resposta.bytes_stream();
        let mut buffer: Vec<u8> = Vec::new();
        // No SSE, um evento pode ter várias linhas `data:` cujos valores são
        // concatenados com \n e só são interpretados na linha em branco que
        // encerra o evento. Parsear cada linha isolada perdia esses eventos.
        let mut dados_evento: Option<String> = None;

        while let Some(pedaco) = fluxo.next().await {
            let pedaco = pedaco.map_err(|e| format!("falha no stream HTTP do MCP: {e}"))?;
            buffer.extend_from_slice(&pedaco);
            while let Some(posicao) = buffer.iter().position(|b| *b == b'\n') {
                let linha = String::from_utf8_lossy(&buffer[..posicao]).into_owned();
                buffer.drain(..=posicao);
                if let Some(resultado) = self
                    .consumir_linha_sse(&linha, &mut dados_evento, id, metodo)
                    .await?
                {
                    return Ok(resultado);
                }
            }
        }
        // Uma linha que terminou o stream sem newline: trata como derradeira.
        if !buffer.is_empty() {
            let linha = String::from_utf8_lossy(&buffer).into_owned();
            buffer.clear();
            if let Some(resultado) = self
                .consumir_linha_sse(&linha, &mut dados_evento, id, metodo)
                .await?
            {
                return Ok(resultado);
            }
        }
        // Evento pendente sem linha em branco antes do fim do stream.
        if let Some(corpo) = dados_evento.take()
            && let Some(resultado) = self.processar_dados_sse(&corpo, id, metodo).await?
        {
            return Ok(resultado);
        }
        Err(format!(
            "o stream HTTP do servidor MCP `{}` terminou sem responder `{metodo}`",
            self.nome
        ))
    }

    fn resultado_http(&self, valor: &Value, id: u64, metodo: &str) -> Result<Value, String> {
        let item = if let Some(lista) = valor.as_array() {
            lista
                .iter()
                .find(|v| v.get("id").and_then(|i| i.as_u64()) == Some(id))
                .or_else(|| lista.first())
        } else {
            Some(valor)
        };
        let Some(item) = item else {
            return Err(format!(
                "resposta do MCP `{}` sem conteúdo para `{metodo}`",
                self.nome
            ));
        };
        if let Some(erro) = item.get("error") {
            return Err(format!(
                "o servidor MCP `{}` respondeu com erro: {}",
                self.nome,
                crate::util::truncate(&erro.to_string(), 300)
            ));
        }
        Ok(item.get("result").cloned().unwrap_or(Value::Null))
    }

    /// Executa uma ferramenta do servidor.
    async fn chamar(&self, original: &str, argumentos: &Value) -> Result<String, String> {
        let resultado = self
            .pedido(
                "tools/call",
                json!({"name": original, "arguments": argumentos}),
                TIMEOUT_CHAMADA,
            )
            .await?;
        Ok(texto_do_resultado(&resultado))
    }

    /// Lista recursos MCP como texto para o modelo.
    async fn listar_recursos(&self) -> Result<String, String> {
        let valor = self
            .pedido("resources/list", json!({}), TIMEOUT_APERTO)
            .await?;
        let recursos = extrair_recursos(&valor);
        if recursos.is_empty() {
            return Ok("O servidor não expõe recursos.".into());
        }
        let mut texto = String::from("Recursos disponíveis:\n");
        for recurso in recursos {
            texto.push_str(&format!(
                "- {} ({}){} — {}\n",
                recurso.nome,
                recurso.uri,
                recurso
                    .mime
                    .as_deref()
                    .map(|m| format!(" [{m}]"))
                    .unwrap_or_default(),
                recurso.descricao.unwrap_or_default()
            ));
        }
        Ok(texto)
    }

    async fn ler_recurso(&self, uri: &str) -> Result<String, String> {
        let valor = self
            .pedido("resources/read", json!({"uri": uri}), TIMEOUT_CHAMADA)
            .await?;
        let Some(conteudos) = valor.get("contents").and_then(|c| c.as_array()) else {
            return Ok(crate::util::truncate(
                &serde_json::to_string_pretty(&valor).unwrap_or_default(),
                20_000,
            ));
        };
        let mut partes = Vec::new();
        for item in conteudos {
            if let Some(texto) = item.get("text").and_then(|t| t.as_str()) {
                partes.push(texto.to_string());
            } else if item.get("blob").is_some() {
                partes.push(format!(
                    "[recurso binário {}]",
                    item.get("uri").and_then(|u| u.as_str()).unwrap_or(uri)
                ));
            }
        }
        Ok(partes.join("\n\n"))
    }

    async fn listar_prompts(&self) -> Result<String, String> {
        let valor = self
            .pedido("prompts/list", json!({}), TIMEOUT_APERTO)
            .await?;
        let prompts = extrair_prompts(&valor);
        if prompts.is_empty() {
            return Ok("O servidor não expõe prompts.".into());
        }
        let mut texto = String::from("Prompts disponíveis:\n");
        for prompt in prompts {
            texto.push_str(&format!(
                "- {}{} — {}\n",
                prompt.nome,
                if prompt.argumentos.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", prompt.argumentos.join(", "))
                },
                prompt.descricao.unwrap_or_default()
            ));
        }
        Ok(texto)
    }

    async fn obter_prompt(&self, nome: &str, argumentos: &Value) -> Result<String, String> {
        let valor = self
            .pedido(
                "prompts/get",
                json!({"name": nome, "arguments": argumentos}),
                TIMEOUT_CHAMADA,
            )
            .await?;
        let Some(mensagens) = valor.get("messages").and_then(|m| m.as_array()) else {
            return Ok(crate::util::truncate(
                &serde_json::to_string_pretty(&valor).unwrap_or_default(),
                20_000,
            ));
        };
        let mut partes = Vec::new();
        for mensagem in mensagens {
            let papel = mensagem
                .get("role")
                .and_then(|r| r.as_str())
                .unwrap_or("user");
            let conteudo = mensagem.get("content");
            let texto = conteudo
                .and_then(|c| c.get("text"))
                .and_then(|t| t.as_str())
                .or_else(|| conteudo.and_then(|c| c.as_str()))
                .unwrap_or("");
            partes.push(format!("[{papel}]\n{texto}"));
        }
        Ok(partes.join("\n\n"))
    }
}

impl Drop for ClienteMcp {
    fn drop(&mut self) {
        // `kill_on_drop` cobre o caso normal; isto é a rede de segurança para
        // um caminho em que o filho fique órfão.
        if let Ok(mut child) = self.child.try_lock()
            && let Some(child) = child.as_mut()
        {
            let _ = child.start_kill();
        }
    }
}

/// Remove o `Sender` pendente ao sair de `pedido`, inclusive se o future for
/// cancelado no meio (o `Drop` roda sem poder aguardar o mutex).
struct LimparPendente {
    id: u64,
    pendentes: Arc<Mutex<HashMap<u64, oneshot::Sender<Value>>>>,
}

impl Drop for LimparPendente {
    fn drop(&mut self) {
        if let Ok(mut pendentes) = self.pendentes.try_lock() {
            pendentes.remove(&self.id);
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let pendentes = self.pendentes.clone();
            let id = self.id;
            handle.spawn(async move {
                pendentes.lock().await.remove(&id);
            });
        }
    }
}

/// Extrai as ferramentas da resposta de `tools/list`.
fn extrair_ferramentas(servidor: &str, resultado: &Value) -> Vec<FerramentaMcp> {
    let Some(lista) = resultado.get("tools").and_then(|t| t.as_array()) else {
        return Vec::new();
    };
    lista
        .iter()
        .filter_map(|t| {
            let original = t.get("name").and_then(|n| n.as_str())?.to_string();
            if original.trim().is_empty() {
                return None;
            }
            Some(FerramentaMcp {
                nome: FerramentaMcp::nome_exposto(servidor, &original),
                descricao: t
                    .get("description")
                    .and_then(|d| d.as_str())
                    .unwrap_or("(sem descrição)")
                    .to_string(),
                esquema: t
                    .get("inputSchema")
                    .cloned()
                    .unwrap_or_else(|| json!({"type": "object", "properties": {}})),
                servidor: servidor.to_string(),
                original,
                metodo: MetodoMcp::Ferramenta,
            })
        })
        .take(64)
        .collect()
}

/// Converte o `result` de `tools/call` em texto legível.
fn texto_do_resultado(resultado: &Value) -> String {
    let Some(conteudo) = resultado.get("content").and_then(|c| c.as_array()) else {
        return crate::util::truncate(
            &serde_json::to_string_pretty(resultado).unwrap_or_default(),
            20_000,
        );
    };
    let mut partes = Vec::new();
    for item in conteudo {
        match item.get("type").and_then(|t| t.as_str()) {
            Some("text") => {
                if let Some(t) = item.get("text").and_then(|t| t.as_str()) {
                    partes.push(t.to_string());
                }
            }
            Some("resource") => {
                let texto = item
                    .get("resource")
                    .and_then(|r| r.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("(recurso sem texto)");
                partes.push(texto.to_string());
            }
            _ => partes.push(item.to_string()),
        }
    }
    let texto = partes.join("\n\n");
    if resultado.get("isError").and_then(|e| e.as_bool()) == Some(true) {
        format!("o servidor MCP reportou erro:\n{texto}")
    } else {
        texto
    }
}

/// Extrai recursos de `resources/list`.
fn extrair_recursos(valor: &Value) -> Vec<RecursoMcp> {
    valor
        .get("resources")
        .and_then(|r| r.as_array())
        .map(|lista| {
            lista
                .iter()
                .filter_map(|r| {
                    let uri = r.get("uri").and_then(|u| u.as_str())?.to_string();
                    let nome = r
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or(&uri)
                        .to_string();
                    Some(RecursoMcp {
                        uri,
                        nome,
                        descricao: r
                            .get("description")
                            .and_then(|d| d.as_str())
                            .map(str::to_string),
                        mime: r
                            .get("mimeType")
                            .and_then(|m| m.as_str())
                            .map(str::to_string),
                    })
                })
                .take(200)
                .collect()
        })
        .unwrap_or_default()
}

/// Extrai prompts de `prompts/list`.
fn extrair_prompts(valor: &Value) -> Vec<PromptMcp> {
    valor
        .get("prompts")
        .and_then(|p| p.as_array())
        .map(|lista| {
            lista
                .iter()
                .filter_map(|p| {
                    let nome = p.get("name").and_then(|n| n.as_str())?.to_string();
                    let argumentos = p
                        .get("arguments")
                        .and_then(|a| a.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|arg| {
                                    arg.get("name").and_then(|n| n.as_str()).map(str::to_string)
                                })
                                .collect()
                        })
                        .unwrap_or_default();
                    Some(PromptMcp {
                        nome,
                        descricao: p
                            .get("description")
                            .and_then(|d| d.as_str())
                            .map(str::to_string),
                        argumentos,
                    })
                })
                .take(200)
                .collect()
        })
        .unwrap_or_default()
}

fn ferramenta_listar_recursos(servidor: &str) -> FerramentaMcp {
    FerramentaMcp {
        nome: FerramentaMcp::nome_exposto(servidor, "list_resources"),
        descricao: "Lista os recursos expostos pelo servidor MCP.".into(),
        esquema: json!({"type": "object", "properties": {}}),
        servidor: servidor.to_string(),
        original: "list_resources".into(),
        metodo: MetodoMcp::ListarRecursos,
    }
}

fn ferramenta_ler_recurso(servidor: &str) -> FerramentaMcp {
    FerramentaMcp {
        nome: FerramentaMcp::nome_exposto(servidor, "read_resource"),
        descricao: "Lê o conteúdo de um recurso MCP por URI.".into(),
        esquema: json!({
            "type": "object",
            "properties": {"uri": {"type": "string", "description": "URI do recurso"}},
            "required": ["uri"]
        }),
        servidor: servidor.to_string(),
        original: "read_resource".into(),
        metodo: MetodoMcp::LerRecurso,
    }
}

fn ferramenta_listar_prompts(servidor: &str) -> FerramentaMcp {
    FerramentaMcp {
        nome: FerramentaMcp::nome_exposto(servidor, "list_prompts"),
        descricao: "Lista os prompts (templates) expostos pelo servidor MCP.".into(),
        esquema: json!({"type": "object", "properties": {}}),
        servidor: servidor.to_string(),
        original: "list_prompts".into(),
        metodo: MetodoMcp::ListarPrompts,
    }
}

fn ferramenta_obter_prompt(servidor: &str) -> FerramentaMcp {
    FerramentaMcp {
        nome: FerramentaMcp::nome_exposto(servidor, "get_prompt"),
        descricao: "Obtém um prompt MCP já preenchido com os argumentos informados.".into(),
        esquema: json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Nome do prompt"},
                "arguments": {"type": "object", "description": "Argumentos do prompt"}
            },
            "required": ["name"]
        }),
        servidor: servidor.to_string(),
        original: "get_prompt".into(),
        metodo: MetodoMcp::ObterPrompt,
    }
}

// ------------------------------------------------------- assinatura/oauth

fn decodificar_hex(texto: &str) -> Result<Vec<u8>, String> {
    let texto = texto.trim();
    if !texto.len().is_multiple_of(2) {
        return Err("hex com número ímpar de dígitos".into());
    }
    let mut saida = Vec::with_capacity(texto.len() / 2);
    let bytes = texto.as_bytes();
    for par in bytes.chunks(2) {
        let high = (par[0] as char)
            .to_digit(16)
            .ok_or_else(|| "caractere hex inválido".to_string())?;
        let low = (par[1] as char)
            .to_digit(16)
            .ok_or_else(|| "caractere hex inválido".to_string())?;
        saida.push(((high << 4) | low) as u8);
    }
    Ok(saida)
}

fn hex(bytes: &[u8]) -> String {
    const DIGITOS: &[u8; 16] = b"0123456789abcdef";
    let mut saida = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        saida.push(DIGITOS[(b >> 4) as usize] as char);
        saida.push(DIGITOS[(b & 0xf) as usize] as char);
    }
    saida
}

fn sha256_arquivo(caminho: &std::path::Path) -> Result<String, String> {
    use std::io::Read;
    let mut arquivo = std::fs::File::open(caminho)
        .map_err(|e| format!("não foi possível ler {}: {e}", caminho.display()))?;
    let mut contexto = ring::digest::Context::new(&ring::digest::SHA256);
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let lido = arquivo
            .read(&mut buffer)
            .map_err(|e| format!("falha ao ler {}: {e}", caminho.display()))?;
        if lido == 0 {
            break;
        }
        contexto.update(&buffer[..lido]);
    }
    Ok(hex(contexto.finish().as_ref()))
}

fn caminho_do_comando(cfg: &McpServidor) -> Result<PathBuf, String> {
    let comando = cfg.comando.trim();
    if comando.is_empty() {
        return Err(format!("servidor MCP `{}` sem comando", cfg.nome));
    }
    if comando.contains('/') {
        let caminho = PathBuf::from(comando);
        if caminho.is_file() {
            return Ok(caminho);
        }
        return Err(format!("executável do MCP não encontrado: {comando}"));
    }
    crate::util::which(comando)
        .ok_or_else(|| format!("executável `{comando}` do MCP não encontrado no PATH"))
}

/// Manifesto canônico assinado: identidade do servidor + conteúdo do executável.
fn manifesto_servidor(cfg: &McpServidor) -> Result<String, String> {
    let http = cfg.transporte == McpTransporte::Http;
    let mut linhas = vec![
        "kestrel-mcp-v1".to_string(),
        format!("nome={}", cfg.nome),
        format!("transporte={}", if http { "http" } else { "stdio" }),
        format!("comando={}", cfg.comando),
        format!("args={}", cfg.args.join("\u{1f}")),
        format!("url={}", cfg.url.as_deref().unwrap_or("")),
        format!(
            "cwd={}",
            cfg.cwd
                .as_deref()
                .map(|c| c.display().to_string())
                .unwrap_or_default()
        ),
    ];
    let mut env: Vec<(&String, &String)> = cfg.env.iter().collect();
    env.sort();
    for (k, v) in env {
        linhas.push(format!("env:{k}={v}"));
    }
    for (k, v) in &cfg.headers {
        linhas.push(format!("header:{k}={v}"));
    }
    if !http {
        let caminho = caminho_do_comando(cfg)?;
        let digest = sha256_arquivo(&caminho)?;
        if let Some(esperado) = &cfg.sha256
            && !esperado.trim().eq_ignore_ascii_case(&digest)
        {
            return Err(format!(
                "sha256 do executável do MCP `{}` não confere (esperado {}, obtido {digest})",
                cfg.nome,
                esperado.trim()
            ));
        }
        linhas.push(format!("sha256={digest}"));
    }
    Ok(format!("{}\n", linhas.join("\n")))
}

/// Verifica o digest e a assinatura Ed25519 do manifesto, quando configurados.
fn verificar_assinatura(
    cfg: &McpServidor,
    confiaveis: &BTreeMap<String, String>,
) -> Result<(), String> {
    if cfg.assinatura.is_none() && cfg.sha256.is_none() {
        return Ok(());
    }
    let manifesto = manifesto_servidor(cfg)?;
    // Sem assinatura, o sha256 já foi conferido em `manifesto_servidor`.
    let Some(assinatura_hex) = cfg.assinatura.as_deref() else {
        return Ok(());
    };
    let chave_id = cfg
        .chave_id
        .as_deref()
        .ok_or_else(|| format!("servidor MCP `{}` assinado sem `chave_id`", cfg.nome))?;
    let chave_hex = confiaveis.get(chave_id).ok_or_else(|| {
        format!(
            "chave `{chave_id}` do servidor MCP `{}` não está em mcp_trusted_keys",
            cfg.nome
        )
    })?;
    let chave = decodificar_hex(chave_hex)
        .map_err(|e| format!("chave pública `{chave_id}` inválida: {e}"))?;
    if chave.len() != 32 {
        return Err(format!(
            "chave pública `{chave_id}` deveria ter 32 bytes (tem {})",
            chave.len()
        ));
    }
    let assinatura = decodificar_hex(assinatura_hex)
        .map_err(|e| format!("assinatura do MCP `{}` inválida: {e}", cfg.nome))?;
    if assinatura.len() != 64 {
        return Err(format!(
            "assinatura do MCP `{}` deveria ter 64 bytes (tem {})",
            cfg.nome,
            assinatura.len()
        ));
    }
    UnparsedPublicKey::new(&ring::signature::ED25519, &chave)
        .verify(manifesto.as_bytes(), &assinatura)
        .map_err(|_| {
            format!(
                "assinatura do servidor MCP `{}` não confere com a chave `{chave_id}`",
                cfg.nome
            )
        })
}

impl HttpMcp {
    /// Bearer token de `token_env` ou OAuth client-credentials, com cache.
    async fn autorizacao(&self) -> Result<Option<String>, String> {
        if let Some(var) = &self.token_env
            && let Ok(valor) = std::env::var(var)
            && !valor.trim().is_empty()
        {
            return Ok(Some(valor));
        }
        let Some(oauth) = &self.oauth else {
            return Ok(None);
        };
        let mut cache = self.token.lock().await;
        if let Some(token) = cache.as_ref()
            && token.expira.is_none_or(|e| e > Instant::now())
        {
            return Ok(Some(token.valor.clone()));
        }
        let segredo = std::env::var(&oauth.client_secret_env).map_err(|_| {
            format!(
                "variável {} com o client secret do MCP não está definida",
                oauth.client_secret_env
            )
        })?;
        let escopo = oauth.scopes.join(" ");
        let mut form = vec![
            ("grant_type", "client_credentials".to_string()),
            ("client_id", oauth.client_id.clone()),
            ("client_secret", segredo),
        ];
        if !escopo.is_empty() {
            form.push(("scope", escopo));
        }
        let resposta = self
            .cliente
            .post(&oauth.token_url)
            .form(&form)
            .timeout(Duration::from_secs(20))
            .send()
            .await
            .map_err(|e| format!("falha ao obter token OAuth do MCP: {e}"))?;
        let status = resposta.status();
        if !status.is_success() {
            return Err(format!("token OAuth do MCP respondeu HTTP {status}"));
        }
        let valor: Value = resposta
            .json()
            .await
            .map_err(|e| format!("resposta OAuth do MCP inválida: {e}"))?;
        let token = valor
            .get("access_token")
            .and_then(|t| t.as_str())
            .ok_or_else(|| "resposta OAuth sem `access_token`".to_string())?
            .to_string();
        let expira = valor
            .get("expires_in")
            .and_then(|e| e.as_u64())
            .map(|segundos| Instant::now() + Duration::from_secs(segundos.saturating_sub(60)));
        *cache = Some(CacheToken {
            valor: token.clone(),
            expira,
        });
        Ok(Some(token))
    }
}

/// Adapta uma ferramenta MCP à interface `Tool` do Kestrel.
pub struct ToolMcp {
    ferramenta: FerramentaMcp,
    cliente: Arc<ClienteMcp>,
    /// Nome devolvido como `&'static str`. A API `Tool::nome` exige `'static`,
    /// mas vazar a cada chamada acumulava memória em sessões longas (`nome` é
    /// chamado a cada `ToolRegistry::get`). `OnceLock` garante um único
    /// `Box::leak` por ferramenta descoberta.
    nome_estatico: std::sync::OnceLock<&'static str>,
}

impl ToolMcp {
    pub fn novo(ferramenta: FerramentaMcp, cliente: Arc<ClienteMcp>) -> Arc<Self> {
        Arc::new(Self {
            ferramenta,
            cliente,
            nome_estatico: std::sync::OnceLock::new(),
        })
    }
}

#[async_trait::async_trait]
impl Tool for ToolMcp {
    fn nome(&self) -> &'static str {
        self.nome_estatico
            .get_or_init(|| Box::leak(self.ferramenta.nome.clone().into_boxed_str()))
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.ferramenta.nome.clone(),
            description: format!(
                "{} (servidor MCP: {})",
                self.ferramenta.descricao, self.ferramenta.servidor
            ),
            parameters: if self.ferramenta.esquema.is_object() {
                self.ferramenta.esquema.clone()
            } else {
                json!({"type": "object", "properties": {}})
            },
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        let alvo = ["path", "caminho", "query", "url", "q"]
            .iter()
            .find_map(|chave| args.get(*chave).and_then(|v| v.as_str()))
            .unwrap_or(&self.ferramenta.original)
            .to_string();
        crate::util::truncate(&alvo, 70)
    }

    fn timeout(&self) -> Duration {
        TIMEOUT_CHAMADA
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        // Caminhos declarados entram na verificação de escopo: um servidor MCP
        // não pode burlar a regra de "fora do projeto" só por não ser nativo.
        ["path", "caminho", "file", "arquivo", "cwd", "diretorio"]
            .iter()
            .filter_map(|chave| {
                args.get(*chave)
                    .and_then(|v| v.as_str())
                    .filter(|s| !s.trim().is_empty())
                    .map(PathBuf::from)
            })
            .collect()
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        ctx.checar_cancelamento()?;
        let cancel = ctx.cancel.clone();
        let cliente = self.cliente.clone();
        let original = self.ferramenta.original.clone();
        let servidor = self.ferramenta.servidor.clone();
        let metodo = self.ferramenta.metodo;
        let resposta = tokio::select! {
            _ = cancel.cancelled() => return Err(ToolError::Cancelado),
            r = async move {
                match metodo {
                    MetodoMcp::Ferramenta => cliente.chamar(&original, &args).await,
                    MetodoMcp::ListarRecursos => cliente.listar_recursos().await,
                    MetodoMcp::LerRecurso => {
                        let uri = args
                            .get("uri")
                            .and_then(|u| u.as_str())
                            .filter(|u| !u.trim().is_empty())
                            .ok_or_else(|| "informe `uri`".to_string())?;
                        cliente.ler_recurso(uri).await
                    }
                    MetodoMcp::ListarPrompts => cliente.listar_prompts().await,
                    MetodoMcp::ObterPrompt => {
                        let nome = args
                            .get("name")
                            .and_then(|n| n.as_str())
                            .filter(|n| !n.trim().is_empty())
                            .ok_or_else(|| "informe `name`".to_string())?;
                        let argumentos = args
                            .get("arguments")
                            .filter(|v| v.is_object())
                            .cloned()
                            .unwrap_or_else(|| json!({}));
                        cliente.obter_prompt(nome, &argumentos).await
                    }
                }
            } => r.map_err(ToolError::msg)?,
        };
        Ok(ToolOutput::texto(recortar_saida(&resposta, 20_000))
            .com_resumo(format!("servidor MCP `{servidor}` respondeu")))
    }
}

/// Sobe todos os servidores configurados e devolve as ferramentas descobertas.
///
/// A falha de um servidor não impede os outros: o aviso volta como mensagem
/// para a interface e o Kestrel segue com o que conseguiu carregar.
pub async fn conectar_todos(
    servidores: &[McpServidor],
    confiaveis: &BTreeMap<String, String>,
    sandbox: &crate::sandbox::SandboxSettings,
) -> (Vec<(FerramentaMcp, Arc<ClienteMcp>)>, Vec<String>) {
    let mut ferramentas = Vec::new();
    let mut avisos = Vec::new();
    for cfg in servidores {
        let http = cfg.transporte == McpTransporte::Http;
        if http && cfg.url.as_deref().map(str::trim).unwrap_or("").is_empty() {
            avisos.push(format!("servidor MCP `{}` HTTP não tem `url`", cfg.nome));
            continue;
        }
        if !http && cfg.comando.trim().is_empty() {
            avisos.push(format!("servidor MCP `{}` não tem comando", cfg.nome));
            continue;
        }
        match ClienteMcp::conectar(cfg, confiaveis, sandbox).await {
            Ok(cliente) => {
                if cliente.ferramentas().is_empty() {
                    avisos.push(format!(
                        "servidor MCP `{}` conectado, mas não expôs ferramentas",
                        cfg.nome
                    ));
                }
                for f in cliente.ferramentas() {
                    ferramentas.push((f.clone(), cliente.clone()));
                }
            }
            Err(e) => avisos.push(e),
        }
    }
    (ferramentas, avisos)
}

impl ClienteMcp {
    /// Cliente com processo real (`cat`) mas sem aperto de mão: usado apenas
    /// pelos testes de registro, que não falam com o servidor.
    #[cfg(test)]
    pub(crate) fn vazio_para_teste() -> Self {
        let mut filho = Command::new("cat")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("`cat` deveria existir no sistema");
        let stdin = filho.stdin.take().expect("stdin do cat");
        Self {
            nome: "teste".into(),
            child: Mutex::new(Some(filho)),
            stdin: Mutex::new(Some(stdin)),
            http: None,
            proximo_id: AtomicU64::new(1),
            pendentes: Arc::new(Mutex::new(HashMap::new())),
            ferramentas: std::sync::OnceLock::new(),
            recursos: std::sync::OnceLock::new(),
            prompts: std::sync::OnceLock::new(),
            ultima_notificacao: Mutex::new(None),
            listas_mudaram: AtomicBool::new(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn assinatura_ed25519_do_manifesto_e_verificada() {
        use ring::rand::SystemRandom;
        use ring::signature::{Ed25519KeyPair, KeyPair};
        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).expect("chave");
        let par = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).expect("par");
        let mut cfg = McpServidor {
            nome: "assinado".into(),
            transporte: McpTransporte::Http,
            url: Some("https://exemplo.invalid/mcp".into()),
            ..Default::default()
        };
        let manifesto = manifesto_servidor(&cfg).unwrap();
        cfg.assinatura = Some(hex(par.sign(manifesto.as_bytes()).as_ref()));
        cfg.chave_id = Some("k1".into());
        let mut confiaveis = BTreeMap::new();
        confiaveis.insert("k1".to_string(), hex(par.public_key().as_ref()));
        verificar_assinatura(&cfg, &confiaveis).expect("assinatura válida");
        // Qualquer alteração no manifesto invalida a assinatura.
        cfg.nome = "outro".into();
        assert!(verificar_assinatura(&cfg, &confiaveis).is_err());
        // Sem a chave confiável, recusa.
        let vazio = BTreeMap::new();
        cfg.nome = "assinado".into();
        assert!(verificar_assinatura(&cfg, &vazio).is_err());
    }

    /// Servidor MCP HTTP mínimo: valida cabeçalhos, responde JSON e SSE.
    async fn servir_mcp_http() -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let ouvinte = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("porta");
        let endereco = ouvinte.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut soquete, _)) = ouvinte.accept().await else {
                    break;
                };
                tokio::spawn(async move {
                    let mut dados = Vec::new();
                    let mut buf = [0u8; 2048];
                    let fim_cabecalho = loop {
                        let Ok(n) = soquete.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        dados.extend_from_slice(&buf[..n]);
                        if let Some(pos) = dados.windows(4).position(|j| j == b"\r\n\r\n") {
                            break pos + 4;
                        }
                    };
                    let cabecalho = String::from_utf8_lossy(&dados[..fim_cabecalho]).to_string();
                    let tamanho: usize = cabecalho
                        .lines()
                        .find_map(|l| {
                            l.to_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|v| v.trim().parse().ok())
                        })
                        .unwrap_or(0);
                    while dados.len() < fim_cabecalho + tamanho {
                        let Ok(n) = soquete.read(&mut buf).await else {
                            return;
                        };
                        if n == 0 {
                            return;
                        }
                        dados.extend_from_slice(&buf[..n]);
                    }
                    let corpo = &dados[fim_cabecalho..fim_cabecalho + tamanho];
                    let valor: Value = serde_json::from_slice(corpo).unwrap_or(Value::Null);
                    let metodo = valor.get("method").and_then(|m| m.as_str()).unwrap_or("");
                    let id = valor.get("id").cloned();
                    let tem_auth = cabecalho
                        .to_lowercase()
                        .contains("authorization: bearer teste");
                    let sessao_ok = metodo == "initialize"
                        || cabecalho
                            .to_lowercase()
                            .contains("mcp-session-id: sessao-123");
                    if !tem_auth
                        || !sessao_ok
                        || (metodo == "tools/list"
                            && !cabecalho
                                .to_lowercase()
                                .contains("accept: application/json"))
                    {
                        let corpo = "cabeçalhos inválidos";
                        let resposta = format!(
                            "HTTP/1.1 400 Bad Request\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{corpo}",
                            corpo.len()
                        );
                        let _ = soquete.write_all(resposta.as_bytes()).await;
                        return;
                    }
                    let resultado = match metodo {
                        "initialize" => json!({
                            "capabilities": {"tools": {}, "resources": {}, "prompts": {}},
                            "serverInfo": {"name": "teste", "version": "1"}
                        }),
                        "tools/list" => json!({
                            "tools": [{"name": "eco", "description": "devolve",
                                       "inputSchema": {"type": "object", "properties": {}}}]
                        }),
                        "resources/list" => json!({
                            "resources": [{"uri": "mem://a", "name": "A", "mimeType": "text/plain"}]
                        }),
                        "prompts/list" => json!({
                            "prompts": [{"name": "p", "description": "teste",
                                         "arguments": [{"name": "x"}]}]
                        }),
                        "tools/call" => json!({
                            "content": [{"type": "text", "text": "ok-http"}]
                        }),
                        "resources/read" => json!({
                            "contents": [{"uri": "mem://a", "text": "recurso-http"}]
                        }),
                        "prompts/get" => json!({
                            "messages": [{"role": "user", "content": {"type": "text", "text": "prompt-http"}}]
                        }),
                        _ => json!({}),
                    };
                    let (tipo, corpo, sessao) = if metodo == "tools/list" {
                        (
                            "text/event-stream",
                            format!(
                                "data: {}\n\n",
                                json!({"jsonrpc": "2.0", "id": id, "result": resultado})
                            ),
                            true,
                        )
                    } else if id.is_some() {
                        (
                            "application/json",
                            json!({"jsonrpc": "2.0", "id": id, "result": resultado}).to_string(),
                            metodo == "initialize",
                        )
                    } else {
                        ("text/plain", String::new(), false)
                    };
                    let cab_sessao = if sessao {
                        "Mcp-Session-Id: sessao-123\r\n"
                    } else {
                        ""
                    };
                    let status = if id.is_some() {
                        "200 OK"
                    } else {
                        "202 Accepted"
                    };
                    let resposta = format!(
                        "HTTP/1.1 {status}\r\nContent-Type: {tipo}\r\nContent-Length: {}\r\n{cab_sessao}Connection: close\r\n\r\n{corpo}",
                        corpo.len()
                    );
                    let _ = soquete.write_all(resposta.as_bytes()).await;
                });
            }
        });
        format!("http://{}/mcp", endereco)
    }

    #[tokio::test]
    async fn conectar_http_com_oauth_headers_recursos_e_prompts() {
        let url = servir_mcp_http().await;
        let cfg = McpServidor {
            nome: "remoto".into(),
            transporte: McpTransporte::Http,
            url: Some(url),
            headers: BTreeMap::from([("Authorization".to_string(), "Bearer teste".to_string())]),
            ..Default::default()
        };
        let cliente = tokio::time::timeout(
            Duration::from_secs(10),
            ClienteMcp::conectar(
                &cfg,
                &BTreeMap::new(),
                &crate::sandbox::SandboxSettings::default(),
            ),
        )
        .await
        .expect("handshake não pode travar")
        .expect("conexão HTTP");
        assert!(
            cliente
                .ferramentas()
                .iter()
                .any(|f| f.original == "eco" && f.metodo == MetodoMcp::Ferramenta)
        );
        assert!(
            cliente
                .ferramentas()
                .iter()
                .any(|f| f.metodo == MetodoMcp::LerRecurso)
        );
        assert_eq!(cliente.recursos().len(), 1);
        assert_eq!(cliente.prompts().len(), 1);
        assert_eq!(cliente.chamar("eco", &json!({})).await.unwrap(), "ok-http");
        assert!(
            cliente
                .ler_recurso("mem://a")
                .await
                .unwrap()
                .contains("recurso-http")
        );
        assert!(
            cliente
                .obter_prompt("p", &json!({"x": "1"}))
                .await
                .unwrap()
                .contains("prompt-http")
        );
    }

    #[test]
    fn nome_exposto_tem_prefixo_e_nome_do_servidor() {
        assert_eq!(
            FerramentaMcp::nome_exposto("github", "create_issue"),
            "mcp__github__create_issue"
        );
        // Nomes estranhos não quebram o identificador.
        assert_eq!(
            FerramentaMcp::nome_exposto("Meu Servidor", "listar.arquivos"),
            "mcp__meu_servidor__listar_arquivos"
        );
        // Sem nome de servidor (config incompleta), o nome continua válido.
        assert_eq!(FerramentaMcp::nome_exposto("", "x"), "mcp__x");
    }

    #[test]
    fn sanitizar_nunca_deixa_underscore_solto() {
        assert_eq!(sanitizar("--foo--bar--"), "foo_bar");
        assert!(sanitizar("   ").len() <= 60);
    }

    #[test]
    fn extrai_ferramentas_da_resposta() {
        let resposta = json!({
            "tools": [
                {
                    "name": "buscar",
                    "description": "busca coisas",
                    "inputSchema": {"type": "object", "properties": {"q": {"type": "string"}}}
                },
                {"name": "sem_esquema"},
                {"name": ""}
            ]
        });
        let ferramentas = extrair_ferramentas("srv", &resposta);
        assert_eq!(ferramentas.len(), 2);
        assert_eq!(ferramentas[0].nome, "mcp__srv__buscar");
        assert_eq!(ferramentas[0].original, "buscar");
        assert_eq!(ferramentas[0].esquema["type"], "object");
        // Sem esquema, recebe um objeto vazio (a API exige objeto).
        assert_eq!(ferramentas[1].esquema["type"], "object");
    }

    #[test]
    fn resposta_sem_tools_nao_gera_ferramenta() {
        assert!(extrair_ferramentas("srv", &json!({})).is_empty());
    }

    #[test]
    fn texto_do_resultado_junta_bloco_de_texto() {
        let resultado = json!({
            "content": [
                {"type": "text", "text": "primeira parte"},
                {"type": "text", "text": "segunda parte"},
                {"type": "resource", "resource": {"text": "recurso"}}
            ]
        });
        assert_eq!(
            texto_do_resultado(&resultado),
            "primeira parte\n\nsegunda parte\n\nrecurso"
        );
    }

    #[test]
    fn erro_do_servidor_e_marcado_no_texto() {
        let resultado = json!({"isError": true, "content": [{"type": "text", "text": "deu ruim"}]});
        assert!(texto_do_resultado(&resultado).contains("erro"));
    }

    #[test]
    fn resposta_sem_content_vira_json() {
        let resultado = json!({"qualquer": 1});
        assert!(texto_do_resultado(&resultado).contains("qualquer"));
    }

    /// `nome()` é chamado a cada `ToolRegistry::get`: vazar uma `String` nova a
    /// cada chamada acumulava memória na sessão. Com `OnceLock` a segunda
    /// chamada devolve o mesmo endereço (um único `Box::leak` por ferramenta).
    // `#[tokio::test]` porque o filho do `ClienteMcp` (com `kill_on_drop`) só
    // pode ser encerrado dentro de um runtime.
    #[tokio::test]
    async fn nome_do_tool_mcp_e_estavel_e_nao_vaza_a_cada_chamada() {
        use crate::tools::Tool;
        let cliente = Arc::new(ClienteMcp::vazio_para_teste());
        let ferramenta = FerramentaMcp {
            nome: "mcp__srv__teste".into(),
            descricao: "d".into(),
            esquema: json!({}),
            servidor: "srv".into(),
            original: "teste".into(),
            metodo: MetodoMcp::Ferramenta,
        };
        let tool = ToolMcp::novo(ferramenta, cliente);
        let a = tool.nome();
        let b = tool.nome();
        assert_eq!(a, "mcp__srv__teste");
        assert_eq!(a.as_ptr(), b.as_ptr(), "o nome deveria ser cacheado");
    }
}
