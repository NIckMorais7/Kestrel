//! Cliente para provedores compatíveis com a API OpenAI (usado pela NVIDIA).
//!
//! Implementa streaming SSE, chamadas de ferramentas, timeouts, novas
//! tentativas com espera progressiva e cancelamento cooperativo.

use super::{
    ChatMessage, ChatRequest, Completion, Provider, ProviderError, Role, StreamEvent, ToolCall,
    Usage,
};
use crate::config::ModelEntry;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Limite de chamadas de ferramenta parciais aceitas num único stream.
/// Chamadas legítimas vêm em pequenos lotes; um índice gigante só aparece em
/// resposta malformada (ou hostil) e não deve virar alocação sem limite.
const MAX_CHAMADAS_PARCIALS: usize = 64;

/// Cliente compatível com OpenAI.
pub struct OpenAiCompatProvider {
    base_url: String,
    api_key_env: String,
    api_key: Option<String>,
    timeout: Duration,
    max_retries: u32,
    reasoning_effort: Option<String>,
    client: reqwest::Client,
}

impl OpenAiCompatProvider {
    pub fn new(
        base_url: impl Into<String>,
        api_key_env: impl Into<String>,
        api_key: Option<String>,
        timeout_secs: u64,
        max_retries: u32,
        reasoning_effort: Option<String>,
    ) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(timeout_secs.max(10)))
            .connect_timeout(Duration::from_secs(20))
            .user_agent("Kestrel/0.1 (agente de codificação)")
            .build()?;
        Ok(Self {
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key_env: api_key_env.into(),
            api_key,
            timeout: Duration::from_secs(timeout_secs),
            max_retries,
            reasoning_effort,
            client,
        })
    }

    fn endpoint(&self, path: &str) -> String {
        format!("{}/{}", self.base_url, path.trim_start_matches('/'))
    }

    fn chave(&self) -> Result<&str, ProviderError> {
        self.api_key
            .as_deref()
            .filter(|k| !k.trim().is_empty())
            .ok_or_else(|| ProviderError::ChaveAusente(self.api_key_env.clone()))
    }

    /// Monta o corpo JSON da requisição de chat.
    fn corpo(&self, req: &ChatRequest) -> Value {
        let mut messages = Vec::with_capacity(req.messages.len());
        for m in &req.messages {
            messages.push(mensagem_json(m));
        }
        let mut body = json!({
            "model": req.model,
            "messages": messages,
            "temperature": req.temperature,
            "max_tokens": req.max_tokens,
            "stream": req.stream,
        });
        if !req.tools.is_empty() {
            let tools: Vec<Value> = req
                .tools
                .iter()
                .map(|t| {
                    json!({
                        "type": "function",
                        "function": {
                            "name": t.name,
                            "description": t.description,
                            "parameters": t.parameters,
                        }
                    })
                })
                .collect();
            body["tools"] = Value::Array(tools);
            body["tool_choice"] = json!("auto");
        }
        let esforco = req
            .reasoning_effort
            .clone()
            .or_else(|| self.reasoning_effort.clone());
        if let Some(esforco) = esforco {
            body["reasoning_effort"] = json!(esforco);
        }
        body
    }

    /// Executa a requisição com novas tentativas para falhas transitórias.
    async fn enviar_com_retentativas(
        &self,
        body: &Value,
        cancel: &CancellationToken,
    ) -> Result<reqwest::Response, ProviderError> {
        let chave = self.chave()?;
        let url = self.endpoint("chat/completions");
        let mut tentativa = 0u32;
        loop {
            if cancel.is_cancelled() {
                return Err(ProviderError::Cancelado);
            }
            let envio = self
                .client
                .post(&url)
                .bearer_auth(chave)
                .header("Accept", "text/event-stream")
                .json(body)
                .send();

            let resultado = tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::Cancelado),
                r = envio => r,
            };

            match resultado {
                Ok(resp) if resp.status().is_success() => return Ok(resp),
                Ok(resp) => {
                    let status = resp.status();
                    let retry_after = resp
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok())
                        .and_then(|v| v.parse::<u64>().ok());
                    let texto = resp.text().await.unwrap_or_default();
                    let erro = classificar_status(status.as_u16(), &texto, retry_after);
                    if erro.transitorio() && tentativa < self.max_retries {
                        tentativa += 1;
                        let espera = retry_after
                            .map(Duration::from_secs)
                            .unwrap_or_else(|| backoff(tentativa));
                        tracing_sleep(espera, cancel).await?;
                        continue;
                    }
                    return Err(erro);
                }
                Err(err) => {
                    let erro = if err.is_timeout() {
                        ProviderError::Timeout(self.timeout.as_secs())
                    } else {
                        ProviderError::Rede(err.to_string())
                    };
                    if erro.transitorio() && tentativa < self.max_retries {
                        tentativa += 1;
                        tracing_sleep(backoff(tentativa), cancel).await?;
                        continue;
                    }
                    return Err(erro);
                }
            }
        }
    }

    async fn chat_sem_stream(
        &self,
        req: &ChatRequest,
        cancel: &CancellationToken,
    ) -> Result<Completion, ProviderError> {
        let mut body = self.corpo(req);
        body["stream"] = json!(false);
        let resp = self.enviar_com_retentativas(&body, cancel).await?;
        let texto = tokio::select! {
            _ = cancel.cancelled() => return Err(ProviderError::Cancelado),
            r = resp.text() => r.map_err(|e| ProviderError::Rede(e.to_string()))?,
        };
        let valor: Value = serde_json::from_str(&texto)
            .map_err(|e| ProviderError::RespostaInvalida(format!("JSON inválido: {e}")))?;
        if let Some(erro) = valor.get("error") {
            return Err(ProviderError::RespostaInvalida(format!(
                "o provedor retornou um erro no corpo: {erro}"
            )));
        }
        let escolha = valor
            .get("choices")
            .and_then(|c| c.get(0))
            .ok_or_else(|| ProviderError::RespostaInvalida("resposta sem `choices`".into()))?;
        let mensagem = escolha
            .get("message")
            .ok_or_else(|| ProviderError::RespostaInvalida("resposta sem `message`".into()))?;
        let texto_resposta = mensagem
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        let raciocinio = mensagem
            .get("reasoning_content")
            .or_else(|| mensagem.get("reasoning"))
            .and_then(|c| c.as_str())
            .unwrap_or_default()
            .to_string();
        let mut tool_calls = Vec::new();
        if let Some(calls) = mensagem.get("tool_calls").and_then(|c| c.as_array()) {
            for c in calls {
                if let Some(tc) = parse_tool_call(c) {
                    tool_calls.push(tc);
                }
            }
        }
        Ok(Completion {
            text: texto_resposta,
            reasoning: raciocinio,
            tool_calls,
            finish_reason: escolha
                .get("finish_reason")
                .and_then(|f| f.as_str())
                .unwrap_or("stop")
                .to_string(),
            usage: parse_usage(valor.get("usage")),
        })
    }

    async fn chat_com_stream(
        &self,
        req: &ChatRequest,
        cancel: &CancellationToken,
        sink: Option<mpsc::UnboundedSender<StreamEvent>>,
    ) -> Result<Completion, ProviderError> {
        let mut body = self.corpo(req);
        body["stream"] = json!(true);
        let resp = self.enviar_com_retentativas(&body, cancel).await?;
        let mut stream = resp.bytes_stream();
        let mut buffer = String::new();
        let mut completion = Completion {
            finish_reason: "stop".into(),
            ..Default::default()
        };
        let mut parciais: Vec<(String, String, String)> = Vec::new(); // (id, name, args)

        'leitura: loop {
            let chunk = tokio::select! {
                _ = cancel.cancelled() => return Err(ProviderError::Cancelado),
                c = stream.next() => c,
            };
            let Some(chunk) = chunk else { break };
            let bytes = chunk.map_err(|e| ProviderError::Rede(e.to_string()))?;
            buffer.push_str(&String::from_utf8_lossy(&bytes));

            while let Some(pos) = buffer.find('\n') {
                let linha = buffer[..pos].trim_end_matches('\r').to_string();
                buffer.drain(..=pos);
                let Some(dados) = linha.strip_prefix("data:") else {
                    continue;
                };
                let dados = dados.trim();
                if dados.is_empty() {
                    continue;
                }
                if dados == "[DONE]" {
                    // Fim do stream: o `[DONE]` encerra a leitura. Sem sair
                    // também do laço externo, a conexão keep-alive do provedor
                    // deixaria o turno pendurado até o timeout.
                    break 'leitura;
                }
                let valor: Value = match serde_json::from_str(dados) {
                    Ok(v) => v,
                    Err(_) => continue, // fragmento inválido: ignora (respostas malformadas de keep-alive)
                };
                if let Some(erro) = valor.get("error") {
                    return Err(ProviderError::RespostaInvalida(format!(
                        "erro no meio do streaming: {erro}"
                    )));
                }
                if let Some(uso) = parse_usage(valor.get("usage")) {
                    completion.usage = Some(uso);
                }
                let Some(escolha) = valor.get("choices").and_then(|c| c.get(0)) else {
                    continue;
                };
                if let Some(fr) = escolha.get("finish_reason").and_then(|f| f.as_str()) {
                    completion.finish_reason = fr.to_string();
                }
                let Some(delta) = escolha.get("delta") else {
                    continue;
                };
                if let Some(texto) = delta.get("content").and_then(|c| c.as_str())
                    && !texto.is_empty()
                {
                    completion.text.push_str(texto);
                    if let Some(s) = &sink {
                        let _ = s.send(StreamEvent::Texto(texto.to_string()));
                    }
                }
                let raciocinio = delta
                    .get("reasoning_content")
                    .or_else(|| delta.get("reasoning"))
                    .and_then(|c| c.as_str());
                if let Some(r) = raciocinio
                    && !r.is_empty()
                {
                    completion.reasoning.push_str(r);
                    if let Some(s) = &sink {
                        let _ = s.send(StreamEvent::Raciocinio(r.to_string()));
                    }
                }
                if let Some(calls) = delta.get("tool_calls").and_then(|c| c.as_array()) {
                    for c in calls {
                        let index = c.get("index").and_then(|i| i.as_u64()).unwrap_or(0);
                        // Um `index` absurdo (resposta malformada ou hostil) não
                        // pode virar alocação sem limite.
                        if index > MAX_CHAMADAS_PARCIALS as u64 {
                            continue;
                        }
                        let index = index as usize;
                        while parciais.len() <= index {
                            parciais.push((String::new(), String::new(), String::new()));
                        }
                        let slot = &mut parciais[index];
                        if let Some(id) = c.get("id").and_then(|i| i.as_str())
                            && slot.0.is_empty()
                        {
                            slot.0 = id.to_string();
                        }
                        if let Some(func) = c.get("function") {
                            if let Some(nome) = func.get("name").and_then(|n| n.as_str()) {
                                slot.1.push_str(nome);
                            }
                            if let Some(args) = func.get("arguments").and_then(|a| a.as_str()) {
                                slot.2.push_str(args);
                            }
                        }
                        if let Some(s) = &sink {
                            let _ = s.send(StreamEvent::ToolCall {
                                name: c
                                    .get("function")
                                    .and_then(|f| f.get("name"))
                                    .and_then(|n| n.as_str())
                                    .map(|s| s.to_string()),
                            });
                        }
                    }
                }
            }
        }

        for (id, nome, args) in parciais {
            if nome.is_empty() {
                continue;
            }
            completion.tool_calls.push(ToolCall {
                id: if id.is_empty() {
                    format!("call_{}", completion.tool_calls.len())
                } else {
                    id
                },
                name: nome,
                arguments: if args.trim().is_empty() {
                    "{}".to_string()
                } else {
                    args
                },
            });
        }
        Ok(completion)
    }
}

