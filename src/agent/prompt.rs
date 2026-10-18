//! Construção do prompt de sistema enviado ao modelo.

use crate::config::ModoOperacao;
use crate::events::{PlanoPasso, TarefaView};
use crate::perm::PermissionLevel;
use crate::project::ProjectInfo;
use crate::skills::SkillRegistry;
use std::path::Path;

/// Dados usados para montar o prompt de sistema.
pub struct PromptContext<'a> {
    pub projeto: &'a ProjectInfo,
    pub skills: &'a SkillRegistry,
    pub nivel: PermissionLevel,
    pub modo: ModoOperacao,
    pub esforco: Option<&'a str>,
    pub plano: Option<(String, Vec<PlanoPasso>)>,
    pub tarefas: Vec<TarefaView>,
    pub dossie: Option<String>,
    pub workdir: &'a Path,
    pub diretorios_extra: Vec<String>,
}

/// Monta o prompt de sistema completo.
pub fn construir(ctx: &PromptContext<'_>) -> String {
    let mut p = String::new();

    p.push_str(
        "Você é Kestrel, um agente de codificação autônomo executando no terminal do usuário.\n\
         Você trabalha diretamente no projeto, com ferramentas reais: lê e edita arquivos, executa \
         comandos, roda testes, usa git e pesquisa na web.\n\n",
    );

    p.push_str("## Regras de trabalho\n\
        - Todo texto visível ao usuário (respostas, planos, relatórios, mensagens de erro) deve estar em português brasileiro.\n\
        - Escreva corretamente: cada frase começa com letra maiúscula, termina com pontuação e mantém todos os acentos \
(você, não, português, raciocínio, ação, método, história, único). Nunca omita acentos nem escreva em caixa alta inteira.\n\
        - Nomes de código, APIs, comandos e identificadores permanecem em suas convenções originais.\n\
        - Antes de editar um arquivo, leia o conteúdo atual. Nunca invente conteúdo de arquivos.\n\
        - Depois de alterar código, valide com o que existir no projeto (build, testes, linters). Não declare sucesso sem verificar.\n\
        - Nunca invente resultados de comandos, saídas de testes ou números: use as ferramentas e reporte o que realmente aconteceu.\n\
        - Prefira passos pequenos e verificáveis. Se algo falhar, diagnostique a causa antes de tentar de novo.\n\
        - Não repita o plano inteiro na conversa: registre-o com `update_plan` e avise só mudanças relevantes.\n\
        - Não peça ao usuário para colar segredos na conversa.\n\
        - Use `load_skill` quando uma skill listada for pertinente à tarefa.\n\
        - Trabalhe dentro do projeto; para agir fora dele, explique por quê.\n\n",
    );

    p.push_str(&format!(
        "## Ambiente\n\
        - Diretório de trabalho: {}\n\
        - Nível de permissão: {} ({})\n",
        ctx.workdir.display(),
        ctx.nivel.label(),
        ctx.nivel.description()
    ));
    match ctx.modo {
        ModoOperacao::Chat => p.push_str(
            "- Modo de operação: chat. Você conversa e pode inspecionar o projeto apenas com ferramentas \
             de leitura. Não modifique arquivos, não execute comandos, não registre planos nem subtarefas. \
             Se a solicitação exigir mudanças, explique o que faria e peça ao usuário para alternar para o \
             modo agente com /agente.\n",
        ),
        ModoOperacao::Agente => p.push_str(
            "- Modo de operação: agente. Você trabalha no projeto com as ferramentas disponíveis, \
             sempre dentro do nível de permissão abaixo.\n",
        ),
    }
    match ctx.nivel {
        PermissionLevel::Read => p.push_str(
            "- Você pode ler e analisar. Não pode modificar arquivos nem executar comandos que alterem estado. \
             Se a tarefa exigir modificações, explique e peça ao usuário para mudar o nível com /permissions.\n",
        ),
        PermissionLevel::Write => p.push_str(
            "- Você pode ler e editar arquivos do projeto. Comandos sensíveis (instalações, rede, operações destrutivas, git destrutivo) exigem confirmação do usuário.\n",
        ),
        PermissionLevel::Semi => p.push_str(
            "- Você trabalha de forma autônoma. Operações sensíveis pedem confirmação; explique o motivo ao pedir.\n",
        ),
        PermissionLevel::Full => p.push_str(
            "- Autonomia máxima: sem confirmações rotineiras. Operações catastróficas continuam bloqueadas pela proteção de comandos.\n",
        ),
    }
    if let Some(esforco) = ctx.esforco {
        p.push_str(&format!("- Esforço de raciocínio solicitado: {esforco}\n"));
    }
    if !ctx.diretorios_extra.is_empty() {
        p.push_str(&format!(
            "- Diretórios adicionais autorizados: {}\n",
            ctx.diretorios_extra.join(", ")
        ));
    }

    p.push_str("\n## Projeto\n");
    p.push_str(&ctx.projeto.prompt_block());
    p.push('\n');

    let tarefa_atual = ctx
        .plano
        .as_ref()
        .map(|(objetivo, _)| objetivo.as_str())
        .unwrap_or("");
    let skills = ctx.skills.prompt_block(tarefa_atual);
    if !skills.is_empty() {
        p.push_str(&format!("\n## Skills\n{skills}\n"));
    }

    if let Some((objetivo, passos)) = &ctx.plano {
        p.push_str(&format!("\n## Plano atual\nObjetivo: {objetivo}\n"));
        for passo in passos {
            p.push_str(&format!(
                "- {} {}{}\n",
                passo.status.icone(),
                passo.titulo,
                passo
                    .observacao
                    .as_deref()
                    .map(|o| format!(" ({o})"))
                    .unwrap_or_default()
            ));
        }
    }

    if !ctx.tarefas.is_empty() {
        p.push_str("\n## Subtarefas\n");
        for t in &ctx.tarefas {
            let bloqueio = if t.dependencias.is_empty() {
                String::new()
            } else {
                format!(" depende de [{}]", t.dependencias.join(", "))
            };
            p.push_str(&format!(
                "- [{}] {} {}{bloqueio}\n",
                t.id,
                t.status.icone(),
                t.titulo
            ));
        }
        p.push_str(
            "Execute uma subtarefa dependente apenas depois que suas dependências estiverem concluídas.\n",
        );
    }

    if let Some(dossie) = &ctx.dossie {
        p.push_str(&format!(
            "\n## Continuidade\nVocê está retomando um trabalho interrompido. Use o dossiê abaixo e confirme o estado real do projeto antes de continuar.\n\n{dossie}\n"
        ));
    }

    p.push_str(
        "\n## Encerramento\n\
         Ao concluir (ou ao interromper por um limite), apresente um relatório curto: o que foi feito, \
         arquivos alterados, comandos/testes executados com resultados reais, pendências e próximos passos.\n",
    );

    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::StatusPasso;
    use crate::project::ProjectInfo;

    #[test]
    fn prompt_contem_secoes_principais() {
        let projeto = ProjectInfo {
            name: "demo".into(),
            languages: vec!["Rust".into()],
            test_command: Some("cargo test".into()),
            ..Default::default()
        };
        let skills = SkillRegistry::vazio();
        let ctx = PromptContext {
            projeto: &projeto,
            skills: &skills,
            nivel: PermissionLevel::Semi,
            modo: ModoOperacao::Agente,
            esforco: Some("high"),
            plano: Some((
                "corrigir parser".into(),
                vec![PlanoPasso {
                    titulo: "analisar".into(),
                    status: StatusPasso::EmAndamento,
                    observacao: None,
                }],
            )),
            tarefas: vec![TarefaView {
                id: "t1".into(),
                titulo: "escrever teste".into(),
                status: StatusPasso::Pendente,
                dependencias: vec![],
                resultado: None,
            }],
            dossie: None,
            workdir: Path::new("/proj"),
            diretorios_extra: vec![],
        };
        let texto = construir(&ctx);
        assert!(texto.contains("Kestrel"));
        assert!(texto.contains("português brasileiro"));
        assert!(texto.contains("Nível de permissão: Semi"));
        assert!(texto.contains("corrigir parser"));
        assert!(texto.contains("escrever teste"));
        assert!(texto.contains("cargo test"));
    }

    #[test]
    fn nivel_read_instrui_a_nao_modificar() {
        let projeto = ProjectInfo::default();
        let skills = SkillRegistry::vazio();
        let ctx = PromptContext {
            projeto: &projeto,
            skills: &skills,
            nivel: PermissionLevel::Read,
            modo: ModoOperacao::Agente,
            esforco: None,
            plano: None,
            tarefas: vec![],
            dossie: None,
            workdir: Path::new("/p"),
            diretorios_extra: vec![],
        };
        let texto = construir(&ctx);
        assert!(texto.contains("Não pode modificar arquivos"));
    }

    #[test]
    fn modo_chat_instrui_a_somente_ler() {
        let projeto = ProjectInfo::default();
        let skills = SkillRegistry::vazio();
        let ctx = PromptContext {
            projeto: &projeto,
            skills: &skills,
            nivel: PermissionLevel::Full,
            modo: ModoOperacao::Chat,
            esforco: None,
            plano: None,
            tarefas: vec![],
            dossie: None,
            workdir: Path::new("/p"),
            diretorios_extra: vec![],
        };
        let texto = construir(&ctx);
        assert!(texto.contains("Modo de operação: chat"));
        assert!(texto.contains("/agente"));
        assert!(texto.contains("não execute comandos"));
    }
}
