//! Cliente MCP (Model Context Protocol) para servidores locais.
//!
//! O Kestrel fala JSON-RPC 2.0 sobre stdio com processos filhos: `initialize`,
//! `tools/list` e `tools/call`. As ferramentas descobertas entram no mesmo
//! registro das nativas, então o modelo vê todas numa lista só e a camada de
//! permissão/auditoria continua valendo para elas (`tool_risk` classifica
//! ferramenta desconhecida como sensível).
//!
//! Decisões de segurança:
//! - o servidor é filho do Kestrel e morre com ele (`kill_on_drop`);
//! - só stdio: não existe superfície de rede nova;
//! - resposta com teto de bytes e tempo limite por chamada;
//! - caminhos declarados nos argumentos entram na verificação de escopo, então
//!   um servidor MCP não burla a regra de "fora do projeto" por não ser nativo.

use super::{Tool, ToolContext, ToolError, ToolOutput, recortar_saida};
use crate::config::McpServidor;
use crate::provider::ToolSpec;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::ChildStdin;
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, oneshot};

/// Tempo limite por chamada de ferramenta MCP.
const TIMEOUT_CHAMADA: Duration = Duration::from_secs(120);

/// Tempo limite do aperto de mão (`initialize` + `tools/list`).
const TIMEOUT_APERTO: Duration = Duration::from_secs(30);

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

/// Cliente de um servidor MCP: processo filho, canal de pedidos e respostas.
pub struct ClienteMcp {
    nome: String,
    child: Mutex<Child>,
    stdin: Mutex<ChildStdin>,
    proximo_id: AtomicU64,
    /// Pedidos pendentes, casados pelo `id` do JSON-RPC.
    pendentes: Mutex<HashMap<u64, oneshot::Sender<Value>>>,
    /// Ferramentas descobertas no `tools/list` do aperto de mão.
    ///
    /// `OnceLock` porque o leitor já segura um clone do `Arc` quando o aperto
    /// termina: preencher por `Mutex` seria ruído, e nesta altura ninguém mais
    /// escreve.
    ferramentas: std::sync::OnceLock<Vec<FerramentaMcp>>,
}

impl ClienteMcp {
    /// Sobe o servidor, faz o aperto de mão e lista as ferramentas.
    pub async fn conectar(cfg: &McpServidor) -> Result<Arc<Self>, String> {
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
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            proximo_id: AtomicU64::new(1),
            pendentes: Mutex::new(HashMap::new()),
            ferramentas: std::sync::OnceLock::new(),
        });

        // Leitor: cada resposta casa com o pedido pendente pelo `id`.
        {
            let leitor = cliente.clone();
            tokio::spawn(async move {
                let mut linhas = BufReader::new(stdout).lines();
                while let Ok(Some(linha)) = linhas.next_line().await {
                    if linha.trim().is_empty() {
                        continue;
                    }
                    let Ok(valor) = serde_json::from_str::<Value>(&linha) else {
                        continue;
                    };
                    let Some(id) = valor.get("id").and_then(|i| i.as_u64()) else {
                        continue;
                    };
                    if let Some(tx) = leitor.pendentes.lock().await.remove(&id) {
                        let _ = tx.send(valor);
                    }
                }
                // Servidor morto: os pedidos pendentes falham em vez de travar.
                let mut pendentes = leitor.pendentes.lock().await;
                pendentes.clear();
            });
        }

        cliente
            .pedido(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "kestrel", "version": env!("CARGO_PKG_VERSION") }
                }),
                TIMEOUT_APERTO,
            )
            .await?;
        cliente
            .notificar("notifications/initialized", json!({}))
            .await;
        let lista = cliente
            .pedido("tools/list", json!({}), TIMEOUT_APERTO)
            .await?;
        // `OnceLock`: o leitor já segura um clone do `Arc`, então o campo é
        // preenchido por interior (imutabilidade partilhada, sem `Mutex`).
        let _ = cliente
            .ferramentas
            .set(extrair_ferramentas(&cliente.nome, &lista));
        Ok(cliente)
    }

    /// Ferramentas expostas pelo servidor.
    pub fn ferramentas(&self) -> &[FerramentaMcp] {
        self.ferramentas.get().map(Vec::as_slice).unwrap_or(&[])
    }

    /// Notificação (sem resposta esperada).
    async fn notificar(&self, metodo: &str, params: Value) {
        let linha = json!({"jsonrpc": "2.0", "method": metodo, "params": params});
        let Ok(mut texto) = serde_json::to_string(&linha) else {
            return;
        };
        texto.push('\n');
        let mut stdin = self.stdin.lock().await;
        let _ = stdin.write_all(texto.as_bytes()).await;
        let _ = stdin.flush().await;
    }

    /// Pedido com resposta, casado pelo `id`.
    async fn pedido(&self, metodo: &str, params: Value, tempo: Duration) -> Result<Value, String> {
        let id = self.proximo_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pendentes.lock().await.insert(id, tx);
        let linha = json!({"jsonrpc": "2.0", "id": id, "method": metodo, "params": params});
        let mut texto = serde_json::to_string(&linha).map_err(|e| e.to_string())?;
        texto.push('\n');
        {
            let mut stdin = self.stdin.lock().await;
            let escrita = stdin.write_all(texto.as_bytes()).await;
            let _ = stdin.flush().await;
            if escrita.is_err() {
                self.pendentes.lock().await.remove(&id);
                return Err(format!("falha ao escrever no servidor MCP `{}`", self.nome));
            }
        }
        let resposta = tokio::time::timeout(tempo, rx)
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
}

impl Drop for ClienteMcp {
    fn drop(&mut self) {
        // `kill_on_drop` cobre o caso normal; isto é a rede de segurança para
        // um caminho em que o filho fique órfão.
        if let Ok(mut child) = self.child.try_lock() {
            let _ = child.start_kill();
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

// ------------------------------------------------------------- ferramenta

/// Adapta uma ferramenta MCP à interface `Tool` do Kestrel.
pub struct ToolMcp {
    ferramenta: FerramentaMcp,
    cliente: Arc<ClienteMcp>,
}

impl ToolMcp {
    pub fn novo(ferramenta: FerramentaMcp, cliente: Arc<ClienteMcp>) -> Arc<Self> {
        Arc::new(Self {
            ferramenta,
            cliente,
        })
    }
}

#[async_trait::async_trait]
impl Tool for ToolMcp {
    fn nome(&self) -> &'static str {
        // O nome é estável durante a sessão: `Box::leak` uma única vez por
        // ferramenta descoberta (poucas dezenas, no máximo).
        Box::leak(self.ferramenta.nome.clone().into_boxed_str())
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
        let resposta = tokio::select! {
            _ = cancel.cancelled() => return Err(ToolError::Cancelado),
            r = cliente.chamar(&original, &args) => r.map_err(ToolError::msg)?,
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
) -> (Vec<(FerramentaMcp, Arc<ClienteMcp>)>, Vec<String>) {
    let mut ferramentas = Vec::new();
    let mut avisos = Vec::new();
    for cfg in servidores {
        if cfg.comando.trim().is_empty() {
            avisos.push(format!("servidor MCP `{}` não tem comando", cfg.nome));
            continue;
        }
        match ClienteMcp::conectar(cfg).await {
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
            child: Mutex::new(filho),
            stdin: Mutex::new(stdin),
            proximo_id: AtomicU64::new(1),
            pendentes: Mutex::new(HashMap::new()),
            ferramentas: std::sync::OnceLock::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
