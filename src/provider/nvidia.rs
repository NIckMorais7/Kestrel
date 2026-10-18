//! Adaptador do provedor NVIDIA API (endpoint compatível com OpenAI).

use super::ChatRequest;
use super::openai::OpenAiCompatProvider;
use super::{Completion, Provider, ProviderError, StreamEvent};
use crate::config::{Config, ModelEntry};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// Provedor NVIDIA: delega o protocolo ao cliente compatível com OpenAI e
/// enriquece a descoberta de modelos com o catálogo interno.
pub struct NvidiaProvider {
    inner: OpenAiCompatProvider,
}

impl NvidiaProvider {
    pub fn from_config(cfg: &Config) -> anyhow::Result<Self> {
        Ok(Self {
            inner: super::openai::from_config(cfg)?,
        })
    }
}

#[async_trait::async_trait]
impl Provider for NvidiaProvider {
    async fn chat(
        &self,
        req: ChatRequest,
        cancel: CancellationToken,
        sink: Option<mpsc::UnboundedSender<StreamEvent>>,
    ) -> Result<Completion, ProviderError> {
        self.inner.chat(req, cancel, sink).await
    }

    async fn list_models(&self) -> Result<Vec<ModelEntry>, ProviderError> {
        let modelos = self.inner.list_models().await?;
        Ok(modelos.into_iter().map(super::catalog::enrich).collect())
    }
}

/// Cria o provedor a partir da configuração (ponto único de fábrica).
///
/// Hoje existe apenas o adaptador NVIDIA. Outros provedores compatíveis com
/// OpenAI podem ser plugados aqui selecionando um cliente diferente, sem
/// alterar o núcleo do agente.
pub fn create_provider(cfg: &Config) -> anyhow::Result<Box<dyn Provider>> {
    Ok(Box::new(NvidiaProvider::from_config(cfg)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;

    #[test]
    fn cria_provedor_a_partir_da_configuracao() {
        let mut cfg = Config::default();
        cfg.provider.model = "meta/llama-3.1-8b-instruct".into();
        // A construção do provedor é o que este teste verifica (sem rede).
        let _provedor = create_provider(&cfg).expect("provedor criado");
    }
}
