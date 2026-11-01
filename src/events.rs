//! Tipos de evento compartilhados entre o agente, as ferramentas e a interface.
//! O estado do agente é explícito e vive fora do texto exibido na conversa.

use crate::perm::{Confirmacao, PermissionLevel};

/// Passo de um plano.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlanoPasso {
    pub titulo: String,
    pub status: StatusPasso,
    pub observacao: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StatusPasso {
    Pendente,
    EmAndamento,
    Concluido,
    Falhou,
    Pulado,
}

impl StatusPasso {
    pub fn label(&self) -> &'static str {
        match self {
            StatusPasso::Pendente => "pendente",
            StatusPasso::EmAndamento => "em andamento",
            StatusPasso::Concluido => "concluído",
            StatusPasso::Falhou => "falhou",
            StatusPasso::Pulado => "pulado",
        }
    }

    pub fn icone(&self) -> &'static str {
        match self {
            StatusPasso::Pendente => "○",
            StatusPasso::EmAndamento => "◐",
            StatusPasso::Concluido => "●",
            StatusPasso::Falhou => "✕",
            StatusPasso::Pulado => "–",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "em_andamento" | "em andamento" | "andamento" | "in_progress" => {
                StatusPasso::EmAndamento
            }
            "concluido" | "concluído" | "concluida" | "done" | "completed" => {
                StatusPasso::Concluido
            }
            "falhou" | "falha" | "failed" => StatusPasso::Falhou,
            "pulado" | "skipped" => StatusPasso::Pulado,
            _ => StatusPasso::Pendente,
        }
    }

    pub fn as_storage(&self) -> &'static str {
        match self {
            StatusPasso::Pendente => "pendente",
            StatusPasso::EmAndamento => "em_andamento",
            StatusPasso::Concluido => "concluido",
            StatusPasso::Falhou => "falhou",
            StatusPasso::Pulado => "pulado",
        }
    }
}

/// Visão de uma subtarefa.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TarefaView {
    pub id: String,
    pub titulo: String,
    pub status: StatusPasso,
    pub dependencias: Vec<String>,
    pub resultado: Option<String>,
}

impl TarefaView {
    /// Identificadores das dependências que ainda não foram concluídas.
    ///
    /// A subtarefa fica bloqueada enquanto essa lista não estiver vazia —
    /// o estado é derivado dos dados reais persistidos, não de texto do modelo.
    pub fn bloqueios<'a>(&'a self, todas: &'a [TarefaView]) -> Vec<&'a str> {
        self.dependencias
            .iter()
            .filter(|dep| {
                todas
                    .iter()
                    .find(|t| &t.id == *dep)
                    .is_some_and(|t| t.status != StatusPasso::Concluido)
            })
            .map(|s| s.as_str())
            .collect()
    }
}

/// Estado de uma execução de processo.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EstadoProcesso {
    Executando,
    Concluido,
    Falhou,
    Cancelado,
    Timeout,
}

impl EstadoProcesso {
    pub fn label(&self) -> &'static str {
        match self {
            EstadoProcesso::Executando => "executando",
            EstadoProcesso::Concluido => "concluído",
            EstadoProcesso::Falhou => "falhou",
            EstadoProcesso::Cancelado => "cancelado",
            EstadoProcesso::Timeout => "timeout",
        }
    }
}

/// Visão de um processo gerenciado.
#[derive(Clone, Debug)]
pub struct ProcessoView {
    pub id: String,
    pub comando: String,
    pub cwd: String,
    pub estado: EstadoProcesso,
    pub codigo_saida: Option<i32>,
    pub iniciado_em: String,
    pub linhas: usize,
}

/// Pedido de confirmação exibido na interface.
#[derive(Debug)]
pub struct ConfirmRequest {
    pub titulo: String,
    pub detalhe: String,
    pub nivel: PermissionLevel,
    pub resposta: tokio::sync::oneshot::Sender<Confirmacao>,
}

