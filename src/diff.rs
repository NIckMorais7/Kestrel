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
        text: format!(
            "(diff textual omitido para arquivo grande: +{additions} -{deletions} linhas)"
        ),
    }
}

/// Apenas as estatísticas (usada pelos testes de contagem).
#[cfg(test)]
pub fn stats(old: &str, new: &str) -> DiffStats {
    if old == new {
        return DiffStats::default();
    }
    diff(old, new).stats
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
