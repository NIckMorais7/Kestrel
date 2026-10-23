//! Busca e análise de código: busca textual/regex, procura de definições por
//! linguagem e análise do projeto.

use super::{
    Tool, ToolContext, ToolError, ToolOutput, arg_bool, arg_str, arg_str_opt, arg_u64,
    resolver_caminho,
};
use crate::provider::ToolSpec;
use regex::Regex;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq)]
enum Modo {
    Texto,
    Definicoes,
}

/// Padrões de definição por linguagem, usados no modo `definicoes`.
fn padroes_definicao(ext: &str) -> Vec<&'static str> {
    match ext {
        "rs" => vec![
            r"^\s*(pub\s+)?(async\s+)?fn\s+{q}\b",
            r"^\s*(pub\s+)?struct\s+{q}\b",
            r"^\s*(pub\s+)?enum\s+{q}\b",
            r"^\s*(pub\s+)?trait\s+{q}\b",
            r"^\s*(pub\s+)?type\s+{q}\b",
            r"^\s*(pub\s+)?const\s+{q}\b",
            r"^\s*(pub\s+)?static\s+{q}\b",
            r"^\s*impl\b.*\b{q}\b",
        ],
        "py" => vec![r"^\s*(async\s+)?def\s+{q}\b", r"^\s*class\s+{q}\b"],
        "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" => vec![
            r"^\s*(export\s+)?(async\s+)?function\s+{q}\b",
            r"^\s*(export\s+)?class\s+{q}\b",
            r"^\s*(export\s+)?(const|let|var)\s+{q}\b",
            r"^\s*interface\s+{q}\b",
            r"^\s*type\s+{q}\b",
        ],
        "go" => vec![r"^\s*func\s+(\([^)]*\)\s*)?{q}\b", r"^\s*type\s+{q}\b"],
        "java" | "kt" | "kts" | "cs" => vec![
            r"^\s*(public|private|protected|internal|open|final|static|abstract|\s)*\s*(class|interface|enum|record|void|[A-Za-z_<>\[\],? ]+)\s+{q}\s*\(",
            r"^\s*(public|private|protected|internal)?\s*(class|interface|enum|record)\s+{q}\b",
        ],
        "rb" => vec![
            r"^\s*def\s+{q}\b",
            r"^\s*class\s+{q}\b",
            r"^\s*module\s+{q}\b",
        ],
        "php" => vec![
            r"^\s*(public|private|protected|static|\s)*function\s+{q}\b",
            r"^\s*class\s+{q}\b",
        ],
        _ => Vec::new(),
    }
}

fn compilar_busca(consulta: &str, regex: bool, case_sensitive: bool) -> Result<Regex, ToolError> {
    let padrao = if regex {
        consulta.to_string()
    } else {
        regex::escape(consulta)
    };
    let padrao = if case_sensitive {
        padrao
    } else {
        format!("(?i){padrao}")
    };
    Regex::new(&padrao).map_err(|e| {
        ToolError::Argumentos(format!(
            "expressão regular inválida: {e}. Verifique a sintaxe ou use regex=false."
        ))
    })
}

pub struct SearchCode;

