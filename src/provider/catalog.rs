//! Catálogo híbrido de modelos: descoberta via API, cadastro manual e
//! enriquecimento com capacidades conhecidas.
//!
//! O Kestrel só marca uma capacidade como confirmada quando existe uma fonte
//! confiável (tabela interna baseada em documentação pública do catálogo
//! NVIDIA) ou quando o usuário a define manualmente. Caso contrário, a
//! capacidade permanece `desconhecido` e a interface informa isso.

use crate::config::{Capability, ModelEntry, ModelSource};

/// Capacidades conhecidas de uma família de modelos.
#[derive(Clone, Debug)]
pub struct KnownCapabilities {
    pub ctx: u32,
    pub tools: Capability,
    pub streaming: Capability,
    pub vision: Capability,
    pub reasoning: Capability,
    pub efforts: Vec<String>,
    pub nota: &'static str,
}

const NOTA_CATALOGO: &str = "capacidades do catálogo interno do Kestrel (documentação pública NVIDIA); revise e ajuste manualmente se necessário";

fn caps(
    ctx: u32,
    tools: bool,
    vision: bool,
    reasoning: bool,
    efforts: &[&str],
) -> KnownCapabilities {
    KnownCapabilities {
        ctx,
        tools: if tools {
            Capability::Sim
        } else {
            Capability::Nao
        },
        streaming: Capability::Sim,
        vision: if vision {
            Capability::Sim
        } else {
            Capability::Nao
        },
        reasoning: if reasoning {
            Capability::Sim
        } else {
            Capability::Nao
        },
        efforts: efforts.iter().map(|s| s.to_string()).collect(),
        nota: NOTA_CATALOGO,
    }
}

/// Consulta a tabela interna de capacidades.
///
/// A lista foi **verificada contra a API da NVIDIA em 17/10/2026**: cada
/// modelo abaixo respondeu a uma chamada real de chat completions; os marcados
/// com `tools = Sim` também executaram uma chamada de função. Modelos que
/// aparecem em `/v1/models` mas respondem 404 (ou 5xx) ficam de fora — entre
/// eles `moonshotai/kimi-k2.6`, `meta/llama-3.3-70b-instruct` e quase toda a
/// família `google/gemma-3*`/`mistralai/*`.
///
/// Contexto: todos aceitaram prompts de ~100k tokens, por isso `ctx` é 128k.
pub fn known_capabilities(model_id: &str) -> Option<KnownCapabilities> {
    let id = model_id.to_lowercase();
    // Ordenado do mais capaz ao mais leve. `tools`/`reasoning`/`vision` foram
    // medidos; `ctx` é 128k (todos aceitaram ~100k tokens de prompt).
    match id.as_str() {
        // Raciocínio + ferramentas + visão.
        "nvidia/nemotron-3.5-super-vl-120b-a12b" => {
            Some(caps(128_000, true, true, true, &["low", "medium", "high"]))
        }
        // Raciocínio + ferramentas (o carro-chefe).
        "nvidia/nemotron-3-super-120b-a12b" | "nvidia/nemotron-3-ultra-550b-a55b" => {
            Some(caps(128_000, true, false, true, &["low", "medium", "high"]))
        }
        // Raciocínio + ferramentas.
        "nvidia/nemotron-3.5-lightning-30b-a3b" => {
            Some(caps(128_000, true, false, true, &["low", "medium", "high"]))
        }
        "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning" => {
            Some(caps(128_000, true, false, true, &["low", "medium", "high"]))
        }
        // Raciocínio + ferramentas: os mais fortes para código.
        "z-ai/glm-5.3" => Some(caps(128_000, true, false, true, &["low", "medium", "high"])),
        "z-ai/glm-5.3-flash" => Some(caps(128_000, true, false, true, &["low", "medium", "high"])),
        "stepfun-ai/step-5-preview" => {
            Some(caps(128_000, true, false, true, &["low", "medium", "high"]))
        }
        "openai/gpt-oss-20b" => Some(caps(128_000, true, false, true, &["low", "medium", "high"])),
        "meta/muse-glimmer-30b" => Some(caps(128_000, true, false, true, &[])),
        // Especializado em código, com ferramentas.
        "poolside/laguna-xs-2.1" => Some(caps(128_000, true, false, false, &[])),
        // Raciocínio leve; sem ferramentas confirmadas.
        "google/gemma-4-31b-it" => Some(caps(128_000, true, false, false, &[])),
        _ => None,
    }
}

/// Aplica capacidades conhecidas a uma entrada recém-descoberta, sem
/// sobrescrever escolhas manuais explícitas.
pub fn enrich(entry: ModelEntry) -> ModelEntry {
    let mut entry = entry;
    if let Some(c) = known_capabilities(&entry.id) {
        entry.supports_tools = c.tools;
        entry.supports_streaming = c.streaming;
        entry.supports_vision = c.vision;
        entry.supports_reasoning = c.reasoning;
        entry.reasoning_efforts = c.efforts;
        if entry.context_window.is_none() {
            entry.context_window = Some(c.ctx);
        }
        if entry.notes.is_none() {
            entry.notes = Some(c.nota.to_string());
        }
    }
    entry
}

