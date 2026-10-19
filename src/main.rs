//! Kestrel — agente autônomo de codificação para o terminal.
//!
//! Ponto de entrada: interpreta os argumentos de linha de comando, prepara os
//! caminhos persistentes, carrega (ou cria) a configuração, abre o banco
//! local e executa o assistente de primeira execução ou a interface principal.
//! Erros de terminal são restaurados em qualquer caminho de saída.

mod agent;
mod app;
mod comps;
mod config;
mod db;
mod diff;
mod events;
mod html;
mod migrar;
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
use crate::ui::mascot::PipGrafico;
use crate::util::Paths;
use clap::Parser;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, EnableMouseCapture,
};
use ratatui_image::picker::{Picker, ProtocolType};
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
        eprintln!("kestrel: aviso: {aviso}");
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
        // Escolha explícita do usuário: registra a entrada no catálogo local
        // como manual (senão, uma atualização do catálogo a sobrescreveria).
        config.provider.model = modelo.clone();
        let mut entry = crate::provider::catalog::enrich(ModelEntry::new(modelo.clone()));
        entry.source = crate::config::ModelSource::Manual;
        config.upsert_model(entry);
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

    // A skin é aplicada antes de qualquer desenho. `KESTREL_SKIN` permite
    // forçar uma skin numa sessão sem alterar a configuração salva. Skins
    // próprias vivem em `~/.config/kestrel/skins/<nome>.toml`.
    let skin = std::env::var("KESTREL_SKIN")
        .ok()
        .unwrap_or_else(|| config.ui.skin.clone());
    ui::theme::inicializar(&paths.skins_dir(), &skin);

    let store = Store::open(&paths.db_file())?;

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
        // O Pip é uma imagem de verdade: consulta o terminal depois de entrar
        // na tela alternativa e antes de começar a ler eventos. Se a consulta
        // falhar, o próprio ratatui-image cai para halfblocks; não existe mais
        // a textura 6×4 desenhada à mão.
        let pip_grafico = if config.ui.show_mascot {
            // A consulta ao terminal continua (ela acerta a métrica real da
            // fonte), mas o Pip é desenhado em halfblocks: protocolos gráficos
            // como Sixel/Kitty podem deixar “rastro”/duplicar o mascote ao
            // redesenhar o mesmo quadro várias vezes. Halfblocks é célula, então
            // cada animação limpa e substitui a anterior de forma confiável.
            let mut picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
            picker.set_protocol_type(ProtocolType::Halfblocks);
            PipGrafico::novo(&picker).ok()
        } else {
            None
        };
        // A interface trata rolagem do mouse e colagem de texto (bracketed paste).
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
        let resultado =
            executar(&mut terminal, config, &paths, store, cli.setup, pip_grafico).await;
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
    pip_grafico: Option<PipGrafico>,
) -> anyhow::Result<()> {
    let config = if !config.setup_complete || forcar_setup {
        let pronto = ui::setup::executar(terminal, config, pip_grafico.clone()).await?;
        pronto.save(paths)?;
        pronto
    } else {
        config
    };
    app::executar(terminal, config, paths.clone(), store, pip_grafico).await
}
