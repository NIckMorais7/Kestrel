//! Ferramentas de planejamento: plano de etapas e subtarefas com dependências.

use super::{Tool, ToolContext, ToolError, ToolOutput, arg_str, arg_str_opt};
use crate::db::TaskRow;
use crate::events::{AgentEvent, PlanoPasso, StatusPasso, TarefaView};
use crate::provider::ToolSpec;
use serde_json::{Value, json};

fn passos_de(valor: &Value) -> Vec<PlanoPasso> {
    valor
        .as_array()
        .map(|itens| {
            itens
                .iter()
                .filter_map(|item| {
                    let titulo = item
                        .get("titulo")
                        .or_else(|| item.get("title"))
                        .and_then(|t| t.as_str())?
                        .to_string();
                    let status = item
                        .get("status")
                        .and_then(|s| s.as_str())
                        .map(StatusPasso::parse)
                        .unwrap_or(StatusPasso::Pendente);
                    let observacao = item
                        .get("observacao")
                        .or_else(|| item.get("nota"))
                        .and_then(|o| o.as_str())
                        .map(|s| s.to_string());
                    Some(PlanoPasso {
                        titulo,
                        status,
                        observacao,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

fn tarefas_de(valor: &Value) -> Vec<TaskRow> {
    valor
        .as_array()
        .map(|itens| {
            itens
                .iter()
                .enumerate()
                .filter_map(|(i, item)| {
                    let titulo = item
                        .get("titulo")
                        .or_else(|| item.get("title"))
                        .and_then(|t| t.as_str())?
                        .to_string();
                    let id = item
                        .get("id")
                        .and_then(|t| t.as_str())
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| format!("t{}", i + 1));
                    let status = item
                        .get("status")
                        .and_then(|s| s.as_str())
                        .map(StatusPasso::parse)
                        .unwrap_or(StatusPasso::Pendente);
                    let depende_de = item
                        .get("depende_de")
                        .or_else(|| item.get("depends_on"))
                        .and_then(|d| d.as_array())
                        .map(|a| {
                            a.iter()
                                .filter_map(|v| v.as_str().map(|s| s.to_string()))
                                .collect()
                        })
                        .unwrap_or_default();
                    let descricao = item
                        .get("descricao")
                        .and_then(|d| d.as_str())
                        .map(|s| s.to_string());
                    let resultado = item
                        .get("resultado")
                        .and_then(|d| d.as_str())
                        .map(|s| s.to_string());
                    Some(TaskRow {
                        id,
                        session_id: String::new(), // preenchido pelo chamador
                        parent_id: None,
                        title: titulo,
                        description: descricao,
                        status: status.as_storage().to_string(),
                        depends_on: depende_de,
                        order_idx: i as i64,
                        result: resultado,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

pub struct UpdatePlan;

#[async_trait::async_trait]
impl Tool for UpdatePlan {
    fn nome(&self) -> &'static str {
        "update_plan"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description:
                "Registra ou atualiza o plano de trabalho e as subtarefas. Use ao iniciar \
                uma tarefa complexa e atualize quando concluir uma etapa ou precisar replanejar. \
                Não repita o plano na conversa: envie-o por aqui. `depende_de` cria dependências \
                entre subtarefas (uma tarefa dependente só deve ser executada depois das suas \
                dependências estarem concluídas)."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "objetivo": {"type": "string", "description": "Objetivo atual da tarefa"},
                    "passos": {
                        "type": "array",
                        "description": "Etapas do plano",
                        "items": {
                            "type": "object",
                            "properties": {
                                "titulo": {"type": "string"},
                                "status": {"type": "string", "description": "pendente | em_andamento | concluido | falhou | pulado"},
                                "observacao": {"type": "string"}
                            },
                            "required": ["titulo"]
                        }
                    },
                    "subtarefas": {
                        "type": "array",
                        "description": "Subtarefas com dependências",
                        "items": {
                            "type": "object",
                            "properties": {
                                "id": {"type": "string"},
                                "titulo": {"type": "string"},
                                "status": {"type": "string"},
                                "depende_de": {"type": "array", "items": {"type": "string"}},
                                "descricao": {"type": "string"},
                                "resultado": {"type": "string"}
                            },
                            "required": ["titulo"]
                        }
                    }
                },
                "required": ["objetivo"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        let objetivo = arg_str_opt(args, "objetivo").unwrap_or_default();
        format!("plano: {}", crate::util::truncate(&objetivo, 60))
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let objetivo = arg_str(&args, "objetivo")?;
        let passos = passos_de(args.get("passos").unwrap_or(&Value::Null));
        let mut tarefas = tarefas_de(args.get("subtarefas").unwrap_or(&Value::Null));
        for t in &mut tarefas {
            t.session_id = ctx.session_id.clone();
        }

        // Valida tudo **antes** de persistir: salvar o plano e só depois
        // descobrir uma dependência inválida deixava objetivo/passos novos no
        // banco sem as subtarefas correspondentes (estado meio atualizado).
        let mut vistos = std::collections::HashSet::new();
        for t in &tarefas {
            if t.id.trim().is_empty() {
                return Err(ToolError::Argumentos(
                    "toda subtarefa precisa de um `id` não vazio".into(),
                ));
            }
            if !vistos.insert(t.id.as_str()) {
                return Err(ToolError::Argumentos(format!(
                    "o id de subtarefa `{}` aparece mais de uma vez; os ids precisam ser únicos",
                    t.id
                )));
            }
        }

        // Valida dependências: todas devem existir entre as subtarefas informadas
        // ou já existir no banco.
        let existentes: Vec<String> = ctx
            .store
            .tasks(&ctx.session_id)?
            .into_iter()
            .map(|t| t.id)
            .collect();
        let ids_atuais: Vec<String> = tarefas.iter().map(|t| t.id.clone()).collect();
        for t in &tarefas {
            for dep in &t.depends_on {
                if dep == &t.id {
                    return Err(ToolError::Argumentos(format!(
                        "a subtarefa `{}` não pode depender de si mesma",
                        t.id
                    )));
                }
                if !ids_atuais.contains(dep) && !existentes.contains(dep) {
                    return Err(ToolError::Argumentos(format!(
                        "a subtarefa `{}` depende de `{dep}`, que não existe. \
                         Defina a dependência com um id válido.",
                        t.id
                    )));
                }
            }
        }

        let passos_json = serde_json::to_string(
            &passos
                .iter()
                .map(|p| {
                    json!({
                        "titulo": p.titulo,
                        "status": p.status.as_storage(),
                        "observacao": p.observacao,
                    })
                })
                .collect::<Vec<_>>(),
        )
        .unwrap_or_else(|_| "[]".into());
        ctx.store
            .save_plan(&ctx.session_id, &objetivo, &passos_json)?;
        ctx.store
            .update_session_objective(&ctx.session_id, &objetivo)
            .ok();

        for t in &tarefas {
            ctx.store.upsert_task(t)?;
        }

        let views: Vec<TarefaView> = ctx
            .store
            .tasks(&ctx.session_id)?
            .into_iter()
            .map(|t| TarefaView {
                id: t.id,
                titulo: t.title,
                status: StatusPasso::parse(&t.status),
                dependencias: t.depends_on,
                resultado: t.result,
            })
            .collect();

        ctx.evento(AgentEvent::PlanoAtualizado {
            objetivo: objetivo.clone(),
            passos: passos.clone(),
        });
        ctx.evento(AgentEvent::TarefasAtualizadas {
            tarefas: views.clone(),
        });

        let mut resumo = format!("Plano atualizado: {objetivo}\n");
        for p in &passos {
            resumo.push_str(&format!(
                "  {} {}{}\n",
                p.status.icone(),
                p.titulo,
                p.observacao
                    .as_deref()
                    .map(|o| format!(" — {o}"))
                    .unwrap_or_default()
            ));
        }
        if !views.is_empty() {
            resumo.push_str("Subtarefas:\n");
            for t in &views {
                let deps = if t.dependencias.is_empty() {
                    String::new()
                } else {
                    format!(" (depende de: {})", t.dependencias.join(", "))
                };
                resumo.push_str(&format!(
                    "  [{}] {} {}{deps}\n",
                    t.id,
                    t.status.icone(),
                    t.titulo
                ));
            }
        }
        Ok(ToolOutput::texto(resumo.trim_end().to_string())
            .com_resumo(format!("plano: {} etapas", passos.len())))
    }
}

pub struct TaskStatus;

#[async_trait::async_trait]
impl Tool for TaskStatus {
    fn nome(&self) -> &'static str {
        "task_status"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Consulta o estado atual das subtarefas registradas, com dependências \
                e resultados. Use antes de iniciar uma subtarefa dependente e ao final do trabalho."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "somente_prontas": {"type": "boolean", "description": "Mostra apenas subtarefas sem dependências pendentes"}
                }
            }),
        }
    }

    async fn executar(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let tarefas = ctx.store.tasks(&ctx.session_id)?;
        if tarefas.is_empty() {
            return Ok(ToolOutput::texto(
                "Nenhuma subtarefa registrada. Use update_plan para criar o plano.",
            ));
        }
        let somente_prontas = _args
            .get("somente_prontas")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let por_id: std::collections::HashMap<&str, &TaskRow> =
            tarefas.iter().map(|t| (t.id.as_str(), t)).collect();
        let mut linhas = Vec::new();
        for t in &tarefas {
            let pendentes: Vec<&String> = t
                .depends_on
                .iter()
                .filter(|d| {
                    // Dependência inexistente não está concluída: tratá-la
                    // como satisfeita liberava a tarefa com um grafo quebrado.
                    por_id
                        .get(d.as_str())
                        .map(|dep| StatusPasso::parse(&dep.status) != StatusPasso::Concluido)
                        .unwrap_or(true)
                })
                .collect();
            if somente_prontas && !pendentes.is_empty() {
                continue;
            }
            let status = StatusPasso::parse(&t.status);
            let bloqueio = if pendentes.is_empty() {
                String::new()
            } else {
                format!(
                    " [bloqueada por: {}]",
                    pendentes
                        .iter()
                        .map(|s| s.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            linhas.push(format!(
                "[{}] {} {}{}{}",
                t.id,
                status.icone(),
                t.title,
                bloqueio,
                t.result
                    .as_deref()
                    .map(|r| format!(" — resultado: {}", crate::util::truncate(r, 120)))
                    .unwrap_or_default()
            ));
        }
        if linhas.is_empty() {
            return Ok(ToolOutput::texto(
                "Nenhuma subtarefa pronta para execução no momento (todas bloqueadas por dependências).",
            ));
        }
        Ok(ToolOutput::texto(format!(
            "Subtarefas ({}):\n{}",
            linhas.len(),
            linhas.join("\n")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converte_passos() {
        let valor = json!([
            {"titulo": "primeiro", "status": "concluido"},
            {"titulo": "segundo", "status": "em_andamento", "observacao": "em curso"}
        ]);
        let passos = passos_de(&valor);
        assert_eq!(passos.len(), 2);
        assert_eq!(passos[0].status, StatusPasso::Concluido);
        assert_eq!(passos[1].observacao.as_deref(), Some("em curso"));
    }

    #[test]
    fn converte_subtarefas_com_dependencias() {
        let valor = json!([
            {"id": "a", "titulo": "base"},
            {"id": "b", "titulo": "dependente", "depende_de": ["a"]}
        ]);
        let tarefas = tarefas_de(&valor);
        assert_eq!(tarefas.len(), 2);
        assert_eq!(tarefas[1].depends_on, vec!["a".to_string()]);
        assert_eq!(tarefas[1].order_idx, 1);
    }

    #[test]
    fn aceita_variacoes_de_campos() {
        let valor = json!([{"title": "x", "depends_on": ["y"]}]);
        let tarefas = tarefas_de(&valor);
        assert_eq!(tarefas[0].title, "x");
        assert_eq!(tarefas[0].depends_on, vec!["y".to_string()]);
    }

    fn contexto() -> ToolContext {
        crate::tools::git::tests::contexto(&std::env::temp_dir())
    }

    /// Uma dependência inválida precisa falhar **antes** de gravar o plano:
    /// antes, objetivo/passos ficavam no banco e as subtarefas não.
    #[tokio::test]
    async fn plano_invalido_nao_deixa_estado_parcial() {
        let ctx = contexto();
        let erro = UpdatePlan
            .executar(
                json!({
                    "objetivo": "objetivo novo",
                    "passos": [{"titulo": "etapa"}],
                    "subtarefas": [{"id": "b", "titulo": "b", "depende_de": ["nao-existe"]}]
                }),
                &ctx,
            )
            .await
            .expect_err("dependência inexistente deveria falhar");
        assert!(erro.to_string().contains("não existe"), "{erro}");
        assert!(ctx.store.latest_plan(&ctx.session_id).unwrap().is_none());
        assert!(ctx.store.tasks(&ctx.session_id).unwrap().is_empty());
    }

    #[tokio::test]
    async fn plano_rejeita_ids_duplicados_e_autodependencia() {
        let ctx = contexto();
        let erro = UpdatePlan
            .executar(
                json!({
                    "objetivo": "x",
                    "subtarefas": [
                        {"id": "a", "titulo": "a"},
                        {"id": "a", "titulo": "outra"}
                    ]
                }),
                &ctx,
            )
            .await
            .expect_err("ids duplicados deveriam falhar");
        assert!(erro.to_string().contains("mais de uma vez"), "{erro}");

        let erro = UpdatePlan
            .executar(
                json!({
                    "objetivo": "x",
                    "subtarefas": [{"id": "a", "titulo": "a", "depende_de": ["a"]}]
                }),
                &ctx,
            )
            .await
            .expect_err("autodependência deveria falhar");
        assert!(erro.to_string().contains("si mesma"), "{erro}");
        assert!(ctx.store.latest_plan(&ctx.session_id).unwrap().is_none());
    }
}
