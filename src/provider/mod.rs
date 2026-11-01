//! Camada de provedores desacoplada do núcleo do agente.
//!
//! O protocolo base é o compatível com OpenAI; adaptadores cuidam das
//! diferenças reais entre provedores e modelos.

pub mod catalog;
pub mod nvidia;
pub mod openai;

use crate::config::ModelEntry;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Papel de uma mensagem na conversa.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

/// Chamada de ferramenta pedida pelo modelo.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Argumentos como JSON em texto (validados antes da execução).
    pub arguments: String,
}

impl ToolCall {
    /// Devolve uma cópia com `arguments` sempre como um **objeto JSON válido**.
    ///
    /// A NVIDIA (e alguns modelos compatíveis) devolvem `arguments` quebrados,
    /// vazios ou até como objeto em vez de texto. Reenviar isso no histórico
    /// faz o provedor responder `HTTP 400 ... must be a valid json`. Aqui a
    /// chamada original continua disponível para validação/execução; esta cópia
    /// é só o que vai para o provedor.
    pub fn com_argumentos_validos(&self) -> Self {
        Self {
            id: self.id.clone(),
            name: self.name.clone(),
            arguments: argumentos_objeto(&self.arguments),
        }
    }
}

/// Normaliza argumentos de tool-call para um objeto JSON válido em texto.
///
/// Preserva a mensagem original quando o modelo acertou o formato; em qualquer
/// outro caso devolve `{}` — que a API aceita — e o resultado da ferramenta
/// explica o que estava errado.
pub fn argumentos_objeto(texto: &str) -> String {
    match serde_json::from_str::<Value>(texto) {
        Ok(Value::Object(objeto)) => Value::Object(objeto).to_string(),
        _ => "{}".to_string(),
    }
}

/// Mensagem no formato do protocolo.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

impl ChatMessage {
    pub fn system(content: impl Into<String>) -> Self {
        Self::simple(Role::System, content)
    }

    pub fn user(content: impl Into<String>) -> Self {
        Self::simple(Role::User, content)
    }

    pub fn assistant(content: impl Into<String>) -> Self {
        Self::simple(Role::Assistant, content)
    }

    pub fn tool_result(
        call_id: impl Into<String>,
        name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(call_id.into()),
            name: Some(name.into()),
        }
    }

    pub fn assistant_with_calls(content: impl Into<String>, tool_calls: Vec<ToolCall>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_calls,
            tool_call_id: None,
            name: None,
        }
    }

    fn simple(role: Role, content: impl Into<String>) -> Self {
        Self {
            role,
            content: content.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
            name: None,
        }
    }
}

/// Descrição de uma ferramenta enviada ao modelo.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

/// Pedido de chat.
#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub tools: Vec<ToolSpec>,
    pub stream: bool,
    pub temperature: f32,
    pub max_tokens: u32,
    pub reasoning_effort: Option<String>,
}

impl ChatRequest {
    pub fn new(model: impl Into<String>, messages: Vec<ChatMessage>) -> Self {
        Self {
            model: model.into(),
            messages,
            tools: Vec::new(),
            stream: true,
            temperature: 0.2,
            max_tokens: 8192,
            reasoning_effort: None,
        }
    }
}

/// Eventos emitidos durante a geração em streaming.
#[derive(Clone, Debug)]
pub enum StreamEvent {
    Texto(String),
    Raciocinio(String),
    /// Início/continuação de uma chamada de ferramenta (o conteúdo é
    /// acumulado internamente e devolvido completo em `Completion`).
    ToolCall {
        name: Option<String>,
    },
}

/// Uso de tokens informado pelo provedor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    /// Tokens de raciocínio, quando o provedor os informa em separado.
    pub reasoning_tokens: u32,
}

/// Resultado completo de uma geração.
#[derive(Clone, Debug, Default)]
pub struct Completion {
    pub text: String,
    pub reasoning: String,
    pub tool_calls: Vec<ToolCall>,
    pub finish_reason: String,
    pub usage: Option<Usage>,
}

/// Erros de provedor classificados para permitir recuperação.
#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error(
        "a variável de ambiente {0} não está definida. Configure a chave para usar o provedor (veja /help)."
    )]
    ChaveAusente(String),
    #[error("falha de rede ao contatar o provedor: {0}")]
    Rede(String),
    #[error("tempo limite excedido após {0}s")]
    Timeout(u64),
    #[error("requisição cancelada pelo usuário")]
    Cancelado,
    #[error("limite de requisições atingido (HTTP {status}){}", .retry_after.map(|s| format!("; tente novamente em ~{s}s")).unwrap_or_default())]
    LimiteRequisicoes {
        status: u16,
        retry_after: Option<u64>,
    },
    #[error("erro HTTP {status} do provedor: {message}")]
    Http { status: u16, message: String },
    #[error("resposta inválida do provedor: {0}")]
    RespostaInvalida(String),
    #[error("modelo indisponível: {0}")]
    ModeloIndisponivel(String),
}

