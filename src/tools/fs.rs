//! Ferramentas de sistema de arquivos: listagem, leitura, edição precisa,
//! criação, remoção e metadados.

use super::{
    Tool, ToolContext, ToolError, ToolOutput, arg_bool, arg_str, arg_str_opt, arg_u64,
    resolver_caminho,
};
use crate::diff;
use crate::provider::ToolSpec;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Detecta conteúdo binário pela presença de byte nulo.
fn parece_binario(bytes: &[u8]) -> bool {
    bytes.iter().take(8192).any(|b| *b == 0)
}

/// Teto para leitura de um arquivo inteiro em memória.
///
/// Sem ele, `read_file` num log/arquivo gigante (ou num FIFO/`/dev/zero`)
/// travava a task e podia estourar a memória sem chance de timeout.
const LIMITE_LEITURA_BYTES: u64 = 32 * 1024 * 1024;

fn ler_texto(path: &Path) -> Result<String, ToolError> {
    let meta = std::fs::metadata(path).map_err(|e| {
        ToolError::msg(format!(
            "não foi possível ler {}: {e}\nDica: verifique se o caminho existe e se você tem permissão de leitura.",
            path.display()
        ))
    })?;
    if !meta.is_file() {
        return Err(ToolError::msg(format!(
            "{} não é um arquivo regular (FIFO, dispositivo ou socket); leitura recusada para não travar o agente.",
            path.display()
        )));
    }
    if meta.len() > LIMITE_LEITURA_BYTES {
        return Err(ToolError::msg(format!(
            "{} tem {} e excede o limite de {} para leitura em memória. Use run_command com `head`/`sed` para recortes, ou start_line/end_line num arquivo menor.",
            path.display(),
            crate::util::human_bytes(meta.len()),
            crate::util::human_bytes(LIMITE_LEITURA_BYTES)
        )));
    }
    let bytes = std::fs::read(path).map_err(|e| {
        ToolError::msg(format!(
            "não foi possível ler {}: {e}\nDica: verifique se o caminho existe e se você tem permissão de leitura.",
            path.display()
        ))
    })?;
    if parece_binario(&bytes) {
        return Err(ToolError::msg(format!(
            "{} parece ser um arquivo binário ({} bytes). Use inspect_file_metadata ou run_command para analisá-lo.",
            path.display(),
            bytes.len()
        )));
    }
    String::from_utf8(bytes).map_err(|e| {
        ToolError::msg(format!(
            "{} não está em UTF-8 válido: {e}. Considere converter o arquivo antes de editar.",
            path.display()
        ))
    })
}

/// Lê o conteúdo anterior de um arquivo que será substituído por inteiro.
///
/// Arquivos binários ou fora do UTF-8 não podem ser reescritos por
/// ferramentas de texto: tratá-los como vazio apagava o original e gravava um
/// diff falso (o `/undo` não teria como restaurá-lo).
fn ler_anterior_para_substituir(path: &Path) -> Result<String, ToolError> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if parece_binario(&bytes) {
                return Err(ToolError::msg(format!(
                    "{} parece ser um arquivo binário; as ferramentas de texto não podem reescrevê-lo. \
                     Use run_command para manipulá-lo.",
                    path.display()
                )));
            }
            String::from_utf8(bytes).map_err(|e| {
                ToolError::msg(format!(
                    "{} não está em UTF-8 válido: {e}. As ferramentas de texto não podem reescrevê-lo.",
                    path.display()
                ))
            })
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(e) => Err(ToolError::msg(format!(
            "não foi possível ler o conteúdo atual de {}: {e}",
            path.display()
        ))),
    }
}

/// Escrita atômica: grava em arquivo temporário e renomeia.
///
/// As permissões do arquivo existente são copiadas para o temporário antes do
/// `rename`: sem isso, reescrever um script/executável com `write_file` ou
/// `edit_file` trocava o modo para o padrão do umask e perdia o `+x`.
fn escrever_atomico(path: &Path, conteudo: &str) -> Result<(), ToolError> {
    let tmp = path.with_extension(format!(
        "{}kestrel-tmp",
        path.extension()
            .map(|e| format!("{}.", e.to_string_lossy()))
            .unwrap_or_default()
    ));
    let permissoes = std::fs::metadata(path).ok().map(|m| m.permissions());
    std::fs::write(&tmp, conteudo).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ToolError::msg(format!("falha ao gravar {}: {e}", tmp.display()))
    })?;
    if let Some(perms) = permissoes {
        // Falha em preservar o modo não deve impedir a gravação do conteúdo.
        let _ = std::fs::set_permissions(&tmp, perms);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ToolError::msg(format!("falha ao substituir {}: {e}", path.display()))
    })
}

