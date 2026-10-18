//! Níveis de permissão, análise contextual de comandos e decisões de execução.
//!
//! A análise é textual e contextual: ela inspeciona o comando, seus argumentos,
//! o diretório de trabalho e os alvos prováveis. **Não é uma sandbox** e não
//! oferece garantia absoluta de segurança — é uma camada de proteção que
//! bloqueia operações claramente proibidas e pede confirmação para as
//! sensíveis.

use crate::util::normalize_path;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Resposta do usuário a um pedido de confirmação.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Confirmacao {
    Permitir,
    PermitirSempre,
    Negar,
}

/// Nível de permissão do agente.
///
/// Os rótulos que a interface exibe (`Read`, `Semi`, …) também são aceitos no
/// config.toml: editar o arquivo "como a tela manda" não pode derrubar a
/// configuração inteira.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Default)]
#[serde(rename_all = "lowercase")]
pub enum PermissionLevel {
    /// Somente leitura e análise.
    #[serde(alias = "read", alias = "Read")]
    Read,
    /// Leitura, criação e modificação de arquivos no projeto.
    #[serde(alias = "write", alias = "Write")]
    Write,
    /// Trabalho autônomo com confirmação para operações sensíveis.
    #[default]
    #[serde(alias = "semi", alias = "Semi")]
    Semi,
    /// Autonomia máxima, sem confirmações rotineiras.
    #[serde(alias = "full", alias = "Full")]
    Full,
}

impl PermissionLevel {
    pub fn label(&self) -> &'static str {
        match self {
            PermissionLevel::Read => "Read",
            PermissionLevel::Write => "Write",
            PermissionLevel::Semi => "Semi",
            PermissionLevel::Full => "Full",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            PermissionLevel::Read => "somente leitura e análise; nenhuma modificação",
            PermissionLevel::Write => "leitura e edição de arquivos; terminais com restrição",
            PermissionLevel::Semi => "autônomo, com confirmação para operações sensíveis",
            PermissionLevel::Full => "autonomia máxima, sem confirmações rotineiras",
        }
    }

    pub fn parse(input: &str) -> Option<Self> {
        match input.trim().to_lowercase().as_str() {
            "read" | "leitura" => Some(PermissionLevel::Read),
            "write" | "escrita" => Some(PermissionLevel::Write),
            "semi" | "semiautonomo" | "semiautônomo" => Some(PermissionLevel::Semi),
            "full" | "completo" => Some(PermissionLevel::Full),
            _ => None,
        }
    }

    pub fn cycle(&self) -> Self {
        match self {
            PermissionLevel::Read => PermissionLevel::Write,
            PermissionLevel::Write => PermissionLevel::Semi,
            PermissionLevel::Semi => PermissionLevel::Full,
            PermissionLevel::Full => PermissionLevel::Read,
        }
    }
}

impl std::fmt::Display for PermissionLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.label())
    }
}

/// Categoria de risco de um comando.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CommandCategory {
    /// Apenas leitura/consulta.
    Leitura,
    /// Compilação, testes, formatação — altera artefatos, não fontes.
    Build,
    /// Modificação dentro do espaço de trabalho.
    Modificacao,
    /// Operação sensível: privilégios, pacotes, rede, reescrita de histórico.
    Sensivel,
    /// Destrutiva ou irreversível.
    Destrutiva,
    /// Proibida em qualquer nível.
    Proibida,
}

impl CommandCategory {
    pub fn label(&self) -> &'static str {
        match self {
            CommandCategory::Leitura => "leitura",
            CommandCategory::Build => "build/testes",
            CommandCategory::Modificacao => "modificação",
            CommandCategory::Sensivel => "sensível",
            CommandCategory::Destrutiva => "destrutiva",
            CommandCategory::Proibida => "proibida",
        }
    }
}

/// Decisão sobre um comando ou chamada de ferramenta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Decision {
    Permitir,
    Confirmar,
    Bloquear,
}

impl Decision {
    pub fn label(&self) -> &'static str {
        match self {
            Decision::Permitir => "permitido",
            Decision::Confirmar => "confirmação necessária",
            Decision::Bloquear => "bloqueado",
        }
    }
}

/// Resultado completo da análise.
#[derive(Clone, Debug)]
pub struct Analysis {
    pub decision: Decision,
    pub category: CommandCategory,
    pub reason: String,
}

impl Analysis {
    fn new(decision: Decision, category: CommandCategory, reason: impl Into<String>) -> Self {
        Self {
            decision,
            category,
            reason: reason.into(),
        }
    }
}

