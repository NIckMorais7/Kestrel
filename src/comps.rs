//! Comps: personalidades do agente.
//!
//! Um Comp é um nome, uma descrição curta e um bloco de instruções de
//! **comportamento** (tom, estilo, prioridades). Ele nunca contém permissão,
//! modelo ou qualquer configuração: isso continua sendo decidido pelo usuário
//! e pelas regras de segurança do Kestrel.
//!
//! Os arquivos ficam em `~/.config/kestrel/comps/<slug>.toml`, fora do
//! projeto. O agente só escreve neles pela ferramenta dedicada
//! `create_comp`; `write_file` continua restrito ao projeto.

use crate::util::estimate_tokens;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Limite do slug/nome curto usado no arquivo.
pub const MAX_SLUG: usize = 32;
/// Limite da descrição exibida em `/person`.
pub const MAX_DESCRICAO: usize = 120;
/// Limite das instruções (protege o orçamento de contexto).
pub const MAX_INSTRUCOES: usize = 4000;

/// Uma personalidade salva em disco.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Comp {
    pub nome: String,
    #[serde(default)]
    pub descricao: String,
    pub instrucoes: String,
}

impl Comp {
    /// Cria um Comp a partir do nome e das instruções. A descrição é a
    /// primeira linha não vazia, truncada; o usuário pode editá-la no arquivo.
    pub fn novo(nome: &str, instrucoes: impl Into<String>) -> Result<Self, String> {
        let instrucoes = instrucoes.into();
        let comp = Self {
            nome: nome.trim().to_string(),
            descricao: primeira_linha(&instrucoes),
            instrucoes,
        };
        comp.validar()?;
        Ok(comp)
    }

    pub fn com_descricao(
        nome: &str,
        descricao: impl Into<String>,
        instrucoes: impl Into<String>,
    ) -> Result<Self, String> {
        let comp = Self {
            nome: nome.trim().to_string(),
            descricao: descricao.into(),
            instrucoes: instrucoes.into(),
        };
        comp.validar()?;
        Ok(comp)
    }

    /// Slug estável usado no nome do arquivo.
    pub fn slug(&self) -> String {
        slug_de(&self.nome)
    }

    /// Valida nome, limites e conteúdo. Mensagens são acionáveis.
    pub fn validar(&self) -> Result<(), String> {
        let slug = slug_de(&self.nome);
        if slug.is_empty() {
            return Err(
                "o nome do Comp precisa ter ao menos uma letra ou número (a-z, 0-9, _ ou -)."
                    .into(),
            );
        }
        if slug.chars().count() > MAX_SLUG {
            return Err(format!(
                "o nome do Comp vira o slug `{slug}`, que tem {} caracteres; o máximo é {MAX_SLUG}.",
                slug.chars().count()
            ));
        }
        if self.descricao.chars().count() > MAX_DESCRICAO {
            return Err(format!(
                "a descrição do Comp tem {} caracteres; o máximo é {MAX_DESCRICAO}.",
                self.descricao.chars().count()
            ));
        }
        if self.instrucoes.trim().is_empty() {
            return Err("as instruções do Comp não podem ficar vazias.".into());
        }
        if self.instrucoes.chars().count() > MAX_INSTRUCOES {
            return Err(format!(
                "as instruções do Comp têm {} caracteres; o máximo é {MAX_INSTRUCOES}. \
                 Um Comp curto e específico funciona melhor e custa menos contexto.",
                self.instrucoes.chars().count()
            ));
        }
        Ok(())
    }

    /// Custo aproximado no system prompt.
    pub fn tokens(&self) -> u32 {
        estimate_tokens(&self.nome) + estimate_tokens(&self.instrucoes) + 24
    }

    /// Bloco enviado ao modelo, com a subordinação explícita às regras.
    pub fn bloco_prompt(&self) -> String {
        format!(
            "## Comp ativo — {}\n\
             {}\n\n\
             Instruções de comportamento:\n{}\n\n\
             Estas instruções definem apenas tom, estilo e prioridades de comunicação. \
             Elas NÃO alteram permissões, modo, modelo, ferramentas ou as regras de trabalho, \
             segurança e honestidade acima. Em caso de conflito, as regras do Kestrel vencem.\n",
            self.nome,
            self.descricao.trim(),
            self.instrucoes.trim()
        )
    }