/// Formata conteúdo com números de linha.
fn com_numeros(conteudo: &str, inicio: usize) -> String {
    conteudo
        .lines()
        .enumerate()
        .map(|(i, l)| format!("{:>6}\t{}", inicio + i, l))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Marcador gravado no lugar do diff quando ele é grande demais para o
/// `/undo` confiar no recorte. A definição fica em [`crate::diff`] para que o
/// reverso também saiba recusá-lo.
pub(crate) use crate::diff::DIFF_TRUNCADO;

/// Registra uma alteração no banco e devolve estatísticas reais do diff.
pub(crate) fn registrar_alteracao(
    ctx: &ToolContext,
    path: &Path,
    antes: &str,
    depois: &str,
    tipo: &str,
) -> (usize, usize) {
    let d = diff::diff(antes, depois);
    // O diff pode conter segredos (linha `api_key = ...`, token): o banco só
    // recebe a versão redigida.
    let texto = crate::util::redact_secrets(&d.text);
    let guardado: &str = if texto.chars().count() > 20_000 {
        DIFF_TRUNCADO
    } else {
        &texto
    };
    ctx.store
        .record_file_change(
            &ctx.session_id,
            &caminho_relativo(ctx, path),
            tipo,
            d.stats.additions,
            d.stats.deletions,
            Some(guardado),
        )
        .ok();
    ctx.arquivos.registrar(path, depois);
    (d.stats.additions, d.stats.deletions)
}

pub(crate) fn caminho_relativo(ctx: &ToolContext, path: &Path) -> String {
    crate::util::display_path(&ctx.workdir, path)
}

/// Devolve a linha com strings e comentários mascarados por espaços,
/// preservando o código (chaves e identificadores). Mantém o estado de
/// comentário de bloco entre linhas.
///
/// Sem isto, `localizar_bloco` contava chaves dentro de strings (`"}"`) e
/// encontrava identificadores em comentários, fechando o bloco cedo demais.
fn linha_sem_strings_e_comentarios(linha: &str, em_bloco: &mut bool) -> String {
    let chars: Vec<char> = linha.chars().collect();
    let mut saida = String::with_capacity(linha.len());
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if *em_bloco {
            if c == '*' && chars.get(i + 1) == Some(&'/') {
                *em_bloco = false;
                saida.push_str("  ");
                i += 2;
            } else {
                saida.push(' ');
                i += 1;
            }
            continue;
        }
        match c {
            '"' | '`' => {
                saida.push(' ');
                i += 1;
                while i < chars.len() {
                    let c2 = chars[i];
                    if c2 == '\\' {
                        saida.push_str("  ");
                        i += 2;
                        continue;
                    }
                    saida.push(' ');
                    i += 1;
                    if c2 == c {
                        break;
                    }
                }
            }
            '\'' => {
                // `'a'`/`'\n'` são literais de caractere; `'a` é lifetime em
                // Rust (não pode abrir "string" e mascarar o resto da linha).
                let fecha = matches!(
                    (chars.get(i + 1), chars.get(i + 2)),
                    (Some('\\'), _) | (Some(_), Some('\''))
                );
                if fecha {
                    saida.push(' ');
                    i += 1;
                    while i < chars.len() {
                        let c2 = chars[i];
                        if c2 == '\\' {
                            saida.push_str("  ");
                            i += 2;
                            continue;
                        }
                        saida.push(' ');
                        i += 1;
                        if c2 == '\'' {
                            break;
                        }
                    }
                } else {
                    saida.push('\'');
                    i += 1;
                }
            }
            '/' if chars.get(i + 1) == Some(&'/') => {
                saida.extend(std::iter::repeat_n(' ', chars.len() - i));
                i = chars.len();
            }
            '/' if chars.get(i + 1) == Some(&'*') => {
                *em_bloco = true;
                saida.push_str("  ");
                i += 2;
            }
            _ => {
                saida.push(c);
                i += 1;
            }
        }
    }
    saida
}

/// Localiza o bloco que contém `identificador` (função, classe, seção).
fn localizar_bloco(conteudo: &str, identificador: &str) -> Option<(usize, usize)> {
    let linhas: Vec<&str> = conteudo.lines().collect();
    let mut em_bloco = false;
    let mascaradas: Vec<String> = linhas
        .iter()
        .map(|l| linha_sem_strings_e_comentarios(l, &mut em_bloco))
        .collect();
    let inicio = mascaradas.iter().position(|l| l.contains(identificador))?;
    // Determina a indentação da linha de abertura (no texto original).
    let indentacao = linhas[inicio].len() - linhas[inicio].trim_start().len();
    let mut profundidade = 0i32;
    let mut fim = inicio;
    let mut encontrou_abertura = false;
    for (i, linha) in mascaradas.iter().enumerate().skip(inicio) {
        for ch in linha.chars() {
            match ch {
                '{' => {
                    profundidade += 1;
                    encontrou_abertura = true;
                }
                '}' => profundidade -= 1,
                _ => {}
            }
        }
        if encontrou_abertura && profundidade <= 0 {
            fim = i;
            break;
        }
        if !encontrou_abertura && i > inicio {
            // Estilo Python/TOML: o bloco termina na primeira linha não vazia
            // com indentação menor ou igual à do início.
            let linha_atual = linhas[i];
            if !linha_atual.trim().is_empty() {
                let indent_atual = linha_atual.len() - linha_atual.trim_start().len();
                if indent_atual <= indentacao {
                    fim = i - 1;
                    break;
                }
            }
            fim = i;
        }
    }
    Some((inicio, fim))
}

fn aplicar_trecho(
    conteudo: &str,
    alvo: &str,
    novo: &str,
    todas: bool,
) -> Result<String, ToolError> {
    if alvo.is_empty() {
        return Err(ToolError::Argumentos(
            "modo `trecho` exige um `alvo` não vazio".into(),
        ));
    }
    let ocorrencias = conteudo.matches(alvo).count();
    if ocorrencias == 0 {
        let dica = if conteudo.matches(alvo.trim()).count() > 0 {
            " O trecho existe sem espaços nas extremidades; ajuste o recorte para incluir a indentação exata."
        } else {
            " Releia o arquivo com read_file e copie o trecho exato (incluindo indentação)."
        };
        return Err(ToolError::msg(format!(
            "o trecho informado não foi encontrado no arquivo atual.{dica}"
        )));
    }
    if ocorrencias > 1 && !todas {
        return Err(ToolError::msg(format!(
            "o trecho aparece {ocorrencias} vezes no arquivo. Inclua mais contexto para torná-lo único ou use `todas: true` para substituir todas as ocorrências."
        )));
    }
    Ok(if todas {
        conteudo.replace(alvo, novo)
    } else {
        conteudo.replacen(alvo, novo, 1)
    })
}

fn substituir_linhas(
    conteudo: &str,
    inicio: usize,
    fim: usize,
    novo: &str,
) -> Result<String, ToolError> {
    let linhas: Vec<&str> = conteudo.lines().collect();
    if inicio == 0 || inicio > linhas.len() || fim < inicio || fim > linhas.len() {
        return Err(ToolError::msg(format!(
            "intervalo de linhas inválido: {inicio}..{fim} (o arquivo tem {} linhas)",
            linhas.len()
        )));
    }
    let mut resultado: Vec<String> = linhas[..inicio - 1].iter().map(|s| s.to_string()).collect();
    resultado.extend(novo.lines().map(|s| s.to_string()));
    resultado.extend(linhas[fim..].iter().map(|s| s.to_string()));
    let terminava_com_nova_linha = conteudo.ends_with('\n');
    let mut texto = resultado.join("\n");
    if terminava_com_nova_linha {
        texto.push('\n');
    }
    Ok(texto)
}