/// Contexto necessário para avaliar um comando.
#[derive(Clone, Debug)]
pub struct AnalysisContext {
    pub roots: Vec<PathBuf>,
}

/// Padrões catastróficos bloqueados em qualquer nível de permissão.
const PADROES_PROIBIDOS: &[(&str, &str)] = &[
    ("mkfs", "formatação de sistema de arquivos"),
    (":(){", "fork bomb"),
    ("shutdown", "desligamento do sistema"),
    ("reboot", "reinicialização do sistema"),
    ("poweroff", "desligamento do sistema"),
    ("halt ", "desligamento do sistema"),
    ("init 0", "desligamento do sistema"),
    ("> /dev/sd", "escrita direta em dispositivo de bloco"),
    ("> /dev/nvme", "escrita direta em dispositivo de bloco"),
    (
        "chmod -R 777 /",
        "liberação de permissões na raiz do sistema",
    ),
    ("chmod 777 /", "liberação de permissões na raiz do sistema"),
];

/// Diretórios de sistema cuja escrita é sempre bloqueada.
const DIRETORIOS_SISTEMA: &[&str] = &[
    "/etc", "/boot", "/sys", "/proc", "/dev", "/usr/bin", "/usr/lib", "/var/lib", "/bin", "/sbin",
];

fn primeira_palavra(cmd: &str) -> String {
    cmd.split_whitespace()
        .next()
        .unwrap_or("")
        .rsplit('/')
        .next()
        .unwrap_or("")
        .to_string()
}

fn contem_algum(haystack: &str, needles: &[&str]) -> Option<String> {
    let lower = haystack.to_lowercase();
    needles
        .iter()
        .find(|n| lower.contains(&n.to_lowercase()))
        .map(|n| n.to_string())
}

/// Extrai tokens que parecem caminhos de um comando.
///
/// Caminhos relativos são ancorados na primeira raiz conhecida: sem isso
/// `cp a.txt ./b.txt` pareceria "fora do projeto" só porque `./b.txt` não
/// começa com `/`.
fn caminhos_no_comando(cmd: &str, raiz: Option<&PathBuf>) -> Vec<PathBuf> {
    cmd.split_whitespace()
        .map(|tok| tok.trim_matches(|c| c == '"' || c == '\'' || c == ';' || c == '|'))
        .filter(|tok| {
            tok.starts_with('/')
                || tok.starts_with("./")
                || tok.starts_with("../")
                || tok.starts_with("~/")
        })
        .map(|tok| {
            if (tok.starts_with("./") || tok.starts_with("../"))
                && let Some(raiz) = raiz
            {
                return raiz.join(tok.trim_start_matches("./"));
            }
            crate::util::expand_tilde(tok)
        })
        .collect()
}

/// Divide o comando em subcomandos, respeitando aspas.
///
/// Sem esta divisão, `echo x; rm -rf ~/docs` era analisada pelo primeiro
/// palavra (`echo`, inofensiva) e liberada como leitura em todos os níveis.
fn subcomandos(cmd: &str) -> Vec<String> {
    let mut saida: Vec<String> = Vec::new();
    let mut atual = String::new();
    let mut aspa: Option<char> = None;
    let chars: Vec<char> = cmd.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if let Some(q) = aspa {
            atual.push(c);
            if c == q {
                aspa = None;
            }
            i += 1;
            continue;
        }
        match c {
            '"' | '\'' => {
                aspa = Some(c);
                atual.push(c);
                i += 1;
            }
            ';' => {
                saida.push(atual.trim().to_string());
                atual = String::new();
                i += 1;
            }
            '|' | '&' => {
                // `&&` e `||` contam como uma única separação.
                saida.push(atual.trim().to_string());
                atual = String::new();
                if c == '&' && i + 1 < chars.len() && chars[i + 1] == '&' {
                    i += 2;
                } else {
                    i += 1;
                }
            }
            _ => {
                atual.push(c);
                i += 1;
            }
        }
    }
    saida.push(atual.trim().to_string());
    saida.into_iter().filter(|s| !s.is_empty()).collect()
}

/// Severidade usada para fundir a análise de vários subcomandos.
fn severidade(a: &Analysis) -> u8 {
    match a.category {
        CommandCategory::Proibida => 5,
        CommandCategory::Destrutiva => 4,
        CommandCategory::Sensivel => 3,
        CommandCategory::Modificacao => 2,
        CommandCategory::Build => 1,
        CommandCategory::Leitura => 0,
    }
}

