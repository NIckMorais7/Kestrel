//! Orçamento de contexto e dossiê de continuidade.
//!
//! O estado do agente vive no banco de dados (planos, tarefas, alterações,
//! ferramentas, decisões). O dossiê é montado a partir desses dados reais —
//! não de um resumo gerado pelo modelo.

use crate::db::Store;
use crate::provider::{ChatMessage, ToolSpec};

/// Orçamento de contexto para o modelo selecionado.
#[derive(Clone, Copy, Debug)]
pub struct OrcamentoContexto {
    pub janela: u32,
    pub reservados_resposta: u32,
}

impl OrcamentoContexto {
    pub fn novo(janela: u32, reservados_resposta: u32) -> Self {
        Self {
            janela,
            reservados_resposta: reservados_resposta.min(janela / 2),
        }
    }

    /// Capacidade utilizável para o prompt (janela menos a reserva de resposta).
    pub fn capacidade_utilizavel(&self) -> u32 {
        self.janela.saturating_sub(self.reservados_resposta).max(1)
    }

    /// Fração já utilizada.
    pub fn uso(&self, usados: u32) -> f32 {
        usados as f32 / self.capacidade_utilizavel() as f32
    }

    /// Indica se é hora de compactar (limiar padrão: 90%).
    pub fn precisa_compactar(&self, usados: u32, limiar: f32) -> bool {
        self.uso(usados) >= limiar
    }
}

/// Estima o total de tokens do prompt enviado ao modelo.
pub fn estimar_prompt(
    modelo: &str,
    sistema: &str,
    mensagens: &[ChatMessage],
    ferramentas: &[ToolSpec],
) -> u32 {
    let mut total = crate::tokenizer::contar(modelo, sistema);
    for m in mensagens {
        total += crate::tokenizer::contar(modelo, &m.content);
        for tc in &m.tool_calls {
            total += crate::tokenizer::contar(modelo, &tc.name)
                + crate::tokenizer::contar(modelo, &tc.arguments)
                + 8;
        }
        if m.tool_call_id.is_some() {
            total += 4;
        }
    }
    for f in ferramentas {
        total += crate::tokenizer::contar(modelo, &f.name)
            + crate::tokenizer::contar(modelo, &f.description)
            + crate::tokenizer::contar(modelo, &f.parameters.to_string())
            + 8;
    }
    total
}

/// Dossiê estruturado de continuidade.
#[derive(Clone, Debug, Default)]
pub struct Dossie {
    pub sessao_anterior: String,
    pub objetivo: String,
    pub instrucoes: Vec<String>,
    pub decisoes: Vec<String>,
    pub plano: Vec<String>,
    pub arquivos: Vec<String>,
    pub comandos: Vec<String>,
    pub testes: Vec<String>,
    pub erros: Vec<String>,
    pub subtarefas_pendentes: Vec<String>,
    pub subtarefas_concluidas: Vec<String>,
    pub ponto_interrupcao: String,
    pub proximas_acoes: String,
}