#[async_trait::async_trait]
impl Tool for SearchCode {
    fn nome(&self) -> &'static str {
        "search_code"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Pesquisa no código do projeto com busca textual ou expressões regulares \
                e, no modo `definicoes`, localiza declarações (funções, classes, structs) na \
                linguagem do arquivo. Respeita .gitignore."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "consulta": {"type": "string", "description": "Texto ou expressão regular"},
                    "path": {"type": "string", "description": "Diretório ou arquivo inicial (padrão: projeto)"},
                    "regex": {"type": "boolean", "description": "Interpretar `consulta` como expressão regular"},
                    "modo": {"type": "string", "description": "texto | definicoes"},
                    "glob": {"type": "string", "description": "Filtro por extensão, ex.: .rs"},
                    "case_sensitive": {"type": "boolean"},
                    "contexto": {"type": "integer", "description": "Linhas de contexto antes/depois (padrão 0)"},
                    "max_resultados": {"type": "integer", "description": "Limite de ocorrências (padrão 80)"}
                },
                "required": ["consulta"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "path")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    fn detalhe(&self, args: &Value) -> String {
        let q = arg_str_opt(args, "consulta").unwrap_or_default();
        let glob = arg_str_opt(args, "glob").unwrap_or_default();
        format!(
            "\"{}\"{}",
            crate::util::truncate(&q, 40),
            if glob.is_empty() {
                String::new()
            } else {
                format!(" em {glob}")
            }
        )
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let consulta = arg_str(&args, "consulta")?;
        let modo = match arg_str_opt(&args, "modo")
            .unwrap_or_else(|| "texto".into())
            .as_str()
        {
            "definicoes" | "definições" | "definitions" => Modo::Definicoes,
            _ => Modo::Texto,
        };
        let usar_regex = arg_bool(
            &args,
            "regex",
            modo == Modo::Texto && regex_aparente(&consulta),
        ) || modo == Modo::Definicoes;
        let case_sensitive = arg_bool(&args, "case_sensitive", false);
        let max = arg_u64(&args, "max_resultados").unwrap_or(80).max(1) as usize;
        let contexto = arg_u64(&args, "contexto").unwrap_or(0) as usize;
        let glob = arg_str_opt(&args, "glob");
        let base = resolver_caminho(
            ctx,
            &arg_str_opt(&args, "path").unwrap_or_else(|| ctx.workdir.display().to_string()),
        );

        // Quando `path` é um arquivo, a busca vale só para ele. Antes o código
        // subia para o diretório pai e varria todos os arquivos vizinhos.
        let arquivo_unico = base.is_file().then(|| base.clone());
        let raiz = if let Some(arquivo) = &arquivo_unico {
            arquivo
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| ctx.workdir.clone())
        } else {
            base.clone()
        };
        if !raiz.exists() {
            return Err(ToolError::msg(format!(
                "o caminho {} não existe",
                raiz.display()
            )));
        }

        let mut resultados: Vec<String> = Vec::new();
        let mut arquivos_com_match = 0usize;
        let mut total = 0usize;

        let inicio_caminhada = arquivo_unico.as_deref().unwrap_or(&raiz);
        let walker = ignore::WalkBuilder::new(inicio_caminhada)
            .hidden(false)
            .git_ignore(true)
            .max_depth(Some(20))
            .build();
        'arquivos: for entrada in walker.flatten() {
            if !entrada.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let caminho = entrada.path();
            if let Some(alvo) = &arquivo_unico
                && caminho != alvo
            {
                continue;
            }
            let ext = caminho
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();
            if let Some(filtro) = &glob {
                let sufixo = filtro.trim_start_matches('*');
                let sufixo = sufixo.trim_start_matches('.');
                if !ext.ends_with(sufixo) {
                    continue;
                }
            }
            // O limite precisa ser checado antes da leitura: antes o arquivo
            // inteiro era carregado em memória para só então ser descartado.
            let Ok(meta) = std::fs::metadata(caminho) else {
                continue;
            };
            if meta.len() > 2_000_000 {
                continue; // arquivo grande demais para busca em memória
            }
            let Ok(bytes) = std::fs::read(caminho) else {
                continue;
            };
            if bytes.iter().take(4096).any(|b| *b == 0) {
                continue; // binário
            }
            let texto = String::from_utf8_lossy(&bytes);

            let padrao = match modo {
                Modo::Texto => compilar_busca(&consulta, usar_regex, case_sensitive),
                Modo::Definicoes => {
                    let escapado = regex::escape(&consulta);
                    let alternativas: Vec<String> = padroes_definicao(&ext)
                        .into_iter()
                        .map(|p| p.replace("{q}", &escapado))
                        .collect();
                    if alternativas.is_empty() {
                        // Linguagem sem padrões: cai para busca textual.
                        compilar_busca(&consulta, false, case_sensitive)
                    } else {
                        // O modo de definições também respeita
                        // `case_sensitive`; antes ele sempre diferenciava
                        // maiúsculas mesmo com a opção padrão (insensível).
                        let flags = if case_sensitive { "(?m)" } else { "(?im)" };
                        Regex::new(&format!("{flags}{}", alternativas.join("|")))
                            .map_err(|e| ToolError::msg(format!("padrão inválido: {e}")))
                    }
                }
            }?;

            let linhas: Vec<&str> = texto.lines().collect();
            let mut achados_no_arquivo = 0usize;
            for (i, linha) in linhas.iter().enumerate() {
                if padrao.is_match(linha) {
                    achados_no_arquivo += 1;
                    let rel = crate::util::display_path(&ctx.workdir, caminho);
                    if contexto == 0 {
                        resultados.push(format!("{}:{}: {}", rel, i + 1, linha.trim_end()));
                    } else {
                        let inicio = i.saturating_sub(contexto);
                        let fim = (i + contexto + 1).min(linhas.len());
                        let mut bloco = vec![format!("{}:{}: {}", rel, i + 1, linha.trim_end())];
                        for (j, l) in linhas.iter().enumerate().take(fim).skip(inicio) {
                            if j != i {
                                bloco.push(format!("{}:{}: {}", rel, j + 1, l.trim_end()));
                            }
                        }
                        resultados.push(bloco.join("\n"));
                    }
                    total += 1;
                    if total >= max {
                        resultados.push(format!(
                            "… limite de {max} ocorrências atingido; refine a busca"
                        ));
                        break 'arquivos;
                    }
                }
            }
            if achados_no_arquivo > 0 {
                arquivos_com_match += 1;
            }
        }

        if resultados.is_empty() {
            return Ok(ToolOutput::texto(format!(
                "Nenhuma ocorrência de `{consulta}` em {} (glob: {:?}).",
                raiz.display(),
                glob
            )));
        }
        Ok(ToolOutput::texto(format!(
            "{} ocorrências em {} arquivos:\n{}",
            total,
            arquivos_com_match,
            resultados.join("\n")
        ))
        .com_resumo(format!(
            "{total} ocorrências de \"{}\"",
            crate::util::truncate(&consulta, 30)
        )))
    }
}

