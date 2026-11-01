//! `apply_patch`: aplica um patch textual com validação de contexto.
//!
//! Aceita o formato nativo do Kestrel/OpenAI:
//!
//! ```text
//! *** Begin Patch
//! *** Update File: src/main.rs
//! @@
//!  contexto
//! -antigo
//! +novo
//! *** Add File: src/novo.rs
//! +conteúdo
//! *** Delete File: src/velho.rs
//! *** End Patch
//! ```
//!
//! ou um diff unificado com hunks `@@` (quando `path` é informado). O contexto
//! de cada hunk é conferido linha a linha; nada é escrito se algum hunk falhar.

use super::{Tool, ToolContext, ToolError, ToolOutput, arg_str, arg_str_opt, resolver_caminho};
use crate::provider::ToolSpec;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Clone, Debug)]
enum TipoPatch {
    Update(Vec<Hunk>),
    Add(String),
    Delete,
}

#[derive(Clone, Debug)]
struct AlvoPatch {
    caminho: String,
    tipo: TipoPatch,
}

/// Linhas de um hunk: ` ` contexto, `-` remoção, `+` adição.
type Hunk = Vec<(char, String)>;

pub struct ApplyPatch;

#[async_trait::async_trait]
impl Tool for ApplyPatch {
    fn nome(&self) -> &'static str {
        "apply_patch"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Aplica um patch em um ou vários arquivos. Use `*** Begin Patch` com \
                `*** Update File: caminho` e hunks (` ` contexto, `-` remover, `+` adicionar), \
                `*** Add File:` e `*** Delete File:`. Também aceita diff unificado em `patch` \
                junto de `path`. O contexto é validado antes de escrever; se algo não bater, \
                nenhum arquivo é alterado."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "patch": {"type": "string", "description": "Patch no formato *** Begin Patch ou diff unificado"},
                    "path": {"type": "string", "description": "Arquivo alvo para diff unificado simples"}
                },
                "required": ["patch"]
            }),
        }
    }

    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        let patch = arg_str_opt(args, "patch").unwrap_or_default();
        let mut caminhos = caminhos_do_patch(&patch);
        if caminhos.is_empty()
            && let Some(path) = arg_str_opt(args, "path")
        {
            caminhos.push(PathBuf::from(path));
        }
        caminhos
    }

    fn detalhe(&self, args: &Value) -> String {
        let patch = arg_str_opt(args, "patch").unwrap_or_default();
        let alvos = caminhos_do_patch(&patch);
        if alvos.is_empty() {
            return "patch".into();
        }
        let lista: Vec<String> = alvos
            .iter()
            .take(3)
            .map(|p| p.display().to_string())
            .collect();
        if alvos.len() > 3 {
            format!("patch em {} arquivos", alvos.len())
        } else {
            format!("patch: {}", lista.join(", "))
        }
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let texto = arg_str(&args, "patch")?;
        let alvos = parse_patch(&texto, arg_str_opt(&args, "path").as_deref())?;
        if alvos.is_empty() {
            return Err(ToolError::Argumentos(
                "o patch não contém nenhum arquivo (use `*** Update File: ...`)".into(),
            ));
        }

        // Resolve e valida tudo antes de tocar em qualquer arquivo, para não
        // deixar estado parcial quando um alvo está fora do escopo.
        let mut resolvidos = Vec::new();
        for alvo in alvos {
            let caminho = resolver_caminho(ctx, &alvo.caminho);
            resolvidos.push((alvo, caminho));
        }

        let mut total_adicoes = 0usize;
        let mut total_remocoes = 0usize;
        let mut relatorio = Vec::new();

        // Fase 1: valida e prepara todos os alvos SEM escrever nada. O contrato
        // da ferramenta ("se algo não bater, nenhum arquivo é alterado") vale
        // entre arquivos — antes, uma falha no 2º alvo deixava o 1º já escrito.
        enum AcaoPreparada {
            Update {
                anterior: String,
                depois: String,
                externo: bool,
            },
            Add(String),
            Delete(String),
        }
        let mut preparados: Vec<(AlvoPatch, PathBuf, AcaoPreparada)> = Vec::new();
        for (alvo, caminho) in &resolvidos {
            match alvo.tipo {
                TipoPatch::Update(ref hunks) => {
                    let anterior = super::fs::ler_anterior_para_substituir(caminho)?;
                    if !caminho.is_file() {
                        return Err(ToolError::msg(format!(
                            "{} não existe; use `*** Add File:` para criar",
                            caminho.display()
                        )));
                    }
                    let depois = aplicar_hunks(&anterior, hunks)?;
                    let externo = ctx.arquivos.mudou_desde_registro(caminho, &anterior);
                    preparados.push((
                        alvo.clone(),
                        caminho.clone(),
                        AcaoPreparada::Update {
                            anterior,
                            depois,
                            externo,
                        },
                    ));
                }
                TipoPatch::Add(ref conteudo) => {
                    if caminho.exists() {
                        return Err(ToolError::msg(format!(
                            "{} já existe; use `*** Update File:` para alterá-lo",
                            caminho.display()
                        )));
                    }
                    preparados.push((
                        alvo.clone(),
                        caminho.clone(),
                        AcaoPreparada::Add(conteudo.clone()),
                    ));
                }
                TipoPatch::Delete => {
                    let anterior = super::fs::ler_anterior_para_substituir(caminho)?;
                    if !caminho.is_file() {
                        return Err(ToolError::msg(format!(
                            "{} não é um arquivo regular",
                            caminho.display()
                        )));
                    }
                    preparados.push((
                        alvo.clone(),
                        caminho.clone(),
                        AcaoPreparada::Delete(anterior),
                    ));
                }
            }
        }

        // Fase 2: tudo validado — agora sim aplica em disco.
        for (_alvo, caminho, acao) in preparados {
            let _lock = ctx.arquivos.lock_de(&caminho);
            let _guarda = _lock.lock().await;
            match acao {
                AcaoPreparada::Update {
                    anterior,
                    depois,
                    externo,
                } => {
                    super::fs::escrever_atomico(&caminho, &depois)?;
                    let (a, r) =
                        super::fs::registrar_alteracao(ctx, &caminho, &anterior, &depois, "patch");
                    total_adicoes += a;
                    total_remocoes += r;
                    relatorio.push(format!(
                        "{}: +{a} -{r}{}",
                        super::fs::caminho_relativo(ctx, &caminho),
                        if externo {
                            " (o arquivo havia mudado desde a última leitura)"
                        } else {
                            ""
                        }
                    ));
                }
                AcaoPreparada::Add(conteudo) => {
                    if let Some(pai) = caminho.parent() {
                        std::fs::create_dir_all(pai).map_err(|e| {
                            ToolError::msg(format!("falha ao criar {}: {e}", pai.display()))
                        })?;
                    }
                    super::fs::escrever_atomico(&caminho, &conteudo)?;
                    let (a, r) =
                        super::fs::registrar_alteracao(ctx, &caminho, "", &conteudo, "criacao");
                    total_adicoes += a;
                    total_remocoes += r;
                    relatorio.push(format!(
                        "{}: criado (+{a})",
                        super::fs::caminho_relativo(ctx, &caminho)
                    ));
                }
                AcaoPreparada::Delete(anterior) => {
                    std::fs::remove_file(&caminho).map_err(|e| {
                        ToolError::msg(format!("falha ao remover {}: {e}", caminho.display()))
                    })?;
                    let (a, r) =
                        super::fs::registrar_alteracao(ctx, &caminho, &anterior, "", "remocao");
                    total_adicoes += a;
                    total_remocoes += r;
                    relatorio.push(format!(
                        "{}: removido (-{r})",
                        super::fs::caminho_relativo(ctx, &caminho)
                    ));
                }
            }
        }

        Ok(ToolOutput::texto(format!(
            "Patch aplicado em {} arquivo(s):\n{}",
            relatorio.len(),
            relatorio.join("\n")
        ))
        .com_resumo(format!(
            "patch aplicado (+{total_adicoes} -{total_remocoes})"
        ))
        .com_diff(total_adicoes, total_remocoes))
    }
}