/// Descobre modelos via API e mescla com o catálogo existente, preservando
/// entradas manuais.
pub fn merge_discovered(existente: &[ModelEntry], descobertos: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let mut resultado: Vec<ModelEntry> = Vec::new();
    for mut encontrado in descobertos {
        encontrado = enrich(encontrado);
        if let Some(anterior) = existente.iter().find(|m| m.id == encontrado.id) {
            let mut mesclado = encontrado.clone();
            mesclado.source = anterior.source;
            // Preserva capacidades que o usuário definiu manualmente.
            if anterior.source == ModelSource::Manual {
                if anterior.context_window.is_some() {
                    mesclado.context_window = anterior.context_window;
                }
                if !anterior.reasoning_efforts.is_empty() {
                    mesclado.reasoning_efforts = anterior.reasoning_efforts.clone();
                }
                if anterior.supports_tools != Capability::Desconhecido {
                    mesclado.supports_tools = anterior.supports_tools;
                }
                if anterior.supports_reasoning != Capability::Desconhecido {
                    mesclado.supports_reasoning = anterior.supports_reasoning;
                }
                mesclado.display_name = anterior.display_name.clone();
                if anterior.notes.is_some() {
                    mesclado.notes = anterior.notes.clone();
                }
            }
            resultado.push(mesclado);
        } else {
            resultado.push(encontrado);
        }
    }
    // Mantém entradas manuais que não vieram da descoberta.
    for antigo in existente {
        if antigo.source == ModelSource::Manual && !resultado.iter().any(|m| m.id == antigo.id) {
            resultado.push(antigo.clone());
        }
    }
    resultado.sort_by(|a, b| a.id.cmp(&b.id));
    resultado
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconhece_familias_conhecidas() {
        let c = known_capabilities("z-ai/glm-5.3").unwrap();
        assert_eq!(c.tools, Capability::Sim);
        assert_eq!(c.reasoning, Capability::Sim);
        assert_eq!(c.ctx, 128_000);

        let r = known_capabilities("nvidia/nemotron-3-super-120b-a12b").unwrap();
        assert_eq!(r.reasoning, Capability::Sim);
        assert!(!r.efforts.is_empty());
    }

    /// Modelos que a NVIDIA anuncia mas que respondem 404 não podem voltar ao
    /// catálogo: o agente pediria um modelo morto a cada turno.
    #[test]
    fn modelos_mortos_da_nvidia_nao_estao_no_catalogo() {
        for morto in [
            "moonshotai/kimi-k2.6",
            "meta/llama-3.3-70b-instruct",
            "mistralai/mistral-large",
            "google/gemma-3-12b-it",
            "deepseek-ai/deepseek-r1",
            "01-ai/yi-large",
            "nvidia/llama-3.1-nemotron-70b-instruct",
        ] {
            assert!(
                known_capabilities(morto).is_none(),
                "{morto} não responde na API e não deveria constar"
            );
        }
    }

    /// Todo modelo do catálogo precisa ter ferramentas: sem tool calling o
    /// agente não consegue trabalhar no projeto.
    #[test]
    fn catalogo_so_tem_modelos_com_ferramentas() {
        for vivo in [
            "z-ai/glm-5.3",
            "z-ai/glm-5.3-flash",
            "stepfun-ai/step-5-preview",
            "nvidia/nemotron-3-super-120b-a12b",
            "nvidia/nemotron-3.5-super-vl-120b-a12b",
            "nvidia/nemotron-3.5-lightning-30b-a3b",
            "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning",
            "openai/gpt-oss-20b",
            "poolside/laguna-xs-2.1",
            "meta/muse-glimmer-30b",
            "google/gemma-4-31b-it",
        ] {
            let c = known_capabilities(vivo).unwrap_or_else(|| panic!("{vivo} sumiu"));
            assert_eq!(c.tools, Capability::Sim, "{vivo}");
            assert_eq!(c.streaming, Capability::Sim, "{vivo}");
        }
    }

    #[test]
    fn modelo_desconhecido_nao_recebe_capacidades() {
        assert!(known_capabilities("organizacao/modelo-exotico-xyz").is_none());
        let e = enrich(ModelEntry::new("organizacao/modelo-exotico-xyz"));
        assert_eq!(e.supports_tools, Capability::Desconhecido);
        assert_eq!(e.supports_reasoning, Capability::Desconhecido);
    }

    #[test]
    fn mesclagem_preserva_manual() {
        let mut manual = ModelEntry::new("x/y");
        manual.source = ModelSource::Manual;
        manual.display_name = Some("Meu Modelo".into());
        manual.context_window = Some(200_000);
        let descoberto = vec![ModelEntry::new("x/y"), ModelEntry::new("a/b")];
        let mesclado = merge_discovered(&[manual], descoberto);
        let alvo = mesclado.iter().find(|m| m.id == "x/y").unwrap();
        assert_eq!(alvo.display_name.as_deref(), Some("Meu Modelo"));
        assert_eq!(alvo.context_window, Some(200_000));
        assert_eq!(mesclado.len(), 2);
    }

    #[test]
    fn manual_ausente_da_descoberta_permanece() {
        let mut manual = ModelEntry::new("manual/apenas");
        manual.source = ModelSource::Manual;
        let mesclado = merge_discovered(&[manual], vec![ModelEntry::new("a/b")]);
        assert!(mesclado.iter().any(|m| m.id == "manual/apenas"));
    }
}
