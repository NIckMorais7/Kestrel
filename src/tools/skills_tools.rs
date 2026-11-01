//! Ferramentas de skills: listar e carregar instruções completas.

use super::{Tool, ToolContext, ToolError, ToolOutput, arg_str, recortar_saida};
use crate::provider::ToolSpec;
use serde_json::{Value, json};

pub struct ListSkills;

#[async_trait::async_trait]
impl Tool for ListSkills {
    fn nome(&self) -> &'static str {
        "list_skills"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Lista as skills disponíveis (do usuário e do projeto) com nome, \
                descrição e escopo. Carregue o conteúdo completo com load_skill quando a skill \
                for pertinente à tarefa."
                .into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn executar(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        if ctx.skills.is_empty() {
            return Ok(ToolOutput::texto(format!(
                "Nenhuma skill instalada.\nDiretórios monitorados:\n- {}\n- {}/.kestrel/skills\n\
                 Adicione arquivos SKILL.md (com frontmatter `name` e `description`) nesses \
                 diretórios para criar novas skills sem recompilar.",
                ctx.skills_dir.display(),
                ctx.workdir.display()
            )));
        }
        let linhas: Vec<String> = ctx
            .skills
            .all()
            .iter()
            .map(|s| {
                format!(
                    "- {} ({}) — {}\n  arquivo: {}",
                    s.name,
                    s.escopo.label(),
                    s.description,
                    s.path.display()
                )
            })
            .collect();
        Ok(ToolOutput::texto(format!(
            "{} skills disponíveis:\n{}",
            linhas.len(),
            linhas.join("\n")
        )))
    }
}

pub struct LoadSkill;

#[async_trait::async_trait]
impl Tool for LoadSkill {
    fn nome(&self) -> &'static str {
        "load_skill"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Carrega as instruções completas de uma skill pelo nome. Use quando a \
                skill for relevante para a tarefa atual; siga as instruções carregadas."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"nome": {"type": "string", "description": "Nome da skill (use list_skills para ver os nomes)"}},
                "required": ["nome"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        args.get("nome")
            .and_then(|n| n.as_str())
            .unwrap_or("skill")
            .to_string()
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let nome = arg_str(&args, "nome")?;
        match ctx.skills.load(&nome) {
            Ok(conteudo) => Ok(ToolOutput::texto(format!(
                // Skills grandes (SKILL.md verboso) entrariam sem limite no
                // contexto: aplica o mesmo recorte das outras ferramentas.
                "Instruções da skill `{nome}`:\n\n{}",
                recortar_saida(&conteudo, ctx.max_saida)
            ))
            .com_resumo(format!("carregou skill {nome}"))),
            Err(e) => {
                let disponiveis: Vec<&str> =
                    ctx.skills.all().iter().map(|s| s.name.as_str()).collect();
                Err(ToolError::msg(format!(
                    "{e}. Skills disponíveis: {}",
                    if disponiveis.is_empty() {
                        "nenhuma".to_string()
                    } else {
                        disponiveis.join(", ")
                    }
                )))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn especificacoes_sao_validas() {
        use super::*;
        use crate::tools::Tool;
        assert_eq!(ListSkills.nome(), "list_skills");
        assert_eq!(LoadSkill.nome(), "load_skill");
        assert_eq!(LoadSkill.spec().parameters["required"][0], "nome");
    }

    /// Conteúdo de skill grande precisa ser recortado antes de entrar no
    /// contexto, como as outras ferramentas.
    #[tokio::test]
    async fn load_skill_recorta_conteudo_gigante() {
        use super::*;
        use crate::tools::Tool;
        let raiz =
            std::env::temp_dir().join(format!("kestrel-skill-trunc-{}", uuid::Uuid::new_v4()));
        let projeto = raiz.join("projeto");
        std::fs::create_dir_all(projeto.join(".kestrel/skills/gigante")).unwrap();
        let corpo = "linha de skill muito longa ".repeat(5000); // bem acima de max_saida
        std::fs::write(
            projeto.join(".kestrel/skills/gigante/SKILL.md"),
            format!("---\nname: gigante\ndescription: teste\n---\n\n{corpo}"),
        )
        .unwrap();
        let paths = crate::util::Paths::with_root(&raiz);
        let reg = std::sync::Arc::new(crate::skills::SkillRegistry::discover(&paths, &projeto));
        let mut ctx = crate::tools::git::tests::contexto(&projeto);
        ctx.skills = reg;
        let saida = LoadSkill
            .executar(serde_json::json!({"nome": "gigante"}), &ctx)
            .await
            .unwrap();
        assert!(
            saida.conteudo.contains("saída truncada"),
            "o conteúdo da skill deveria vir truncado: {}",
            saida.conteudo
        );
        assert!(
            saida.conteudo.chars().count() < 40_000,
            "a skill não pode entrar inteira no contexto"
        );
        let _ = std::fs::remove_dir_all(&raiz);
    }
}