/// Fundir duas análises: a mais severa vence (empate mantém a primeira).
fn fundir(atual: Option<Analysis>, outra: Analysis) -> Option<Analysis> {
    match atual {
        None => Some(outra),
        Some(a) => {
            if severidade(&outra) > severidade(&a) {
                Some(outra)
            } else {
                Some(a)
            }
        }
    }
}

/// `rm` com recursão apontando para a raiz do sistema, em qualquer grafia de
/// flags (`rm -rf /`, `rm -r -f /`, `rm --recursive --force /`, `rm -fr /*`).
fn remove_raiz_do_sistema(cmd: &str) -> bool {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    let tem_rm = tokens.iter().any(|t| t.rsplit('/').next() == Some("rm"));
    if !tem_rm {
        return false;
    }
    let recursivo = tokens.iter().any(|t| {
        t.starts_with('-') && (t.contains('r') || t.contains('R') || t.contains("--recursive"))
    });
    let alvo_raiz = tokens.iter().any(|t| {
        let alvo = t.trim_matches(|c| c == '"' || c == '\'');
        alvo == "/" || alvo == "/*"
    });
    recursivo && alvo_raiz
}

/// `find` capaz de apagar ou executar comandos.
fn find_destrutivo(cmd: &str) -> bool {
    let tokens: Vec<&str> = cmd.split_whitespace().collect();
    if tokens.first().map(|t| t.rsplit('/').next()) != Some(Some("find")) {
        return false;
    }
    tokens.iter().any(|t| {
        *t == "-delete" || *t == "-exec" || *t == "-execdir" || *t == "-ok" || *t == "-okdir"
    })
}

/// Analisa textualmente um comando de terminal.
///
/// O comando é dividido em subcomandos (`;`, `&&`, `||`, `|`) e cada um é
/// analisado em separado: a classificação mais severa vence. Assim um comando
/// inofensivo seguido de um destrutivo (`echo x; rm -rf ~/docs`) nunca é
/// liberado como leitura.
pub fn analyze_command(command: &str, ctx: &AnalysisContext) -> Analysis {
    let cmd = command.trim();
    if cmd.is_empty() {
        return Analysis::new(
            Decision::Bloquear,
            CommandCategory::Proibida,
            "comando vazio",
        );
    }

    // 1. Padrões catastróficos — bloqueados sempre.
    if let Some((padrao, motivo)) = PADROES_PROIBIDOS
        .iter()
        .find(|(p, _)| cmd.to_lowercase().contains(p))
    {
        return Analysis::new(
            Decision::Bloquear,
            CommandCategory::Proibida,
            format!("padrão catastrófico detectado (`{padrao}`): {motivo}"),
        );
    }
    if remove_raiz_do_sistema(cmd) {
        return Analysis::new(
            Decision::Bloquear,
            CommandCategory::Proibida,
            "remoção da raiz do sistema",
        );
    }

    // 3. Pipelines perigosos (baixar e executar) — avaliados no comando
    // inteiro: o próprio pipe é o risco, e ele desapareceria ao dividir.
    let pipe_perigoso = [
        "| sh", "| bash", "|sh", "|bash", "| zsh", "|zsh", "| python", "|python",
    ]
    .iter()
    .any(|p| cmd.to_lowercase().contains(p));
    if pipe_perigoso {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Destrutiva,
            "execução direta de conteúdo baixado da rede (pipe para interpretador)",
        );
    }

    // Cada subcomando é classificado sozinho; a análise mais severa vence.
    let mut resultado: Option<Analysis> = None;
    for parte in subcomandos(cmd) {
        let analise = classificar_parte(&parte, ctx);
        resultado = fundir(resultado, analise);
    }
    resultado.unwrap_or_else(|| {
        Analysis::new(
            Decision::Confirmar,
            CommandCategory::Modificacao,
            "efeito do comando não pôde ser avaliado com segurança",
        )
    })
}

