//! Kestrel — agente autônomo de codificação para o terminal.
//!
//! Ponto de entrada: interpreta os argumentos de linha de comando, prepara os
//! caminhos persistentes, carrega (ou cria) a configuração, abre o banco
//! local e executa o assistente de primeira execução ou a interface principal.
//! Erros de terminal são restaurados em qualquer caminho de saída.

mod agent;
mod app;
mod config;
mod db;
mod diff;
mod events;
mod html;
mod perm;
mod project;
mod provider;
mod skills;
mod tools;
mod ui;
mod util;

use crate::config::{Config, ModelEntry};
use crate::db::Store;
use crate::perm::PermissionLevel;
use crate::util::Paths;
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use std::path::PathBuf;

/// Kestrel — agente autônomo de codificação para o terminal.
#[derive(Parser, Debug)]
#[command(
    name = "kestrel",
    version,
    about = "Agente autônomo de codificação para o terminal",
    long_about = "O Kestrel analisa o projeto, planeja, executa comandos reais, edita arquivos \
                  e valida o resultado com testes — tudo no seu terminal, com permissões \
                  explícitas e histórico local.\n\nA chave de API é lida apenas da variável de \
                  ambiente (padrão: NVIDIA_API_KEY) e nunca é exibida nem gravada."
)]
struct Cli {
    /// Diretório de trabalho do agente (padrão: diretório atual).
    #[arg(short = 'C', long = "workdir", value_name = "DIR")]
    workdir: Option<PathBuf>,

    /// Modelo a usar nesta sessão (o catálogo local é atualizado).
    #[arg(short = 'm', long = "model", value_name = "MODELO")]
    modelo: Option<String>,

    /// Nível de permissão inicial: read, write, semi ou full.
    #[arg(short = 'p', long = "permissions", value_name = "NÍVEL")]
    permissao: Option<String>,

    /// Diretório de configuração (padrão: KESTREL_CONFIG_DIR ou ~/.config/kestrel).
    #[arg(long = "config-dir", value_name = "DIR")]
    config_dir: Option<PathBuf>,

    /// Diretório de dados: banco, logs e sessões (padrão: KESTREL_DATA_DIR ou ~/.local/share/kestrel).
    #[arg(long = "data-dir", value_name = "DIR")]
    data_dir: Option<PathBuf>,

    /// Refaz a configuração inicial antes de abrir a interface.
    #[arg(long = "setup")]
    setup: bool,
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let paths = Paths::custom(cli.config_dir, cli.data_dir);
    paths.ensure()?;

    let (mut config, aviso_config) = Config::load(&paths);
    if let Some(aviso) = &aviso_config {
        eprintln!("kestrel: aviso: {aviso}; usando a configuração padrão");
    }

    if let Some(valor) = &cli.permissao {
        let nivel = PermissionLevel::parse(valor).ok_or_else(|| {
            anyhow::anyhow!(
                "nível de permissão inválido: `{valor}` (use read, write, semi ou full)"
            )
        })?;
        config.agent.permission = nivel;
    }

    if let Some(dir) = &cli.workdir {
        let dir = crate::util::expand_tilde(&dir.to_string_lossy());
        if !dir.is_dir() {
            anyhow::bail!("diretório de trabalho inválido: `{}`", dir.display());
        }
        config.agent.workdir = Some(std::fs::canonicalize(&dir).unwrap_or(dir));
    }

    if let Some(modelo) = &cli.modelo {
        // Escolha explícita do usuário: registra a entrada no catálogo local.
        config.provider.model = modelo.clone();
        config.upsert_model(crate::provider::catalog::enrich(ModelEntry::new(
            modelo.clone(),
        )));
        config.save(&paths)?;
    }

    if cli.setup {
        config.setup_complete = false;
    }

    if config.setup_complete && !config.provider_pronto() {
        eprintln!(
            "kestrel: aviso: chave de API ausente ou modelo não definido; \
             use /config na interface ou reabra o assistente com --setup"
        );
    }

    let store = Store::open(&paths.db_file())?;

    // Sem diretório definido na configuração, retoma o último projeto
    // registrado pela interface (troca de projeto com /projetos).
    if config.agent.workdir.is_none()
        && let Ok(Some(dir)) = store.kv_get("ultimo_projeto")
    {
        let dir = PathBuf::from(dir);
        if dir.is_dir() {
            config.agent.workdir = Some(std::fs::canonicalize(&dir).unwrap_or(dir));
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| anyhow::anyhow!("falha ao iniciar o runtime assíncrono: {e}"))?;

    runtime.block_on(async move {
        let mut terminal = ratatui::try_init().map_err(|e| {
            anyhow::anyhow!(
                "falha ao iniciar o terminal: {e}. O Kestrel precisa de um terminal interativo \
                 (TTY); redirecionamento de entrada/saída não é suportado."
            )
        })?;
        // A interface trata rolagem do mouse e colagem de texto (bracketed paste).
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
        let resultado = executar(&mut terminal, config, &paths, store, cli.setup).await;
        let _ = crossterm::execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture
        );
        ratatui::restore();
        resultado
    })
}

/// Executa o assistente (quando necessário) e a interface principal,
/// restaurando o terminal mesmo em caso de erro.
async fn executar(
    terminal: &mut ratatui::DefaultTerminal,
    config: Config,
    paths: &Paths,
    store: Store,
    forcar_setup: bool,
) -> anyhow::Result<()> {
    let config = if !config.setup_complete || forcar_setup {
        let pronto = ui::setup::executar(terminal, config).await?;
        pronto.save(paths)?;
        pronto
    } else {
        config
    };
    app::executar(terminal, config, paths.clone(), store).await
}
