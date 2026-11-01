//! Ferramenta dedicada para criar/atualizar Comps.
//!
//! É o único caminho pelo qual o agente escreve em `~/.config/kestrel/comps/`.
//! `write_file` continua restrito ao projeto. O agente nunca remove um Comp;
//! remoção é uma decisão da interface (`/person remove`).

use super::{Tool, ToolContext, ToolError, ToolOutput, arg_bool, arg_str, arg_str_opt};
use crate::comps::{Comp, MAX_DESCRICAO, MAX_INSTRUCOES};
use crate::provider::ToolSpec;
use serde_json::{Value, json};

/// Cria (ou sobrescreve, com confirmação) uma personalidade.
pub struct CreateComp;

#[async_trait::async_trait]
impl Tool for CreateComp {
    fn nome(&self) -> &'static str {
        "create_comp"
    }

    fn detalhe(&self, args: &Value) -> String {
        args.get("nome")
            .and_then(|v| v.as_str())
            .unwrap_or("Comp")
            .to_string()
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Cria uma personalidade (Comp) em ~/.config/kestrel/comps/<slug>.toml. \
                          Use para atender pedidos como \"crie um Comp Técnico, sério e preciso\". \
                          O Comp controla apenas tom/estilo; nunca permissões, modelo ou configuração. \
                          Sobrescrever um Comp existente exige sobrescrever=true e confirmação do usuário."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "nome": {
                        "type": "string",
                        "description": "Nome curto do Comp (até 32 caracteres; vira o slug do arquivo)."
                    },
                    "descricao": {
                        "type": "string",
                        "description": format!("Resumo de uma linha (até {MAX_DESCRICAO} caracteres). Se omitido, usa a primeira linha das instruções.")
                    },
                    "instrucoes": {
                        "type": "string",
                        "description": format!("Instruções de comportamento (até {MAX_INSTRUCOES} caracteres). Seja específico e curto.")
                    },
                    "sobrescrever": {
                        "type": "boolean",
                        "description": "Só use quando o usuário pediu explicitamente para substituir um Comp existente."
                    }
                },
                "required": ["nome", "instrucoes"]
            }),
        }
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        ctx.checar_cancelamento()?;
        let nome = arg_str(&args, "nome")?;
        let instrucoes = arg_str(&args, "instrucoes")?;
        let descricao = arg_str_opt(&args, "descricao");
        let sobrescrever = arg_bool(&args, "sobrescrever", false);

        let comp = match descricao.filter(|d| !d.trim().is_empty()) {
            Some(descricao) => Comp::com_descricao(&nome, descricao, instrucoes),
            None => Comp::novo(&nome, instrucoes),
        }
        .map_err(ToolError::msg)?;

        let caminho = comp.caminho(&ctx.comps_dir);
        let existe = caminho.exists();
        if existe && !sobrescrever {
            return Err(ToolError::msg(format!(
                "o Comp `{}` já existe em {}. Para alterar, use sobrescrever=true \
                 (o usuário será consultado) ou escolha outro nome.",
                comp.nome,
                caminho.display()
            )));
        }
        if existe {
            let permitido = ctx
                .pedir_confirmacao(
                    "Sobrescrever Comp",
                    format!(
                        "O Comp `{}` já existe em {}.\nSubstituir as instruções atuais?",
                        comp.nome,
                        caminho.display()
                    ),
                )
                .await?;
            if !permitido {
                return Err(ToolError::Permissao(format!(
                    "o usuário não autorizou sobrescrever o Comp `{}`",
                    comp.nome
                )));
            }
        }

        let salvo = comp
            .salvar(&ctx.comps_dir, sobrescrever)
            .map_err(ToolError::msg)?;
        let resumo = format!("Comp `{}` salvo ({} tokens)", comp.nome, comp.tokens());
        Ok(ToolOutput::texto(format!(
            "{resumo} em {}.\nPara ativar, o usuário deve usar `/person use {}` (a ativação é \
             uma escolha dele, não do agente).",
            salvo.display(),
            comp.nome
        ))
        .com_resumo(resumo))
    }
}

#[cfg(test)]
mod testes {
    use super::*;
    use crate::config::WebConfig;
    use crate::events::AgentEvent;
    use crate::project::ProjectInfo;
    use crate::skills::SkillRegistry;
    use crate::tools::FileRegistry;
    use std::sync::{Arc, Mutex};

    fn contexto(dir: &std::path::Path) -> ToolContext {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel::<AgentEvent>();
        let (ctx_tx, _) = tokio::sync::mpsc::unbounded_channel();
        ToolContext {
            workdir: dir.to_path_buf(),
            roots: vec![dir.to_path_buf()],
            level: crate::perm::PermissionLevel::Full,
            modo: crate::config::ModoOperacao::Agente,
            cancel: tokio_util::sync::CancellationToken::new(),
            store: crate::db::Store::open_in_memory().unwrap(),
            session_id: "s".into(),
            processos: Arc::new(crate::tools::shell::ProcessManager::new()),
            sandbox: Default::default(),
            arquivos: Arc::new(FileRegistry::new()),
            eventos: tx,
            confirmacoes: ctx_tx,
            permitidos: Arc::new(Mutex::new(std::collections::HashSet::new())),
            skills: Arc::new(SkillRegistry::vazio()),
            skills_dir: dir.join("skills"),
            comps_dir: dir.join("comps"),
            projeto: Arc::new(ProjectInfo::default()),
            web: WebConfig::default(),
            max_saida: 10_000,
            subagente: None,
            subagente_tarefa: None,
            escopo_processos: String::new(),
            chamada_id: None,
        }
    }

    #[tokio::test]
    async fn cria_comp_e_recusa_sobrescrever_sem_flag() {
        let dir = std::env::temp_dir().join(format!(
            "kestrel-create-comp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = contexto(&dir);
        let saida = CreateComp
            .executar(json!({"nome": "Lazy", "instrucoes": "seja breve"}), &ctx)
            .await
            .unwrap();
        assert!(saida.conteudo.contains("lazy.toml"), "{}", saida.conteudo);
        assert!(dir.join("comps/lazy.toml").exists());
        let erro = CreateComp
            .executar(json!({"nome": "Lazy", "instrucoes": "outra coisa"}), &ctx)
            .await
            .unwrap_err();
        assert!(erro.to_string().contains("já existe"), "{erro}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn valida_limites_antes_de_gravar() {
        let dir = std::env::temp_dir().join(format!(
            "kestrel-create-comp2-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = contexto(&dir);
        let erro = CreateComp
            .executar(
                json!({"nome": "Lazy", "instrucoes": "x".repeat(MAX_INSTRUCOES + 1)}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(erro.to_string().contains("máximo"), "{erro}");
        assert!(!dir.join("comps/lazy.toml").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
