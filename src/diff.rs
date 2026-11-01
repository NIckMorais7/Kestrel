//! Cálculo de diferenças reais entre versões de arquivo (linhas adicionadas e
//! removidas), usado para relatar alterações com números exatos.

/// Estatísticas de uma diferença.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DiffStats {
    pub additions: usize,
    pub deletions: usize,
}

/// Resultado completo: estatísticas e diff unificado em texto.
#[derive(Clone, Debug, Default)]
pub struct Diff {
    pub stats: DiffStats,
    pub text: String,
}

/// Limite de linhas para o algoritmo exato (LCS). Acima disso usamos uma
/// comparação por conjuntos, que continua contando linhas reais.
const LIMITE_EXATO: usize = 4000;

/// Linha de metadados do diff: `str::lines()` perde a informação de nova linha
/// final, e sem ela o `/undo` não consegue restaurar exatamente o arquivo.
/// O prefixo `\` nunca aparece num diff normal (`+`, `-` ou espaço), então
/// diffs antigos continuam válidos.
const MARCA_NOVA_LINHA: &str = "\\ kestrel-nova-linha-final";

/// Calcula o diff entre dois textos.
pub fn diff(old: &str, new: &str) -> Diff {
    let a: Vec<&str> = if old.is_empty() {
        Vec::new()
    } else {
        old.lines().collect()
    };
    let b: Vec<&str> = if new.is_empty() {
        Vec::new()
    } else {
        new.lines().collect()
    };

    if a.len() > LIMITE_EXATO || b.len() > LIMITE_EXATO {
        return diff_aproximado(&a, &b);
    }

    // Programação dinâmica de subsequência comum mais longa.
    let n = a.len();
    let m = b.len();
    let mut tabela = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            tabela[i][j] = if a[i] == b[j] {
                tabela[i + 1][j + 1] + 1
            } else {
                tabela[i + 1][j].max(tabela[i][j + 1])
            };
        }
    }

    let mut linhas = Vec::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    let (mut i, mut j) = (0usize, 0usize);
    while i < n && j < m {
        if a[i] == b[j] {
            linhas.push(format!(" {}", a[i]));
            i += 1;
            j += 1;
        } else if tabela[i + 1][j] >= tabela[i][j + 1] {
            linhas.push(format!("-{}", a[i]));
            deletions += 1;
            i += 1;
        } else {
            linhas.push(format!("+{}", b[j]));
            additions += 1;
            j += 1;
        }
    }
    while i < n {
        linhas.push(format!("-{}", a[i]));
        deletions += 1;
        i += 1;
    }
    while j < m {
        linhas.push(format!("+{}", b[j]));
        additions += 1;
        j += 1;
    }

    // Só registra quando as pontas diferem: assim o caso comum (ambas com
    // nova linha final) não ganha ruído no `/diff`.
    if old.ends_with('\n') != new.ends_with('\n') {
        linhas.push(format!(
            "{MARCA_NOVA_LINHA} antes={} depois={}",
            u8::from(old.ends_with('\n')),
            u8::from(new.ends_with('\n'))
        ));
    }

    Diff {
        stats: DiffStats {
            additions,
            deletions,
        },
        text: linhas.join("\n"),
    }
}

/// Versão sem LCS, para arquivos muito grandes.
fn diff_aproximado(a: &[&str], b: &[&str]) -> Diff {
    use std::collections::HashMap;
    let mut contagem: HashMap<&str, i64> = HashMap::new();
    for linha in a {
        *contagem.entry(linha).or_insert(0) -= 1;
    }
    for linha in b {
        *contagem.entry(linha).or_insert(0) += 1;
    }
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for (_, delta) in contagem {
        if delta > 0 {
            additions += delta as usize;
        } else {
            deletions += (-delta) as usize;
        }
    }
    Diff {
        stats: DiffStats {
            additions,
            deletions,
        },
        // O marcador de truncamento vem junto: sem ele, o `/undo` via zero
        // operadores nesse texto e dizia "o arquivo mudou desde a alteração
        // registrada" em vez de "diff grande demais para desfazer".
        text: format!(
            "{DIFF_TRUNCADO} (diff textual omitido para arquivo grande: +{additions} -{deletions} linhas)"
        ),
    }
}

