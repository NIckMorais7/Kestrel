//! Ferramenta de retrieval lexical do projeto (BM25).
//!
//! Diferente de `search_code` (grep exato), esta busca ranqueia arquivos por
//! relevância aos termos da consulta e devolve trechos com número de linha —
//! útil para descobrir onde um assunto mora antes de ler arquivos inteiros.

use super::{Tool, ToolContext, ToolError, ToolOutput, arg_str_opt, arg_u64};
use crate::provider::ToolSpec;
use serde_json::{Value, json};

pub struct SearchProject;

#[async_trait::async_trait]
impl Tool for SearchProject {
    fn nome(&self) -> &'static str {
        "search_project"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Busca por relevância (BM25) nos arquivos de código do projeto. \
                Aceita termos em linguagem natural ou identificadores e devolve os arquivos \
                mais relevantes com trechos e números de linha. Use antes de ler arquivos \
                quando não souber onde algo está definido; para padrões exatos/regex use search_code."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "consulta": {"type": "string", "description": "Termos, identificadores ou pergunta curta"},
                    "max_resultados": {"type": "integer", "description": "Máximo de arquivos (padrão 8, teto 20)"}
                },
                "required": ["consulta"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        let consulta = arg_str_opt(args, "consulta")
            .or_else(|| arg_str_opt(args, "query"))
            .unwrap_or_default();
        format!("retrieval: {}", crate::util::truncate(&consulta, 60))
    }

    fn timeout(&self) -> std::time::Duration {
        std::time::Duration::from_secs(60)
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let consulta = arg_str_opt(&args, "consulta")
            .or_else(|| arg_str_opt(&args, "query"))
            .filter(|c| !c.trim().is_empty())
            .ok_or_else(|| ToolError::Argumentos("informe `consulta`".into()))?;
        let limite = arg_u64(&args, "max_resultados").unwrap_or(8).clamp(1, 20) as usize;
        let raiz = ctx.workdir.clone();
        ctx.checar_cancelamento()?;
        let indice = tokio::task::spawn_blocking(move || crate::index::obter(&raiz))
            .await
            .map_err(|e| ToolError::msg(format!("falha ao indexar o projeto: {e}")))?;
        ctx.checar_cancelamento()?;
        let resultados = indice.buscar(&consulta, limite);
        let aviso_parcial = if indice.parcial() {
            format!(
                "; índice parcial (teto de {} arquivos/{} MB atingido)",
                crate::index::MAX_ARQUIVOS,
                crate::index::MAX_BYTES / (1024 * 1024)
            )
        } else {
            String::new()
        };
        if resultados.is_empty() {
            return Ok(ToolOutput::texto(format!(
                "Nenhum arquivo relevante para `{consulta}` (índice com {} arquivos{aviso_parcial}). \
                 Tente termos mais específicos ou use search_code para padrões exatos.",
                indice.total_documentos()
            ))
            .com_resumo("retrieval sem resultados"));
        }
        let mut texto = format!(
            "Arquivos mais relevantes para `{consulta}` (índice de {} arquivos{aviso_parcial}):\n",
            indice.total_documentos()
        );
        for (posicao, resultado) in resultados.iter().enumerate() {
            let caminho = crate::util::display_path(indice.raiz(), &resultado.caminho);
            texto.push_str(&format!(
                "\n{}. {}  (score {:.2})\n",
                posicao + 1,
                caminho,
                resultado.pontos
            ));
            for (linha, trecho) in &resultado.trechos {
                texto.push_str(&format!("   {linha}: {trecho}\n"));
            }
        }
        Ok(
            ToolOutput::texto(crate::util::truncate(&texto, ctx.max_saida))
                .com_resumo(format!("retrieval: {} arquivo(s)", resultados.len())),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_tem_consulta_obrigatoria() {
        let spec = SearchProject.spec();
        assert_eq!(spec.name, "search_project");
        assert!(
            spec.parameters["required"]
                .as_array()
                .unwrap()
                .iter()
                .any(|v| v == "consulta")
        );
    }
}