/// Caminhos declarados nos cabeçalhos do patch (para o check de escopo).
fn caminhos_do_patch(patch: &str) -> Vec<PathBuf> {
    let mut caminhos = Vec::new();
    for linha in patch.lines() {
        let linha = linha.trim_end();
        for prefixo in ["*** Update File:", "*** Add File:", "*** Delete File:"] {
            if let Some(resto) = linha.strip_prefix(prefixo) {
                let caminho = resto.trim();
                if !caminho.is_empty() {
                    caminhos.push(PathBuf::from(caminho));
                }
            }
        }
        if let Some(resto) = linha.strip_prefix("+++ ") {
            let caminho = resto.split('\t').next().unwrap_or(resto).trim();
            let caminho = caminho.strip_prefix("b/").unwrap_or(caminho);
            if !caminho.is_empty() && caminho != "/dev/null" {
                caminhos.push(PathBuf::from(caminho));
            }
        }
    }
    caminhos.sort();
    caminhos.dedup();
    caminhos
}

fn parse_patch(texto: &str, caminho_unico: Option<&str>) -> Result<Vec<AlvoPatch>, ToolError> {
    let linhas: Vec<&str> = texto.lines().collect();
    if linhas
        .iter()
        .any(|l| l.trim_start().starts_with("*** Begin Patch"))
    {
        parse_formato_nativo(&linhas)
    } else {
        let caminho = caminho_unico.ok_or_else(|| {
            ToolError::Argumentos(
                "diff unificado precisa do argumento `path`; para vários arquivos use `*** Begin Patch`"
                    .into(),
            )
        })?;
        let corpo: Vec<&str> = linhas
            .into_iter()
            .filter(|l| !l.starts_with("--- ") && !l.starts_with("+++ "))
            .collect();
        let hunks = parse_hunks(&corpo)?;
        if hunks.is_empty() {
            return Err(ToolError::Argumentos(
                "o diff não contém hunks (`@@`) nem linhas com prefixo ` `, `-` ou `+`".into(),
            ));
        }
        Ok(vec![AlvoPatch {
            caminho: caminho.to_string(),
            tipo: TipoPatch::Update(hunks),
        }])
    }
}