/// Classifica um subcomando já isolado (sem `;`, `&&`, `||`, `|`).
fn classificar_parte(cmd: &str, ctx: &AnalysisContext) -> Analysis {
    let cmd = cmd.trim();
    if cmd.is_empty() {
        return Analysis::new(
            Decision::Permitir,
            CommandCategory::Leitura,
            "comando vazio",
        );
    }
    let programa = primeira_palavra(cmd);

    // 2. Escrita em diretórios de sistema.
    for caminho in caminhos_no_comando(cmd, ctx.roots.first()) {
        let norm = normalize_path(&caminho);
        if DIRETORIOS_SISTEMA.iter().any(|dir| norm.starts_with(dir)) {
            return Analysis::new(
                Decision::Bloquear,
                CommandCategory::Proibida,
                format!("operação sobre caminho de sistema: {}", norm.display()),
            );
        }
    }

    // 4. Comandos privilegiados.
    if programa == "sudo" || programa == "su" || programa == "doas" {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Sensivel,
            "execução com privilégios elevados",
        );
    }

    // 5. Pacotes e serviços.
    let sensiveis_servico = [
        "apt install",
        "apt-get install",
        "apt remove",
        "apt-get remove",
        "dnf install",
        "yum install",
        "pacman -S",
        "snap install",
        "systemctl",
        "service ",
        "npm install -g",
        "npm i -g",
        "pip install",
        "pip3 install",
        "cargo install",
    ];
    if contem_algum(cmd, &sensiveis_servico).is_some() {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Sensivel,
            "instalação de pacotes ou alteração de serviços do sistema",
        );
    }

    // 6. Git destrutivo.
    let git_destrutivo = [
        "git reset --hard",
        "git clean -fd",
        "git clean -fdx",
        "git push --force",
        "git push -f",
        "git branch -D",
        "git checkout -- .",
        "git restore .",
        "git stash drop",
        "git filter-branch",
    ];
    if contem_algum(cmd, &git_destrutivo).is_some() {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Destrutiva,
            "operação Git que descarta ou reescreve histórico/alterações",
        );
    }

    // 7. Remoção de arquivos.
    if programa == "rm" {
        let alvos = caminhos_no_comando(cmd, ctx.roots.first());
        let dentro = !alvos.is_empty()
            && alvos
                .iter()
                .all(|p| ctx.roots.iter().any(|r| normalize_path(p).starts_with(r)));
        if cmd.contains("-r") || cmd.contains("-f") {
            return Analysis::new(
                Decision::Confirmar,
                CommandCategory::Destrutiva,
                if dentro {
                    "remoção recursiva/forçada de arquivos"
                } else {
                    "remoção recursiva/forçada de arquivos fora do projeto"
                },
            );
        }
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Modificacao,
            "remoção de arquivos",
        );
    }

    // 8. Permissões e propriedade.
    if matches!(programa.as_str(), "chmod" | "chown" | "chgrp") {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Sensivel,
            "alteração de permissões ou propriedade",
        );
    }

    // 9. Encerramento de processos.
    if matches!(programa.as_str(), "kill" | "pkill" | "killall") {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Sensivel,
            "encerramento de processos",
        );
    }

    // 10. Contêineres e virtualização.
    if matches!(
        programa.as_str(),
        "docker" | "podman" | "kubectl" | "systemd-nspawn"
    ) {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Sensivel,
            "gerenciamento de contêineres",
        );
    }

    // 11. Rede.
    if matches!(
        programa.as_str(),
        "curl" | "wget" | "nc" | "ncat" | "ssh" | "scp" | "rsync"
    ) {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Sensivel,
            "acesso à rede/transferência de dados",
        );
    }

    // 11b. `find` com apagamento ou execução de comandos.
    if find_destrutivo(cmd) {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Destrutiva,
            "busca que apaga ou executa comandos sobre os resultados",
        );
    }

    // 12. Escrita fora do espaço de trabalho.
    let fora = caminhos_no_comando(cmd, ctx.roots.first())
        .into_iter()
        .filter(|p| !ctx.roots.iter().any(|r| normalize_path(p).starts_with(r)))
        .collect::<Vec<_>>();
    let modifica = matches!(
        programa.as_str(),
        "mv" | "cp" | "touch" | "mkdir" | "tee" | "truncate" | "ln" | "install" | "shred" | "dd"
    ) || cmd.contains('>');
    if modifica && !fora.is_empty() {
        return Analysis::new(
            Decision::Confirmar,
            CommandCategory::Sensivel,
            format!(
                "modificação fora do projeto: {}",
                fora.iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        );
    }

    // 13. Build/testes.
    let build = [
        "cargo build",
        "cargo test",
        "cargo check",
        "cargo clippy",
        "cargo fmt",
        "cargo run",
        "npm test",
        "npm run",
        "yarn ",
        "pnpm ",
        "pytest",
        "python -m pytest",
        "make",
        "cmake",
        "go build",
        "go test",
        "tsc",
        "gradle",
        "mvn ",
        "dotnet build",
        "dotnet test",
        "bun ",
    ];
    if contem_algum(cmd, &build).is_some() || programa == "make" {
        return Analysis::new(
            Decision::Permitir,
            CommandCategory::Build,
            "compilação/teste no projeto",
        );
    }

    // 14. Leitura.
    let leitura = [
        "ls",
        "cat",
        "head",
        "tail",
        "less",
        "more",
        "grep",
        "rg",
        "find",
        "fd",
        "file",
        "wc",
        "stat",
        "pwd",
        "echo",
        "printf",
        "tree",
        "du",
        "df",
        "which",
        "type",
        "date",
        "whoami",
        "id",
        "uname",
        "ps",
        "top",
        "free",
        "git status",
        "git diff",
        "git log",
        "git show",
        "git branch",
        "git remote",
        "git describe",
        "git rev-parse",
        "git blame",
        "cargo metadata",
        "cargo tree",
        "rustc --version",
        "python --version",
        "python3 --version",
        "node --version",
        "npm --version",
        "java -version",
        "sed -n",
        "awk",
        "sort",
        "uniq",
        "cut",
        "jn",
        "jq",
        "yq",
        "diff",
        "comm",
        "md5sum",
        "sha256sum",
        "realpath",
        "readlink",
        "basename",
        "dirname",
        "sleep",
        "true",
        "false",
        "test ",
        "[ ",
    ];
    if leitura
        .iter()
        .any(|p| cmd == *p || cmd.starts_with(&format!("{p} ")))
    {
        // Redirecionamentos transformam leitura em escrita.
        if cmd.contains('>') {
            return Analysis::new(
                Decision::Confirmar,
                CommandCategory::Modificacao,
                "comando de leitura com redirecionamento de saída",
            );
        }
        return Analysis::new(
            Decision::Permitir,
            CommandCategory::Leitura,
            "comando de leitura",
        );
    }

    // 15. Desconhecido: exige confirmação, pois o efeito não pôde ser avaliado.
    Analysis::new(
        Decision::Confirmar,
        CommandCategory::Modificacao,
        format!("efeito do comando `{programa}` não pôde ser avaliado com segurança"),
    )
}

