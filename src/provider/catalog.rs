//! Catálogo híbrido de modelos: descoberta via API, cadastro manual e
//! enriquecimento com capacidades conhecidas.
//!
//! O Kestrel só marca uma capacidade como confirmada quando existe uma fonte
//! confiável (tabela interna baseada em documentação pública do catálogo
//! NVIDIA) ou quando o usuário a define manualmente. Caso contrário, a
//! capacidade permanece `desconhecido` e a interface informa isso.

use crate::config::{Capability, ModelEntry, ModelSource};
use futures_util::StreamExt;
use serde_json::Value;
use std::time::Duration;

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
/// A lista foi **reverificada contra o catálogo público da NVIDIA em
/// 24/10/2026** (`build.nvidia.com`): todos os modelos abaixo têm **free
/// endpoint** confirmado e são modelos de chat (texto e/ou imagem) com
/// ferramentas. Modelos que aparecem em `/v1/models` mas já não respondem
/// (404/5xx), não têm free endpoint ou não são chat (embeddings, guards,
/// OCR, ASR, TTS, rerank, geração de imagem/vídeo…) ficam de fora — entre
/// eles `moonshotai/kimi-k2.6`, `meta/llama-3.3-70b-instruct`,
/// `mistralai/mistral-large`, `google/gemma-3-12b-it`,
/// `deepseek-ai/deepseek-r1`, `01-ai/yi-large` e
/// `nvidia/llama-3.1-nemotron-70b-instruct`.
///
/// Como esta tabela é a **única fonte** de modelos aceitos na descoberta
/// automática (ver [`buscar_modelos`]), ela também funciona como allowlist:
/// nada de lixo do `/v1/models` entra no catálogo local.
pub fn known_capabilities(model_id: &str) -> Option<KnownCapabilities> {
    let id = model_id.to_lowercase();
    match id.as_str() {
        // ── NVIDIA Nemotron ──────────────────────────────────────────────
        // Raciocínio + ferramentas + visão (early access, free tier).
        "nvidia/nemotron-3.5-super-vl-120b-a12b" => {
            Some(caps(128_000, true, true, true, &["low", "medium", "high"]))
        }
        // Raciocínio + ferramentas (1M de contexto).
        "nvidia/nemotron-3-super-120b-a12b" | "nvidia/nemotron-3-ultra-550b-a55b" => {
            Some(caps(128_000, true, false, true, &["low", "medium", "high"]))
        }
        // Raciocínio + ferramentas (1M de contexto).
        "nvidia/nemotron-3.5-lightning-30b-a3b" => {
            Some(caps(128_000, true, false, true, &["low", "medium", "high"]))
        }
        // Omni-modal (texto, imagem, vídeo, fala) + raciocínio + ferramentas.
        "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning" => {
            Some(caps(128_000, true, true, true, &["low", "medium", "high"]))
        }
        // Texto, raciocínio + ferramentas (contexto 64K).
        "nvidia/nemotron-3.5-bolt-e4b" => Some(caps(64_000, true, false, true, &[])),
        // ── Z.ai GLM ────────────────────────────────────────────────────
        "z-ai/glm-5.3" => Some(caps(128_000, true, false, true, &["low", "medium", "high"])),
        "z-ai/glm-5.3-flash" => Some(caps(128_000, true, true, true, &["low", "medium", "high"])),
        // ── StepFun ─────────────────────────────────────────────────────
        "stepfun-ai/step-5-preview" => {
            Some(caps(128_000, true, true, true, &["low", "medium", "high"]))
        }
        // ── OpenAI (open weights) ────────────────────────────────────────
        "openai/gpt-oss-20b" => Some(caps(128_000, true, false, true, &["low", "medium", "high"])),
        // ── Meta ────────────────────────────────────────────────────────
        "meta/muse-glimmer-30b" => Some(caps(128_000, true, true, true, &[])),
        // ── Poolside ────────────────────────────────────────────────────
        // Especializado em código, com ferramentas; sem raciocínio explícito.
        "poolside/laguna-xs-2.1" => Some(caps(128_000, true, false, false, &[])),
        // ── Google ──────────────────────────────────────────────────────
        "google/gemma-4-31b-it" => Some(caps(128_000, true, false, true, &[])),
        // ── MiniMax ─────────────────────────────────────────────────────
        // Raciocínio sempre ligado; esforços em 5 níveis (low…max).
        // `none`/desligar NÃO é aceito (a API responde 400).
        "minimaxai/minimax-m3.1-flash" => Some(caps(
            128_000,
            true,
            true,
            true,
            &["low", "medium", "high", "xhigh", "max"],
        )),
        // ── DeepSeek ────────────────────────────────────────────────────
        // Raciocínio com esforço *numérico* (1–100), não low/medium/high:
        // fica fora do seletor de esforços (o modelo pensa com o padrão).
        "deepseek-ai/deepseek-v4.1-flash" => Some(caps(128_000, true, true, true, &[])),
        // ── Mistral ────────────────────────────────────────────────────
        // Mistral Large 4: 1T-total/52B-ativo (MoE granular), texto+imagem,
        // ferramentas e saída estruturada. Raciocínio com esforços
        // low/medium/high — NÃO aceita none/minimal/xhigh/max. Free endpoint
        // NVIDIA NIM confirmado; contexto 262K.
        "mistralai/mistral-large-4-1t-a52b" => {
            Some(caps(262_000, true, true, true, &["low", "medium", "high"]))
        }
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

/// Preserva, em `mesclado`, os ajustes que o usuário fez em `anterior`.
///
/// O critério é o mesmo para entradas manuais e descobertas: um campo só é
/// preservado quando tem valor **explícito** — `Some`, não vazio ou diferente
/// de `desconhecido`. Campos que a descoberta/catálogo gravaria como padrão
/// (`None`, vazios, `desconhecido`) continuam sendo preenchidos, então o
/// catálogo interno ainda atualiza entradas antigas; o que o usuário editou à
/// mão no `config.toml` (`context_window`, `display_name`, `notes`, capacidades,
/// esforços) não é mais sobrescrito a cada atualização automática ou `load`.
fn preservar_ajustes(anterior: &ModelEntry, mesclado: &mut ModelEntry) {
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
    if anterior.supports_streaming != Capability::Desconhecido {
        mesclado.supports_streaming = anterior.supports_streaming;
    }
    if anterior.supports_vision != Capability::Desconhecido {
        mesclado.supports_vision = anterior.supports_vision;
    }
    mesclado.display_name = anterior.display_name.clone();
    if anterior.notes.is_some() {
        mesclado.notes = anterior.notes.clone();
    }
    if anterior.tokenizer.is_some() {
        mesclado.tokenizer = anterior.tokenizer.clone();
    }
}

/// Descobre modelos via API e mescla com o catálogo existente, preservando os
/// ajustes feitos pelo usuário (manuais ou não — ver [`preservar_ajustes`]).
pub fn merge_discovered(existente: &[ModelEntry], descobertos: Vec<ModelEntry>) -> Vec<ModelEntry> {
    let mut resultado: Vec<ModelEntry> = Vec::new();
    for mut encontrado in descobertos {
        encontrado = enrich(encontrado);
        if let Some(anterior) = existente.iter().find(|m| m.id == encontrado.id) {
            let mut mesclado = encontrado.clone();
            mesclado.source = anterior.source;
            // Preserva capacidades que o usuário definiu manualmente.
            preservar_ajustes(anterior, &mut mesclado);
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

/// Teto de bytes do corpo de `/models`.
const LIMITE_MODELS_BYTES: usize = 16 * 1024 * 1024;

/// Remove do catálogo as entradas **descobertas** que não estão na allowlist
/// (tabela [`known_capabilities`]). Isso limpa a lista de modelos moribundos,
/// indisponíveis ou não-chat que uma descoberta antiga do `/v1/models` possa
/// ter trazido. Entradas manuais nunca são tocadas.
///
/// Devolve quantas entradas foram removidas.
pub fn podar_catalogo(entradas: &mut Vec<ModelEntry>) -> usize {
    let antes = entradas.len();
    entradas.retain(|e| e.source == ModelSource::Manual || known_capabilities(&e.id).is_some());
    antes.saturating_sub(entradas.len())
}

/// Poda o catálogo **e re-aplica as capacidades conhecidas** nas entradas
/// descobertas que ficaram.
///
/// Uma entrada gravada em disco antes de o modelo entrar na tabela interna
/// (ex.: o MiniMax M3.1 Flash com esforços vazios) continuaria mostrando
/// `desconhecido` para sempre — o `enrich` corrige isso sem depender de uma
/// nova chamada à rede. Escolhas manuais nunca são alteradas.
pub fn podar_e_atualizar_catalogo(entradas: &mut Vec<ModelEntry>) -> usize {
    let removidos = podar_catalogo(entradas);
    for entrada in entradas.iter_mut() {
        if entrada.source == ModelSource::Descoberto {
            let mut nova = enrich(entrada.clone());
            // Ajustes explícitos do usuário sobrevivem ao re-enriquecimento
            // (mesma regra da mesclagem com a descoberta).
            preservar_ajustes(entrada, &mut nova);
            *entrada = nova;
        }
    }
    removidos
}

/// Consulta o endpoint `/models` do provedor (API compatível com OpenAI) e
/// devolve as entradas descobertas **que estão na allowlist** (tabela
/// [`known_capabilities`]) — o `/v1/models` lista centenas de modelos que não
/// são chat, já foram descontinuados ou não são free endpoints; só o que a
/// allowlist confirma entra. A falha de rede ou do endpoint é um erro normal:
/// a atualização automática é silenciosa quando o provedor não responde.
pub async fn buscar_modelos(base_url: &str, api_key: &str) -> Result<Vec<ModelEntry>, String> {
    let base = base_url.trim_end_matches('/');
    let url = format!("{base}/models");
    let cliente = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(20))
        .user_agent(format!("Kestrel/{} (agente de codificação)", crate::VERSAO))
        .build()
        .map_err(|e| format!("cliente HTTP: {e}"))?;
    let resposta = cliente
        .get(&url)
        .bearer_auth(api_key)
        .send()
        .await
        .map_err(|e| format!("falha ao consultar {url}: {e}"))?;
    if !resposta.status().is_success() {
        return Err(format!(
            "o provedor respondeu {} em /models",
            resposta.status()
        ));
    }

    let mut bytes = Vec::new();
    let mut stream = resposta.bytes_stream();
    while let Some(pedaco) = stream.next().await {
        let pedaco = pedaco.map_err(|e| format!("leitura da resposta de /models: {e}"))?;
        if bytes.len().saturating_add(pedaco.len()) > LIMITE_MODELS_BYTES {
            return Err("resposta de /models grande demais".into());
        }
        bytes.extend_from_slice(&pedaco);
    }
    let texto =
        String::from_utf8(bytes).map_err(|_| "resposta de /models não é UTF-8".to_string())?;
    let corpo: Value =
        serde_json::from_str(&texto).map_err(|e| format!("resposta inválida de /models: {e}"))?;

    let Some(dados) = corpo.get("data").and_then(|d| d.as_array()) else {
        return Ok(Vec::new());
    };
    let mut entradas = Vec::new();
    for item in dados {
        let Some(id) = item.get("id").and_then(|i| i.as_str()) else {
            continue;
        };
        if id.trim().is_empty() || known_capabilities(id).is_none() {
            continue; // fora da allowlist: não é chat/free confirmado
        }
        let mut entrada = ModelEntry::new(id);
        entrada.owned_by = item
            .get("owned_by")
            .and_then(|o| o.as_str())
            .map(str::to_string);
        entradas.push(entrada);
        if entradas.len() >= 100 {
            break;
        }
    }
    Ok(entradas)
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

    /// O MiniMax M3.1 Flash suporta raciocínio com cinco níveis de esforço
    /// (low, medium, high, xhigh, max) — e *não* aceita `none`.
    #[test]
    fn minimax_m3_1_flash_suporta_os_cinco_esforcos() {
        let c = known_capabilities("minimaxai/minimax-m3.1-flash")
            .expect("minimax m3.1 flash no catálogo");
        assert_eq!(c.tools, Capability::Sim);
        assert_eq!(c.reasoning, Capability::Sim);
        assert_eq!(c.vision, Capability::Sim);
        assert_eq!(c.efforts, vec!["low", "medium", "high", "xhigh", "max"]);
        assert!(!c.efforts.contains(&"none".to_string()));
    }

    /// Modelos que a NVIDIA anuncia mas que respondem 404, perderam o free
    /// endpoint ou não são chat não podem voltar ao catálogo: o agente pediria
    /// um modelo morto (ou inútil) a cada turno.
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
            // Não-chat ou sem free endpoint confirmado:
            "nvidia/nemotron-3-embed-1b",
            "nvidia/llama-3.1-nemoguard-8b-content-safety",
            "nvidia/nvclip",
            "nvidia/vila",
            "nvidia/neva-22b",
            "snowflake/arctic-embed-l",
            "zyphra/zamba2-7b-instruct",
            "writer/palmyra-fin-70b-32k",
            "databricks/dbrx-instruct",
            "bigcode/starcoder2-15b",
            "microsoft/kosmos-2",
            "adept/fuyu-8b",
            "nvidia/nemotron-4-340b-instruct",
            "meta/llama2-70b",
            "ibm/granite-34b-code-instruct",
        ] {
            assert!(
                known_capabilities(morto).is_none(),
                "{morto} não deveria constar no catálogo"
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
            "nvidia/nemotron-3-ultra-550b-a55b",
            "nvidia/nemotron-3.5-super-vl-120b-a12b",
            "nvidia/nemotron-3.5-lightning-30b-a3b",
            "nvidia/nemotron-3-nano-omni-30b-a3b-reasoning",
            "nvidia/nemotron-3.5-bolt-e4b",
            "openai/gpt-oss-20b",
            "poolside/laguna-xs-2.1",
            "meta/muse-glimmer-30b",
            "google/gemma-4-31b-it",
            "minimaxai/minimax-m3.1-flash",
            "deepseek-ai/deepseek-v4.1-flash",
            "mistralai/mistral-large-4-1t-a52b",
        ] {
            let c = known_capabilities(vivo).unwrap_or_else(|| panic!("{vivo} sumiu"));
            assert_eq!(c.tools, Capability::Sim, "{vivo}");
            assert_eq!(c.streaming, Capability::Sim, "{vivo}");
        }
    }

    /// A poda remove só o que foi descoberto e saiu da allowlist; o manual
    /// fica intacto.
    #[test]
    fn poda_remove_descobertos_fora_da_allowlist() {
        let mut entradas = vec![ModelEntry::new("nvidia/nemotron-3-embed-1b")];
        let mut manual = ModelEntry::new("org/modelo-manual");
        manual.source = ModelSource::Manual;
        entradas.push(manual.clone());
        entradas.push(ModelEntry::new("minimaxai/minimax-m3.1-flash"));

        let removidos = podar_catalogo(&mut entradas);
        assert_eq!(removidos, 1);
        assert!(entradas.iter().any(|e| e.id == "org/modelo-manual"));
        assert!(
            entradas
                .iter()
                .any(|e| e.id == "minimaxai/minimax-m3.1-flash")
        );
        assert!(
            !entradas
                .iter()
                .any(|e| e.id == "nvidia/nemotron-3-embed-1b")
        );
    }

    /// A poda + re-enriquecimento corrige entradas antigas: o MiniMax que foi
    /// gravado como `desconhecido` (antes de entrar na tabela) ganha os cinco
    /// esforços — é exatamente o bug visível na interface do usuário.
    #[test]
    fn poda_re_enriquece_descobertos_antigos() {
        let mut minimax = ModelEntry::new("minimaxai/minimax-m3.1-flash");
        minimax.supports_reasoning = Capability::Desconhecido;
        minimax.reasoning_efforts = Vec::new();
        let mut manual = ModelEntry::new("org/modelo-manual");
        manual.source = ModelSource::Manual;
        manual.supports_tools = Capability::Nao; // escolha manual: preservar

        let mut entradas = vec![minimax, manual];
        let removidos = podar_e_atualizar_catalogo(&mut entradas);
        assert_eq!(removidos, 0);

        let minimax = entradas
            .iter()
            .find(|e| e.id == "minimaxai/minimax-m3.1-flash")
            .unwrap();
        assert_eq!(minimax.supports_reasoning, Capability::Sim);
        assert_eq!(
            minimax.reasoning_efforts,
            vec!["low", "medium", "high", "xhigh", "max"]
        );
        let manual = entradas
            .iter()
            .find(|e| e.id == "org/modelo-manual")
            .unwrap();
        assert_eq!(manual.supports_tools, Capability::Nao, "manual não muda");
    }

    /// Mistral Large 4 (NVIDIA NIM, free endpoint): raciocínio com esforços
    /// low/medium/high — sem `none`/`minimal`/`xhigh`/`max`, contexto 262K.
    #[test]
    fn mistral_large_4_suporta_esforcos_low_medium_high() {
        let c = known_capabilities("mistralai/mistral-large-4-1t-a52b")
            .expect("mistral large 4 no catálogo");
        assert_eq!(c.tools, Capability::Sim);
        assert_eq!(c.reasoning, Capability::Sim);
        assert_eq!(c.vision, Capability::Sim);
        assert_eq!(c.ctx, 262_000);
        assert_eq!(c.efforts, vec!["low", "medium", "high"]);
        assert!(!c.efforts.contains(&"none".to_string()));
        assert!(!c.efforts.contains(&"max".to_string()));
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
    fn mesclagem_preserva_streaming_e_visao_manuais() {
        let mut manual = ModelEntry::new("x/y");
        manual.source = ModelSource::Manual;
        manual.supports_streaming = Capability::Nao;
        manual.supports_vision = Capability::Sim;
        let descoberto = vec![ModelEntry::new("x/y")];
        let mesclado = merge_discovered(&[manual], descoberto);
        let alvo = mesclado.iter().find(|m| m.id == "x/y").unwrap();
        assert_eq!(alvo.supports_streaming, Capability::Nao);
        assert_eq!(alvo.supports_vision, Capability::Sim);
    }

    /// Entrada **descoberta** editada no `config.toml` não pode voltar ao
    /// padrão do catálogo interno: `context_window`, `display_name` e `notes`
    /// ajustados pelo usuário eram sobrescritos a cada atualização.
    #[test]
    fn mesclagem_preserva_ajustes_de_entrada_descoberta() {
        let mut editada = ModelEntry::new("minimaxai/minimax-m3.1-flash");
        editada.source = ModelSource::Descoberto;
        editada.context_window = Some(200_000);
        editada.display_name = Some("MiniMax (200k)".into());
        editada.notes = Some("teste meu, não do catálogo".into());
        editada.supports_reasoning = Capability::Nao;

        let descoberto = vec![ModelEntry::new("minimaxai/minimax-m3.1-flash")];
        let mesclado = merge_discovered(&[editada], descoberto);
        let alvo = mesclado
            .iter()
            .find(|m| m.id == "minimaxai/minimax-m3.1-flash")
            .unwrap();
        assert_eq!(alvo.context_window, Some(200_000));
        assert_eq!(alvo.display_name.as_deref(), Some("MiniMax (200k)"));
        assert_eq!(alvo.notes.as_deref(), Some("teste meu, não do catálogo"));
        assert_eq!(alvo.supports_reasoning, Capability::Nao);
    }

    /// O catálogo ainda atualiza o que o usuário **não** ajustou: uma entrada
    /// descoberta antiga, gravada como `desconhecido`, ganha os esforços.
    #[test]
    fn mesclagem_atualiza_campos_nao_ajustados() {
        let antiga = ModelEntry::new("minimaxai/minimax-m3.1-flash");
        let mesclado = merge_discovered(
            &[antiga],
            vec![ModelEntry::new("minimaxai/minimax-m3.1-flash")],
        );
        let alvo = mesclado.first().unwrap();
        assert_eq!(alvo.supports_reasoning, Capability::Sim);
        assert_eq!(
            alvo.reasoning_efforts,
            vec!["low", "medium", "high", "xhigh", "max"]
        );
        assert_eq!(alvo.context_window, Some(128_000));
    }

    #[test]
    fn manual_ausente_da_descoberta_permanece() {
        let mut manual = ModelEntry::new("manual/apenas");
        manual.source = ModelSource::Manual;
        let mesclado = merge_discovered(&[manual], vec![ModelEntry::new("a/b")]);
        assert!(mesclado.iter().any(|m| m.id == "manual/apenas"));
    }

    /// A descoberta consulta `/models` de verdade (servidor local) e devolve
    /// **apenas** o que está na allowlist — o lixo do endpoint (embeddings,
    /// guards, mortos) fica de fora.
    #[tokio::test]
    async fn buscar_modelos_le_o_endpoint_e_filtra_allowlist() {
        let ouvinte = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endereco = ouvinte.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let Ok((mut soquete, _)) = ouvinte.accept().await else {
                    continue;
                };
                tokio::spawn(async move {
                    use tokio::io::{AsyncReadExt, AsyncWriteExt};
                    let mut lido = Vec::new();
                    let mut buf = [0u8; 1024];
                    while !lido.windows(4).any(|j| j == b"\r\n\r\n") {
                        match soquete.read(&mut buf).await {
                            Ok(0) => return,
                            Ok(n) => lido.extend_from_slice(&buf[..n]),
                            Err(_) => return,
                        }
                    }
                    let corpo = r#"{"object":"list","data":[
                        {"id":"minimaxai/minimax-m3.1-flash","object":"model","owned_by":"minimaxai"},
                        {"id":"nvidia/nemotron-3-embed-1b","object":"model","owned_by":"nvidia"},
                        {"id":"org/modelo-y","object":"model","owned_by":"org"},
                        {"id":"01-ai/yi-large","object":"model","owned_by":"01-ai"}
                    ]}"#;
                    let resposta = format!(
                        "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\r\n{}",
                        corpo.len(),
                        corpo
                    );
                    let _ = soquete.write_all(resposta.as_bytes()).await;
                    let _ = soquete.flush().await;
                });
            }
        });
        let base = format!("http://{endereco}/v1");
        let modelos = buscar_modelos(&base, "chave").await.expect("descoberta");
        assert_eq!(modelos.len(), 1, "só a allowlist passa: {modelos:?}");
        assert_eq!(modelos[0].id, "minimaxai/minimax-m3.1-flash");
        assert_eq!(modelos[0].owned_by.as_deref(), Some("minimaxai"));
        assert_eq!(modelos[0].source, ModelSource::Descoberto);
    }

    /// Endpoint indisponível é um erro de descoberta, não um pânico.
    #[tokio::test]
    async fn buscar_modelos_falha_sem_panico() {
        let erro = buscar_modelos("http://127.0.0.1:1/v1", "chave").await;
        assert!(erro.is_err());
    }
}