fn mensagem_json(m: &ChatMessage) -> Value {
    let mut obj = serde_json::Map::new();
    obj.insert("role".into(), json!(m.role.as_str()));
    obj.insert("content".into(), json!(m.content));
    if !m.tool_calls.is_empty() {
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|tc| {
                json!({
                    "id": tc.id,
                    "type": "function",
                    "function": { "name": tc.name, "arguments": tc.arguments }
                })
            })
            .collect();
        obj.insert("tool_calls".into(), Value::Array(calls));
    }
    if let Some(id) = &m.tool_call_id {
        obj.insert("tool_call_id".into(), json!(id));
    }
    if let Some(nome) = &m.name {
        obj.insert("name".into(), json!(nome));
    }
    Value::Object(obj)
}

fn parse_tool_call(valor: &Value) -> Option<ToolCall> {
    let func = valor.get("function")?;
    Some(ToolCall {
        id: valor
            .get("id")
            .and_then(|i| i.as_str())
            .unwrap_or("call_0")
            .to_string(),
        name: func.get("name").and_then(|n| n.as_str())?.to_string(),
        arguments: func
            .get("arguments")
            .and_then(|a| a.as_str())
            .unwrap_or("{}")
            .to_string(),
    })
}

fn parse_usage(valor: Option<&Value>) -> Option<Usage> {
    let v = valor?;
    Some(Usage {
        prompt_tokens: v.get("prompt_tokens").and_then(|t| t.as_u64()).unwrap_or(0) as u32,
        completion_tokens: v
            .get("completion_tokens")
            .and_then(|t| t.as_u64())
            .unwrap_or(0) as u32,
    })
}