impl Dossie {
    /// Texto enviado como contexto para a nova sessão.
    pub fn para_texto(&self) -> String {
        let mut s = String::new();
        s.push_str("# Dossiê de continuidade\n\n");
        s.push_str(&format!("Sessão anterior: {}\n", self.sessao_anterior));
        s.push_str(&format!("Objetivo: {}\n\n", self.objetivo));
        let secao = |titulo: &str, itens: &[String]| -> String {
            if itens.is_empty() {
                return String::new();
            }
            format!(
                "## {titulo}\n{}\n\n",
                itens
                    .iter()
                    .map(|i| format!("- {i}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        s.push_str(&secao("Instruções originais", &self.instrucoes));
        s.push_str(&secao("Decisões tomadas", &self.decisoes));
        s.push_str(&secao("Plano atual", &self.plano));
        s.push_str(&secao("Arquivos modificados", &self.arquivos));
        s.push_str(&secao("Comandos executados", &self.comandos));
        s.push_str(&secao("Testes realizados", &self.testes));
        s.push_str(&secao("Erros e tentativas de recuperação", &self.erros));
        s.push_str(&secao("Subtarefas concluídas", &self.subtarefas_concluidas));
        s.push_str(&secao("Subtarefas pendentes", &self.subtarefas_pendentes));
        s.push_str(&format!(
            "## Ponto de interrupção\n{}\n\n",
            self.ponto_interrupcao
        ));
        s.push_str(&format!("## Próximas ações\n{}\n", self.proximas_acoes));
        s
    }

    /// Resumo curto para a interface.
    pub fn resumo(&self) -> String {
        format!(
            "{} arquivos alterados, {} comandos, {} subtarefas pendentes",
            self.arquivos.len(),
            self.comandos.len(),
            self.subtarefas_pendentes.len()
        )
    }
}

/// Monta o dossiê a partir do estado persistido.
pub fn construir_dossie(
    store: &Store,
    session_id: &str,
    projeto_summary: &str,
    ponto_interrupcao: &str,
    proximas_acoes: &str,
) -> anyhow::Result<Dossie> {
    let mut d = Dossie {
        sessao_anterior: session_id.to_string(),
        ponto_interrupcao: ponto_interrupcao.to_string(),
        proximas_acoes: proximas_acoes.to_string(),
        ..Default::default()
    };

    if let Some(sessao) = store.get_session(session_id)? {
        d.objetivo = sessao.objective.unwrap_or_else(|| sessao.title.clone());
    }

    // Instruções: mensagens do usuário (as mais recentes — o dossiê precisa
    // do que está em andamento, não do começo da conversa).
    for m in store.recent_messages(session_id, 2000)? {
        if m.role == "user" {
            d.instrucoes
                .push(crate::util::truncate(m.content.trim(), 400));
        }
    }

    // Decisões.
    for (_, resumo, _) in store.decisions(session_id, 50)? {
        d.decisoes.push(resumo);
    }

    // Plano mais recente.
    if let Some((objetivo, passos, _)) = store.latest_plan(session_id)? {
        if d.objetivo.is_empty() {
            d.objetivo = objetivo;
        }
        if let Ok(lista) = serde_json::from_str::<Vec<serde_json::Value>>(&passos) {
            for p in lista {
                let titulo = p.get("titulo").and_then(|t| t.as_str()).unwrap_or("");
                let status = p
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("pendente");
                d.plano.push(format!("{titulo} [{status}]"));
            }
        }
    }

    // Subtarefas.
    for t in store.tasks(session_id)? {
        let descricao = format!("{} [{}]", t.title, t.status);
        if t.status == "concluido" {
            d.subtarefas_concluidas.push(descricao);
        } else {
            d.subtarefas_pendentes.push(descricao);
        }
    }

    // Arquivos alterados: o dossiê guarda a alteração mais recente de cada
    // caminho. Antes o `dedup_by` comparava o primeiro token separado por
    // espaço, então caminhos com espaço colidiam e duplicatas não adjacentes
    // sobreviviam.
    let mut vistos = std::collections::HashSet::new();
    let mut arquivos = Vec::new();
    for (path, tipo, adicoes, remocoes) in store.file_changes(session_id)?.into_iter().rev() {
        if vistos.insert(path.clone()) {
            arquivos.push(format!("{path} ({tipo}, +{adicoes} -{remocoes})"));
        }
    }
    arquivos.reverse();
    d.arquivos = arquivos;

    // Comandos e testes.
    let recentes = store.recent_messages(session_id, 400)?;
    for m in &recentes {
        if m.role == "tool" {
            let nome = m.tool_name.clone().unwrap_or_default();
            let resumo = crate::util::truncate(m.content.lines().next().unwrap_or(""), 160);
            if nome == "run_tests" {
                d.testes.push(resumo);
            } else if nome == "run_command" {
                d.comandos.push(resumo);
            }
        }
    }
    // O dossiê precisa do que está em andamento: `recent_messages` devolve a
    // conversa em ordem cronológica, então o excedente é descartado do INÍCIO
    // (os mais antigos). Um `truncate` manteria os primeiros N e a compactação
    // jogaria fora justamente os comandos e erros recentes.
    manter_os_ultimos(&mut d.comandos, 20);
    manter_os_ultimos(&mut d.testes, 20);

    // Erros: mensagens de ferramenta que indicam falha.
    for m in &recentes {
        let texto = m.content.to_lowercase();
        if m.role == "tool"
            && (texto.contains("error")
                || texto.contains("falh")
                || texto.contains("bloqueado")
                || texto.contains("negado"))
        {
            d.erros.push(crate::util::truncate(m.content.trim(), 200));
        }
    }
    manter_os_ultimos(&mut d.erros, 10);
    manter_os_ultimos(&mut d.instrucoes, 10);
    if !projeto_summary.is_empty() {
        d.decisoes.push(format!(
            "Contexto do projeto: {}",
            crate::util::truncate(projeto_summary, 400)
        ));
    }
    Ok(d)
}

/// Mantém apenas os últimos `maximo` itens, descartando o excedente do início.
///
/// `truncate` mantém os primeiros N; como as listas do dossiê são montadas em
/// ordem cronológica, isso guardaria as instruções e os erros do COMEÇO da
/// conversa em vez do que está em andamento.
fn manter_os_ultimos(itens: &mut Vec<String>, maximo: usize) {
    if itens.len() > maximo {
        let excesso = itens.len() - maximo;
        itens.drain(..excesso);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{Store, TaskRow};

    #[test]
    fn orcamento_respeita_reserva() {
        let o = OrcamentoContexto::novo(1000, 100);
        assert_eq!(o.capacidade_utilizavel(), 900);
        assert!(o.precisa_compactar(810, 0.9));
        assert!(!o.precisa_compactar(800, 0.9));
        assert!(o.uso(450) > 0.49 && o.uso(450) < 0.51);
    }

    #[test]
    fn reserva_nao_pode_dominar_a_janela() {
        let o = OrcamentoContexto::novo(1000, 900);
        assert_eq!(o.capacidade_utilizavel(), 500);
    }

    #[test]
    fn estimativa_cresce_com_mensagens() {
        let sistema = "sistema";
        let uma = vec![ChatMessage::user("mensagem curta")];
        let muitas = vec![
            ChatMessage::user("mensagem curta"),
            ChatMessage::assistant("resposta"),
        ];
        let a = estimar_prompt("modelo/teste", sistema, &uma, &[]);
        let b = estimar_prompt("modelo/teste", sistema, &muitas, &[]);
        assert!(b > a);
    }

    #[test]
    fn dossie_monta_estado_persistido() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_session("s1", "/p", "corrigir parser", "m", None, "semi", None)
            .unwrap();
        store
            .append_message(
                "s1",
                "user",
                "corrija o parser de expressões",
                None,
                None,
                None,
                None,
            )
            .unwrap();
        store
            .save_plan("s1", "corrigir parser", r#"[{"titulo":"analisar","status":"concluido"},{"titulo":"testar","status":"pendente"}]"#)
            .unwrap();
        store
            .record_file_change("s1", "src/parser.rs", "edicao", 5, 2, None)
            .unwrap();
        store
            .upsert_task(&TaskRow {
                id: "t1".into(),
                session_id: "s1".into(),
                parent_id: None,
                title: "escrever teste".into(),
                description: None,
                status: "pendente".into(),
                depends_on: vec![],
                order_idx: 0,
                result: None,
            })
            .unwrap();
        store
            .append_message(
                "s1",
                "tool",
                "error[E0308]: mismatched types",
                None,
                Some("call_1"),
                Some("run_command"),
                None,
            )
            .unwrap();

        let d = construir_dossie(
            &store,
            "s1",
            "Rust",
            "corrigindo o erro E0308",
            "rodar cargo test",
        )
        .unwrap();
        assert_eq!(d.objetivo, "corrigir parser");
        assert!(d.instrucoes[0].contains("parser de expressões"));
        assert_eq!(d.plano.len(), 2);
        assert!(d.arquivos[0].contains("src/parser.rs"));
        assert_eq!(d.subtarefas_pendentes.len(), 1);
        assert!(!d.erros.is_empty());
        let texto = d.para_texto();
        assert!(texto.contains("## Ponto de interrupção"));
        assert!(texto.contains("rodar cargo test"));
    }

    /// Deduplicar pelo primeiro token separado por espaço fazia dois caminhos
    /// com espaço colidirem e deixava duplicatas não adjacentes passarem.
    #[test]
    fn dossie_deduplica_por_caminho_inteiro() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_session("s1", "/p", "tarefa", "m", None, "semi", None)
            .unwrap();
        for (path, tipo) in [
            ("src/meu arquivo.rs", "edicao"),
            ("src/outro.rs", "edicao"),
            ("src/meu arquivo.rs", "edicao"),
            ("src/outro.rs", "criacao"),
        ] {
            store
                .record_file_change("s1", path, tipo, 1, 0, None)
                .unwrap();
        }
        let d = construir_dossie(&store, "s1", "", "", "").unwrap();
        assert_eq!(d.arquivos.len(), 2, "{:?}", d.arquivos);
        assert_eq!(
            d.arquivos
                .iter()
                .filter(|a| a.contains("meu arquivo.rs"))
                .count(),
            1
        );
        assert!(
            d.arquivos.iter().any(|a| a.contains("outro.rs (criacao")),
            "deveria manter a alteração mais recente: {:?}",
            d.arquivos
        );
    }

    /// Depois de compactar, o agente continua a partir do que estava em
    /// andamento: as listas do dossiê mantêm os itens MAIS RECENTES. O
    /// `truncate` anterior preservava os primeiros N — as instruções e os
    /// erros do início da conversa.
    #[test]
    fn dossie_mantem_as_instrucoes_e_erros_mais_recentes() {
        let store = Store::open_in_memory().unwrap();
        store
            .create_session("s1", "/p", "tarefa longa", "m", None, "semi", None)
            .unwrap();
        // Instruções, comandos e testes numerados: cada lista tem o próprio
        // teto (10 instruções, 20 comandos, 20 testes, 10 erros).
        for i in 0..25 {
            store
                .append_message(
                    "s1",
                    "user",
                    &format!("instrução {i:02} do usuário"),
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap();
            store
                .append_message(
                    "s1",
                    "tool",
                    &format!("Comando: cargo check {i:02}\nsaida {i:02}"),
                    None,
                    Some(&format!("c{i:02}")),
                    Some("run_command"),
                    None,
                )
                .unwrap();
            store
                .append_message(
                    "s1",
                    "tool",
                    &format!("test result: FAILED (falha numerada {i:02})\nerror[E0308]"),
                    None,
                    Some(&format!("e{i:02}")),
                    Some("run_tests"),
                    None,
                )
                .unwrap();
        }
        let d = construir_dossie(&store, "s1", "", "", "").unwrap();
        assert_eq!(d.instrucoes.len(), 10, "{:?}", d.instrucoes);
        assert!(
            d.instrucoes.iter().any(|i| i.contains("instrução 24")),
            "a instrução mais recente deveria sobreviver: {:?}",
            d.instrucoes
        );
        assert!(
            !d.instrucoes.iter().any(|i| i.contains("instrução 14")),
            "instruções antigas deveriam ter sido descartadas: {:?}",
            d.instrucoes
        );
        assert_eq!(d.comandos.len(), 20, "{:?}", d.comandos);
        assert!(
            d.comandos.iter().any(|c| c.contains("cargo check 24")),
            "o comando mais recente deveria sobreviver: {:?}",
            d.comandos
        );
        assert!(
            !d.comandos.iter().any(|c| c.contains("cargo check 03")),
            "comandos antigos deveriam ter sido descartados: {:?}",
            d.comandos
        );
        assert_eq!(d.testes.len(), 20, "{:?}", d.testes);
        assert!(
            d.testes.iter().any(|t| t.contains("numerada 24")),
            "o teste mais recente deveria sobreviver: {:?}",
            d.testes
        );
        assert_eq!(d.erros.len(), 10, "{:?}", d.erros);
        assert!(
            d.erros.iter().any(|e| e.contains("numerada 24")),
            "o erro mais recente deveria sobreviver: {:?}",
            d.erros
        );
        assert!(
            !d.erros.iter().any(|e| e.contains("numerada 14")),
            "erros antigos deveriam ter sido descartados: {:?}",
            d.erros
        );
        let texto = d.para_texto();
        assert!(texto.contains("cargo check 24"));
        assert!(!texto.contains("cargo check 03\n"), "{texto}");
        assert!(texto.contains("falha numerada 24"));
    }

    #[test]
    fn manter_os_ultimos_descarta_do_inicio() {
        let mut itens: Vec<String> = (0..5).map(|i| i.to_string()).collect();
        manter_os_ultimos(&mut itens, 3);
        assert_eq!(itens, vec!["2", "3", "4"]);
        manter_os_ultimos(&mut itens, 10);
        assert_eq!(itens, vec!["2", "3", "4"], "não deveria mexer na lista");
    }
}
