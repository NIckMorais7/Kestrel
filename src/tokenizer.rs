//! Contagem de tokens por modelo.
//!
//! Usa o tokenizer real (tiktoken) quando o modelo declara uma codificação
//! conhecida — em `ModelEntry.tokenizer` (`o200k_base`, `cl100k_base`,
//! `p50k_base`, `r50k_base`) ou por reconhecimento do id (família GPT/OSS).
//! Para modelos sem tokenizer disponível, cai na estimativa por caracteres.
//!
//! O provedor informa o `usage` real de cada resposta; `calibrar` ajusta a
//! contagem do modelo com esse valor, compensando templates de chat e
//! tokenizers desconhecidos sem inventar precisão.

use crate::config::ModelEntry;
use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Codificacao {
    O200k,
    Cl100k,
    P50k,
    R50k,
}

fn codificacao_por_nome(nome: &str) -> Option<Codificacao> {
    match nome.trim().to_lowercase().replace(['-', ' '], "_").as_str() {
        "o200k" | "o200k_base" | "o200kbase" => Some(Codificacao::O200k),
        "cl100k" | "cl100k_base" | "cl100kbase" => Some(Codificacao::Cl100k),
        "p50k" | "p50k_base" | "p50kbase" => Some(Codificacao::P50k),
        "r50k" | "r50k_base" | "r50kbase" | "gpt2" => Some(Codificacao::R50k),
        _ => None,
    }
}

/// Codificação reconhecida pelo id do modelo.
fn codificacao_conhecida(modelo: &str) -> Option<Codificacao> {
    let m = modelo.to_lowercase();
    if m.contains("gpt-oss")
        || m.contains("gpt-4o")
        || m.contains("gpt-4.1")
        || m.contains("gpt-5")
        || m.contains("chatgpt-4o")
        || m.starts_with("o1")
        || m.starts_with("o3")
        || m.starts_with("o4")
    {
        return Some(Codificacao::O200k);
    }
    if m.contains("gpt-4")
        || m.contains("gpt-3.5")
        || m.contains("gpt-35")
        || m.contains("text-embedding")
        || m.contains("codex")
    {
        return Some(Codificacao::Cl100k);
    }
    if m.contains("davinci") || m.contains("curie") || m.contains("babbage") {
        return Some(Codificacao::P50k);
    }
    None
}

struct Estado {
    /// Tokenizer explícito vindo do catálogo, por id de modelo em minúsculas.
    declarados: HashMap<String, String>,
    /// Fator real/estimado aprendido com o `usage` do provedor.
    fatores: HashMap<String, f64>,
}

fn estado() -> &'static RwLock<Estado> {
    static ESTADO: OnceLock<RwLock<Estado>> = OnceLock::new();
    ESTADO.get_or_init(|| {
        RwLock::new(Estado {
            declarados: HashMap::new(),
            fatores: HashMap::new(),
        })
    })
}

/// Registra os tokenizers declarados no catálogo (`ModelEntry.tokenizer`).
pub fn registrar_modelos(modelos: &[ModelEntry]) {
    let Ok(mut estado) = estado().write() else {
        return;
    };
    for modelo in modelos {
        if let Some(tokenizer) = modelo.tokenizer.as_deref() {
            let tokenizer = tokenizer.trim();
            if !tokenizer.is_empty() {
                estado
                    .declarados
                    .insert(modelo.id.to_lowercase(), tokenizer.to_string());
            }
        }
    }
}

fn codificacao_do_modelo(modelo: &str) -> Option<Codificacao> {
    let chave = modelo.to_lowercase();
    let declarado = estado()
        .read()
        .ok()
        .and_then(|e| e.declarados.get(&chave).cloned());
    if let Some(nome) = declarado {
        // "heuristica"/"none"/"auto" desliga o tiktoken para esse modelo.
        return codificacao_por_nome(&nome);
    }
    codificacao_conhecida(modelo)
}