fn classificar_status(status: u16, corpo: &str, retry_after: Option<u64>) -> ProviderError {
    let mensagem = extrair_mensagem(corpo);
    match status {
        401 | 403 => ProviderError::Http {
            status,
            message: format!(
                "{mensagem} (verifique se a chave em NVIDIA_API_KEY é válida e tem acesso ao modelo)"
            ),
        },
        404 => ProviderError::ModeloIndisponivel(format!(
            "{mensagem} (verifique o modelo selecionado em /model)"
        )),
        429 => ProviderError::LimiteRequisicoes {
            status,
            retry_after,
        },
        _ => ProviderError::Http {
            status,
            message: mensagem,
        },
    }
}

fn extrair_mensagem(corpo: &str) -> String {
    if let Ok(v) = serde_json::from_str::<Value>(corpo)
        && let Some(msg) = v
            .get("error")
            .and_then(|e| e.get("message"))
            .or_else(|| v.get("message"))
            .or_else(|| v.get("detail"))
            .and_then(|m| m.as_str())
    {
        return crate::util::truncate(msg, 400);
    }
    crate::util::truncate(corpo.trim(), 400)
}

fn backoff(tentativa: u32) -> Duration {
    Duration::from_millis(800 * 2u64.pow(tentativa.saturating_sub(1)).min(8))
}