// ---------------------------------------------------------------- list_files

pub struct ListFiles;

#[async_trait::async_trait]
impl Tool for ListFiles {
    fn nome(&self) -> &'static str {
        "list_files"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Lista arquivos e diretórios dentro do projeto (respeita .gitignore). \
                Use `recursive` para árvores e `glob` para filtrar por extensão, por exemplo `*.rs`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Diretório inicial (padrão: raiz do projeto)"},
                    "recursive": {"type": "boolean", "description": "Percorre subdiretórios"},
                    "max_depth": {"type": "integer", "description": "Profundidade máxima"},
                    "glob": {"type": "string", "description": "Filtro simples por sufixo/nome, ex.: *.rs"},
                    "max_entries": {"type": "integer", "description": "Limite de entradas (padrão 300)"}
                }
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "path")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    fn detalhe(&self, args: &Value) -> String {
        let base = arg_str_opt(args, "path").unwrap_or_else(|| ".".into());
        if arg_bool(args, "recursive", false) {
            format!("{base} (recursivo)")
        } else {
            base
        }
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let base = arg_str_opt(&args, "path").unwrap_or_else(|| ".".into());
        let raiz = resolver_caminho(ctx, &base);
        if !raiz.exists() {
            return Err(ToolError::msg(format!(
                "o caminho {} não existe. Use list_files sem argumentos para ver a raiz do projeto.",
                raiz.display()
            )));
        }
        let recursivo = arg_bool(&args, "recursive", false);
        let max_entradas = arg_u64(&args, "max_entries").unwrap_or(300).max(1) as usize;
        let glob = arg_str_opt(&args, "glob");
        let max_depth = arg_u64(&args, "max_depth").map(|d| d as usize);

        let mut construtor = ignore::WalkBuilder::new(&raiz);
        construtor
            .hidden(false)
            .git_ignore(true)
            .git_global(false)
            .max_depth(Some(if recursivo {
                max_depth.unwrap_or(12)
            } else {
                1
            }));
        if !recursivo {
            construtor.max_depth(Some(1));
        }

        let mut linhas: Vec<String> = Vec::new();
        let mut total = 0usize;
        for entrada in construtor.build().flatten() {
            if entrada.path() == raiz {
                continue;
            }
            let nome_rel = crate::util::display_path(&raiz, entrada.path());
            if let Some(filtro) = &glob {
                let alvo = filtro.trim_start_matches('*').trim_start_matches('.');
                if !nome_rel.ends_with(alvo) {
                    continue;
                }
            }
            if entrada.file_type().is_some_and(|t| t.is_dir()) {
                linhas.push(format!("{nome_rel}/"));
            } else {
                let tamanho = entrada
                    .metadata()
                    .map(|m| crate::util::human_bytes(m.len()))
                    .unwrap_or_default();
                linhas.push(format!("{nome_rel}  ({tamanho})"));
            }
            total += 1;
            if total >= max_entradas {
                linhas.push(format!("… limite de {max_entradas} entradas atingido"));
                break;
            }
        }
        linhas.sort();
        if linhas.is_empty() {
            return Ok(ToolOutput::texto(format!(
                "Nenhum arquivo encontrado em {} (filtro: {:?}).",
                raiz.display(),
                glob
            )));
        }
        Ok(ToolOutput::texto(format!(
            "{} entradas em {}:\n{}",
            linhas.len(),
            raiz.display(),
            linhas.join("\n")
        )))
    }
}

// ----------------------------------------------------------------- read_file

pub struct ReadFile;

#[async_trait::async_trait]
impl Tool for ReadFile {
    fn nome(&self) -> &'static str {
        "read_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Lê um arquivo de texto com números de linha. Aceita intervalos \
                (start_line/end_line) e leitura progressiva com max_lines. Sempre use esta \
                ferramenta antes de editar para confirmar o conteúdo atual."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Caminho do arquivo"},
                    "start_line": {"type": "integer", "description": "Primeira linha (1-based)"},
                    "end_line": {"type": "integer", "description": "Última linha (inclusive)"},
                    "max_lines": {"type": "integer", "description": "Máximo de linhas retornadas (padrão 400)"},
                    "sem_numeros": {"type": "boolean", "description": "Omitir números de linha"}
                },
                "required": ["path"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "path")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    fn detalhe(&self, args: &Value) -> String {
        let p = arg_str_opt(args, "path").unwrap_or_default();
        match (arg_u64(args, "start_line"), arg_u64(args, "end_line")) {
            (Some(i), Some(f)) => format!("{p} (linhas {i}-{f})"),
            (Some(i), None) => format!("{p} (a partir de {i})"),
            _ => p,
        }
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let caminho = resolver_caminho(ctx, &arg_str(&args, "path")?);
        if caminho.is_dir() {
            return Err(ToolError::msg(format!(
                "{} é um diretório. Use list_files.",
                caminho.display()
            )));
        }
        let conteudo = ler_texto(&caminho)?;
        // A comparação precisa acontecer *antes* de registrar o conteúdo lido:
        // depois, o hash registrado já é o atual e o aviso nunca aparecia.
        let externo = ctx.arquivos.mudou_desde_registro(&caminho, &conteudo);
        ctx.arquivos.registrar(&caminho, &conteudo);
        let linhas: Vec<&str> = conteudo.lines().collect();
        let total = linhas.len();
        let inicio = arg_u64(&args, "start_line").unwrap_or(1).max(1) as usize;
        let max_lines = arg_u64(&args, "max_lines").unwrap_or(400) as usize;
        if max_lines == 0 {
            return Err(ToolError::Argumentos(
                "max_lines precisa ser maior que zero".into(),
            ));
        }
        let fim = arg_u64(&args, "end_line")
            .map(|f| f as usize)
            // `saturating_*`: max_lines enorme não pode estourar o usize.
            .unwrap_or(inicio.saturating_add(max_lines).saturating_sub(1))
            .min(total.max(inicio));
        let sem_numeros = arg_bool(&args, "sem_numeros", false);
        if inicio > total {
            // Arquivo vazio com o início padrão não é erro: é uma leitura sem
            // linhas. (Antes respondia "start_line=1 além do fim".)
            if total == 0 && inicio == 1 {
                let aviso = if externo {
                    "\n[atenção: o arquivo mudou desde a última interação do Kestrel com ele]"
                } else {
                    ""
                };
                return Ok(ToolOutput::texto(aviso).com_resumo("0 linhas lidas de 0"));
            }
            return Err(ToolError::msg(format!(
                "start_line={inicio} além do fim do arquivo ({total} linhas)"
            )));
        }
        // `end_line` menor que `start_line` não é um recorte: sem esta checagem
        // o fatiamento abaixo panicaria e derrubaria a task do turno.
        if fim < inicio {
            return Err(ToolError::msg(format!(
                "end_line={fim} é anterior a start_line={inicio}; informe um intervalo crescente"
            )));
        }
        let mostradas = fim.min(total).saturating_sub(inicio) + 1;
        let recorte = linhas[inicio - 1..fim.min(total)].join("\n");
        let corpo = if sem_numeros {
            recorte
        } else {
            com_numeros(&recorte, inicio)
        };
        let aviso = if fim < total {
            format!(
                "\n\n[mostrando linhas {inicio}-{fim} de {total} totais; use start_line/end_line para continuar]"
            )
        } else {
            String::new()
        };
        let externo = if externo {
            "\n[atenção: o arquivo mudou desde a última interação do Kestrel com ele]"
        } else {
            ""
        };
        Ok(ToolOutput::texto(format!("{}{}{}", corpo, aviso, externo))
            .com_resumo(format!("{} linhas lidas de {total}", mostradas)))
    }
}