/// Frase que as versões antigas gravavam no lugar do diff de arquivo grande,
/// sem o marcador [`DIFF_TRUNCADO`]. Continua reconhecida porque diffs antigos
/// ainda estão no banco.
const FRASE_DIFF_OMITIDO: &str = "(diff textual omitido";

/// O texto gravado descreve a alteração completa? Diff truncado não descreve:
/// reverter com ele escreveria um arquivo parcial.
fn diff_truncado(texto: &str) -> bool {
    texto.contains(DIFF_TRUNCADO) || texto.contains(FRASE_DIFF_OMITIDO)
}

/// Apenas as estatísticas (usada pelos testes de contagem).
#[cfg(test)]
pub fn stats(old: &str, new: &str) -> DiffStats {
    if old == new {
        return DiffStats::default();
    }
    diff(old, new).stats
}

/// Marcador gravado no lugar do diff quando ele é grande demais para o
/// `/undo` confiar no recorte.
///
/// Fica aqui (e não em `tools::fs`) porque o reverso precisa recusá-lo: um
/// patch parcial escreveria o arquivo errado.
pub const DIFF_TRUNCADO: &str =
    "[diff truncado: grande demais para desfazer automaticamente com segurança]";

/// Reaplica um diff no sentido inverso, devolvendo o texto anterior.
///
/// O diff do Kestrel é linha a linha (`+` adicionada, `-` removida, ` `
/// contexto), então reverter é mecânico: as linhas `+` saem e as linhas `-`
/// voltam — nenhuma correspondência com o arquivo atual é necessária, o diff
/// já descreve as duas pontas. Usado pelo `/undo`, para desfazer a alteração de
/// um arquivo sem depender de git.
///
/// O único requisito é que o arquivo atual seja de fato a ponta "depois" do
/// diff; por isso conferimos as linhas de contexto. Se não baterem, devolvemos
/// `None` e o chamador avisa em vez de escrever porcaria.
pub fn reverter(diff_texto: &str, atual: &str) -> Option<String> {
    // Diff truncado não descreve a alteração completa: reverter com ele
    // escreveria um arquivo parcial. Recusa explícita, nunca patch silencioso.
    if diff_truncado(diff_texto) {
        return None;
    }
    let atuais: Vec<&str> = if atual.is_empty() {
        Vec::new()
    } else {
        atual.lines().collect()
    };
    let mut operacoes: Vec<(char, &str)> = Vec::new();
    for linha in diff_texto.lines() {
        if let Some(resto) = linha.strip_prefix('+') {
            operacoes.push(('+', resto));
        } else if let Some(resto) = linha.strip_prefix('-') {
            operacoes.push(('-', resto));
        } else if let Some(resto) = linha.strip_prefix(' ') {
            operacoes.push((' ', resto));
        }
    }
    if operacoes.is_empty() {
        return None;
    }
    // Reconstrução: as operações do diff, na ordem, descrevem a ponta "depois"
    // nas linhas de contexto e de `+`. Caminhamos o arquivo atual por esse
    // caminho para provar que ele ainda é essa ponta.
    let mut esperado: Vec<&str> = Vec::new();
    for (sinal, conteudo) in &operacoes {
        if *sinal == '+' || *sinal == ' ' {
            esperado.push(conteudo);
        }
    }
    if atuais.len() < esperado.len() || atuais[..esperado.len()] != esperado[..] {
        return None;
    }
    // Reverso: onde o diff removeu (`-`) a linha volta; onde acrescentou (`+`)
    // ela sai; o contexto permanece nos dois.
    let mut saida: Vec<&str> = Vec::new();
    for (sinal, conteudo) in operacoes {
        if sinal != '+' {
            saida.push(conteudo);
        }
    }
    // Linhas acrescentadas depois do diff (por outra edição) são preservadas.
    let acrescentadas = atuais.len() > esperado.len();
    if acrescentadas {
        saida.extend_from_slice(&atuais[esperado.len()..]);
    }
    // O estado da nova linha final vem do diff (as duas pontas); diffs antigos
    // não têm a marca, então usamos o arquivo atual como referência. Quando
    // houve linhas acrescentadas depois, o fim real é o do arquivo atual.
    let termina_com_nova_linha = if acrescentadas {
        atual.ends_with('\n')
    } else {
        nova_linha_original(diff_texto).unwrap_or_else(|| atual.ends_with('\n'))
    };
    let mut texto = saida.join("\n");
    // `saida` vazio significa arquivo original vazio: não inventa um "\n".
    if termina_com_nova_linha && !saida.is_empty() {
        texto.push('\n');
    }
    Some(texto)
}

