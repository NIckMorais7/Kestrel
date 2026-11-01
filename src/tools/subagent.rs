//! Ferramenta de subagente: delega uma subtarefa isolada a um executor próprio.
//!
//! O subagente trabalha com as mesmas ferramentas reais do agente principal
//! (arquivos, busca, comandos, testes, web, skills), mas sem planejamento,
//! gestão de processos ou recursão. O resultado é um relatório textual que
//! volta para o modelo principal como resultado da ferramenta.

use super::{Tool, ToolContext, ToolError, ToolOutput, arg_str, arg_str_opt, recortar_saida};
use crate::events::AgentEvent;
use crate::provider::ToolSpec;
use serde_json::{Value, json};
use std::time::Duration;

pub struct SpawnSubagent;

#[async_trait::async_trait]
impl Tool for SpawnSubagent {
    fn nome(&self) -> &'static str {
        "spawn_subagent"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Delega uma subtarefa isolada a um subagente com acesso às mesmas \
                ferramentas de arquivos, busca, comandos e testes. Use para paralelizar trabalho \
                independente (por exemplo, investigar uma biblioteca enquanto o código principal é \
                editado) ou para buscas longas. O subagente não cria planos nem abre outros \
                subagentes e devolve um relatório objetivo com o que fez, alterou e descobriu."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "tarefa": {
                        "type": "string",
                        "description": "Descrição objetiva da subtarefa, incluindo o resultado esperado"
                    },
                    "contexto": {
                        "type": "string",
                        "description": "Contexto adicional relevante: arquivos, decisões já tomadas, restrições"
                    }
                },
                "required": ["tarefa"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        args.get("tarefa")
            .and_then(|t| t.as_str())
            .map(|s| crate::util::truncate(s, 70))
            .unwrap_or_else(|| "subtarefa".to_string())
    }

    fn timeout(&self) -> Duration {
        // Subtarefas podem envolver builds e testes completos.
        Duration::from_secs(900)
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let tarefa = arg_str(&args, "tarefa")?;
        let contexto = arg_str_opt(&args, "contexto").unwrap_or_default();
        let Some(runner) = &ctx.subagente else {
            return Err(ToolError::msg(
                "subagentes não estão disponíveis neste contexto; execute a subtarefa diretamente",
            ));
        };
        ctx.evento(AgentEvent::Estado {
            mensagem: format!(
                "subagente trabalhando: {}",
                crate::util::truncate(&tarefa, 60)
            ),
        });
        match runner
            .executar_subagente(&tarefa, &contexto, ctx.cancel.clone(), ctx.eventos.clone())
            .await
        {
            Ok(relatorio) => Ok(ToolOutput::texto(format!(
                // Um relatório gigante sem limite estouraria o contexto na
                // próxima chamada do provedor: recorta como as outras
                // ferramentas, com marcador visível de truncamento.
                "Relatório do subagente:\n\n{}",
                recortar_saida(&relatorio, ctx.max_saida)
            ))
            .com_resumo(format!(
                "subagente concluiu: {}",
                crate::util::truncate(&tarefa, 60)
            ))),
            Err(e) => Err(ToolError::msg(format!(
                "o subagente falhou: {e}. Continue a tarefa sem ele ou reformule a subtarefa."
            ))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::Tool;

    #[test]
    fn especificacao_exige_tarefa() {
        let spec = SpawnSubagent.spec();
        assert_eq!(spec.name, "spawn_subagent");
        assert_eq!(spec.parameters["required"][0], "tarefa");
    }

    #[test]
    fn detalhe_usa_a_tarefa() {
        let detalhe = SpawnSubagent.detalhe(&json!({"tarefa": "investigar a API da biblioteca X"}));
        assert!(detalhe.contains("investigar a API"));
    }

    #[test]
    fn timeout_maior_que_o_padrao() {
        assert!(SpawnSubagent.timeout() > Duration::from_secs(120));
    }

    struct RunnerGigante;

    #[async_trait::async_trait]
    impl crate::tools::SubagentRunner for RunnerGigante {
        async fn executar_subagente(
            &self,
            _tarefa: &str,
            _contexto: &str,
            _contexto_extra: tokio_util::sync::CancellationToken,
            _eventos: tokio::sync::mpsc::UnboundedSender<crate::events::AgentEvent>,
        ) -> Result<String, String> {
            Ok("relatorio ".repeat(6000)) // dezenas de milhares de caracteres
        }
    }

    /// Relatório gigante do subagente precisa ser recortado antes de virar
    /// contexto: sem o limite, a próxima chamada do provedor estourava.
    #[tokio::test]
    async fn relatorio_gigante_do_subagente_e_recortado() {
        let dir = std::env::temp_dir().join(format!("kestrel-subag-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut ctx = crate::tools::git::tests::contexto(&dir);
        ctx.subagente = Some(std::sync::Arc::new(RunnerGigante));
        let saida = SpawnSubagent
            .executar(serde_json::json!({"tarefa": "investigar"}), &ctx)
            .await
            .unwrap();
        assert!(
            saida.conteudo.contains("saída truncada"),
            "o relatório deveria vir truncado: {}",
            saida.conteudo
        );
        assert!(
            saida.conteudo.chars().count() < 40_000,
            "o relatório não pode entrar inteiro no contexto"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