/// Eventos enviados do agente para a interface.
#[derive(Debug)]
pub enum AgentEvent {
    TurnoIniciado,
    Texto {
        delta: String,
    },
    Raciocinio {
        delta: String,
    },
    MensagemFinal {
        conteudo: String,
    },
    FerramentaIniciada {
        /// Identificador único da chamada do modelo (permite casar início e
        /// fim mesmo com ferramentas de mesmo nome rodando em paralelo).
        id: Option<String>,
        nome: String,
        detalhe: String,
        /// Subtarefa do subagente que a chamou; `None` quando é o agente
        /// principal. Marca o bloco como trabalho delegado.
        subagente: Option<String>,
    },
    FerramentaSaida {
        id: Option<String>,
        nome: String,
        linha: String,
    },
    FerramentaConcluida {
        id: Option<String>,
        nome: String,
        sucesso: bool,
        resumo: String,
        adicoes: usize,
        remocoes: usize,
        /// Diff unificado, quando a ferramenta produziu um (edições de arquivo).
        diff: String,
    },
    /// Um subagente começou a trabalhar numa subtarefa (abre um grupo).
    SubagenteIniciado {
        tarefa: String,
    },
    /// O subagente terminou a subtarefa (fecha o grupo).
    SubagenteConcluido {
        sucesso: bool,
    },
    PlanoAtualizado {
        objetivo: String,
        passos: Vec<PlanoPasso>,
    },
    /// Catálogo de modelos atualizado em segundo plano (descoberta remota).
    /// `modelos` já vem mesclado com as entradas manuais; `novos` é quantos
    /// modelos inéditos entraram nesta atualização (0 = nada mudou).
    CatalogoAtualizado {
        modelos: Vec<crate::config::ModelEntry>,
        novos: usize,
    },
    TarefasAtualizadas {
        tarefas: Vec<TarefaView>,
    },
    Contexto {
        usados: u32,
        capacidade: u32,
    },
    /// Uso de tokens informado pelo provedor ao fim de uma geração.
    Tokens {
        prompt: u32,
        completion: u32,
        raciocinio: u32,
    },
    DossieCriado {
        resumo: String,
        nova_sessao: String,
    },
    Erro {
        mensagem: String,
        dica: Option<String>,
        recuperavel: bool,
    },
    Aviso {
        mensagem: String,
    },
    Recuperacao {
        descricao: String,
        tentativa: u32,
        maximo: u32,
    },
    TurnoConcluido {
        resumo: String,
    },
    Interrompido {
        resumo: String,
    },
    Confirmacao {
        pedido: ConfirmRequest,
    },
    Estado {
        mensagem: String,
    },
    SessaoAtiva {
        id: String,
        titulo: String,
        /// Diretório do projeto da sessão. Retomar uma sessão de outro
        /// projeto precisa trocar o diretório de trabalho junto.
        workdir: String,
    },
    /// A sessão atual virou um ramo novo; a interface continua na mesma
    /// conversa, apenas muda o id ativo.
    Ramificada {
        id: String,
        titulo: String,
        mensagens: usize,
    },
    ModeloAtual {
        modelo: String,
        esforco: Option<String>,
    },
    Processos {
        lista: Vec<ProcessoView>,
    },
    /// Atualiza a fotografia dos processos sem abrir o seletor na interface.
    /// Ferramentas em segundo plano emitem este evento; `Processos` é
    /// reservado ao comando explícito `/processes`.
    ProcessosAtualizados {
        lista: Vec<ProcessoView>,
    },
    Encerrado,
}

/// Comandos enviados da interface para o agente.
#[derive(Debug)]
pub enum UiCommand {
    EnviarMensagem(String),
    Redirecionar(String),
    Interromper,
    DefinirPermissao(PermissionLevel),
    DefinirModo(crate::config::ModoOperacao),
    DefinirModelo {
        modelo: String,
        esforco: Option<String>,
    },
    DefinirEsforco(Option<String>),
    /// Liga/desliga a exibição do raciocínio do modelo na conversa.
    DefinirRaciocinio(bool),
    /// Define a skin visual ativa e mantém a cópia do agente sincronizada.
    DefinirSkin(String),
    /// Ativa (ou desativa, com `None`) uma personalidade.
    DefinirComp(Option<String>),
    NovaSessao,
    RetomarSessao(String),
    /// Cria um ramo/checkpoint da sessão atual (cópia do histórico e estado).
    Ramificar {
        titulo: Option<String>,
    },
    /// Relê os diretórios de skills (instalação sem reiniciar).
    RecarregarSkills,
    /// Troca o diretório de trabalho, reanalisa o projeto e abre nova sessão.
    TrocarProjeto(String),
    AnalisarProjeto,
    ListarProcessos,
    Encerrar,
}