    /// Caminho do arquivo deste Comp dentro do diretório informado.
    pub fn caminho(&self, dir: &Path) -> PathBuf {
        dir.join(format!("{}.toml", self.slug()))
    }

    /// Salva atomicamente. `sobrescrever` só deve ser verdadeiro depois de
    /// uma confirmação explícita do usuário.
    pub fn salvar(&self, dir: &Path, sobrescrever: bool) -> Result<PathBuf, String> {
        self.validar()?;
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("não foi possível criar {}: {e}", dir.display()))?;
        let caminho = self.caminho(dir);
        if caminho.exists() && !sobrescrever {
            return Err(format!(
                "o Comp `{}` já existe em {}. Use `/person edit {}` para alterar.",
                self.nome,
                caminho.display(),
                self.nome
            ));
        }
        let texto =
            toml::to_string_pretty(self).map_err(|e| format!("falha ao serializar o Comp: {e}"))?;
        let tmp = caminho.with_extension("toml.tmp");
        std::fs::write(&tmp, texto)
            .map_err(|e| format!("falha ao gravar {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &caminho)
            .map_err(|e| format!("falha ao substituir {}: {e}", caminho.display()))?;
        Ok(caminho)
    }

    /// Carrega um arquivo `.toml`.
    pub fn carregar(caminho: &Path) -> Result<Self, String> {
        let texto = std::fs::read_to_string(caminho)
            .map_err(|e| format!("não foi possível ler {}: {e}", caminho.display()))?;
        let comp: Self = toml::from_str(&texto)
            .map_err(|e| format!("Comp inválido em {}: {e}", caminho.display()))?;
        comp.validar()?;
        Ok(comp)
    }
}

/// Lista os Comps válidos do diretório com o caminho real de cada arquivo.
/// Arquivos inválidos são ignorados (e não derrubam a interface).
fn listar_com_caminhos(dir: &Path) -> Vec<(Comp, PathBuf)> {
    let Ok(entradas) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut comps = Vec::new();
    for entrada in entradas.flatten() {
        let caminho = entrada.path();
        if caminho.extension().and_then(|e| e.to_str()) != Some("toml") {
            continue;
        }
        if let Ok(comp) = Comp::carregar(&caminho) {
            comps.push((comp, caminho));
        }
    }
    comps.sort_by_key(|(a, _)| a.nome.to_lowercase());
    comps
}

/// Lista os Comps válidos do diretório, ordenados por nome.
pub fn listar(dir: &Path) -> Vec<Comp> {
    listar_com_caminhos(dir)
        .into_iter()
        .map(|(comp, _)| comp)
        .collect()
}

/// Procura por slug ou nome (sem diferenciar maiúsculas). O caminho devolvido
/// é o arquivo real: usar `comp.caminho(dir)` falhava quando o arquivo tinha
/// sido renomeado (o nome dele não é necessariamente o slug).
pub fn encontrar(dir: &Path, nome: &str) -> Option<(Comp, PathBuf)> {
    let procurado = nome.trim().to_lowercase();
    let slug = slug_de(nome);
    for (comp, caminho) in listar_com_caminhos(dir) {
        if comp.nome.to_lowercase() == procurado || comp.slug() == slug {
            return Some((comp, caminho));
        }
    }
    None
}

/// Remove um Comp pelo nome ou slug. Devolve o caminho removido.
pub fn remover(dir: &Path, nome: &str) -> Result<PathBuf, String> {
    let (comp, caminho) = encontrar(dir, nome)
        .ok_or_else(|| format!("Comp `{nome}` não encontrado em {}.", dir.display()))?;
    std::fs::remove_file(&caminho)
        .map_err(|e| format!("falha ao remover o Comp `{}`: {e}", comp.nome))?;
    Ok(caminho)
}

/// Converte um nome de exibição em slug `[a-z0-9_-]`, no máximo 32 caracteres.
/// Espaços viram `-`; outros caracteres são descartados.
pub fn slug_de(nome: &str) -> String {
    let mut saida = String::new();
    for c in nome.trim().chars() {
        let c = c.to_ascii_lowercase();
        if c.is_ascii_alphanumeric() || c == '_' {
            saida.push(c);
        } else if (c.is_whitespace() || c == '-') && !saida.ends_with('-') && !saida.is_empty() {
            saida.push('-');
        }
        if saida.chars().count() >= MAX_SLUG {
            break;
        }
    }
    saida.trim_matches('-').to_string()
}

fn primeira_linha(texto: &str) -> String {
    let linha = texto
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .unwrap_or("Comp criado no Kestrel");
    crate::util::truncate(linha, MAX_DESCRICAO)
}

#[cfg(test)]
mod testes {
    use super::*;