fn parse_formato_nativo(linhas: &[&str]) -> Result<Vec<AlvoPatch>, ToolError> {
    let mut alvos = Vec::new();
    let mut i = 0;
    while i < linhas.len() {
        let linha = linhas[i].trim_end();
        let trimmed = linha.trim_start();
        if trimmed.starts_with("*** Begin Patch")
            || trimmed.starts_with("*** End Patch")
            || trimmed.is_empty()
        {
            i += 1;
            continue;
        }
        if let Some(resto) = trimmed.strip_prefix("*** Update File:") {
            let caminho = resto.trim().to_string();
            if caminho.is_empty() {
                return Err(ToolError::Argumentos(
                    "`*** Update File:` sem caminho".into(),
                ));
            }
            i += 1;
            let inicio = i;
            while i < linhas.len() && !linhas[i].trim_start().starts_with("*** ") {
                i += 1;
            }
            let hunks = parse_hunks(&linhas[inicio..i])?;
            if hunks.is_empty() {
                return Err(ToolError::Argumentos(format!(
                    "`{caminho}` não tem nenhum hunk de alteração"
                )));
            }
            alvos.push(AlvoPatch {
                caminho,
                tipo: TipoPatch::Update(hunks),
            });
            continue;
        }
        if let Some(resto) = trimmed.strip_prefix("*** Add File:") {
            let caminho = resto.trim().to_string();
            if caminho.is_empty() {
                return Err(ToolError::Argumentos("`*** Add File:` sem caminho".into()));
            }
            i += 1;
            let mut conteudo = String::new();
            while i < linhas.len() && !linhas[i].trim_start().starts_with("*** ") {
                let linha = linhas[i];
                let Some(resto) = linha.strip_prefix('+') else {
                    return Err(ToolError::Argumentos(format!(
                        "linha de `*** Add File:` precisa começar com `+`: {linha:?}"
                    )));
                };
                conteudo.push_str(resto);
                conteudo.push('\n');
                i += 1;
            }
            alvos.push(AlvoPatch {
                caminho,
                tipo: TipoPatch::Add(conteudo),
            });
            continue;
        }
        if let Some(resto) = trimmed.strip_prefix("*** Delete File:") {
            let caminho = resto.trim().to_string();
            if caminho.is_empty() {
                return Err(ToolError::Argumentos(
                    "`*** Delete File:` sem caminho".into(),
                ));
            }
            alvos.push(AlvoPatch {
                caminho,
                tipo: TipoPatch::Delete,
            });
            i += 1;
            continue;
        }
        return Err(ToolError::Argumentos(format!(
            "cabeçalho de patch desconhecido: {linha:?}"
        )));
    }
    Ok(alvos)
}