/// Aplica o nível de permissão a uma análise de comando.
///
/// Regra central: `Destrutiva` **sempre** exige confirmação, em qualquer nível
/// — inclusive `Full`. Autonomia máxima vale para as operações de rotina
/// (build, instalação, leitura), não para apagar ou reescrever: o README
/// promete que comandos destrutivos continuam sob confirmação/bloqueio, e o
/// agente não deve remover árvores nem rescritar histórico sem o OK do usuário.
pub fn apply_level(base: Analysis, level: PermissionLevel) -> Analysis {
    // Proibições absolutas valem em todos os níveis.
    if base.category == CommandCategory::Proibida {
        return base;
    }
    match level {
        PermissionLevel::Read => match base.category {
            CommandCategory::Leitura => base,
            _ => Analysis::new(
                Decision::Bloquear,
                base.category,
                format!(
                    "nível Read não permite operações de {} ({}). Use /permissions para mudar.",
                    base.category.label(),
                    base.reason
                ),
            ),
        },
        PermissionLevel::Write => match base.category {
            CommandCategory::Leitura | CommandCategory::Build => base,
            CommandCategory::Modificacao => base,
            CommandCategory::Sensivel | CommandCategory::Destrutiva => {
                Analysis::new(Decision::Confirmar, base.category, base.reason)
            }
            CommandCategory::Proibida => base,
        },
        PermissionLevel::Semi => match base.category {
            CommandCategory::Sensivel | CommandCategory::Destrutiva => {
                Analysis::new(Decision::Confirmar, base.category, base.reason)
            }
            _ => base,
        },
        PermissionLevel::Full => match base.category {
            // Rotina segue autônoma; o destrutivo nunca.
            CommandCategory::Sensivel => Analysis::new(
                Decision::Permitir,
                base.category,
                format!("nível Full permite autonomous: {}", base.reason),
            ),
            CommandCategory::Destrutiva => Analysis::new(
                Decision::Confirmar,
                base.category,
                format!(
                    "operação destrutiva exige confirmação mesmo no nível Full: {}",
                    base.reason
                ),
            ),
            _ => base,
        },
    }
}

/// Classificação de risco de uma ferramenta.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolRisk {
    /// Somente leitura.
    Leitura,
    /// Modifica arquivos dentro do projeto.
    Escrita,
    /// Executa processos / acessa rede / muda o repositório.
    Sensivel,
}

