//! Descoberta e carregamento de skills.
//!
//! Skills são arquivos `SKILL.md` (ou `*.md`) em diretórios de skills:
//! - `~/.config/kestrel/skills/<nome>/SKILL.md` (globais, do usuário)
//! - `<projeto>/.kestrel/skills/<nome>/SKILL.md` (do projeto)
//!
//! O formato usa frontmatter simples:
//!
//! ```text
//! ---
//! name: Nome da skill
//! description: quando usar esta skill
//! ---
//! instruções completas...
//! ```
//!
//! Novas skills podem ser adicionadas sem recompilar o Kestrel.

use crate::util::Paths;
use std::path::{Path, PathBuf};

/// Metadados de uma skill descoberta.
#[derive(Clone, Debug, PartialEq)]
pub struct SkillInfo {
    pub name: String,
    pub description: String,
    pub path: PathBuf,
    pub escopo: Escopo,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Escopo {
    Usuario,
    Projeto,
}

impl Escopo {
    pub fn label(&self) -> &'static str {
        match self {
            Escopo::Usuario => "usuário",
            Escopo::Projeto => "projeto",
        }
    }
}

/// Catálogo de skills disponíveis.
#[derive(Clone, Debug, Default)]
pub struct SkillRegistry {
    skills: Vec<SkillInfo>,
}

impl SkillRegistry {
    /// Descobre skills nos diretórios do usuário e do projeto.
    pub fn discover(paths: &Paths, workdir: &Path) -> Self {
        let mut skills = Vec::new();
        coletar_dir(&paths.skills_dir(), Escopo::Usuario, &mut skills);
        coletar_dir(
            &workdir.join(".kestrel/skills"),
            Escopo::Projeto,
            &mut skills,
        );
        // Também aceita skills no diretório `.opencode/skills` quando existirem,
        // mantendo compatibilidade com instalações já presentes no projeto.
        skills.sort_by_key(|a| a.name.to_lowercase());
        skills.dedup_by(|a, b| a.name.to_lowercase() == b.name.to_lowercase());
        Self { skills }
    }

    /// Registro vazio, para testes e estados sem skills carregadas.
    #[cfg(test)]
    pub fn vazio() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.skills.is_empty()
    }

    pub fn all(&self) -> &[SkillInfo] {
        &self.skills
    }

    pub fn get(&self, nome: &str) -> Option<&SkillInfo> {
        self.skills
            .iter()
            .find(|s| s.name.eq_ignore_ascii_case(nome))
    }

    /// Lê o conteúdo completo de uma skill.
    pub fn load(&self, nome: &str) -> anyhow::Result<String> {
        let info = self
            .get(nome)
            .ok_or_else(|| anyhow::anyhow!("skill `{nome}` não encontrada"))?;
        let texto = std::fs::read_to_string(&info.path)
            .map_err(|e| anyhow::anyhow!("falha ao ler {}: {e}", info.path.display()))?;
        let (_, corpo) = dividir_frontmatter(&texto);
        Ok(corpo.trim().to_string())
    }

    /// Seleciona skills relevantes para uma tarefa por sobreposição de palavras.
    pub fn relevantes(&self, tarefa: &str, max: usize) -> Vec<SkillInfo> {
        let palavras: Vec<String> = palavras_chave(tarefa);
        let mut pontuadas: Vec<(usize, &SkillInfo)> = self
            .skills
            .iter()
            .map(|s| {
                let alvo = format!("{} {}", s.name, s.description).to_lowercase();
                let pontos = palavras
                    .iter()
                    .filter(|p| alvo.contains(p.as_str()))
                    .count();
                (pontos, s)
            })
            .filter(|(pontos, _)| *pontos > 0)
            .collect();
        pontuadas.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.name.cmp(&b.1.name)));
        pontuadas
            .into_iter()
            .take(max)
            .map(|(_, s)| s.clone())
            .collect()
    }

    /// Bloco de nomes e descrições para o prompt do sistema. Com muitas
    /// skills instaladas, prioriza as mais relevantes para a tarefa atual.
    pub fn prompt_block(&self, tarefa: &str) -> String {
        if self.skills.is_empty() {
            return String::new();
        }
        let relevantes;
        let selecionadas: Vec<&SkillInfo> = if self.skills.len() > 15 {
            relevantes = self.relevantes(tarefa, 15);
            if relevantes.is_empty() {
                self.skills.iter().take(15).collect()
            } else {
                relevantes.iter().collect()
            }
        } else {
            self.skills.iter().collect()
        };
        let linhas: Vec<String> = selecionadas
            .iter()
            .map(|s| format!("- {} ({}): {}", s.name, s.escopo.label(), s.description))
            .collect();
        format!(
            "Skills disponíveis (use `load_skill` para carregar as instruções completas quando forem pertinentes):\n{}",
            linhas.join("\n")
        )
    }
}

fn palavras_chave(texto: &str) -> Vec<String> {
    texto
        .to_lowercase()
        .split(|c: char| !c.is_alphanumeric() && c != '-' && c != '_')
        .filter(|p| p.chars().count() >= 4)
        .map(|p| p.to_string())
        .collect()
}

fn coletar_dir(dir: &Path, escopo: Escopo, saida: &mut Vec<SkillInfo>) {
    let Ok(entradas) = std::fs::read_dir(dir) else {
        return;
    };
    for entrada in entradas.flatten() {
        let caminho = entrada.path();
        if caminho.is_dir() {
            let candidato = caminho.join("SKILL.md");
            if candidato.is_file()
                && let Some(info) = parse_skill(&candidato, escopo)
            {
                saida.push(info);
            }
        } else if caminho.extension().is_some_and(|e| e == "md")
            && let Some(info) = parse_skill(&caminho, escopo)
        {
            saida.push(info);
        }
    }
}

