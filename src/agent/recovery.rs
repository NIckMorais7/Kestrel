//! Classificação de falhas e orçamento de recuperação automática.
//!
//! Regras aplicadas:
//! - o diagnóstico inicial de um incidente não conta como tentativa;
//! - até 9 tentativas de correção por incidente;
//! - limite de 20 minutos de recuperação automática por incidente;
//! - depois de uma correção malsucedida, a estratégia seguinte deve ser
//!   diferente (repetir exige justificativa técnica registrada).

use std::collections::HashMap;
use std::time::{Duration, Instant};

/// Classificação de um incidente de falha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Classificacao {
    /// Falha transitória de rede/API.
    ApiTransitoria,
    /// Erro de execução de ferramenta.
    ExecucaoFerramenta,
    /// Erro de compilação.
    Compilacao,
    /// Falha de testes.
    Testes,
    /// Resposta malformada do modelo.
    RespostaMalformada,
    /// Falha persistente de implementação.
    Implementacao,
}

impl Classificacao {
    pub fn label(&self) -> &'static str {
        match self {
            Classificacao::ApiTransitoria => "falha transitória de API",
            Classificacao::ExecucaoFerramenta => "erro de execução de ferramenta",
            Classificacao::Compilacao => "erro de compilação",
            Classificacao::Testes => "falha de testes",
            Classificacao::RespostaMalformada => "resposta malformada do modelo",
            Classificacao::Implementacao => "falha de implementação",
        }
    }

    pub fn as_storage(&self) -> &'static str {
        match self {
            Classificacao::ApiTransitoria => "api_transitoria",
            Classificacao::ExecucaoFerramenta => "execucao_ferramenta",
            Classificacao::Compilacao => "compilacao",
            Classificacao::Testes => "testes",
            Classificacao::RespostaMalformada => "resposta_malformada",
            Classificacao::Implementacao => "implementacao",
        }
    }
}

/// Classifica uma falha a partir do nome da ferramenta e da mensagem.
pub fn classificar(ferramenta: Option<&str>, mensagem: &str, transitorio: bool) -> Classificacao {
    if transitorio {
        return Classificacao::ApiTransitoria;
    }
    let m = mensagem.to_lowercase();
    if m.contains("error[e") || m.contains("could not compile") || m.contains("erro de compilação")
    {
        return Classificacao::Compilacao;
    }
    if m.contains("test result: failed")
        || m.contains("testes falhando")
        || m.contains("falha de testes")
    {
        return Classificacao::Testes;
    }
    if m.contains("resposta inválida") || m.contains("json inválido") || m.contains("malformada")
    {
        return Classificacao::RespostaMalformada;
    }
    match ferramenta {
        Some("run_tests") => Classificacao::Testes,
        Some("run_command") | Some("git_commit") => Classificacao::ExecucaoFerramenta,
        _ => Classificacao::Implementacao,
    }
}

/// Estado de recuperação de um incidente.
struct Incidente {
    tentativas: u32,
    iniciado_em: Instant,
    estrategias: Vec<String>,
}

/// Diagnóstico apresentado ao agente antes da correção.
#[derive(Clone, Debug)]
pub struct Diagnostico {
    pub classificacao: Classificacao,
    pub tentativas: u32,
    pub restantes: u32,
    pub tempo_restante: Duration,
    pub expirado: bool,
    pub estrategias_usadas: Vec<String>,
    pub sugestao: String,
}

/// Motivo pelo qual a recuperação automática foi interrompida.
#[derive(Clone, Debug)]
pub struct LimiteAtingido {
    pub classificacao: Classificacao,
    pub tentativas: u32,
    pub tempo_gasto: Duration,
    pub estrategias: Vec<String>,
}

/// Orçamento de recuperação para a sessão de trabalho atual.
pub struct Recuperacao {
    incidentes: HashMap<Classificacao, Incidente>,
    max_tentativas: u32,
    limite_tempo: Duration,
}

impl Recuperacao {
    pub fn novo(max_tentativas: u32, limite_tempo: Duration) -> Self {
        Self {
            incidentes: HashMap::new(),
            max_tentativas: max_tentativas.max(1),
            limite_tempo,
        }
    }

    /// Registra o diagnóstico inicial (não conta como tentativa) e devolve o
    /// estado da recuperação para essa classificação.
    pub fn diagnosticar(&mut self, classificacao: Classificacao) -> Diagnostico {
        let incidente = self
            .incidentes
            .entry(classificacao)
            .or_insert_with(|| Incidente {
                tentativas: 0,
                iniciado_em: Instant::now(),
                estrategias: Vec::new(),
            });
        let gasto = incidente.iniciado_em.elapsed();
        let expirado = gasto >= self.limite_tempo;
        Diagnostico {
            classificacao,
            tentativas: incidente.tentativas,
            restantes: self.max_tentativas.saturating_sub(incidente.tentativas),
            tempo_restante: self.limite_tempo.saturating_sub(gasto),
            expirado,
            estrategias_usadas: incidente.estrategias.clone(),
            sugestao: sugestao_de_estrategia(classificacao, &incidente.estrategias),
        }
    }

