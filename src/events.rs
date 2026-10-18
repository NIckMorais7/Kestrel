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
        nome: String,
        detalhe: String,
    },
    FerramentaSaida {
        nome: String,
        linha: String,
    },
    FerramentaConcluida {
        nome: String,
        sucesso: bool,
        resumo: String,
        adicoes: usize,
        remocoes: usize,
    },
    PlanoAtualizado {
        objetivo: String,
        passos: Vec<PlanoPasso>,
    },
    TarefasAtualizadas {
        tarefas: Vec<TarefaView>,
    },
    Contexto {
        usados: u32,
        capacidade: u32,
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
    },
    ModeloAtual {
        modelo: String,
        esforco: Option<String>,
    },
    Processos {
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
    NovaSessao,
    RetomarSessao(String),
    /// Troca o diretório de trabalho, reanalisa o projeto e abre nova sessão.
    TrocarProjeto(String),
    AnalisarProjeto,
    ListarProcessos,
    Encerrar,
}