// -------------------------------------------------------- inspect_file_metadata

pub struct InspectFileMetadata;

fn linguagem_por_extensao(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "rs" => "Rust",
        "toml" => "TOML",
        "json" => "JSON",
        "jsonc" => "JSON with comments",
        "js" | "mjs" | "cjs" => "JavaScript",
        "ts" | "tsx" => "TypeScript",
        "py" => "Python",
        "go" => "Go",
        "java" => "Java",
        "kt" | "kts" => "Kotlin",
        "rb" => "Ruby",
        "php" => "PHP",
        "c" | "h" => "C",
        "cpp" | "cc" | "hpp" | "hxx" => "C++",
        "cs" => "C#",
        "sh" | "bash" | "zsh" => "Shell",
        "sql" => "SQL",
        "md" => "Markdown",
        "yaml" | "yml" => "YAML",
        "html" | "htm" => "HTML",
        "css" | "scss" => "CSS",
        "xml" => "XML",
        _ => "desconhecida",
    }
}

#[async_trait::async_trait]
impl Tool for InspectFileMetadata {
    fn nome(&self) -> &'static str {
        "inspect_file_metadata"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Mostra metadados de um arquivo ou diretório: tamanho, datas, \
                permissões, número de linhas e linguagem detectada."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "Caminho do arquivo"}},
                "required": ["path"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "path")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let caminho = resolver_caminho(ctx, &arg_str(&args, "path")?);
        let meta = std::fs::metadata(&caminho).map_err(|e| {
            ToolError::msg(format!(
                "não foi possível inspecionar {}: {e}",
                caminho.display()
            ))
        })?;
        let mut linhas = vec![
            format!("caminho: {}", caminho.display()),
            format!(
                "tipo: {}",
                if meta.is_dir() {
                    "diretório"
                } else {
                    "arquivo"
                }
            ),
            format!("tamanho: {}", crate::util::human_bytes(meta.len())),
        ];
        if let Ok(modificado) = meta.modified() {
            let dt: chrono::DateTime<chrono::Local> = modificado.into();
            linhas.push(format!("modificado em: {}", dt.format("%d/%m/%Y %H:%M:%S")));
        }
        if let Ok(criado) = meta.created() {
            let dt: chrono::DateTime<chrono::Local> = criado.into();
            linhas.push(format!("criado em: {}", dt.format("%d/%m/%Y %H:%M:%S")));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            linhas.push(format!(
                "permissões: {:o}",
                meta.permissions().mode() & 0o777
            ));
        }
        linhas.push(format!(
            "somente leitura: {}",
            if meta.permissions().readonly() {
                "sim"
            } else {
                "não"
            }
        ));
        if meta.is_file() {
            linhas.push(format!(
                "linguagem detectada: {}",
                linguagem_por_extensao(&caminho)
            ));
            // Conta linhas em streaming: ler o arquivo inteiro só para exibir
            // metadados estourava a memória em arquivos grandes.
            if let Ok(mut arquivo) = std::fs::File::open(&caminho) {
                use std::io::Read;
                let mut buffer = vec![0u8; 64 * 1024];
                let mut total_quebras = 0usize;
                let mut ultimo = 0u8;
                let mut tem_bytes = false;
                let mut binario = false;
                loop {
                    match arquivo.read(&mut buffer) {
                        Ok(0) => break,
                        Ok(n) => {
                            let pedaco = &buffer[..n];
                            if !tem_bytes && parece_binario(pedaco) {
                                binario = true;
                                break;
                            }
                            tem_bytes = true;
                            total_quebras += pedaco.iter().filter(|b| **b == b'\n').count();
                            ultimo = pedaco[n - 1];
                        }
                        Err(_) => break,
                    }
                }
                if binario {
                    linhas.push("conteúdo: binário".into());
                } else {
                    let total = total_quebras + usize::from(tem_bytes && ultimo != b'\n');
                    linhas.push(format!("linhas: {total}"));
                }
            }
        }
        Ok(ToolOutput::texto(linhas.join("\n")))
    }
}

// ----------------------------------------------------------------- write_file

pub struct WriteFile;

