//! Confinamento real de processos filhos com Landlock (Linux).
//!
//! A camada de permissões decide *o que* o agente pode pedir; o sandbox
//! garante que o processo filho não consiga escrever fora das raízes
//! autorizadas nem abrir conexões TCP (quando desligado). Leitura do sistema
//! continua permitida para compiladores e ferramentas funcionarem.

use crate::config::SandboxMode;
use landlock::{
    ABI, Access, AccessFs, AccessNet, BitFlags, CompatLevel, Compatible, Ruleset, RulesetAttr,
    RulesetCreated, RulesetCreatedAttr, RulesetStatus,
};
use std::path::{Path, PathBuf};

/// Configuração efetiva do sandbox para uma chamada.
#[derive(Clone, Debug)]
pub struct SandboxSettings {
    pub mode: SandboxMode,
    pub allow_network: bool,
    pub extra_writable: Vec<PathBuf>,
}

impl Default for SandboxSettings {
    fn default() -> Self {
        Self {
            mode: SandboxMode::Off,
            allow_network: true,
            extra_writable: Vec::new(),
        }
    }
}

impl SandboxSettings {
    pub fn novo(mode: SandboxMode, allow_network: bool, extra_writable: Vec<PathBuf>) -> Self {
        Self {
            mode,
            allow_network,
            extra_writable,
        }
    }

    pub fn ativo(&self) -> bool {
        self.mode != SandboxMode::Off
    }
}

/// Sonda o kernel uma única vez (criar um ruleset não aplica restrição).
pub fn disponivel() -> bool {
    static DISPONIVEL: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *DISPONIVEL.get_or_init(|| {
        let abi = ABI::V4;
        Ruleset::default()
            .handle_access(AccessFs::from_all(abi))
            .ok()
            .and_then(|r| r.create().ok())
            .is_some()
    })
}

/// Prepara as regras que serão aplicadas no filho via `restrict_self`.
///
/// Devolve `None` quando o sandbox está desligado ou indisponível em modo
/// `Auto`. Em modo `Require`, indisponibilidade é erro.
pub fn preparar(
    config: &SandboxSettings,
    cwd: &Path,
    roots: &[PathBuf],
) -> anyhow::Result<Option<RulesetCreated>> {
    if !config.ativo() {
        return Ok(None);
    }
    if !disponivel() {
        if config.mode == SandboxMode::Require {
            anyhow::bail!("sandbox exigido, mas o kernel não oferece Landlock");
        }
        return Ok(None);
    }

    let abi = ABI::V4;
    let mut ruleset = Ruleset::default().handle_access(AccessFs::from_all(abi))?;
    if !config.allow_network {
        ruleset = ruleset
            .handle_access(AccessNet::BindTcp)?
            .handle_access(AccessNet::ConnectTcp)?;
    }
    let ruleset = ruleset.create()?;

    // Leitura e execução em todo o sistema: compiladores, bibliotecas e
    // binários precisam ser legíveis. O confinamento é de escrita.
    let leitura: BitFlags<AccessFs> = AccessFs::from_read(abi);
    let mut regras = ruleset.add_rules(landlock::path_beneath_rules(["/"], leitura))?;

    // Escrita apenas nas raízes do projeto, diretórios extras e caches que
    // ferramentas reais usam (cargo/npm/pip/temp).
    let mut gravaveis: Vec<PathBuf> = Vec::new();
    gravaveis.push(cwd.to_path_buf());
    gravaveis.extend(roots.iter().cloned());
    gravaveis.extend(config.extra_writable.iter().cloned());
    if let Ok(tmp) = std::env::var("TMPDIR") {
        gravaveis.push(PathBuf::from(tmp));
    }
    gravaveis.push(PathBuf::from("/tmp"));
    gravaveis.push(PathBuf::from("/var/tmp"));
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty()
    {
        let home = PathBuf::from(home);
        gravaveis.push(home.join(".cache"));
        gravaveis.push(home.join(".cargo"));
        gravaveis.push(home.join(".npm"));
        // Ferramentas comuns de plugins MCP (uv/uvx, pipx).
        gravaveis.push(home.join(".local/share/uv"));
        gravaveis.push(home.join(".local/share/pipx"));
    }
    if let Ok(cargo_home) = std::env::var("CARGO_HOME")
        && !cargo_home.is_empty()
    {
        gravaveis.push(PathBuf::from(cargo_home));
    }
    for dispositivo in ["/dev/null", "/dev/zero", "/dev/urandom"] {
        gravaveis.push(PathBuf::from(dispositivo));
    }
    gravaveis.sort();
    gravaveis.dedup();
    // `path_beneath_rules` falha em caminho inexistente ou em link especial
    // (`/dev/stdout` aponta para `/proc/self/fd/1`, que não aceita regra).
    let gravaveis: Vec<PathBuf> = gravaveis
        .into_iter()
        .filter(|p| {
            p.exists()
                && std::fs::symlink_metadata(p)
                    .map(|m| !m.file_type().is_symlink())
                    .unwrap_or(false)
        })
        .collect();
    let escrita: BitFlags<AccessFs> = AccessFs::from_all(abi);
    regras = regras.add_rules(landlock::path_beneath_rules(gravaveis, escrita))?;

    Ok(Some(regras.set_compatibility(CompatLevel::BestEffort)))
}

