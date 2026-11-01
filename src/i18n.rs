//! Traduções da interface.
//!
//! O núcleo do agente e os logs continuam em português; esta camada cobre o
//! chrome visível (cabeçalho, rodapé, recepção, busca e atalhos) para que a
//! interface possa alternar entre pt-BR e en-US com `ui.language` ou `/lang`.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Idioma {
    #[default]
    PtBr,
    EnUs,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Chave {
    Modo,
    Permissao,
    Comp,
    Contexto,
    VoltarUltimo,
    CtrlCInterrompe,
    NovaSessao,
    Sugestoes,
    TodosComandos,
    ModoBuild,
    ModoConversa,
    DescrevaTarefa,
    BuscarTitulo,
    BuscarDica,
    BuscarNenhum,
    Copiada,
    SemResposta,
    Atalhos,
}

impl Idioma {
    pub fn de_config(valor: &str) -> Self {
        match valor.trim().to_lowercase().as_str() {
            "en" | "en-us" | "en_us" | "english" | "ingles" | "inglês" => Idioma::EnUs,
            _ => Idioma::PtBr,
        }
    }

    pub fn codigo(&self) -> &'static str {
        match self {
            Idioma::PtBr => "pt-BR",
            Idioma::EnUs => "en-US",
        }
    }

    pub fn t(&self, chave: Chave) -> &'static str {
        match (self, chave) {
            (Idioma::PtBr, Chave::Modo) => "modo",
            (Idioma::EnUs, Chave::Modo) => "mode",
            (Idioma::PtBr, Chave::Permissao) => "permissão",
            (Idioma::EnUs, Chave::Permissao) => "permission",
            (Idioma::PtBr, Chave::Comp) => "comp",
            (Idioma::EnUs, Chave::Comp) => "comp",
            (Idioma::PtBr, Chave::Contexto) => "contexto",
            (Idioma::EnUs, Chave::Contexto) => "context",
            (Idioma::PtBr, Chave::VoltarUltimo) => " ↓ voltar para o último ",
            (Idioma::EnUs, Chave::VoltarUltimo) => " ↓ back to latest ",
            (Idioma::PtBr, Chave::CtrlCInterrompe) => " Ctrl+C interrompe ",
            (Idioma::EnUs, Chave::CtrlCInterrompe) => " Ctrl+C interrupts ",
            (Idioma::PtBr, Chave::NovaSessao) => {
                "Nova sessão. Descreva a tarefa em linguagem natural."
            }
            (Idioma::EnUs, Chave::NovaSessao) => {
                "New session. Describe the task in natural language."
            }
            (Idioma::PtBr, Chave::Sugestoes) => "Sugestões para começar:",
            (Idioma::EnUs, Chave::Sugestoes) => "Suggestions to start:",
            (Idioma::PtBr, Chave::TodosComandos) => "todos os comandos",
            (Idioma::EnUs, Chave::TodosComandos) => "all commands",
            (Idioma::PtBr, Chave::ModoBuild) => "modo build (trabalha no projeto)",
            (Idioma::EnUs, Chave::ModoBuild) => "build mode (works on the project)",
            (Idioma::PtBr, Chave::ModoConversa) => "modo conversa (somente leitura)",
            (Idioma::EnUs, Chave::ModoConversa) => "chat mode (read only)",
            (Idioma::PtBr, Chave::DescrevaTarefa) => "descreva a tarefa em linguagem natural…",
            (Idioma::EnUs, Chave::DescrevaTarefa) => "describe the task in natural language…",
            (Idioma::PtBr, Chave::BuscarTitulo) => "buscar na conversa",
            (Idioma::EnUs, Chave::BuscarTitulo) => "search conversation",
            (Idioma::PtBr, Chave::BuscarDica) => "Enter/↓ próximo · ↑ anterior · Esc fecha",
            (Idioma::EnUs, Chave::BuscarDica) => "Enter/↓ next · ↑ previous · Esc closes",
            (Idioma::PtBr, Chave::BuscarNenhum) => "nenhum resultado",
            (Idioma::EnUs, Chave::BuscarNenhum) => "no results",
            (Idioma::PtBr, Chave::Copiada) => "resposta copiada para a área de transferência",
            (Idioma::EnUs, Chave::Copiada) => "answer copied to the clipboard",
            (Idioma::PtBr, Chave::SemResposta) => "não há resposta para copiar",
            (Idioma::EnUs, Chave::SemResposta) => "there is no answer to copy",
            (Idioma::PtBr, Chave::Atalhos) => "atalhos",
            (Idioma::EnUs, Chave::Atalhos) => "keybindings",
        }
    }
}