#[async_trait::async_trait]
impl Tool for WriteFile {
    fn nome(&self) -> &'static str {
        "write_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Substitui integralmente o conteúdo de um arquivo existente \
                (ou cria, se `create_dirs` for true). Prefira edit_file para alterações pontuais."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string", "description": "Novo conteúdo completo"},
                    "create_dirs": {"type": "boolean", "description": "Cria diretórios intermediários"}
                },
                "required": ["path", "content"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "path")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let caminho = resolver_caminho(ctx, &arg_str(&args, "path")?);
        let conteudo = arg_str(&args, "content")?;
        let _lock = ctx.arquivos.lock_de(&caminho);
        let guarda = _lock.lock().await;

        let anterior = ler_anterior_para_substituir(&caminho)?;
        let existia = caminho.exists();
        if !existia
            && !arg_bool(&args, "create_dirs", false)
            && let Some(pai) = caminho.parent()
            && !pai.exists()
        {
            return Err(ToolError::msg(format!(
                "o diretório {} não existe. Use create_file com create_dirs=true ou crie o diretório antes.",
                pai.display()
            )));
        }
        if arg_bool(&args, "create_dirs", false)
            && let Some(pai) = caminho.parent()
        {
            std::fs::create_dir_all(pai)
                .map_err(|e| ToolError::msg(format!("falha ao criar {}: {e}", pai.display())))?;
        }
        let externo = existia && ctx.arquivos.mudou_desde_registro(&caminho, &anterior);
        escrever_atomico(&caminho, &conteudo)?;
        let (adicoes, remocoes) = registrar_alteracao(
            ctx,
            &caminho,
            &anterior,
            &conteudo,
            if existia { "reescrita" } else { "criacao" },
        );
        drop(guarda);
        let aviso = if externo {
            "\nAtenção: o arquivo havia sido alterado por outro processo desde a última leitura; a reescrita substituiu o conteúdo atual. Confirme o resultado com read_file."
        } else {
            ""
        };
        Ok(ToolOutput::texto(format!(
            "Arquivo {} gravado ({} linhas, +{adicoes} -{remocoes}).{aviso}",
            caminho.display(),
            crate::util::count_lines(&conteudo)
        ))
        .com_resumo(format!(
            "{} arquivo {}",
            if existia { "reescreveu" } else { "criou" },
            caminho_relativo(ctx, &caminho)
        ))
        .com_diff(adicoes, remocoes))
    }
}

// ---------------------------------------------------------------- create_file

pub struct CreateFile;

#[async_trait::async_trait]
impl Tool for CreateFile {
    fn nome(&self) -> &'static str {
        "create_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Cria um arquivo novo (com diretórios intermediários). Falha se o \
                arquivo já existir, a menos que `sobrescrever` seja true."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string", "description": "Conteúdo inicial (padrão vazio)"},
                    "sobrescrever": {"type": "boolean", "description": "Permite sobrescrever um arquivo existente"}
                },
                "required": ["path"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "path")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let caminho = resolver_caminho(ctx, &arg_str(&args, "path")?);
        let conteudo = arg_str_opt(&args, "content").unwrap_or_default();
        let lock = ctx.arquivos.lock_de(&caminho);
        let _guarda = lock.lock().await;
        if caminho.exists() && !arg_bool(&args, "sobrescrever", false) {
            return Err(ToolError::msg(format!(
                "o arquivo {} já existe. Use edit_file para alterá-lo ou passe sobrescrever=true.",
                caminho.display()
            )));
        }
        let anterior = ler_anterior_para_substituir(&caminho)?;
        if let Some(pai) = caminho.parent() {
            std::fs::create_dir_all(pai)
                .map_err(|e| ToolError::msg(format!("falha ao criar {}: {e}", pai.display())))?;
        }
        escrever_atomico(&caminho, &conteudo)?;
        let (adicoes, remocoes) =
            registrar_alteracao(ctx, &caminho, &anterior, &conteudo, "criacao");
        Ok(ToolOutput::texto(format!(
            "Arquivo criado: {} ({} linhas, +{adicoes} -{remocoes})",
            caminho.display(),
            crate::util::count_lines(&conteudo)
        ))
        .com_resumo(format!("criou {}", caminho_relativo(ctx, &caminho)))
        .com_diff(adicoes, remocoes))
    }
}

// ------------------------------------------------------------------ edit_file

pub struct EditFile;

#[async_trait::async_trait]
impl Tool for EditFile {
    fn nome(&self) -> &'static str {
        "edit_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Edita um arquivo existente com precisão. Modos: \
                `trecho` (substitui um trecho exato por outro), \
                `linhas` (substitui um intervalo de linhas), \
                `bloco` (substitui a função/bloco que contém um identificador) e \
                `integral` (substitui todo o conteúdo). \
                O contexto é sempre verificado contra o arquivo atual antes de aplicar."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "mode": {"type": "string", "description": "trecho | linhas | bloco | integral"},
                    "alvo": {"type": "string", "description": "modo trecho: texto exato a localizar"},
                    "novo": {"type": "string", "description": "texto de substituição"},
                    "todas": {"type": "boolean", "description": "modo trecho: substituir todas as ocorrências"},
                    "linha_inicial": {"type": "integer", "description": "modo linhas"},
                    "linha_final": {"type": "integer", "description": "modo linhas"},
                    "identificador": {"type": "string", "description": "modo bloco: trecho que identifica a abertura do bloco, ex. `fn processar`"}
                },
                "required": ["path", "mode"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "path")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    fn detalhe(&self, args: &Value) -> String {
        let p = arg_str_opt(args, "path").unwrap_or_default();
        let m = arg_str_opt(args, "mode").unwrap_or_else(|| "trecho".into());
        format!("{p} ({m})")
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let caminho = resolver_caminho(ctx, &arg_str(&args, "path")?);
        let modo = arg_str(&args, "mode")?.to_lowercase();
        let lock = ctx.arquivos.lock_de(&caminho);
        let _guarda = lock.lock().await;

        if !caminho.is_file() {
            return Err(ToolError::msg(format!(
                "{} não existe ou não é um arquivo. Use read_file/list_files para confirmar o caminho.",
                caminho.display()
            )));
        }
        // O conteúdo é relido agora: qualquer alteração externa é considerada.
        let atual = ler_texto(&caminho)?;
        let externo = ctx.arquivos.mudou_desde_registro(&caminho, &atual);

        let novo_conteudo = match modo.as_str() {
            "trecho" => {
                let alvo = arg_str(&args, "alvo")?;
                let novo = arg_str(&args, "novo")?;
                aplicar_trecho(&atual, &alvo, &novo, arg_bool(&args, "todas", false))?
            }
            "linhas" => {
                let inicio = arg_u64(&args, "linha_inicial").ok_or_else(|| {
                    ToolError::Argumentos("modo `linhas` exige `linha_inicial`".into())
                })? as usize;
                let fim = arg_u64(&args, "linha_final").unwrap_or(inicio as u64) as usize;
                let novo = arg_str(&args, "novo")?;
                substituir_linhas(&atual, inicio, fim, &novo)?
            }
            "bloco" => {
                let identificador = arg_str(&args, "identificador")?;
                if identificador.trim().is_empty() {
                    return Err(ToolError::Argumentos(
                        "modo `bloco` exige um `identificador` não vazio".into(),
                    ));
                }
                let novo = arg_str(&args, "novo")?;
                let (inicio, fim) = localizar_bloco(&atual, &identificador).ok_or_else(|| {
                    ToolError::msg(format!(
                        "não encontrei `{identificador}` no arquivo atual. Releia o arquivo para localizar o alvo."
                    ))
                })?;
                substituir_linhas(&atual, inicio + 1, fim + 1, &novo)?
            }
            "integral" => arg_str(&args, "novo")?,
            outro => {
                return Err(ToolError::Argumentos(format!(
                    "modo `{outro}` desconhecido. Use trecho, linhas, bloco ou integral."
                )));
            }
        };

        if novo_conteudo == atual {
            return Ok(ToolOutput::texto(
                "Nenhuma alteração necessária: o conteúdo resultante é idêntico ao atual.",
            ));
        }
        escrever_atomico(&caminho, &novo_conteudo)?;
        let (adicoes, remocoes) = registrar_alteracao(
            ctx,
            &caminho,
            &atual,
            &novo_conteudo,
            &format!("edicao:{modo}"),
        );
        let aviso = if externo {
            "\nAtenção: o arquivo tinha alterações externas desde a última leitura. O contexto foi revalidado contra o conteúdo atual; confira o resultado."
        } else {
            ""
        };
        Ok(ToolOutput::texto(format!(
            "Editado {} (modo {modo}): +{adicoes} -{remocoes} linhas.{aviso}",
            caminho.display()
        ))
        .com_resumo(format!(
            "{} +{adicoes} -{remocoes}",
            caminho_relativo(ctx, &caminho)
        ))
        .com_diff(adicoes, remocoes))
    }
}