/// Lê `antes=0/1` da marca de nova linha final, quando presente.
fn nova_linha_original(diff_texto: &str) -> Option<bool> {
    let resto = diff_texto
        .lines()
        .find_map(|l| l.strip_prefix(MARCA_NOVA_LINHA))?;
    match resto
        .split_whitespace()
        .find_map(|p| p.strip_prefix("antes="))
    {
        Some("1") => Some(true),
        Some("0") => Some(false),
        _ => None,
    }
}

/// O diff do Kestrel é linha a linha, então reverter é mecânico. Sem isso o
/// `/undo` (desfazer sem git) não teria base.
#[test]
fn reverter_devolve_o_texto_anterior() {
    let antes = "linha um\nlinha dois\nlinha tres\n";
    let depois = "linha um\nlinha DOIS alterada\nlinha tres\nlinha quatro\n";
    let d = diff(antes, depois);
    assert_eq!(d.stats.additions, 2);
    assert_eq!(d.stats.deletions, 1);
    let recuperado = reverter(&d.text, depois).expect("deveria reverter");
    assert_eq!(recuperado, antes);
}

/// Diff truncado nunca é aplicado: o patch parcial corromperia o arquivo.
#[test]
fn reverter_recusa_diff_truncado() {
    assert!(reverter(DIFF_TRUNCADO, "qualquer\n").is_none());
    assert!(reverter(&format!("{DIFF_TRUNCADO}\n+linha"), "x\n").is_none());
}

/// O caminho de arquivo grande (>4000 linhas) grava a frase de omissão **com**
/// o marcador: sem ele o `/undo` via zero operadores e culpava o arquivo por
/// ter mudado, em vez de dizer que o diff é grande demais.
#[test]
fn diff_de_arquivo_grande_marca_truncamento() {
    let grande: String = (0..5000).map(|i| format!("linha {i}\n")).collect();
    let modificado = format!("{grande}extra\n");
    let d = diff(&grande, &modificado);
    assert!(d.text.contains(DIFF_TRUNCADO), "{}", d.text);
    assert!(d.text.contains("omitido"));
    // E o reverso recusa no motivo certo.
    assert!(reverter(&d.text, &modificado).is_none());
    // A frase das versões antigas (sem marcador) também é recusada.
    assert!(
        reverter(
            "(diff textual omitido para arquivo grande: +1 -1 linhas)",
            "x\n"
        )
        .is_none()
    );
}

/// Reverter é reversível: ida e volta dão no mesmo lugar.
#[test]
fn reverter_e_estavel_em_duas_chamadas() {
    let a = "um\ndois\ntres\n";
    let b = "um\nDOIS\ntres\n";
    // diff(a→b); para desfazer, o estado atual é a ponta final (`b`).
    let ida = diff(a, b);
    let de_volta = reverter(&ida.text, b).expect("volta");
    assert_eq!(de_volta, a);
    // No sentido inverso, diff(b→a) com o estado `a` devolve `b`.
    let volta = diff(b, a);
    assert_eq!(
        reverter(&volta.text, a).as_deref(),
        Some(b),
        "reverter não é reversível"
    );
}

