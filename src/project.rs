//! Análise inicial do projeto: linguagem, dependências, comandos, instruções
//! relevantes (`AGENTS.md` e equivalentes) e estrutura de diretórios.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Documento de instruções encontrado no projeto.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstructionDoc {
    pub path: String,
    pub content: String,
}

/// Resumo do projeto reutilizado durante a sessão.
#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub struct ProjectInfo {
    pub root: PathBuf,
    pub name: String,
    pub languages: Vec<String>,
    pub frameworks: Vec<String>,
    pub package_manager: Option<String>,
    pub build_command: Option<String>,
    pub test_command: Option<String>,
    pub lint_command: Option<String>,
    pub instructions: Vec<InstructionDoc>,
    pub structure: Vec<String>,
    pub git_branch: Option<String>,
    pub git_dirty: bool,
    pub analyzed_at: String,
}

const MAX_INSTRUCOES: usize = 6;
const MAX_CHARS_INSTRUCAO: usize = 8000;
const MAX_ESTRUTURA: usize = 40;

/// Resultado da detecção de linguagens e comandos a partir dos arquivos do projeto.
struct Deteccao {
    linguagens: Vec<String>,
    frameworks: Vec<String>,
    gerenciador: Option<String>,
    build: Option<String>,
    test: Option<String>,
    lint: Option<String>,
}

fn ler_limitado(path: &Path, max: usize) -> Option<String> {
    use std::io::Read;
    if max == 0 {
        return Some(String::new());
    }
    // Lê no máximo 4 bytes por caractere desejado (o pior caso do UTF-8).
    // Antes o arquivo inteiro era carregado só para depois truncar: um README
    // gigante ou um `package.json` gerado consumia memória sem necessidade.
    let arquivo = std::fs::File::open(path).ok()?;
    let mut bytes = Vec::new();
    arquivo
        .take(max.saturating_mul(4).saturating_add(1) as u64)
        .read_to_end(&mut bytes)
        .ok()?;
    let texto = String::from_utf8_lossy(&bytes);
    if texto.chars().count() > max {
        Some(crate::util::truncate(&texto, max))
    } else {
        Some(texto.into_owned())
    }
}

fn nome_projeto(root: &Path) -> String {
    root.file_name()
        .map(|n| n.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| root.display().to_string())
}

/// Executa um comando git simples e devolve a saída.
fn git(root: &Path, args: &[&str]) -> Option<String> {
    let saida = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    if !saida.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&saida.stdout).trim().to_string())
}

/// Lista a estrutura de primeiro nível, respeitando `.gitignore`.
fn estrutura(root: &Path) -> Vec<String> {
    let mut itens = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .max_depth(Some(1))
        .hidden(false)
        .filter_entry(|e| {
            let nome = e.file_name().to_string_lossy();
            nome != ".git" && nome != "target" && nome != "node_modules" && nome != ".venv"
        })
        .build();
    for entrada in walker.flatten() {
        if entrada.path() == root {
            continue;
        }
        let nome = entrada.file_name().to_string_lossy().to_string();
        let sufixo = if entrada.file_type().is_some_and(|t| t.is_dir()) {
            "/"
        } else {
            ""
        };
        itens.push(format!("{nome}{sufixo}"));
        if itens.len() >= MAX_ESTRUTURA {
            break;
        }
    }
    itens.sort();
    itens
}