/// Classifica ferramentas nativas por risco.
pub fn tool_risk(tool: &str) -> ToolRisk {
    match tool {
        "list_files"
        | "read_file"
        | "search_code"
        | "inspect_file_metadata"
        | "analyze_project"
        | "list_skills"
        | "load_skill"
        | "git_status"
        | "git_diff"
        | "git_log"
        | "list_processes"
        | "process_status"
        | "read_process_output"
        | "web_search"
        | "fetch_url"
        | "update_plan"
        | "task_status" => ToolRisk::Leitura,
        "write_file" | "create_file" | "edit_file" | "delete_file" | "git_stage" => {
            ToolRisk::Escrita
        }
        "run_command" | "git_commit" | "git_branch" | "download_file" | "run_tests"
        | "kill_process" | "spawn_subagent" => ToolRisk::Sensivel,
        _ => ToolRisk::Sensivel,
    }
}

/// Decide se uma ferramenta pode ser executada no nível atual.
pub fn check_tool(tool: &str, level: PermissionLevel, detalhe: &str) -> Analysis {
    match (tool_risk(tool), level) {
        (ToolRisk::Leitura, _) => Analysis::new(
            Decision::Permitir,
            CommandCategory::Leitura,
            "ferramenta de leitura",
        ),
        (ToolRisk::Escrita, PermissionLevel::Read) => Analysis::new(
            Decision::Bloquear,
            CommandCategory::Modificacao,
            format!(
                "a ferramenta `{tool}` modifica o projeto, mas o nível atual é Read. Use /permissions."
            ),
        ),
        (ToolRisk::Escrita, _) => Analysis::new(
            Decision::Permitir,
            CommandCategory::Modificacao,
            format!("escrita permitida ({detalhe})"),
        ),
        (ToolRisk::Sensivel, PermissionLevel::Read | PermissionLevel::Write) => Analysis::new(
            Decision::Confirmar,
            CommandCategory::Sensivel,
            format!("operação sensível em nível {level}: {detalhe}"),
        ),
        (ToolRisk::Sensivel, PermissionLevel::Semi) => {
            if tool == "run_tests" || tool == "git_status" || tool == "git_diff" {
                Analysis::new(
                    Decision::Permitir,
                    CommandCategory::Build,
                    "operação comum permitida no nível Semi",
                )
            } else {
                Analysis::new(
                    Decision::Confirmar,
                    CommandCategory::Sensivel,
                    format!("operação sensível em nível Semi: {detalhe}"),
                )
            }
        }
        (ToolRisk::Sensivel, PermissionLevel::Full) => Analysis::new(
            Decision::Permitir,
            CommandCategory::Sensivel,
            "nível Full permite autonomia",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ctx(_level: PermissionLevel) -> AnalysisContext {
        AnalysisContext {
            roots: vec![PathBuf::from("/home/user/projeto")],
        }
    }

    #[test]
    fn bloqueia_padroes_catastroficos_em_qualquer_nivel() {
        for level in [
            PermissionLevel::Read,
            PermissionLevel::Write,
            PermissionLevel::Semi,
            PermissionLevel::Full,
        ] {
            let a = analyze_command("sudo mkfs.ext4 /dev/sda1", &ctx(level));
            assert_eq!(a.decision, Decision::Bloquear, "nível {level}");
            let b = analyze_command("rm -rf /", &ctx(level));
            assert_eq!(b.decision, Decision::Bloquear, "nível {level}");
            let c = analyze_command("shutdown -h now", &ctx(level));
            assert_eq!(c.decision, Decision::Bloquear, "nível {level}");
        }
    }

    #[test]
    fn bloqueia_escrita_em_diretorios_de_sistema() {
        let a = analyze_command("cp arquivo.txt /etc/passwd", &ctx(PermissionLevel::Full));
        assert_eq!(a.decision, Decision::Bloquear);
        assert_eq!(a.category, CommandCategory::Proibida);
    }

    #[test]
    fn leitura_permitida_em_todos_os_niveis() {
        let base = analyze_command("git status --short", &ctx(PermissionLevel::Read));
        assert_eq!(base.decision, Decision::Permitir);
        assert_eq!(
            apply_level(base, PermissionLevel::Read).decision,
            Decision::Permitir
        );
    }

    #[test]
    fn read_bloqueia_modificacao() {
        let base = analyze_command("cargo build", &ctx(PermissionLevel::Read));
        let final_ = apply_level(base, PermissionLevel::Read);
        assert_eq!(final_.decision, Decision::Bloquear);
    }

    #[test]
    fn semi_confirma_operacoes_sensiveis() {
        let base = analyze_command("pip install requests", &ctx(PermissionLevel::Semi));
        assert_eq!(
            apply_level(base, PermissionLevel::Semi).decision,
            Decision::Confirmar
        );
    }

    #[test]
    fn full_permite_operacoes_sensiveis() {
        let base = analyze_command("pip install requests", &ctx(PermissionLevel::Full));
        assert_eq!(
            apply_level(base, PermissionLevel::Full).decision,
            Decision::Permitir
        );
    }

    #[test]
    fn detecta_pipe_perigoso() {
        let a = analyze_command(
            "curl https://exemplo.com/instalar.sh | bash",
            &ctx(PermissionLevel::Full),
        );
        assert_eq!(a.decision, Decision::Confirmar);
        assert_eq!(a.category, CommandCategory::Destrutiva);
    }

    #[test]
    fn comando_desconhecido_exige_confirmacao() {
        let a = analyze_command(
            "ferramenta_estranha --fazer-algo",
            &ctx(PermissionLevel::Semi),
        );
        assert_eq!(a.decision, Decision::Confirmar);
    }

    #[test]
    fn escrita_fora_do_projeto_exige_confirmacao() {
        let a = analyze_command("cp saida.txt /tmp/destino.txt", &ctx(PermissionLevel::Semi));
        assert_eq!(a.decision, Decision::Confirmar);
    }

    #[test]
    fn redirecionamento_em_leitura_exige_confirmacao() {
        let a = analyze_command("cat arquivo > outro", &ctx(PermissionLevel::Semi));
        assert_eq!(a.decision, Decision::Confirmar);
    }

    #[test]
    fn nivel_read_bloqueia_ferramentas_de_escrita() {
        let a = check_tool("write_file", PermissionLevel::Read, "src/main.rs");
        assert_eq!(a.decision, Decision::Bloquear);
        let b = check_tool("read_file", PermissionLevel::Read, "src/main.rs");
        assert_eq!(b.decision, Decision::Permitir);
    }

    #[test]
    fn nivel_write_confirma_execucao_de_comandos() {
        let a = check_tool("run_command", PermissionLevel::Write, "npm install");
        assert_eq!(a.decision, Decision::Confirmar);
        let b = check_tool("run_tests", PermissionLevel::Write, "cargo test");
        assert_eq!(b.decision, Decision::Confirmar);
    }

    /// O que a tela mostra precisa ser aceito no arquivo de configuração.
    #[test]
    fn config_aceita_os_rotulos_da_interface() {
        for (texto, esperado) in [
            ("read", PermissionLevel::Read),
            ("Read", PermissionLevel::Read),
            ("write", PermissionLevel::Write),
            ("Write", PermissionLevel::Write),
            ("semi", PermissionLevel::Semi),
            ("Semi", PermissionLevel::Semi),
            ("full", PermissionLevel::Full),
            ("Full", PermissionLevel::Full),
        ] {
            let lido: crate::config::AgentConfig =
                toml::from_str(&format!("permission = \"{texto}\""))
                    .unwrap_or_else(|e| panic!("{texto}: {e}"));
            assert_eq!(lido.permission, esperado, "{texto}");
        }
    }

    #[test]
    fn level_parse_e_cycle() {
        assert_eq!(PermissionLevel::parse("SEMI"), Some(PermissionLevel::Semi));
        assert_eq!(PermissionLevel::parse("xyz"), None);
        assert_eq!(PermissionLevel::Read.cycle(), PermissionLevel::Write);
        assert_eq!(PermissionLevel::Full.cycle(), PermissionLevel::Read);
    }

    fn blocado_ou_confirmado(cmd: &str, level: PermissionLevel) -> bool {
        let a = apply_level(analyze_command(cmd, &ctx(level)), level);
        a.decision != Decision::Permitir
    }

    /// `;`/`&&`/`|` escondiam um comando destrutivo atrás de um prefixo
    /// inofensivo: a análise só olhava a primeira palavra.
    #[test]
    fn comando_composto_nao_esconde_comando_destrutivo() {
        // No nível Read nada disso pode passar — antes passava tudo.
        for cmd in [
            "echo x; rm -rf ~/docs",
            "echo x && git reset --hard",
            "ls | rm -rf /home/user/projeto",
            "echo x; pip install requisitos",
        ] {
            let a = apply_level(
                analyze_command(cmd, &ctx(PermissionLevel::Read)),
                PermissionLevel::Read,
            );
            assert_ne!(a.decision, Decision::Permitir, "Read liberou: {cmd}");
        }
        // Os destrutivos exigem confirmação em qualquer nível.
        for level in [
            PermissionLevel::Read,
            PermissionLevel::Write,
            PermissionLevel::Semi,
            PermissionLevel::Full,
        ] {
            for cmd in [
                "echo x; rm -rf ~/docs",
                "echo x && git reset --hard",
                "ls | rm -rf /home/user/projeto",
            ] {
                assert!(
                    blocado_ou_confirmado(cmd, level),
                    "nível {level} liberou: {cmd}"
                );
            }
        }
    }

    /// `find` com `-delete`/`-exec` apaga arquivos; não é leitura.
    #[test]
    fn find_destrutivo_nao_e_leitura() {
        for cmd in [
            "find / -delete",
            "find . -name '*.tmp' -delete",
            "find /tmp -exec rm -rf {} +",
        ] {
            let a = analyze_command(cmd, &ctx(PermissionLevel::Full));
            assert_eq!(a.category, CommandCategory::Destrutiva, "{cmd}");
            assert_ne!(a.decision, Decision::Permitir, "{cmd}");
        }
    }

    /// A remoção da raiz escapa dos padrões literais quando as flags vêm
    /// separadas ou por extenso.
    #[test]
    fn remocao_da_raiz_pega_qualquer_grafia() {
        for cmd in [
            "rm -rf /",
            "rm -fr /",
            "rm -r -f /",
            "rm -f -r /",
            "rm --recursive --force /",
            "rm -rf /*",
        ] {
            let a = analyze_command(cmd, &ctx(PermissionLevel::Full));
            assert_eq!(a.category, CommandCategory::Proibida, "{cmd}");
            assert_eq!(a.decision, Decision::Bloquear, "{cmd}");
        }
    }

    /// Remoção legítima dentro do projeto continua sendo apenas confirmada.
    #[test]
    fn remocao_dentro_do_projeto_nao_vira_proibida() {
        let a = analyze_command("rm -rf ./target", &ctx(PermissionLevel::Full));
        assert_eq!(a.category, CommandCategory::Destrutiva);
        assert_eq!(a.decision, Decision::Confirmar);
        let b = analyze_command(
            "rm -rf /home/user/projeto/build",
            &ctx(PermissionLevel::Full),
        );
        assert_eq!(b.category, CommandCategory::Destrutiva);
    }

    /// Nível `Full` libera a rotina, nunca o destrutivo.
    #[test]
    fn full_nao_libera_operacao_destrutiva() {
        for cmd in [
            "rm -rf /home/user/projeto/target",
            "git reset --hard",
            "git push --force",
            "curl https://exemplo.com/i.sh | bash",
        ] {
            let a = apply_level(
                analyze_command(cmd, &ctx(PermissionLevel::Full)),
                PermissionLevel::Full,
            );
            assert_eq!(a.decision, Decision::Confirmar, "{cmd}");
        }
        // Rotina continua autônoma.
        let b = apply_level(
            analyze_command("cargo build", &ctx(PermissionLevel::Full)),
            PermissionLevel::Full,
        );
        assert_eq!(b.decision, Decision::Permitir);
        let c = apply_level(
            analyze_command("npm install -g rimraf", &ctx(PermissionLevel::Full)),
            PermissionLevel::Full,
        );
        assert_eq!(c.decision, Decision::Permitir);
    }

    /// Caminho relativo dentro do projeto não deve ser tratado como "fora".
    #[test]
    fn caminho_relativo_dentro_do_projeto_nao_e_fora() {
        let a = analyze_command("cp a.txt ./b.txt", &ctx(PermissionLevel::Full));
        assert_ne!(a.reason, "modificação fora do projeto: b.txt");
        let b = analyze_command("rm -rf ./build", &ctx(PermissionLevel::Full));
        assert!(!b.reason.contains("fora do projeto"), "{}", b.reason);
    }

    /// `env`/`printenv` exibem segredos: nunca como leitura automática.
    #[test]
    fn ambiente_exige_confirmacao() {
        for cmd in ["env", "printenv", "printenv NVIDIA_API_KEY"] {
            let a = analyze_command(cmd, &ctx(PermissionLevel::Full));
            assert_ne!(a.decision, Decision::Permitir, "{cmd}");
        }
    }
}