/// Arquivo que mudou desde a alteração registrada: o diff não bate mais e o
/// reverso é recusado em vez de escrever porcaria.
#[test]
fn reverter_recusa_quando_o_arquivo_mudou() {
    let antes = "a\nb\n";
    let depois = "a\nB\n";
    let d = diff(antes, depois);
    let estado_atual = "completamente diferente\noutra linha\n";
    assert!(reverter(&d.text, estado_atual).is_none());
}

/// Mudança que só remove linhas também reverte.
#[test]
fn reverter_remocao_de_linhas() {
    let antes = "fica\nsai\ntambem fica\n";
    let depois = "fica\ntambem fica\n";
    let d = diff(antes, depois);
    let recuperado = reverter(&d.text, depois).expect("remocao deveria reverter");
    assert_eq!(recuperado, antes);
}

/// Arquivo sem nova linha final precisa voltar sem nova linha final (antes o
/// `/undo` sempre acrescentava `\n`).
#[test]
fn reverter_preserva_ausencia_de_nova_linha_final() {
    let antes = "a\nb";
    let depois = "a\nB";
    let d = diff(antes, depois);
    assert_eq!(reverter(&d.text, depois).as_deref(), Some(antes));
}

/// A edição pode só acrescentar/remover a nova linha final: o diff não muda
/// nenhuma linha, mas o `/undo` ainda precisa restaurar o byte exato.
#[test]
fn reverter_restaura_estado_da_nova_linha_final() {
    let sem = "a\nb";
    let com = "a\nb\n";
    let d = diff(sem, com);
    assert_eq!(reverter(&d.text, com).as_deref(), Some(sem));
    let d = diff(com, sem);
    assert_eq!(reverter(&d.text, sem).as_deref(), Some(com));
}

/// Reverter para um arquivo vazio não pode deixar um `\n` solto.
#[test]
fn reverter_para_arquivo_vazio() {
    let d = diff("", "novo\n");
    assert_eq!(reverter(&d.text, "novo\n").as_deref(), Some(""));
}

/// Crescer o arquivo depois do fato não deve impedir o reverso.
#[test]
fn reverter_preserva_linhas_acrescentadas_depois() {
    let antes = "a\nb\n";
    let depois = "a\nB\n";
    let d = diff(antes, depois);
    let atual = "a\nB\nc nova\nd nova\n";
    let recuperado = reverter(&d.text, atual).expect("deveria reverter");
    assert_eq!(recuperado, "a\nb\nc nova\nd nova\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conta_adicoes_e_remocoes() {
        let s = stats("a\nb\nc", "a\nB\nc\nd");
        assert_eq!(s.additions, 2); // "B" e "d"
        assert_eq!(s.deletions, 1); // "b"
    }

    #[test]
    fn arquivo_novo_conta_todas_as_linhas() {
        let s = stats("", "x\ny\nz");
        assert_eq!(s.additions, 3);
        assert_eq!(s.deletions, 0);
    }

    #[test]
    fn arquivo_apagado_conta_todas_as_linhas() {
        let s = stats("x\ny", "");
        assert_eq!(s.additions, 0);
        assert_eq!(s.deletions, 2);
    }

    #[test]
    fn identico_nao_tem_alteracoes() {
        assert_eq!(stats("a", "a"), DiffStats::default());
    }

    #[test]
    fn gera_texto_unificado() {
        let d = diff("a\nb", "a\nc");
        assert!(d.text.contains("-b"));
        assert!(d.text.contains("+c"));
        assert!(d.text.contains(" a"));
    }

    #[test]
    fn arquivo_grande_usa_aproximacao() {
        let grande: String = (0..5000).map(|i| format!("linha {i}\n")).collect();
        let modificado = format!("{grande}extra\n");
        let d = diff(&grande, &modificado);
        assert_eq!(d.stats.additions, 1);
        assert!(d.text.contains("omitido"));
    }
}