async fn tracing_sleep(d: Duration, cancel: &CancellationToken) -> Result<(), ProviderError> {
    tokio::select! {
        _ = cancel.cancelled() => Err(ProviderError::Cancelado),
        _ = tokio::time::sleep(d) => Ok(()),
    }
}

#[async_trait::async_trait]
impl Provider for OpenAiCompatProvider {
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
        sink: Option<mpsc::UnboundedSender<StreamEvent>>,
    ) -> Result<Completion, ProviderError> {
        if req.messages.iter().all(|m| m.role != Role::User)
            && req.messages.iter().all(|m| m.role != Role::Tool)
        {
            return Err(ProviderError::RespostaInvalida(
                "requisição sem mensagem de usuário".into(),
            ));
        }
        if req.model.trim().is_empty() {
            return Err(ProviderError::ModeloIndisponivel(
                "nenhum modelo selecionado; escolha um modelo com /model".into(),
            ));
        }
        if req.stream && sink.is_some() {
            self.chat_com_stream(&req, &cancel, sink).await
        } else {
            self.chat_sem_stream(&req, &cancel).await
        }
    }

    async fn list_models(&self) -> Result<Vec<ModelEntry>, ProviderError> {
        let chave = self.chave()?;
        let url = self.endpoint("models");
        let resp = self
            .client
            .get(&url)
            .bearer_auth(chave)
            .send()
            .await
            .map_err(|e| {
                if e.is_timeout() {
                    ProviderError::Timeout(self.timeout.as_secs())
                } else {
                    ProviderError::Rede(e.to_string())
                }
            })?;
        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let corpo = resp.text().await.unwrap_or_default();
            return Err(classificar_status(status, &corpo, None));
        }
        let valor: Value = resp
            .json()
            .await
            .map_err(|e| ProviderError::RespostaInvalida(e.to_string()))?;
        let mut modelos = Vec::new();
        if let Some(lista) = valor.get("data").and_then(|d| d.as_array()) {
            for item in lista {
                if let Some(id) = item.get("id").and_then(|i| i.as_str()) {
                    let mut entry = ModelEntry::new(id);
                    entry.owned_by = item
                        .get("owned_by")
                        .and_then(|o| o.as_str())
                        .map(|s| s.to_string());
                    modelos.push(entry);
                }
            }
        }
        modelos.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(modelos)
    }
}