/// Aplica as regras no processo atual (chamado dentro de `pre_exec`).
pub fn aplicar(regras: RulesetCreated) -> std::io::Result<()> {
    let status = regras
        .restrict_self()
        .map_err(|e| std::io::Error::other(format!("sandbox Landlock: {e}")))?;
    if status.ruleset == RulesetStatus::NotEnforced {
        return Err(std::io::Error::other(
            "sandbox Landlock não pôde ser aplicado neste kernel",
        ));
    }
    Ok(())
}

/// Instala as regras num `Command` já configurado.
#[cfg(unix)]
pub fn aplicar_no_comando(
    cmd: &mut tokio::process::Command,
    config: &SandboxSettings,
    cwd: &Path,
    roots: &[PathBuf],
) -> anyhow::Result<()> {
    use std::os::unix::process::CommandExt;
    let Some(regras) = preparar(config, cwd, roots)? else {
        return Ok(());
    };
    // O sucesso/falha do `restrict_self` acontece no filho; o `pre_exec`
    // devolve o erro para o `spawn`.
    let regras = std::sync::Mutex::new(Some(regras));
    unsafe {
        cmd.as_std_mut().pre_exec(move || {
            let regras = regras
                .lock()
                .map_err(|_| std::io::Error::other("lock do sandbox envenenado"))?
                .take()
                .ok_or_else(|| std::io::Error::other("regras do sandbox ausentes"))?;
            aplicar(regras)
        });
    }
    Ok(())
}

#[cfg(not(unix))]
pub fn aplicar_no_comando(
    _cmd: &mut tokio::process::Command,
    _config: &SandboxSettings,
    _cwd: &Path,
    _roots: &[PathBuf],
) -> anyhow::Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn modo_off_nao_prepara_regras() {
        let cfg = SandboxSettings::novo(SandboxMode::Off, true, vec![]);
        assert!(preparar(&cfg, Path::new("/tmp"), &[]).unwrap().is_none());
    }

    #[test]
    fn modo_auto_nao_falha_em_kernel_sem_landlock() {
        // Em kernel com Landlock, prepara; sem, devolve None. Nunca erro em Auto.
        let cfg = SandboxSettings::novo(SandboxMode::Auto, true, vec![]);
        let _ = preparar(&cfg, Path::new("/tmp"), &[]).unwrap();
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn sandbox_impede_escrita_fora_das_raizes() {
        if !disponivel() {
            return;
        }
        let raiz =
            std::env::temp_dir().join(format!("kestrel-sandbox-{}", uuid::Uuid::new_v4().simple()));
        let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
        let fora = PathBuf::from(home).join(format!(
            ".kestrel-sandbox-fora-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&raiz).unwrap();
        std::fs::create_dir_all(&fora).unwrap();
        let cfg = SandboxSettings::novo(SandboxMode::Require, true, vec![]);

        let dentro_arquivo = raiz.join("dentro.txt");
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("echo ok > {}", dentro_arquivo.display()));
        aplicar_no_comando(&mut cmd, &cfg, &raiz, std::slice::from_ref(&raiz)).unwrap();
        let status = cmd.status().await.unwrap();
        assert!(status.success(), "escrita dentro da raiz deveria funcionar");
        assert!(dentro_arquivo.exists());

        let fora_arquivo = fora.join("fora.txt");
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(format!("echo x > {}", fora_arquivo.display()));
        aplicar_no_comando(&mut cmd, &cfg, &raiz, std::slice::from_ref(&raiz)).unwrap();
        let resultado = cmd.status().await;
        if let Ok(status) = resultado {
            assert!(!status.success(), "escrita fora deveria falhar");
        }
        // falha ao aplicar/encerrar também é confinamento
        assert!(!fora_arquivo.exists(), "arquivo fora não pode existir");
        let _ = std::fs::remove_dir_all(&raiz);
        let _ = std::fs::remove_dir_all(&fora);
    }
}