fn regex_aparente(consulta: &str) -> bool {
    consulta.contains('\\')
        || consulta.contains(".*")
        || consulta.contains('^')
        || consulta.contains('$')
        || consulta.contains('[')
        || consulta.contains("(?")
}

pub struct AnalyzeProject;

#[async_trait::async_trait]
impl Tool for AnalyzeProject {
    fn nome(&self) -> &'static str {
        "analyze_project"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Reanalisa o projeto: linguagens, dependências, comandos de build/teste, \
                instruções (AGENTS.md e equivalentes) e estrutura. Use quando o projeto mudar \
                durante a sessão."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Raiz a analisar (padrão: projeto atual)"}
                }
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "path")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let raiz = resolver_caminho(
            ctx,
            &arg_str_opt(&args, "path").unwrap_or_else(|| ctx.workdir.display().to_string()),
        );
        let info = crate::project::analyze(&raiz);
        Ok(ToolOutput::texto(info.prompt_block()).com_resumo(format!(
            "analisou {} ({} linguagens)",
            info.name,
            info.languages.len()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn escapa_busca_literal() {
        let re = compilar_busca("fn main(", false, true).unwrap();
        assert!(re.is_match("fn main( {"));
        assert!(!re.is_match("fn mainX"));
    }

    #[test]
    fn busca_case_insensitive_por_padrao() {
        let re = compilar_busca("ERRO", false, false).unwrap();
        assert!(re.is_match("um erro ocorreu"));
    }

    #[test]
    fn padroes_de_definicao_por_linguagem() {
        assert!(!padroes_definicao("rs").is_empty());
        assert!(!padroes_definicao("py").is_empty());
        assert!(!padroes_definicao("ts").is_empty());
        assert!(padroes_definicao("txt").is_empty());
        let rust = padroes_definicao("rs")
            .into_iter()
            .map(|p| p.replace("{q}", "main"))
            .collect::<Vec<_>>()
            .join("|");
        let re = Regex::new(&format!("(?m){rust}")).unwrap();
        assert!(re.is_match("fn main() {"));
        assert!(re.is_match("pub struct main {"));
    }

    #[test]
    fn detecta_regex_aparente() {
        assert!(regex_aparente("fn\\s+main"));
        assert!(regex_aparente("a.*b"));
        assert!(!regex_aparente("fn main"));
    }

    /// `path` apontando para um arquivo limitava a busca ao arquivo; antes o
    /// diretório pai inteiro era varrido.
    #[tokio::test]
    async fn busca_em_arquivo_unico_nao_varre_o_diretorio() {
        use crate::tools::Tool;
        let dir = std::env::temp_dir().join(format!("kestrel-busca-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("alvo.txt");
        std::fs::write(&alvo, "agulha no alvo\n").unwrap();
        std::fs::write(dir.join("vizinho.txt"), "agulha no vizinho\n").unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        let args = serde_json::json!({
            "consulta": "agulha",
            "path": alvo.display().to_string(),
        });
        let saida = SearchCode.executar(args, &ctx).await.unwrap();
        assert!(saida.conteudo.contains("alvo.txt"), "{}", saida.conteudo);
        assert!(
            !saida.conteudo.contains("vizinho.txt"),
            "a busca vazou para o diretório: {}",
            saida.conteudo
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