/// Constrói o provedor compatível a partir da configuração.
pub fn from_config(cfg: &crate::config::Config) -> anyhow::Result<OpenAiCompatProvider> {
    OpenAiCompatProvider::new(
        cfg.provider.base_url.clone(),
        cfg.provider.api_key_env.clone(),
        cfg.api_key(),
        cfg.provider.request_timeout_secs,
        cfg.provider.max_retries,
        cfg.provider.reasoning_effort.clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    // Testes de streaming SSE do cliente OpenAI-compatível.
    //
    // `chat_com_stream` é o caminho mais crítico do produto e não tinha nenhum
    // teste: dois bugs reais saíram dele (`[DONE]` não encerrava o laço externo, e
    // `index` de tool_call sem limite). Estes testes sobem um servidor HTTP local
    // em porta efêmera e alimentam o cliente com bytes de verdade, inclusive
    // fragmentados no meio de uma linha.

    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// Ajudante: sobe um servidor HTTP minúsculo que responde com `corpo`,
    /// escrevendo em pedaços de `pedaco` bytes para simular fragmentação de rede.
    ///
    /// Devolve a URL base (já com o caminho `/v1`) e o contador de requisições.
    async fn servir_sse(
        corpo: String,
        pedaco: usize,
        atraso_ms: u64,
    ) -> (String, Arc<std::sync::atomic::AtomicUsize>) {
        // Listener assíncrono desde o início: aceitar em `tokio::spawn` (nenhum
        // `LocalSet` envolvido).
        let ouvinte = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("porta efemera");
        let endereco = ouvinte.local_addr().unwrap();
        let requisicoes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let contador = requisicoes.clone();
        let base = format!("http://{endereco}/v1");

        tokio::spawn(async move {
            loop {
                let Ok((mut soquete, _)) = ouvinte.accept().await else {
                    continue;
                };
                let corpo = corpo.clone();
                let pedaco = pedaco.max(1);
                let atraso = atraso_ms;
                let contador = contador.clone();
                tokio::spawn(async move {
                    contador.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    // Lê os cabeçalhos até a linha vazia.
                    let mut lido = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !lido.windows(4).any(|j| j == b"\r\n\r\n") {
                        match soquete.read(&mut buf).await {
                            Ok(0) => return,
                            Ok(n) => lido.extend_from_slice(&buf[..n]),
                            Err(_) => return,
                        }
                    }
                    let cabecalho = concat!(
                        "HTTP/1.1 200 OK\r\n",
                        "content-type: text/event-stream\r\n",
                        "\r\n"
                    );
                    if soquete.write_all(cabecalho.as_bytes()).await.is_err() {
                        return;
                    }
                    let bytes = corpo.into_bytes();
                    for parte in bytes.chunks(pedaco) {
                        if soquete.write_all(parte).await.is_err() {
                            return;
                        }
                        if atraso > 0 {
                            tokio::time::sleep(Duration::from_millis(atraso)).await;
                        }
                    }
                    let _ = soquete.flush().await;
                    // Fecha logo em seguida: o cliente deve ter terminado no [DONE].
                    drop(soquete);
                });
            }
        });

        (base, requisicoes)
    }

    /// Sink descartável: obriga o caminho de streaming mesmo quando o teste não
    /// quer observar os eventos delta.
    fn pia() -> (
        mpsc::UnboundedSender<StreamEvent>,
        mpsc::UnboundedReceiver<StreamEvent>,
    ) {
        mpsc::unbounded_channel()
    }

    fn cliente(base: &str) -> OpenAiCompatProvider {
        OpenAiCompatProvider::new(base, "NVIDIA_API_KEY", Some("chave".into()), 10, 1, None)
            .expect("cliente de teste")
    }

    #[tokio::test]
    async fn stream_termina_no_done_sem_pendurar() {
        let corpo = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"oi \"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"mundo\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _reqs) = servir_sse(corpo.to_string(), 64, 0).await;
        let p = cliente(&base);
        let (tx, mut rx) = mpsc::unbounded_channel();
        let inicio = std::time::Instant::now();
        let saida = p
            .chat(
                ChatRequest::new("modelo/teste", vec![ChatMessage::user("oi")]),
                CancellationToken::new(),
                Some(tx),
            )
            .await
            .expect("stream deveria funcionar");
        // Sem o `break 'leitura`, o `[DONE]` só sairia do laço interno e a
        // requisição ficaria pendurada até o timeout (10s aqui).
        assert!(
            inicio.elapsed() < Duration::from_secs(3),
            "o stream não terminou no [DONE]: {:?}",
            inicio.elapsed()
        );
        assert_eq!(saida.text, "oi mundo");
        // Os dois deltas chegaram ao sink.
        let mut conteudos = Vec::new();
        while let Ok(evento) = rx.try_recv() {
            if let StreamEvent::Texto(t) = evento {
                conteudos.push(t);
            }
        }
        assert_eq!(conteudos, vec!["oi ", "mundo"]);
    }

    #[tokio::test]
    async fn stream_suporta_linha_fragmentada() {
        // A quebra de linha cai no meio de um JSON: o parser tem que juntar.
        let corpo = concat!(
            "data: {\"choices\":[{\"delta\":{\"cont",
            "ent\":\"fragmentado\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _reqs) = servir_sse(corpo.to_string(), 7, 0).await;
        let p = cliente(&base);
        let saida = p
            .chat(
                ChatRequest::new("modelo/teste", vec![ChatMessage::user("oi")]),
                CancellationToken::new(),
                Some(pia().0),
            )
            .await
            .expect("stream fragmentado deveria funcionar");
        assert_eq!(saida.text, "fragmentado");
    }

    #[tokio::test]
    async fn stream_ignora_linhas_invalidas_e_keepalive() {
        let corpo = concat!(
            ": comentario do servidor\n\n",
            "data: \n\n",
            "data: nao-e-json\n\n",
            "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _reqs) = servir_sse(corpo.to_string(), 23, 0).await;
        let p = cliente(&base);
        let saida = p
            .chat(
                ChatRequest::new("modelo/teste", vec![ChatMessage::user("oi")]),
                CancellationToken::new(),
                Some(pia().0),
            )
            .await
            .expect("linhas inválidas não deveriam derrubar o stream");
        assert_eq!(saida.text, "ok");
    }

    #[tokio::test]
    async fn stream_remonta_chamada_de_ferramenta_fragmentada() {
        let corpo = concat!(
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"c1\",\"function\":{\"name\":\"list_",
            "files\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"function\":{\"arguments\":\"{\\\"path\\\":",
            "\\\".\\\"}\"}}]}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _reqs) = servir_sse(corpo.to_string(), 11, 0).await;
        let p = cliente(&base);
        let saida = p
            .chat(
                ChatRequest::new("modelo/teste", vec![ChatMessage::user("liste")]),
                CancellationToken::new(),
                Some(pia().0),
            )
            .await
            .expect("chamada fragmentada deveria ser remontada");
        assert_eq!(saida.tool_calls.len(), 1);
        let chamada = &saida.tool_calls[0];
        assert_eq!(chamada.id, "c1");
        assert_eq!(chamada.name, "list_files");
        assert_eq!(chamada.arguments, "{\"path\":\".\"}");
    }

    #[tokio::test]
    async fn erro_no_meio_do_stream_interrompe() {
        let corpo = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"parcial\"}}]}\n\n",
            "data: {\"error\":{\"message\":\"estouro de cota\"}}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _reqs) = servir_sse(corpo.to_string(), 40, 0).await;
        let p = cliente(&base);
        let erro = p
            .chat(
                ChatRequest::new("modelo/teste", vec![ChatMessage::user("oi")]),
                CancellationToken::new(),
                Some(pia().0),
            )
            .await
            .expect_err("erro no meio do stream deveria interromper");
        assert!(erro.to_string().contains("estouro de cota"), "{erro}");
    }

    #[tokio::test]
    async fn cancelamento_para_o_stream() {
        let corpo = concat!(
            "data: {\"choices\":[{\"delta\":{\"content\":\"devagar\"}}]}\n\n",
            "data: [DONE]\n\n"
        );
        let (base, _reqs) = servir_sse(corpo.to_string(), 30, 400).await;
        let p = cliente(&base);
        let cancel = CancellationToken::new();
        let cancel2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(150)).await;
            cancel2.cancel();
        });
        let saida = p
            .chat(
                ChatRequest::new("modelo/teste", vec![ChatMessage::user("oi")]),
                cancel,
                Some(pia().0),
            )
            .await
            .expect_err("cancelamento deveria interromper");
        assert!(matches!(saida, ProviderError::Cancelado), "{saida:?}");
    }

    #[test]
    fn corpo_inclui_ferramentas_e_esforco() {
        let p = OpenAiCompatProvider::new(
            "https://exemplo/v1",
            "NVIDIA_API_KEY",
            Some("chave".into()),
            30,
            2,
            Some("high".into()),
        )
        .unwrap();
        let mut req = ChatRequest::new("modelo/x", vec![ChatMessage::user("oi")]);
        req.tools.push(super::super::ToolSpec {
            name: "read_file".into(),
            description: "lê".into(),
            parameters: json!({"type":"object"}),
        });
        let body = p.corpo(&req);
        assert_eq!(body["model"], "modelo/x");
        assert_eq!(body["tools"][0]["function"]["name"], "read_file");
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn corpo_sem_ferramentas_omite_campo() {
        let p =
            OpenAiCompatProvider::new("https://exemplo/v1", "NVIDIA_API_KEY", None, 30, 0, None)
                .unwrap();
        let req = ChatRequest::new("m", vec![ChatMessage::user("oi")]);
        let body = p.corpo(&req);
        assert!(body.get("tools").is_none());
        assert!(body.get("reasoning_effort").is_none());
    }

    #[test]
    fn chave_ausente_gera_erro_explicativo() {
        let p =
            OpenAiCompatProvider::new("https://exemplo/v1", "NVIDIA_API_KEY", None, 30, 0, None)
                .unwrap();
        let err = p.chave().unwrap_err();
        assert!(matches!(err, ProviderError::ChaveAusente(_)));
        assert!(err.mensagem_util().contains("NVIDIA_API_KEY"));
    }

    #[test]
    fn classifica_status_http() {
        let e = classificar_status(429, "{}", Some(7));
        assert!(matches!(
            e,
            ProviderError::LimiteRequisicoes {
                retry_after: Some(7),
                ..
            }
        ));
        let e = classificar_status(401, r#"{"error":{"message":"invalid key"}}"#, None);
        assert!(e.to_string().contains("invalid key"));
        assert!(!e.transitorio());
    }

    #[test]
    fn classifica_modelo_indisponivel_e_corpo_malformado() {
        let e = classificar_status(404, r#"{"error":{"message":"model not found"}}"#, None);
        assert!(matches!(e, ProviderError::ModeloIndisponivel(_)));
        assert!(e.to_string().contains("/model"));
        // Corpo fora do JSON: a mensagem crua é usada, sem pânico.
        let e = classificar_status(500, "erro interno do servidor", None);
        assert!(e.to_string().contains("erro interno"));
        assert!(e.transitorio());
    }

    #[test]
    fn streaming_parcial_acumula_tool_calls() {
        // Simula o acumulador de tool calls parciais usado no streaming.
        let mut parciais: Vec<(String, String, String)> = Vec::new();
        for (index, id, nome, args) in [
            (0usize, Some("call_a"), Some("read_file"), Some("{\"pa")),
            (0, None, None, Some("th\":\"src/main.rs\"}")),
        ] {
            while parciais.len() <= index {
                parciais.push((String::new(), String::new(), String::new()));
            }
            let slot = &mut parciais[index];
            if let Some(id) = id {
                slot.0 = id.to_string();
            }
            if let Some(n) = nome {
                slot.1.push_str(n);
            }
            if let Some(a) = args {
                slot.2.push_str(a);
            }
        }
        assert_eq!(parciais[0].0, "call_a");
        assert_eq!(parciais[0].1, "read_file");
        assert_eq!(parciais[0].2, "{\"path\":\"src/main.rs\"}");
    }
}