fn detectar_por_arquivos(root: &Path) -> Deteccao {
    let mut linguagens = Vec::new();
    let mut frameworks = Vec::new();
    let mut gerenciador = None;
    let mut build = None;
    let mut test = None;
    let mut lint = None;

    let existe = |nome: &str| root.join(nome).exists();

    if existe("Cargo.toml") {
        linguagens.push("Rust".into());
        build = Some("cargo build".into());
        test = Some("cargo test".into());
        lint = Some("cargo clippy --all-targets -- -D warnings".into());
        if let Some(texto) = ler_limitado(&root.join("Cargo.toml"), 20_000) {
            for linha in texto.lines() {
                let linha = linha.trim();
                if let Some(resto) = linha.strip_prefix("name =") {
                    let _ = resto;
                }
                for marca in [
                    "ratatui", "clap", "tokio", "axum", "actix", "serde", "reqwest", "sqlx",
                    "diesel", "tonic", "bevy",
                ] {
                    if (linha.starts_with(marca) || linha.starts_with(&format!("\"{marca}\"")))
                        && !frameworks.contains(&marca.to_string())
                    {
                        frameworks.push(marca.to_string());
                    }
                }
            }
        }
    }
    if existe("package.json") {
        linguagens.push("JavaScript/TypeScript".into());
        gerenciador = Some(
            if existe("pnpm-lock.yaml") {
                "pnpm"
            } else if existe("yarn.lock") {
                "yarn"
            } else {
                "npm"
            }
            .to_string(),
        );
        if let Some(texto) = ler_limitado(&root.join("package.json"), 60_000)
            && let Ok(valor) = serde_json::from_str::<serde_json::Value>(&texto)
        {
            // Ferramentas de build/teste (vite, jest, vitest, playwright)
            // costumam viver em `devDependencies`; olhar só `dependencies`
            // escondia justamente as tecnologias mais relevantes.
            let deps: Vec<String> = ["dependencies", "devDependencies"]
                .iter()
                .filter_map(|chave| valor.get(*chave).and_then(|d| d.as_object()))
                .flat_map(|o| o.keys().cloned())
                .collect();
            for marco in [
                "react",
                "next",
                "vue",
                "svelte",
                "express",
                "fastify",
                "nestjs",
                "angular",
                "vite",
                "jest",
                "vitest",
                "playwright",
            ] {
                if deps.iter().any(|d| d.contains(marco)) {
                    frameworks.push(marco.to_string());
                }
            }
            if let Some(scripts) = valor.get("scripts").and_then(|s| s.as_object()) {
                let g = gerenciador.clone().unwrap_or_else(|| "npm".into());
                if scripts.contains_key("test") {
                    test = Some(format!("{g} test"));
                }
                if scripts.contains_key("build") {
                    build = Some(format!("{g} run build"));
                }
                if scripts.contains_key("lint") {
                    lint = Some(format!("{g} run lint"));
                }
            }
        }
    }
    if existe("pyproject.toml") || existe("requirements.txt") || existe("setup.py") {
        linguagens.push("Python".into());
        gerenciador = Some(
            if existe("poetry.lock") {
                "poetry"
            } else {
                "pip"
            }
            .to_string(),
        );
        test = Some("python -m pytest".into());
        if let Some(texto) = ler_limitado(&root.join("pyproject.toml"), 40_000) {
            for marco in [
                "django", "flask", "fastapi", "pydantic", "pytorch", "numpy", "pandas",
            ] {
                if texto.to_lowercase().contains(marco) {
                    frameworks.push(marco.to_string());
                }
            }
        }
    }
    if existe("go.mod") {
        linguagens.push("Go".into());
        build = Some("go build ./...".into());
        test = Some("go test ./...".into());
    }
    if existe("Makefile") && build.is_none() {
        build = Some("make".into());
    }
    if existe("CMakeLists.txt") {
        linguagens.push("C/C++".into());
        build = Some("cmake --build build".into());
    }
    if existe("pom.xml") {
        linguagens.push("Java".into());
        build = Some("mvn package".into());
        test = Some("mvn test".into());
    }
    if existe("build.gradle") || existe("build.gradle.kts") {
        linguagens.push("Java/Kotlin".into());
        build = Some("gradle build".into());
        test = Some("gradle test".into());
    }
    if existe("composer.json") {
        linguagens.push("PHP".into());
        test = Some("composer test".into());
    }
    if existe("Gemfile") {
        linguagens.push("Ruby".into());
        test = Some("bundle exec rspec".into());
    }
    if existe("Dockerfile") {
        frameworks.push("docker".into());
    }

    linguagens.sort();
    linguagens.dedup();
    frameworks.sort();
    frameworks.dedup();
    Deteccao {
        linguagens,
        frameworks,
        gerenciador,
        build,
        test,
        lint,
    }
}

fn instrucoes(root: &Path) -> Vec<InstructionDoc> {
    let candidatos = [
        "AGENTS.md",
        "CLAUDE.md",
        ".cursorrules",
        ".kestrel/AGENTS.md",
        ".kestrel/instructions.md",
        "CONTRIBUTING.md",
        "README.md",
    ];
    let mut docs = Vec::new();
    for candidato in candidatos {
        let caminho = root.join(candidato);
        if !caminho.is_file() {
            continue;
        }
        if let Some(conteudo) = ler_limitado(&caminho, MAX_CHARS_INSTRUCAO) {
            docs.push(InstructionDoc {
                path: candidato.to_string(),
                content: conteudo,
            });
        }
        if docs.len() >= MAX_INSTRUCOES {
            break;
        }
    }
    docs
}