fn parse_hunks(linhas: &[&str]) -> Result<Vec<Hunk>, ToolError> {
    let mut hunks: Vec<Hunk> = Vec::new();
    let mut atual: Option<Hunk> = None;
    for linha in linhas {
        let linha = *linha;
        if linha.trim_start().starts_with("@@") {
            if let Some(hunk) = atual.take()
                && !hunk.is_empty()
            {
                hunks.push(hunk);
            }
            atual = Some(Vec::new());
            continue;
        }
        if linha == "\\ No newline at end of file" {
            continue;
        }
        if linha.starts_with("*** ") {
            break;
        }
        if linha.is_empty() {
            // Linha vazia em diff unificado costuma representar contexto vazio.
            atual
                .get_or_insert_with(Vec::new)
                .push((' ', String::new()));
            continue;
        }
        let mut chars = linha.chars();
        let Some(prefixo) = chars.next() else {
            continue;
        };
        if matches!(prefixo, ' ' | '-' | '+') {
            atual
                .get_or_insert_with(Vec::new)
                .push((prefixo, chars.as_str().to_string()));
        } else if atual.is_some() {
            return Err(ToolError::Argumentos(format!(
                "linha de hunk inválida (esperava ` `, `-` ou `+`): {linha:?}"
            )));
        }
    }
    if let Some(hunk) = atual
        && !hunk.is_empty()
    {
        hunks.push(hunk);
    }
    Ok(hunks)
}

fn sem_terminador(peca: &str) -> &str {
    peca.strip_suffix("\r\n")
        .or_else(|| peca.strip_suffix('\n'))
        .unwrap_or(peca)
}

fn terminador(peca: &str) -> &'static str {
    if peca.ends_with("\r\n") {
        "\r\n"
    } else if peca.ends_with('\n') {
        "\n"
    } else {
        ""
    }
}

