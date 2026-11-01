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
mod i18n;
mod index;
mod migrar;
mod perm;
mod project;
mod provider;
mod sandbox;
mod skills;
mod tokenizer;
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
use ratatui_image::picker::Picker;
use std::path::PathBuf;

/// Versão exibida na interface e em `--version`.
///
/// O Cargo exige semver de três componentes, então o pacote continua em
/// `0.9.5`; o hotfix é identificado como `0.9.5.5`.
pub const VERSAO: &str = "0.9.8";

/// Kestrel — agente autônomo de codificação para o terminal.
#[derive(Parser, Debug)]
#[command(
    name = "kestrel",
    version = VERSAO,
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
    crate::tokenizer::registrar_modelos(&config.provider.models);
    if config.agent.sandbox == crate::config::SandboxMode::Require && !crate::sandbox::disponivel()
    {
        anyhow::bail!(
            "sandbox exigido em agent.sandbox, mas este kernel não oferece Landlock; \
             use `sandbox = \"auto\"` ou \"off\""
        );
    }

    // O nível pedido na linha de comando vale só para esta execução. Ele é
    // guardado à parte porque `Config::save` (bloco do `--model` e o assistente
    // de configuração) persistiria o override no config.toml do usuário para
    // sempre: `kestrel -p full -m x` não pode marcar `permission = "full"`
    // em todas as sessões seguintes.
    let permissao_sessao = if let Some(valor) = &cli.permissao {
        let nivel = PermissionLevel::parse(valor).ok_or_else(|| {
            anyhow::anyhow!(
                "nível de permissão inválido: `{valor}` (use read, write, semi ou full)"
            )
        })?;
        Some(nivel)
    } else {
        None
    };

    if let Some(dir) = &cli.workdir {
        let dir = crate::util::expand_tilde(&dir.to_string_lossy());
        if !dir.is_dir() {
            anyhow::bail!("diretório de trabalho inválido: `{}`", dir.display());
        }
        config.agent.workdir = Some(std::fs::canonicalize(&dir).unwrap_or(dir));
        // Sem fixar, `Config::workdir()` ignora o valor e usa o diretório
        // atual: a opção `-C` não teria efeito nenhum numa configuração nova.
        config.agent.pin_workdir = true;
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
        // O Pip e o ícone usam o protocolo real do terminal (Sixel/Kitty quando
        // disponível): halfblocks deixava o Pip pixelado. O Picker é consultado
        // depois da tela alternativa e acerta a métrica real da fonte.
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        let pip_grafico = if config.ui.show_mascot {
            PipGrafico::novo(&picker).ok()
        } else {
            None
        };
        // A interface trata rolagem do mouse e colagem de texto (bracketed paste).
        let _ = crossterm::execute!(std::io::stdout(), EnableMouseCapture, EnableBracketedPaste);
        // O guard restaura também num panic: o hook do ratatui devolve o modo
        // bruto e a tela alternativa, mas não desliga mouse/paste. Sem isso, um
        // panic deixava o terminal do usuário colando e reportando mouse.
        let _restaurar = RestaurarTerminal;
        executar(
            &mut terminal,
            config,
            &paths,
            store,
            SessaoAtual {
                forcar_setup: cli.setup,
                permissao: permissao_sessao,
            },
            pip_grafico,
        )
        .await
    })
}

/// Restaura os modos de entrada do terminal no fim da execução — inclusive
/// durante o desenrolar de um panic.
struct RestaurarTerminal;

impl Drop for RestaurarTerminal {
    fn drop(&mut self) {
        let _ = crossterm::execute!(
            std::io::stdout(),
            DisableBracketedPaste,
            DisableMouseCapture
        );
        ratatui::restore();
    }
}

/// Ajustes que valem só para esta execução — nada aqui é persistido.
struct SessaoAtual {
    /// Força o assistente de configuração (`--setup`).
    forcar_setup: bool,
    /// Nível pedido em `-p/--permissions`.
    permissao: Option<PermissionLevel>,
}

/// Executa o assistente (quando necessário) e a interface principal,
/// restaurando o terminal mesmo em caso de erro.
async fn executar(
    terminal: &mut ratatui::DefaultTerminal,
    config: Config,
    paths: &Paths,
    store: Store,
    sessao: SessaoAtual,
    pip_grafico: Option<PipGrafico>,
) -> anyhow::Result<()> {
    let mut config = if !config.setup_complete || sessao.forcar_setup {
        // O mascote não aparece no assistente: ele é elemento de nova sessão.
        let pronto = ui::setup::executar(terminal, config, None).await?;
        pronto.save(paths)?;
        pronto
    } else {
        config
    };
    // O override de sessão entra aqui, depois de tudo que persiste a
    // configuração: ele não pode sobreviver a esta execução.
    if let Some(nivel) = sessao.permissao {
        config.aplicar_permissao_sessao(nivel);
    }
    app::executar(terminal, config, paths.clone(), store, pip_grafico).await
}