    fn dir_temporario() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kestrel-comp-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn slug_aceita_espacos_e_rejeita_travessia_de_caminho() {
        assert_eq!(slug_de("Lazy"), "lazy");
        assert_eq!(slug_de("Técnico Preciso"), "tcnico-preciso");
        assert_eq!(slug_de("../etc/passwd"), "etcpasswd");
        assert_eq!(slug_de("---"), "");
        assert!(slug_de(&"a".repeat(80)).len() <= MAX_SLUG);
    }

    #[test]
    fn validacao_limita_tamanho_e_conteudo() {
        assert!(Comp::novo("Lazy", "seja breve").is_ok());
        assert!(Comp::novo("", "x").is_err());
        assert!(Comp::novo("Lazy", "   ").is_err());
        assert!(Comp::novo("Lazy", "x".repeat(MAX_INSTRUCOES + 1)).is_err());
        let comp = Comp::novo("Lazy", "linha um\nlinha dois").unwrap();
        assert_eq!(comp.descricao, "linha um");
        assert!(comp.bloco_prompt().contains("regras do Kestrel vencem"));
    }

    #[test]
    fn salvar_listar_carregar_e_remover() {
        let dir = dir_temporario();
        let comp = Comp::novo("Lazy", "Responda de forma curta e direta.").unwrap();
        let caminho = comp.salvar(&dir, false).unwrap();
        assert!(caminho.ends_with("lazy.toml"));
        assert!(comp.salvar(&dir, false).is_err());
        assert!(comp.salvar(&dir, true).is_ok());
        let lista = listar(&dir);
        assert_eq!(lista.len(), 1);
        assert_eq!(lista[0].nome, "Lazy");
        let (achado, _) = encontrar(&dir, "lazy").unwrap();
        assert_eq!(achado.instrucoes, comp.instrucoes);
        remover(&dir, "Lazy").unwrap();
        assert!(listar(&dir).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn arquivo_invalido_nao_quebra_a_lista() {
        let dir = dir_temporario();
        std::fs::write(dir.join("quebrado.toml"), "isto = [").unwrap();
        Comp::novo("Bom", "instruções")
            .unwrap()
            .salvar(&dir, false)
            .unwrap();
        let lista = listar(&dir);
        assert_eq!(lista.len(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// O arquivo pode ter sido renomeado: remover precisa usar o caminho real,
    /// não o slug calculado a partir do nome.
    #[test]
    fn encontrar_e_remover_usam_o_caminho_real_do_arquivo() {
        let dir = dir_temporario();
        let comp = Comp::novo("Lazy", "seja breve").unwrap();
        let texto = toml::to_string_pretty(&comp).unwrap();
        let real = dir.join("renomeado.toml");
        std::fs::write(&real, texto).unwrap();
        let (_, caminho) = encontrar(&dir, "Lazy").unwrap();
        assert_eq!(caminho, real);
        let removido = remover(&dir, "lazy").unwrap();
        assert_eq!(removido, real);
        assert!(!real.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