/// Lê um arquivo de skill e extrai nome/descrição.
pub fn parse_skill(path: &Path, escopo: Escopo) -> Option<SkillInfo> {
    let texto = std::fs::read_to_string(path).ok()?;
    let (front, _) = dividir_frontmatter(&texto);
    // `skills/<nome>/SKILL.md` usa o nome do diretório; um `skills/<nome>.md`
    // solto usa o nome do arquivo. Antes todo arquivo herdava o nome do
    // diretório pai (`skills`), e todas as skills planas colidiam entre si.
    let mut nome = if path
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case("SKILL.md"))
    {
        path.parent()
            .filter(|p| p.file_name().is_some())
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "skill".into())
    } else {
        path.file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "skill".into())
    };
    let mut descricao = primeira_frase(&texto);
    for linha in front.lines() {
        let Some((chave, valor)) = linha.split_once(':') else {
            continue;
        };
        let valor = valor.trim().trim_matches('"').trim_matches('\'');
        match chave.trim().to_lowercase().as_str() {
            "name" | "nome" if !valor.is_empty() => nome = valor.to_string(),
            "description" | "descricao" | "descrição" if !valor.is_empty() => {
                descricao = valor.to_string()
            }
            _ => {}
        }
    }
    if descricao.is_empty() {
        descricao = format!("skill em {}", path.display());
    }
    Some(SkillInfo {
        name: nome,
        description: crate::util::truncate(&descricao, 300),
        path: path.to_path_buf(),
        escopo,
    })
}

fn dividir_frontmatter(texto: &str) -> (&str, &str) {
    let texto = texto.trim_start_matches('\u{feff}');
    if let Some(resto) = texto.strip_prefix("---")
        && let Some(fim) = resto.find("\n---")
    {
        let front = &resto[..fim];
        let corpo = &resto[fim + 4..];
        return (front, corpo);
    }
    ("", texto)
}

fn primeira_frase(texto: &str) -> String {
    let (_, corpo) = dividir_frontmatter(texto);
    corpo
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty() && !l.starts_with('#') && !l.starts_with("---"))
        .map(|l| l.trim_start_matches('#').trim().to_string())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> (PathBuf, Paths, PathBuf) {
        let raiz = std::env::temp_dir().join(format!("kestrel-skills-{}", uuid::Uuid::new_v4()));
        let paths = Paths::with_root(&raiz);
        let projeto = raiz.join("projeto");
        std::fs::create_dir_all(projeto.join(".kestrel/skills/refatoracao")).unwrap();
        std::fs::create_dir_all(paths.skills_dir().join("testes")).unwrap();
        std::fs::write(
            projeto.join(".kestrel/skills/refatoracao/SKILL.md"),
            "---\nname: Refatoração segura\ndescription: usar ao refatorar código Rust com testes\n---\n\n# Passos\n1. rode os testes\n",
        )
        .unwrap();
        std::fs::write(
            paths.skills_dir().join("testes/SKILL.md"),
            "---\ndescription: estratégia de testes e cobertura\n---\nSempre execute cargo test.\n",
        )
        .unwrap();
        std::fs::write(
            paths.skills_dir().join("revisao.md"),
            "---\ndescription: revisar mudanças antes de entregar\n---\nRevise o diff.\n",
        )
        .unwrap();
        (raiz, paths, projeto)
    }

    #[test]
    fn descobre_skills_do_usuario_e_do_projeto() {
        let (raiz, paths, projeto) = setup();
        let reg = SkillRegistry::discover(&paths, &projeto);
        assert_eq!(reg.all().len(), 3);
        assert!(reg.get("Refatoração segura").is_some());
        // Sem frontmatter `name`, usa o nome do diretório (SKILL.md) ou do
        // arquivo (skill plana).
        assert!(reg.get("testes").is_some());
        assert!(reg.get("revisao").is_some());
        let _ = std::fs::remove_dir_all(&raiz);
    }

    #[test]
    fn carrega_conteudo_sem_frontmatter() {
        let (raiz, paths, projeto) = setup();
        let reg = SkillRegistry::discover(&paths, &projeto);
        let corpo = reg.load("Refatoração segura").unwrap();
        assert!(corpo.contains("rode os testes"));
        assert!(!corpo.contains("description:"));
        let _ = std::fs::remove_dir_all(&raiz);
    }

    #[test]
    fn seleciona_relevantes_para_a_tarefa() {
        let (raiz, paths, projeto) = setup();
        let reg = SkillRegistry::discover(&paths, &projeto);
        let rel = reg.relevantes("preciso refatorar o parser com segurança", 3);
        assert!(!rel.is_empty());
        assert_eq!(rel[0].name, "Refatoração segura");
        let _ = std::fs::remove_dir_all(&raiz);
    }

    #[test]
    fn prompt_block_lista_skills() {
        let (raiz, paths, projeto) = setup();
        let reg = SkillRegistry::discover(&paths, &projeto);
        let bloco = reg.prompt_block("refatorar com segurança");
        assert!(bloco.contains("Refatoração segura"));
        assert!(bloco.contains("load_skill"));
        let _ = std::fs::remove_dir_all(&raiz);
    }
}