impl ProviderError {
    /// Erros transitórios que justificam nova tentativa com espera progressiva.
    pub fn transitorio(&self) -> bool {
        match self {
            ProviderError::Rede(_)
            | ProviderError::Timeout(_)
            | ProviderError::LimiteRequisicoes { .. } => true,
            ProviderError::Http { status, .. } => *status >= 500 || *status == 408,
            _ => false,
        }
    }

    /// Erros que exigem nova resposta do modelo (formato inválido).
    pub fn resposta_invalida(&self) -> bool {
        matches!(self, ProviderError::RespostaInvalida(_))
    }

    pub fn mensagem_util(&self) -> String {
        match self {
            ProviderError::ChaveAusente(var) => format!(
                "A variável {var} não está definida.\n\
                 Configure com: export {var}=\"sua-chave\"\n\
                 A chave é lida apenas do ambiente e nunca é gravada em disco pela interface."
            ),
            other => other.to_string(),
        }
    }

    /// Erros em que vale a pena tentar o próximo modelo da lista de fallback.
    /// Erros de configuração/autenticação não melhoram com outro modelo.
    pub fn permite_fallback(&self) -> bool {
        match self {
            ProviderError::Cancelado | ProviderError::ChaveAusente(_) => false,
            // Resposta inválida é problema de formato; trocar de modelo pode
            // resolver (alguns modelos devolvem tool calls quebradas).
            ProviderError::RespostaInvalida(_) => true,
            ProviderError::ModeloIndisponivel(_) => true,
            ProviderError::Rede(_) | ProviderError::Timeout(_) => true,
            ProviderError::LimiteRequisicoes { .. } => true,
            ProviderError::Http { status, .. } => {
                matches!(*status, 404 | 408 | 429) || *status >= 500
            }
        }
    }
}

/// Interface de provedor de modelos.
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Executa uma geração. Quando `sink` é informado e o provedor suporta
    /// streaming, os deltas são emitidos em tempo real.
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
        sink: Option<mpsc::UnboundedSender<StreamEvent>>,
    ) -> Result<Completion, ProviderError>;

    /// Descobre os modelos disponíveis no provedor.
    async fn list_models(&self) -> Result<Vec<ModelEntry>, ProviderError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mensagens_serializam_no_formato_esperado() {
        let msg = ChatMessage::assistant_with_calls(
            "vou ler o arquivo",
            vec![ToolCall {
                id: "call_1".into(),
                name: "read_file".into(),
                arguments: "{\"path\":\"src/main.rs\"}".into(),
            }],
        );
        let v = serde_json::to_value(&msg).unwrap();
        assert_eq!(v["role"], "assistant");
        assert_eq!(v["tool_calls"][0]["name"], "read_file");
    }

    #[test]
    fn resultado_de_ferramenta_tem_tool_call_id() {
        let m = ChatMessage::tool_result("call_9", "run_command", "ok");
        let v = serde_json::to_value(&m).unwrap();
        assert_eq!(v["role"], "tool");
        assert_eq!(v["tool_call_id"], "call_9");
        assert_eq!(v["name"], "run_command");
    }

    #[test]
    fn argumentos_invalidos_viram_objeto_valido_para_o_historico() {
        let chamada = ToolCall {
            id: "call_1".into(),
            name: "read_file".into(),
            arguments: "{\"path\": \"src/main.rs\"".into(),
        };
        let segura = chamada.com_argumentos_validos();
        assert_eq!(segura.arguments, "{}");
        // A chamada original continua disponível para diagnóstico/validação.
        assert_eq!(chamada.arguments, "{\"path\": \"src/main.rs\"");
        // Um objeto válido é preservado (sem reescrever a ordem/campos).
        let ok = ToolCall {
            id: "call_2".into(),
            name: "read_file".into(),
            arguments: "{\"path\":\"README.md\"}".into(),
        };
        assert_eq!(
            ok.com_argumentos_validos().arguments,
            "{\"path\":\"README.md\"}"
        );
    }

    #[test]
    fn classificacao_de_erros_transitorios() {
        assert!(ProviderError::Rede("conexão".into()).transitorio());
        assert!(ProviderError::Timeout(30).transitorio());
        assert!(
            ProviderError::LimiteRequisicoes {
                status: 429,
                retry_after: None
            }
            .transitorio()
        );
        assert!(
            ProviderError::Http {
                status: 503,
                message: "indisponível".into()
            }
            .transitorio()
        );
        assert!(
            !ProviderError::Http {
                status: 400,
                message: "inválido".into()
            }
            .transitorio()
        );
        assert!(!ProviderError::Cancelado.transitorio());
        assert!(ProviderError::RespostaInvalida("json".into()).resposta_invalida());
    }
}