// ---------------------------------------------------------------- delete_file

pub struct DeleteFile;

#[async_trait::async_trait]
impl Tool for DeleteFile {
    fn nome(&self) -> &'static str {
        "delete_file"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Remove um arquivo ou diretório. Operação destrutiva: respeita o nível \
                de permissão e pede confirmação quando necessário. Diretórios exigem `recursive: true`."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "recursive": {"type": "boolean", "description": "Permite remover diretórios com conteúdo"}
                },
                "required": ["path"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        arg_str_opt(args, "path")
            .map(|p| vec![PathBuf::from(p)])
            .unwrap_or_default()
    }

    fn detalhe(&self, args: &Value) -> String {
        format!("remover {}", arg_str_opt(args, "path").unwrap_or_default())
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let entrada = arg_str(&args, "path")?;
        // O caminho literal preserva o symlink final: `resolver_caminho`
        // canonicaliza e faria o `remove_file` apagar o **alvo** do link.
        let literal = {
            let bruto = crate::util::expand_tilde(entrada.trim());
            let absoluto = if bruto.is_absolute() {
                bruto
            } else {
                ctx.workdir.join(bruto)
            };
            // Canonicaliza só o **diretório pai**: o último componente pode ser
            // um symlink e deve ser removido como link, não como alvo. Fazer
            // normalização lexical aqui resolveria `link/../x` errado e poderia
            // apagar outro arquivo.
            match (absoluto.parent(), absoluto.file_name()) {
                (Some(pai), Some(nome)) => std::fs::canonicalize(pai)
                    .unwrap_or_else(|_| crate::util::normalize_path(pai))
                    .join(nome),
                _ => crate::util::normalize_path(&absoluto),
            }
        };
        let caminho = resolver_caminho(ctx, &entrada);
        if caminho == ctx.workdir
            || ctx.roots.contains(&caminho)
            || literal == ctx.workdir
            || ctx.roots.contains(&literal)
            // Remover um ancestral apaga o projeto junto: `delete_file ..`
            // passava porque só a igualdade com a raiz era comparada.
            || ctx
                .roots
                .iter()
                .any(|r| r.starts_with(&caminho) || r.starts_with(&literal))
            || ctx.workdir.starts_with(&caminho)
            || ctx.workdir.starts_with(&literal)
        {
            return Err(ToolError::msg(
                "recusando remover a raiz do projeto, do espaço de trabalho ou um ancestral dela",
            ));
        }
        let lock = ctx.arquivos.lock_de(&literal);
        let _guarda = lock.lock().await;
        let meta = std::fs::symlink_metadata(&literal).map_err(|e| {
            ToolError::msg(format!(
                "não foi possível remover {}: {e}",
                literal.display()
            ))
        })?;
        let eh_link = meta.file_type().is_symlink();
        let alvo = if eh_link { &literal } else { &caminho };
        let anterior = if meta.file_type().is_file() {
            std::fs::read_to_string(alvo).unwrap_or_default()
        } else {
            String::new()
        };
        if meta.is_dir() {
            if !arg_bool(&args, "recursive", false) {
                return Err(ToolError::msg(format!(
                    "{} é um diretório; use recursive=true para removê-lo com conteúdo.",
                    caminho.display()
                )));
            }
            std::fs::remove_dir_all(alvo)
                .map_err(|e| ToolError::msg(format!("falha ao remover {}: {e}", alvo.display())))?;
        } else {
            std::fs::remove_file(alvo)
                .map_err(|e| ToolError::msg(format!("falha ao remover {}: {e}", alvo.display())))?;
        }
        let remocoes = if meta.file_type().is_file() {
            crate::util::count_lines(&anterior)
        } else {
            0
        };
        ctx.store
            .record_file_change(
                &ctx.session_id,
                &caminho_relativo(ctx, &caminho),
                "remocao",
                0,
                remocoes,
                None,
            )
            .ok();
        Ok(
            ToolOutput::texto(format!("Removido: {}", caminho.display()))
                .com_resumo(format!("removeu {}", caminho_relativo(ctx, &caminho)))
                .com_diff(0, remocoes),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aplica_trecho_exige_unicidade() {
        let base = "a\nb\na\n";
        assert!(aplicar_trecho(base, "a", "x", false).is_err());
        assert_eq!(aplicar_trecho(base, "a", "x", true).unwrap(), "x\nb\nx\n");
        assert!(aplicar_trecho(base, "z", "x", false).is_err());
    }

    #[test]
    fn substitui_intervalo_de_linhas() {
        let base = "1\n2\n3\n4\n";
        let novo = substituir_linhas(base, 2, 3, "dois\ntres\n").unwrap();
        assert_eq!(novo, "1\ndois\ntres\n4\n");
        assert!(substituir_linhas(base, 0, 1, "x").is_err());
        assert!(substituir_linhas(base, 3, 99, "x").is_err());
    }

    #[test]
    fn localiza_bloco_em_rust() {
        let codigo = "fn antes() {}\n\nfn processar(x: u32) -> u32 {\n    let y = x;\n    y + 1\n}\n\nfn depois() {}\n";
        let (i, f) = localizar_bloco(codigo, "fn processar").unwrap();
        assert_eq!(i, 2);
        assert_eq!(f, 5);
    }

    #[test]
    fn localiza_bloco_com_indentacao_python() {
        let codigo = "def a():\n    pass\n\ndef b():\n    x = 1\n    return x\n";
        let (i, f) = localizar_bloco(codigo, "def b").unwrap();
        assert_eq!(i, 3);
        assert_eq!(f, 5);
    }

    #[test]
    fn numeros_de_linha_sao_1_based() {
        let texto = com_numeros("a\nb", 1);
        assert!(texto.starts_with("     1\ta"));
    }

    /// O recorte de `read_file` precisa validar o intervalo antes de fatiar:
    /// `end_line < start_line` panicava ("slice index starts at N but ends at
    /// M") e ainda subtraria com estouro no resumo — o pânico derrubava a task
    /// do turno e travava a sessão.
    #[tokio::test]
    async fn leitura_com_intervalo_invertido_devolve_erro_em_vez_de_panicar() {
        use crate::tools::Tool;
        let dir = std::env::temp_dir().join(format!("kestrel-fs-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("numeros.txt");
        std::fs::write(&alvo, "1\n2\n3\n4\n5\n").unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        let ferramenta = ReadFile;
        for (inicio, fim) in [(3usize, 2usize), (3, 1), (2, 1), (100, 3)] {
            let args = serde_json::json!({
                "path": alvo.display().to_string(),
                "start_line": inicio,
                "end_line": fim,
            });
            let saida = ferramenta.executar(args, &ctx).await;
            assert!(
                saida.is_err(),
                "intervalo {inicio}-{fim} deveria ser recusado"
            );
        }
        // O intervalo normal continua funcionando.
        let args = serde_json::json!({
            "path": alvo.display().to_string(),
            "start_line": 2,
            "end_line": 4,
        });
        let saida = ferramenta.executar(args, &ctx).await.unwrap();
        assert!(saida.conteudo.contains("2"), "{}", saida.conteudo);
        assert!(saida.resumo.contains("3 linhas"), "{}", saida.resumo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn detecta_binario() {
        assert!(parece_binario(&[0x00, 0x01, 0x02]));
        assert!(!parece_binario(b"texto normal"));
    }

    #[test]
    fn linguagem_por_extensao_funciona() {
        assert_eq!(linguagem_por_extensao(Path::new("a/b.rs")), "Rust");
        assert_eq!(linguagem_por_extensao(Path::new("x.py")), "Python");
        assert_eq!(
            linguagem_por_extensao(Path::new("x.semextensao")),
            "desconhecida"
        );
    }

    /// Chaves dentro de strings e identificadores em comentários confundiam a
    /// localização do bloco (fechava cedo demais ou abria no lugar errado).
    #[test]
    fn localiza_bloco_ignora_strings_e_comentarios() {
        let codigo = "fn f() {\n    let s = \"}\";\n    let t = 1;\n}\n";
        assert_eq!(localizar_bloco(codigo, "fn f").unwrap(), (0, 3));

        let codigo = "// fn processar aparece aqui\nfn processar() {\n    x();\n}\n";
        assert_eq!(localizar_bloco(codigo, "fn processar").unwrap(), (1, 3));

        // Lifetimes em Rust não podem mascarar o resto da linha.
        let codigo = "fn f<'a>(x: &'a str) -> &'a str {\n    x\n}\n";
        assert_eq!(localizar_bloco(codigo, "fn f").unwrap(), (0, 2));
    }

    #[cfg(unix)]
    #[test]
    fn escrita_preserva_modo_do_executavel() {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("kestrel-fs-mode-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("script.sh");
        std::fs::write(&alvo, "#!/bin/sh\necho oi\n").unwrap();
        std::fs::set_permissions(&alvo, std::fs::Permissions::from_mode(0o755)).unwrap();
        escrever_atomico(&alvo, "#!/bin/sh\necho tchau\n").unwrap();
        let modo = std::fs::metadata(&alvo).unwrap().permissions().mode() & 0o777;
        assert_eq!(modo, 0o755, "o bit de execução foi perdido");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Substituir um binário por texto não pode tratar o conteúdo anterior
    /// como vazio: isso apagava o original e gravava um diff falso.
    #[tokio::test]
    async fn write_file_recusa_sobrescrever_binario() {
        use crate::tools::Tool;
        let dir = std::env::temp_dir().join(format!("kestrel-fs-bin-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("binario.dat");
        let original = b"inicio\0fim";
        std::fs::write(&alvo, original).unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        let erro = WriteFile
            .executar(
                serde_json::json!({"path": "binario.dat", "content": "texto"}),
                &ctx,
            )
            .await
            .unwrap_err();
        assert!(erro.to_string().contains("binário"), "{erro}");
        assert_eq!(
            std::fs::read(&alvo).unwrap(),
            original,
            "o binário original foi alterado"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn leitura_recusa_arquivo_gigante() {
        let dir = std::env::temp_dir().join(format!("kestrel-fs-big-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("grande.log");
        let arquivo = std::fs::File::create(&alvo).unwrap();
        arquivo.set_len(LIMITE_LEITURA_BYTES + 1).unwrap();
        let erro = ler_texto(&alvo).unwrap_err().to_string();
        assert!(erro.contains("excede o limite"), "{erro}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `delete_file` num symlink precisa remover o link, não o alvo.
    #[cfg(unix)]
    #[tokio::test]
    async fn delete_file_remove_o_link_nao_o_alvo() {
        use crate::tools::Tool;
        let dir = std::env::temp_dir().join(format!("kestrel-fs-link-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("alvo.txt");
        std::fs::write(&alvo, "conteudo").unwrap();
        let link = dir.join("link.txt");
        std::os::unix::fs::symlink(&alvo, &link).unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        DeleteFile
            .executar(
                serde_json::json!({"path": link.display().to_string()}),
                &ctx,
            )
            .await
            .unwrap();
        assert!(
            std::fs::symlink_metadata(&link).is_err(),
            "o link deveria sumir"
        );
        assert!(alvo.exists(), "o alvo não pode ser removido");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Diff maior que o teto não pode ser guardado truncado: o `/undo`
    /// aplicaria um patch parcial e corromperia o arquivo.
    #[tokio::test]
    async fn diff_grande_nao_e_guardado_truncado() {
        let dir = std::env::temp_dir().join(format!("kestrel-fs-diff-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        let depois: String = (0..3000)
            .map(|i| format!("linha numero {i} com algum texto\n"))
            .collect();
        registrar_alteracao(&ctx, &dir.join("x.txt"), "", &depois, "criacao");
        let alteracao = ctx
            .store
            .ultima_alteracao(&ctx.session_id)
            .unwrap()
            .unwrap();
        assert_eq!(alteracao.diff, DIFF_TRUNCADO);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// O aviso de alteração externa precisa ser calculado antes de registrar o
    /// conteúdo lido; registrando primeiro, o hash sempre batia e o aviso
    /// nunca aparecia.
    #[tokio::test]
    async fn read_file_avisa_mudanca_externa() {
        use crate::tools::Tool;
        let dir = std::env::temp_dir().join(format!("kestrel-fs-ext-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("a.txt");
        std::fs::write(&alvo, "antes\n").unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        let args = serde_json::json!({"path": alvo.display().to_string()});
        ReadFile.executar(args.clone(), &ctx).await.unwrap();
        std::fs::write(&alvo, "depois\n").unwrap();
        let saida = ReadFile.executar(args, &ctx).await.unwrap();
        assert!(
            saida.conteudo.contains("mudou desde a última interação"),
            "o aviso de mudança externa não apareceu: {}",
            saida.conteudo
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `max_lines` zero era aceito e gerava intervalo invertido; um valor
    /// enorme podia estourar a soma de `usize`.
    #[tokio::test]
    async fn read_file_valida_max_lines() {
        use crate::tools::Tool;
        let dir = std::env::temp_dir().join(format!("kestrel-fs-max-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("a.txt");
        std::fs::write(&alvo, "1\n2\n3\n").unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        let zero = serde_json::json!({
            "path": alvo.display().to_string(),
            "max_lines": 0,
        });
        assert!(ReadFile.executar(zero, &ctx).await.is_err());
        let enorme = serde_json::json!({
            "path": alvo.display().to_string(),
            "max_lines": u64::MAX,
        });
        let saida = ReadFile.executar(enorme, &ctx).await.unwrap();
        assert!(saida.conteudo.contains("\t3"), "{}", saida.conteudo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Arquivo vazio com o início padrão é leitura válida de zero linhas.
    #[tokio::test]
    async fn read_file_de_arquivo_vazio_nao_e_erro() {
        use crate::tools::Tool;
        let dir = std::env::temp_dir().join(format!("kestrel-fs-vazio-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("vazio.txt");
        std::fs::write(&alvo, "").unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        let args = serde_json::json!({"path": alvo.display().to_string()});
        let saida = ReadFile.executar(args, &ctx).await.unwrap();
        assert_eq!(saida.conteudo, "");
        assert!(saida.resumo.contains("0 linhas"), "{}", saida.resumo);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `alvo` vazio fazia `str::replace("")` inserir o texto entre todos os
    /// caracteres, corrompendo o arquivo.
    #[test]
    fn trecho_vazio_e_recusado() {
        assert!(aplicar_trecho("abc", "", "x", true).is_err());
        assert!(aplicar_trecho("abc", "", "x", false).is_err());
    }

    /// `identificador` vazio localizava a primeira linha e substituía um bloco
    /// arbitrário.
    #[tokio::test]
    async fn modo_bloco_exige_identificador() {
        use crate::tools::Tool;
        let dir = std::env::temp_dir().join(format!("kestrel-fs-bloco-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let alvo = dir.join("a.rs");
        std::fs::write(&alvo, "fn a() {}\n").unwrap();
        let ctx = crate::tools::git::tests::contexto(&dir);
        let args = serde_json::json!({
            "path": alvo.display().to_string(),
            "mode": "bloco",
            "identificador": "  ",
            "novo": "x",
        });
        let erro = EditFile.executar(args, &ctx).await.unwrap_err();
        assert!(erro.to_string().contains("identificador"), "{erro}");
        assert_eq!(std::fs::read_to_string(&alvo).unwrap(), "fn a() {}\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Remover um ancestral apagaria o projeto inteiro; só a igualdade com a
    /// raiz era recusada.
    #[tokio::test]
    async fn delete_file_recusa_ancestral_do_projeto() {
        use crate::tools::Tool;
        let base = std::env::temp_dir().join(format!("kestrel-fs-pai-{}", uuid::Uuid::new_v4()));
        let projeto = base.join("projeto");
        std::fs::create_dir_all(&projeto).unwrap();
        let ctx = crate::tools::git::tests::contexto(&projeto);
        let args = serde_json::json!({"path": "..", "recursive": true});
        let erro = DeleteFile.executar(args, &ctx).await.unwrap_err();
        assert!(erro.to_string().contains("ancestral"), "{erro}");
        assert!(projeto.exists(), "o projeto não pode ser removido");
        assert!(base.exists(), "o ancestral não pode ser removido");
        let _ = std::fs::remove_dir_all(&base);
    }
}