/// Executa a análise inicial completa do projeto.
pub fn analyze(root: &Path) -> ProjectInfo {
    let root = crate::util::normalize_path(root);
    let deteccao = detectar_por_arquivos(&root);
    let git_branch = git(&root, &["rev-parse", "--abbrev-ref", "HEAD"]);
    let git_dirty = git(&root, &["status", "--porcelain"])
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false);
    ProjectInfo {
        name: nome_projeto(&root),
        root: root.clone(),
        languages: deteccao.linguagens,
        frameworks: deteccao.frameworks,
        package_manager: deteccao.gerenciador,
        build_command: deteccao.build,
        test_command: deteccao.test,
        lint_command: deteccao.lint,
        instructions: instrucoes(&root),
        structure: estrutura(&root),
        git_branch,
        git_dirty,
        analyzed_at: crate::util::now_rfc3339(),
    }
}

impl ProjectInfo {
    /// Resumo compacto usado no prompt do sistema e na interface.
    pub fn summary(&self) -> String {
        let mut linhas = Vec::new();
        linhas.push(format!("Projeto: {} ({})", self.name, self.root.display()));
        if !self.languages.is_empty() {
            linhas.push(format!("Linguagens: {}", self.languages.join(", ")));
        }
        if !self.frameworks.is_empty() {
            linhas.push(format!("Tecnologias: {}", self.frameworks.join(", ")));
        }
        if let Some(pm) = &self.package_manager {
            linhas.push(format!("Gerenciador: {pm}"));
        }
        if let Some(t) = &self.test_command {
            linhas.push(format!("Testes: {t}"));
        }
        if let Some(b) = &self.build_command {
            linhas.push(format!("Build: {b}"));
        }
        if let Some(git) = &self.git_branch {
            linhas.push(format!(
                "Git: branch {git}{}",
                if self.git_dirty {
                    " (alterações pendentes)"
                } else {
                    ""
                }
            ));
        }
        if !self.structure.is_empty() {
            linhas.push(format!("Estrutura: {}", self.structure.join(" ")));
        }
        linhas.join("\n")
    }

    /// Bloco de contexto enviado ao modelo (instruções do projeto incluídas).
    pub fn prompt_block(&self) -> String {
        let mut partes = vec![self.summary()];
        for doc in &self.instructions {
            partes.push(format!(
                "Instruções do projeto ({}):\n{}",
                doc.path, doc.content
            ));
        }
        partes.join("\n\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projeto_temp() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("kestrel-proj-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn detecta_projeto_rust() {
        let dir = projeto_temp();
        std::fs::write(
            dir.join("Cargo.toml"),
            "[package]\nname = \"x\"\n\n[dependencies]\nratatui = \"0.30\"\ntokio = \"1\"\n",
        )
        .unwrap();
        std::fs::write(dir.join("AGENTS.md"), "Sempre escreva testes.").unwrap();
        let info = analyze(&dir);
        assert!(info.languages.contains(&"Rust".to_string()));
        assert!(info.frameworks.contains(&"ratatui".to_string()));
        assert_eq!(info.test_command.as_deref(), Some("cargo test"));
        assert_eq!(info.instructions.len(), 1);
        assert!(info.prompt_block().contains("Sempre escreva testes"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detecta_projeto_node() {
        let dir = projeto_temp();
        std::fs::write(
            dir.join("package.json"),
            r#"{"dependencies":{"react":"18","express":"4"},"devDependencies":{"vite":"5","vitest":"1"},"scripts":{"test":"jest","build":"vite build"}}"#,
        )
        .unwrap();
        let info = analyze(&dir);
        assert!(info.languages.iter().any(|l| l.contains("JavaScript")));
        assert!(info.frameworks.contains(&"react".to_string()));
        // `vite`/`vitest` só aparecem em devDependencies.
        assert!(info.frameworks.contains(&"vite".to_string()));
        assert!(info.frameworks.contains(&"vitest".to_string()));
        assert_eq!(info.test_command.as_deref(), Some("npm test"));
        assert_eq!(info.build_command.as_deref(), Some("npm run build"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leitura_limitada_respeita_caracteres_multibyte() {
        let dir = projeto_temp();
        let conteudo = "á".repeat(100);
        std::fs::write(dir.join("nota.txt"), &conteudo).unwrap();
        let lido = ler_limitado(&dir.join("nota.txt"), 10).unwrap();
        assert!(lido.chars().count() <= 10, "veio {lido:?}");
        assert!(lido.ends_with('…'));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn projeto_vazio_nao_quebra() {
        let dir = projeto_temp();
        let info = analyze(&dir);
        assert!(info.languages.is_empty());
        assert!(info.test_command.is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
