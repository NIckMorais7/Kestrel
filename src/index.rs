//! Índice lexical do projeto (BM25) para retrieval de arquivos.
//!
//! Construído sob demanda, respeita `.gitignore` e limites de tamanho. A
//! busca é lexical (não usa embeddings): ranqueia arquivos por termos do
//! código e devolve trechos reais com número de linha. O cache expira para
//! acompanhar edições feitas fora do agente.

use ignore::WalkBuilder;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Teto de arquivos e bytes: mantém a memória e o tempo de varredura baixos.
pub const MAX_ARQUIVOS: usize = 4000;
pub const MAX_BYTES: usize = 64 * 1024 * 1024;
const MAX_ARQUIVO_BYTES: u64 = 512 * 1024;
const VALIDADE: Duration = Duration::from_secs(120);

const EXTENSOES: &[&str] = &[
    "rs",
    "toml",
    "md",
    "txt",
    "json",
    "yaml",
    "yml",
    "js",
    "jsx",
    "ts",
    "tsx",
    "py",
    "go",
    "java",
    "c",
    "h",
    "cc",
    "cpp",
    "hpp",
    "cs",
    "rb",
    "php",
    "sh",
    "fish",
    "zsh",
    "sql",
    "html",
    "css",
    "scss",
    "vue",
    "svelte",
    "kt",
    "kts",
    "swift",
    "ex",
    "exs",
    "lua",
    "r",
    "dart",
    "proto",
    "tf",
    "dockerfile",
    "makefile",
    "cmake",
    "gradle",
];

#[derive(Debug)]
struct Documento {
    caminho: PathBuf,
    conteudo: String,
    tamanho: u32,
}

#[derive(Debug)]
pub struct ProjectIndex {
    raiz: PathBuf,
    criado: Instant,
    documentos: Vec<Documento>,
    /// termo → [(documento, frequência)]
    termos: HashMap<String, Vec<(usize, u32)>>,
    media_tamanho: f64,
    /// `true` quando a varredura parou num teto (arquivos/bytes): o índice é
    /// parcial e pode omitir arquivos relevantes.
    parcial: bool,
}

impl ProjectIndex {
    pub fn raiz(&self) -> &Path {
        &self.raiz
    }

    pub fn total_documentos(&self) -> usize {
        self.documentos.len()
    }

    /// Indica se a varredura parou num teto e o índice é parcial.
    pub fn parcial(&self) -> bool {
        self.parcial
    }

    /// Ranqueia documentos por BM25 e devolve trechos com as linhas que
    /// contêm os termos consultados.
    pub fn buscar(&self, consulta: &str, limite: usize) -> Vec<ResultadoBusca> {
        let termos = termos_unicos(consulta);
        if termos.is_empty() || self.documentos.is_empty() {
            return Vec::new();
        }
        let n = self.documentos.len() as f64;
        let k1 = 1.2f64;
        let b = 0.75f64;
        let mut pontos: HashMap<usize, f64> = HashMap::new();
        for termo in &termos {
            let Some(postings) = self.termos.get(termo) else {
                continue;
            };
            let df = postings.len() as f64;
            let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
            for (doc, tf) in postings {
                let tf = f64::from(*tf);
                let tamanho = self.documentos[*doc].tamanho as f64;
                let denominador = tf + k1 * (1.0 - b + b * tamanho / self.media_tamanho.max(1.0));
                *pontos.entry(*doc).or_insert(0.0) += idf * (tf * (k1 + 1.0)) / denominador;
            }
        }
        let mut ordenados: Vec<(usize, f64)> = pontos.into_iter().collect();
        ordenados.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
        ordenados
            .into_iter()
            .take(limite)
            .map(|(indice, pontos)| {
                let doc = &self.documentos[indice];
                ResultadoBusca {
                    caminho: doc.caminho.clone(),
                    pontos,
                    trechos: trechos(&doc.conteudo, &termos, 3),
                }
            })
            .collect()
    }
}

#[derive(Clone, Debug)]
pub struct ResultadoBusca {
    pub caminho: PathBuf,
    pub pontos: f64,
    /// `(número da linha, texto)`.
    pub trechos: Vec<(usize, String)>,
}

fn termos_unicos(texto: &str) -> Vec<String> {
    let mut termos: Vec<String> = texto
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.chars().count() >= 2)
        .map(|t| t.to_lowercase())
        .collect();
    // Também indexa as partes de identificadores snake_case.
    let mut partes: Vec<String> = termos
        .iter()
        .flat_map(|t| t.split('_'))
        .filter(|t| t.chars().count() >= 2)
        .map(str::to_string)
        .collect();
    termos.append(&mut partes);
    termos.sort();
    termos.dedup();
    termos
}

fn termos_documento(texto: &str) -> HashMap<String, u32> {
    let mut tf = HashMap::new();
    for termo in texto
        .split(|c: char| !c.is_alphanumeric() && c != '_')
        .filter(|t| t.chars().count() >= 2)
    {
        let termo = termo.to_lowercase();
        *tf.entry(termo.clone()).or_insert(0) += 1;
        for parte in termo.split('_') {
            if parte.chars().count() >= 2 {
                *tf.entry(parte.to_string()).or_insert(0) += 1;
            }
        }
    }
    tf
}