    /// Registra uma tentativa de correção com a estratégia adotada.
    pub fn registrar_tentativa(
        &mut self,
        classificacao: Classificacao,
        estrategia: &str,
    ) -> Result<u32, LimiteAtingido> {
        let max = self.max_tentativas;
        let limite = self.limite_tempo;
        let incidente = self
            .incidentes
            .entry(classificacao)
            .or_insert_with(|| Incidente {
                tentativas: 0,
                iniciado_em: Instant::now(),
                estrategias: Vec::new(),
            });
        incidente.tentativas += 1;
        incidente.estrategias.push(estrategia.to_string());
        let gasto = incidente.iniciado_em.elapsed();
        if incidente.tentativas > max || gasto >= limite {
            return Err(LimiteAtingido {
                classificacao,
                tentativas: incidente.tentativas,
                tempo_gasto: gasto,
                estrategias: incidente.estrategias.clone(),
            });
        }
        Ok(incidente.tentativas)
    }

    /// Indica se a estratégia já foi usada (repetição exige justificativa).
    pub fn estrategia_repetida(&self, classificacao: Classificacao, estrategia: &str) -> bool {
        self.incidentes
            .get(&classificacao)
            .map(|i| i.estrategias.iter().any(|e| e == estrategia))
            .unwrap_or(false)
    }

    /// Máximo de tentativas de correção por incidente.
    pub fn max_tentativas(&self) -> u32 {
        self.max_tentativas
    }

    /// Marca o incidente como resolvido.
    pub fn resolver(&mut self, classificacao: Classificacao) {
        self.incidentes.remove(&classificacao);
    }

    pub fn limpar(&mut self) {
        self.incidentes.clear();
    }
}

fn sugestao_de_estrategia(c: Classificacao, usadas: &[String]) -> String {
    let base = match c {
        Classificacao::ApiTransitoria => {
            "aguarde e repita a chamada; se persistir, verifique a rede e a chave de API"
        }
        Classificacao::ExecucaoFerramenta => {
            "leia a mensagem de erro completa e ajuste os argumentos da ferramenta"
        }
        Classificacao::Compilacao => {
            "corrija o erro de compilação indicado; se a mesma correção falhar, revise o diagnóstico lendo o código completo da região"
        }
        Classificacao::Testes => {
            "analise o teste que falhou, leia o código envolvido e corrija a causa raiz (não o teste) quando o teste estiver correto"
        }
        Classificacao::RespostaMalformada => {
            "peça uma nova resposta: rejeite a resposta inválida e repita a chamada ao modelo"
        }
        Classificacao::Implementacao => {
            "releia os arquivos envolvidos, formule uma hipótese diferente e valide com o menor passo possível"
        }
    };
    if usadas.len() >= 2 {
        format!(
            "{base}. Estratégias já usadas: {}. Use uma abordagem diferente.",
            usadas.join("; ")
        )
    } else {
        base.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnostico_inicial_nao_conta_como_tentativa() {
        let mut r = Recuperacao::novo(9, Duration::from_secs(1200));
        let d = r.diagnosticar(Classificacao::Compilacao);
        assert_eq!(d.tentativas, 0);
        assert_eq!(d.restantes, 9);
        assert!(!d.expirado);
    }

    #[test]
    fn limite_de_nove_tentativas() {
        let mut r = Recuperacao::novo(9, Duration::from_secs(1200));
        for i in 0..9 {
            assert!(
                r.registrar_tentativa(Classificacao::Testes, &format!("estrategia {i}"))
                    .is_ok()
            );
        }
        let erro = r
            .registrar_tentativa(Classificacao::Testes, "decima")
            .unwrap_err();
        assert_eq!(erro.tentativas, 10);
        assert_eq!(erro.classificacao, Classificacao::Testes);
    }

    #[test]
    fn limite_de_tempo_interrompe_recuperacao() {
        let mut r = Recuperacao::novo(9, Duration::from_millis(1));
        // O relógio do incidente começa no primeiro diagnóstico.
        let inicial = r.diagnosticar(Classificacao::Implementacao);
        assert!(!inicial.expirado);
        std::thread::sleep(Duration::from_millis(5));
        let d = r.diagnosticar(Classificacao::Implementacao);
        assert!(d.expirado);
        assert!(
            r.registrar_tentativa(Classificacao::Implementacao, "x")
                .is_err()
        );
    }

    #[test]
    fn detecta_estrategia_repetida() {
        let mut r = Recuperacao::novo(9, Duration::from_secs(60));
        r.registrar_tentativa(Classificacao::Compilacao, "corrigir import")
            .unwrap();
        assert!(r.estrategia_repetida(Classificacao::Compilacao, "corrigir import"));
        assert!(!r.estrategia_repetida(Classificacao::Compilacao, "outra"));
    }

    #[test]
    fn classifica_por_saida() {
        assert_eq!(
            classificar(
                Some("run_command"),
                "error[E0432]: unresolved import",
                false
            ),
            Classificacao::Compilacao
        );
        assert_eq!(
            classificar(
                Some("run_tests"),
                "test result: FAILED. 1 passed; 1 failed",
                false
            ),
            Classificacao::Testes
        );
        assert_eq!(
            classificar(None, "resposta inválida do provedor", false),
            Classificacao::RespostaMalformada
        );
        assert_eq!(
            classificar(None, "qualquer coisa", true),
            Classificacao::ApiTransitoria
        );
    }

    #[test]
    fn incidentes_independentes_por_classificacao() {
        let mut r = Recuperacao::novo(9, Duration::from_secs(1200));
        r.registrar_tentativa(Classificacao::Compilacao, "a")
            .unwrap();
        let d = r.diagnosticar(Classificacao::Testes);
        assert_eq!(d.tentativas, 0);
        r.resolver(Classificacao::Compilacao);
        assert_eq!(r.diagnosticar(Classificacao::Compilacao).tentativas, 0);
    }
}