fn aplicar_hunks(conteudo: &str, hunks: &[Hunk]) -> Result<String, ToolError> {
    let mut pecas: Vec<String> = if conteudo.is_empty() {
        Vec::new()
    } else {
        conteudo.split_inclusive('\n').map(str::to_string).collect()
    };
    let mut cursor = 0usize;
    for (numero, hunk) in hunks.iter().enumerate() {
        let antigas: Vec<&str> = hunk
            .iter()
            .filter(|(c, _)| *c != '+')
            .map(|(_, texto)| texto.as_str())
            .collect();
        if antigas.is_empty() {
            return Err(ToolError::Argumentos(format!(
                "hunk {} não tem contexto nem remoções; use `*** Add File:`",
                numero + 1
            )));
        }
        if antigas.len() > pecas.len() {
            return Err(ToolError::msg(format!(
                "hunk {} espera {} linha(s), mas o arquivo tem {}",
                numero + 1,
                antigas.len(),
                pecas.len()
            )));
        }
        let mut encontrado = None;
        for inicio in cursor..=pecas.len() - antigas.len() {
            if antigas
                .iter()
                .enumerate()
                .all(|(k, alvo)| sem_terminador(&pecas[inicio + k]) == *alvo)
            {
                encontrado = Some(inicio);
                break;
            }
        }
        let inicio = encontrado.ok_or_else(|| {
            ToolError::msg(format!(
                "hunk {} não encontrou o contexto no arquivo. Releia o arquivo e refaça o patch \
                 (uma linha de contexto difere).",
                numero + 1
            ))
        })?;
        let fim = inicio + antigas.len();
        let eol = pecas
            .get(inicio)
            .map(|p| terminador(p))
            .filter(|e| !e.is_empty())
            .unwrap_or("\n");
        let mut novas = Vec::with_capacity(hunk.len());
        let mut indice_antigo = 0usize;
        for (prefixo, texto) in hunk {
            match prefixo {
                ' ' => {
                    novas.push(pecas[inicio + indice_antigo].clone());
                    indice_antigo += 1;
                }
                '-' => indice_antigo += 1,
                '+' => novas.push(format!("{texto}{eol}")),
                _ => {}
            }
        }
        let quantidade = novas.len();
        pecas.splice(inicio..fim, novas);
        cursor = inicio + quantidade;
    }
    Ok(pecas.concat())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aplica_hunk_com_contexto_e_preserva_crlf() {
        let base = "um\r\ndois\r\ntres\r\n";
        let hunks = vec![vec![
            (' ', "um".into()),
            ('-', "dois".into()),
            ('+', "DOIS".into()),
            (' ', "tres".into()),
        ]];
        assert_eq!(
            aplicar_hunks(base, &hunks).unwrap(),
            "um\r\nDOIS\r\ntres\r\n"
        );
    }

    #[test]
    fn contexto_errado_nao_altera_nada() {
        let erro = aplicar_hunks(
            "a\nb\n",
            &[vec![(' ', "nao-existe".into()), ('+', "x".into())]],
        )
        .expect_err("deveria falhar");
        assert!(erro.to_string().contains("contexto"));
    }

    #[test]
    fn parseia_formato_nativo_com_multiplos_arquivos() {
        let patch = "*** Begin Patch\n\
                     *** Update File: a.rs\n\
                     @@\n\
                     -um\n\
                     +dois\n\
                     *** Add File: b.rs\n\
                     +novo\n\
                     *** Delete File: c.rs\n\
                     *** End Patch";
        let alvos = parse_patch(patch, None).unwrap();
        assert_eq!(alvos.len(), 3);
        assert_eq!(alvos[0].caminho, "a.rs");
        assert!(matches!(alvos[1].tipo, TipoPatch::Add(ref c) if c == "novo\n"));
        assert!(matches!(alvos[2].tipo, TipoPatch::Delete));
    }

    #[test]
    fn diff_unificado_usa_path() {
        let patch = "--- a/a.rs\n+++ b/a.rs\n@@ -1 +1 @@\n-um\n+dois\n";
        let alvos = parse_patch(patch, Some("a.rs")).unwrap();
        assert_eq!(alvos.len(), 1);
        assert!(matches!(alvos[0].tipo, TipoPatch::Update(_)));
    }

    #[test]
    fn caminhos_do_patch_incluem_headers() {
        let patch =
            "*** Begin Patch\n*** Update File: src/a.rs\n*** Add File: src/b.rs\n*** End Patch";
        let caminhos = caminhos_do_patch(patch);
        assert_eq!(caminhos.len(), 2);
    }

    /// O contrato "se algo não bater, nenhum arquivo é alterado" vale entre
    /// arquivos: uma falha no 2º alvo não pode deixar o 1º já escrito.
    #[tokio::test]
    async fn falha_em_um_arquivo_nao_escreve_nenhum() {
        use crate::tools::git::tests::contexto;
        use serde_json::json;

        let dir = std::env::temp_dir().join(format!(
            "kestrel-patch-atomico-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "um\ndois\n").unwrap();
        std::fs::write(dir.join("b.txt"), "x\n").unwrap();
        let ctx = contexto(&dir);

        // `a.txt` tem hunk válido; `b.txt` tem contexto que não existe. O
        // patch como um todo precisa falhar SEM ter escrito `a.txt`.
        let patch_texto = "*** Begin Patch\n\
                           *** Update File: a.txt\n\
                           @@\n\
                           -um\n\
                           +UM\n\
                           *** Update File: b.txt\n\
                           @@\n\
                           -nao-existe\n\
                           +novo\n\
                           *** End Patch";
        let resultado = ApplyPatch
            .executar(json!({"patch": patch_texto}), &ctx)
            .await;
        assert!(resultado.is_err(), "patch com hunk quebrado deveria falhar");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "um\ndois\n",
            "a.txt não pode ter sido alterado quando outro arquivo falhou"
        );
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "x\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Controle positivo: com todos os alvos válidos, ambos são escritos.
    #[tokio::test]
    async fn patch_valido_escreve_todos_os_arquivos() {
        use crate::tools::git::tests::contexto;
        use serde_json::json;

        let dir = std::env::temp_dir().join(format!(
            "kestrel-patch-ok-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.txt"), "um\ndois\n").unwrap();
        std::fs::write(dir.join("b.txt"), "x\n").unwrap();
        let ctx = contexto(&dir);

        let patch_texto = "*** Begin Patch\n\
                           *** Update File: a.txt\n\
                           @@\n\
                           -um\n\
                           +UM\n\
                           *** Update File: b.txt\n\
                           @@\n\
                           -x\n\
                           +X\n\
                           *** End Patch";
        let resultado = ApplyPatch
            .executar(json!({"patch": patch_texto}), &ctx)
            .await;
        assert!(resultado.is_ok(), "patch válido deveria aplicar");
        assert_eq!(
            std::fs::read_to_string(dir.join("a.txt")).unwrap(),
            "UM\ndois\n"
        );
        assert_eq!(std::fs::read_to_string(dir.join("b.txt")).unwrap(), "X\n");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