fn trechos(conteudo: &str, termos: &[String], maximo: usize) -> Vec<(usize, String)> {
    let mut saida = Vec::new();
    for (indice, linha) in conteudo.lines().enumerate() {
        let minuscula = linha.to_lowercase();
        if termos.iter().any(|t| minuscula.contains(t.as_str())) {
            saida.push((indice + 1, crate::util::truncate(linha.trim(), 180)));
            if saida.len() >= maximo {
                break;
            }
        }
    }
    saida
}

fn extensao_aceita(caminho: &Path) -> bool {
    let nome = caminho
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    if matches!(
        nome.as_str(),
        "dockerfile" | "makefile" | "cmakelists.txt" | "justfile" | "procfile"
    ) {
        return true;
    }
    caminho
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .is_some_and(|e| EXTENSOES.contains(&e.as_str()))
}

/// Constrói o índice do projeto.
pub fn construir(raiz: &Path) -> ProjectIndex {
    let inicio = Instant::now();
    let mut documentos = Vec::new();
    let mut bytes = 0usize;
    let caminhante = WalkBuilder::new(raiz)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .filter_entry(|e| {
            let nome = e.file_name().to_string_lossy();
            !matches!(
                nome.as_ref(),
                ".git"
                    | "target"
                    | "node_modules"
                    | "dist"
                    | "build"
                    | "vendor"
                    | ".venv"
                    | "__pycache__"
                    | "coverage"
            )
        })
        .build();

    let mut parcial = false;
    for entrada in caminhante.flatten() {
        if documentos.len() >= MAX_ARQUIVOS || bytes >= MAX_BYTES {
            parcial = true;
            break;
        }
        let Ok(meta) = entrada.metadata() else {
            continue;
        };
        if !meta.is_file() || meta.len() > MAX_ARQUIVO_BYTES {
            continue;
        }
        let caminho = entrada.path();
        if !extensao_aceita(caminho) {
            continue;
        }
        let Ok(conteudo) = std::fs::read_to_string(caminho) else {
            continue;
        };
        if conteudo.contains('\0') {
            continue;
        }
        bytes += conteudo.len();
        let tamanho = conteudo.len().min(u32::MAX as usize) as u32;
        documentos.push(Documento {
            caminho: caminho.to_path_buf(),
            conteudo,
            tamanho,
        });
    }

    let mut termos: HashMap<String, Vec<(usize, u32)>> = HashMap::new();
    let mut soma_tamanhos = 0u64;
    for (indice, doc) in documentos.iter().enumerate() {
        soma_tamanhos += u64::from(doc.tamanho);
        for (termo, tf) in termos_documento(&doc.conteudo) {
            termos.entry(termo).or_default().push((indice, tf));
        }
    }
    let media_tamanho = if documentos.is_empty() {
        1.0
    } else {
        soma_tamanhos as f64 / documentos.len() as f64
    };
    ProjectIndex {
        raiz: raiz.to_path_buf(),
        criado: inicio,
        documentos,
        termos,
        media_tamanho,
        parcial,
    }
}

fn chave(raiz: &Path) -> PathBuf {
    std::fs::canonicalize(raiz).unwrap_or_else(|_| raiz.to_path_buf())
}

fn cache() -> &'static Mutex<HashMap<PathBuf, Arc<ProjectIndex>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Arc<ProjectIndex>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Devolve o índice em cache ou constrói um novo.
pub fn obter(raiz: &Path) -> Arc<ProjectIndex> {
    let chave = chave(raiz);
    if let Ok(cache) = cache().lock()
        && let Some(indice) = cache.get(&chave)
        && indice.criado.elapsed() < VALIDADE
    {
        return indice.clone();
    }
    let indice = Arc::new(construir(raiz));
    if let Ok(mut cache) = cache().lock() {
        cache.insert(chave, indice.clone());
    }
    indice
}

/// Esquece o índice (usado em testes e após alterações em lote).
#[cfg(test)]
pub fn invalidar(raiz: &Path) {
    let chave = chave(raiz);
    if let Ok(mut cache) = cache().lock() {
        cache.remove(&chave);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn projeto() -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("kestrel-index-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(
            dir.join("src/parser.rs"),
            "pub fn analisar_expressao() {\n    let token = lexer();\n}\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("src/render.rs"),
            "pub fn desenhar_tela() {\n    println!(\"render\");\n}\n",
        )
        .unwrap();
        std::fs::write(dir.join("README.md"), "documentação do parser").unwrap();
        dir
    }

    #[test]
    fn ranqueia_arquivo_relevante_e_devolve_trechos() {
        let dir = projeto();
        let indice = construir(&dir);
        assert_eq!(indice.total_documentos(), 3);
        let resultados = indice.buscar("analisar expressao lexer", 3);
        assert!(!resultados.is_empty());
        assert!(resultados[0].caminho.ends_with("src/parser.rs"));
        assert!(
            resultados[0]
                .trechos
                .iter()
                .any(|(_, l)| l.contains("analisar_expressao"))
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn consulta_sem_termo_nao_retorna_nada() {
        let dir = projeto();
        let indice = construir(&dir);
        assert!(indice.buscar("xyzabc-nao-existe", 5).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn cache_devolve_a_mesma_instancia_e_invalida() {
        let dir = projeto();
        invalidar(&dir);
        let a = obter(&dir);
        let b = obter(&dir);
        assert!(Arc::ptr_eq(&a, &b));
        invalidar(&dir);
        let c = obter(&dir);
        assert!(!Arc::ptr_eq(&a, &c));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