/// Conta tokens de um texto para o modelo.
pub fn contar(modelo: &str, texto: &str) -> u32 {
    if texto.is_empty() {
        return 0;
    }
    let bruto = match codificacao_do_modelo(modelo) {
        Some(Codificacao::O200k) => tiktoken_rs::o200k_base_singleton()
            .encode_ordinary(texto)
            .len(),
        Some(Codificacao::Cl100k) => tiktoken_rs::cl100k_base_singleton()
            .encode_ordinary(texto)
            .len(),
        Some(Codificacao::P50k) => tiktoken_rs::p50k_base_singleton()
            .encode_ordinary(texto)
            .len(),
        Some(Codificacao::R50k) => tiktoken_rs::r50k_base_singleton()
            .encode_ordinary(texto)
            .len(),
        None => crate::util::estimate_tokens(texto) as usize,
    };
    let fator = estado()
        .read()
        .ok()
        .and_then(|e| e.fatores.get(&modelo.to_lowercase()).copied())
        .unwrap_or(1.0);
    ((bruto as f64 * fator).round() as u32).max(1)
}

/// Ajusta o fator do modelo com o `usage` real do provedor.
///
/// `real` é o `prompt_tokens` informado; `estimado` é a contagem feita antes
/// da requisição. A média móvel evita oscilar com uma única resposta.
pub fn calibrar(modelo: &str, real: u32, estimado: u32) {
    if real == 0 || estimado == 0 {
        return;
    }
    let Ok(mut estado) = estado().write() else {
        return;
    };
    let chave = modelo.to_lowercase();
    let atual = estado.fatores.get(&chave).copied().unwrap_or(1.0);
    let observado = (real as f64 / estimado as f64).clamp(0.25, 4.0);
    let novo = (atual * 0.7 + observado * 0.3).clamp(0.25, 4.0);
    estado.fatores.insert(chave, novo);
}

/// Fator atual (para diagnóstico/testes).
#[cfg(test)]
pub fn fator(modelo: &str) -> f64 {
    estado()
        .read()
        .ok()
        .and_then(|e| e.fatores.get(&modelo.to_lowercase()).copied())
        .unwrap_or(1.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconhece_codificacoes_por_nome_e_por_modelo() {
        assert_eq!(codificacao_por_nome("o200k_base"), Some(Codificacao::O200k));
        assert_eq!(codificacao_por_nome("CL100K"), Some(Codificacao::Cl100k));
        assert_eq!(
            codificacao_conhecida("openai/gpt-oss-20b"),
            Some(Codificacao::O200k)
        );
        assert_eq!(
            codificacao_conhecida("gpt-4-turbo"),
            Some(Codificacao::Cl100k)
        );
        assert!(codificacao_conhecida("meta/llama-3").is_none());
    }

    #[test]
    fn tiktoken_conta_mais_que_zero_e_menos_que_caracteres() {
        let texto = "fn main() { println!(\"olá, mundo\"); }";
        let n = contar("openai/gpt-oss-20b", texto);
        assert!(n >= 1);
        assert!(n < texto.chars().count() as u32);
        // Modelo sem tokenizer conhecido cai na estimativa, sem pânico.
        assert!(contar("modelo/desconhecido", texto) >= 1);
    }

    #[test]
    fn calibracao_aproxima_do_uso_real() {
        let modelo = "teste/calibracao";
        for _ in 0..40 {
            calibrar(modelo, 200, 100);
        }
        assert!(fator(modelo) > 1.8, "fator: {}", fator(modelo));
        let antes = contar(modelo, "uma frase qualquer com várias palavras");
        calibrar(modelo, 10, 1000);
        // Um único caso extremo não joga o fator para o outro extremo.
        assert!(fator(modelo) > 0.5, "fator: {}", fator(modelo));
        let depois = contar(modelo, "uma frase qualquer com várias palavras");
        assert!(depois >= 1 && antes >= 1);
    }

    #[test]
    fn tokenizer_declarado_tem_prioridade() {
        let mut modelo = ModelEntry::new("org/modelo-explicito");
        modelo.tokenizer = Some("cl100k_base".into());
        registrar_modelos(&[modelo]);
        assert_eq!(
            codificacao_do_modelo("org/modelo-explicito"),
            Some(Codificacao::Cl100k)
        );
    }
}
