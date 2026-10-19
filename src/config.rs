//! Configuração do Kestrel, persistida em TOML.

use crate::perm::PermissionLevel;
use crate::util::Paths;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Nível de confiança nas capacidades de um modelo.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum Capability {
    /// Não há informação confiável sobre suporte.
    #[default]
    Desconhecido,
    Sim,
    // Os rótulos que a interface mostra também são aceitos: quem edita o
    // config.toml "como a tela manda" não perde a configuração inteira.
    #[serde(alias = "não")]
    Nao,
}

impl Capability {
    pub fn label(&self) -> &'static str {
        match self {
            Capability::Desconhecido => "desconhecido",
            Capability::Sim => "sim",
            Capability::Nao => "não",
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModelSource {
    #[default]
    Descoberto,
    Manual,
}

/// Entrada do catálogo de modelos.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct ModelEntry {
    pub id: String,
    #[serde(default)]
    pub display_name: Option<String>,
    #[serde(default)]
    pub source: ModelSource,
    #[serde(default)]
    pub supports_tools: Capability,
    #[serde(default)]
    pub supports_streaming: Capability,
    #[serde(default)]
    pub supports_reasoning: Capability,
    #[serde(default)]
    pub supports_vision: Capability,
    #[serde(default)]
    pub context_window: Option<u32>,
    #[serde(default)]
    pub reasoning_efforts: Vec<String>,
    #[serde(default)]
    pub notes: Option<String>,
    #[serde(default)]
    pub owned_by: Option<String>,
}

impl ModelEntry {
    pub fn new(id: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            display_name: None,
            source: ModelSource::Descoberto,
            supports_tools: Capability::Desconhecido,
            supports_streaming: Capability::Desconhecido,
            supports_reasoning: Capability::Desconhecido,
            supports_vision: Capability::Desconhecido,
            context_window: None,
            reasoning_efforts: Vec::new(),
            notes: None,
            owned_by: None,
        }
    }

    pub fn label(&self) -> String {
        self.display_name.clone().unwrap_or_else(|| self.id.clone())
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct ProviderConfig {
    /// Identificador do provedor (por enquanto somente `nvidia`).
    pub id: String,
    pub base_url: String,
    pub api_key_env: String,
    /// Modelo selecionado (id real do provedor). Nunca trocado automaticamente.
    pub model: String,
    /// Esforço de raciocínio selecionado, quando suportado.
    pub reasoning_effort: Option<String>,
    pub max_tokens: u32,
    pub temperature: f32,
    pub request_timeout_secs: u64,
    pub max_retries: u32,
    pub stream: bool,
    /// Modelos conhecidos (descobertos via API e cadastrados manualmente).
    pub models: Vec<ModelEntry>,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            id: "nvidia".to_string(),
            base_url: "https://integrate.api.nvidia.com/v1".to_string(),
            api_key_env: "NVIDIA_API_KEY".to_string(),
            model: String::new(),
            reasoning_effort: None,
            max_tokens: 8192,
            temperature: 0.2,
            request_timeout_secs: 300,
            max_retries: 3,
            stream: true,
            models: Vec::new(),
        }
    }
}

/// Modo de operação do agente.
#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq, Default)]
#[serde(rename_all = "lowercase")]
pub enum ModoOperacao {
    /// Somente conversa e leitura: nenhuma ferramenta altera arquivos ou executa comandos.
    #[serde(alias = "conversa")]
    Chat,
    /// Trabalho completo no projeto, sempre respeitando o nível de permissão.
    #[default]
    #[serde(alias = "agent")]
    Agente,
}

impl ModoOperacao {
    pub fn label(&self) -> &'static str {
        match self {
            ModoOperacao::Chat => "chat",
            ModoOperacao::Agente => "build",
        }
    }

    pub fn description(&self) -> &'static str {
        match self {
            ModoOperacao::Chat => {
                "conversa e leitura apenas; nada no projeto é modificado ou executado"
            }
            ModoOperacao::Agente => {
                "trabalha no projeto: lê, planeja, edita e executa conforme as permissões"
            }
        }
    }

    pub fn parse(valor: &str) -> Option<Self> {
        match valor.trim().to_lowercase().as_str() {
            "chat" | "conversa" => Some(ModoOperacao::Chat),
            "agente" | "agent" | "build" | "trabalho" => Some(ModoOperacao::Agente),
            _ => None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct AgentConfig {
    pub permission: PermissionLevel,
    /// Modo de operação atual (chat ou agente).
    pub modo: ModoOperacao,
    /// Fração do contexto utilizável que dispara o dossiê de continuidade.
    pub context_compact_threshold: f32,
    /// Tokens reservados para a resposta do modelo.
    pub reserve_response_tokens: u32,
    /// Janela de contexto presumida quando o modelo não informa.
    pub default_context_window: u32,
    /// Analisa o projeto automaticamente no início de cada sessão.
    pub auto_analyze_project: bool,
    /// Diretório de trabalho (vazio = diretório atual).
    pub workdir: Option<PathBuf>,
    /// Fixa o diretório de trabalho entre execuções.
    ///
    /// Por padrão `false`: o Kestrel começa no diretório de onde foi chamado,
    /// não no último projeto usado. Só `/projetos` (escolha explícita) fixa um
    /// diretório, gravando `true` aqui.
    #[serde(default)]
    pub pin_workdir: bool,
    /// Diretórios adicionais permitidos fora do espaço de trabalho.
    pub external_dirs: Vec<PathBuf>,
    /// Número máximo de mensagens mantidas no contexto enviado ao modelo.
    pub max_history_messages: usize,
    /// Segundos máximos por incidente de recuperação automática.
    pub recovery_timeout_secs: u64,
    /// Tentativas máximas de correção por incidente.
    pub recovery_max_attempts: u32,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            permission: PermissionLevel::Semi,
            modo: ModoOperacao::Agente,
            context_compact_threshold: 0.90,
            reserve_response_tokens: 4096,
            default_context_window: 128_000,
            auto_analyze_project: true,
            workdir: None,
            pin_workdir: false,
            external_dirs: Vec::new(),
            max_history_messages: 200,
            recovery_timeout_secs: 1200,
            recovery_max_attempts: 15,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct UiConfig {
    pub show_mascot: bool,
    pub max_input_history: usize,
    pub show_reasoning: bool,
    pub language: String,
    /// Skin visual ativa (`kestrel`, `mono` ou `daylight`).
    pub skin: String,
    /// Comp (personalidade) ativo; `None` usa o tom padrão do Kestrel.
    pub active_comp: Option<String>,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            show_mascot: true,
            max_input_history: 200,
            show_reasoning: false,
            language: "pt-BR".to_string(),
            skin: "mono".to_string(),
            active_comp: None,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug)]
#[serde(default)]
pub struct WebConfig {
    /// Mecanismo de pesquisa padrão: `duckduckgo` (sem chave) ou `searx`.
    pub engine: String,
    pub searx_url: Option<String>,
    pub user_agent: String,
    pub max_download_bytes: u64,
    /// Navegador para páginas que exigem JavaScript (detectado automaticamente).
    pub browser_bin: Option<String>,
    pub request_timeout_secs: u64,
}

impl Default for WebConfig {
    fn default() -> Self {
        Self {
            engine: "duckduckgo".to_string(),
            searx_url: None,
            user_agent: format!(
                "Mozilla/5.0 (X11; Linux x86_64) Kestrel/{} (agente de codificação; +terminal)",
                env!("CARGO_PKG_VERSION")
            ),
            max_download_bytes: 50 * 1024 * 1024,
            browser_bin: None,
            request_timeout_secs: 60,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, Default)]
#[serde(default)]
pub struct Config {
    pub setup_complete: bool,
    pub provider: ProviderConfig,
    pub agent: AgentConfig,
    pub ui: UiConfig,
    pub web: WebConfig,
}

impl Config {
    /// Carrega a configuração do disco.
    ///
    /// Devolve a configuração e um aviso opcional. Duas situações agora são
    /// distintas — antes qualquer problema resultava em `Config::default()`
    /// silenciosamente, apagando modelo e permissão do usuário:
    ///
    /// - **arquivo ausente**: padrão, sem aviso (primeira execução);
    /// - **arquivo corrompido**: padrão + aviso claro, **mas** o arquivo ruim é
    ///   preservado como `.corrompido-<data>` em vez de ser sobrescrito, para o
    ///   usuário poder recuperar o que tinha;
    /// - **valor fora da faixa**: o valor é **preservado** (não descartamos a
    ///   configuração do usuário) e um aviso lista o que revisar.
    pub fn load(paths: &Paths) -> (Self, Option<String>) {
        let file = paths.config_file();
        let texto = match std::fs::read_to_string(&file) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return (Config::default(), None),
            Err(e) => {
                return (
                    Config::default(),
                    Some(format!(
                        "não foi possível ler a configuração em {}: {e}. \
                         Usando a configuração padrão.",
                        file.display()
                    )),
                );
            }
        };
        let inteiro: Config = match toml::from_str::<Config>(&texto) {
            Ok(cfg) => cfg,
            Err(err) => {
                // Preserva o original antes de qualquer reescrita.
                let reserva = file.with_extension(format!(
                    "corrompido-{}",
                    crate::util::now_rfc3339().replace(':', "-")
                ));
                let _ = std::fs::rename(&file, &reserva);
                return (
                    Config::default(),
                    Some(format!(
                        "configuração inválida em {}: {err}.\n\
                         O arquivo foi preservado como {}. \
                         A configuração foi reiniciada no padrão.",
                        file.display(),
                        reserva.display()
                    )),
                );
            }
        };
        let avisos = inteiro.validar();
        if avisos.is_empty() {
            (inteiro, None)
        } else {
            let lista = avisos.join("; ");
            (inteiro, Some(format!("verifique a configuração: {lista}")))
        }
    }

    /// Valores que, embaralhados à mão, fariam o agente falhar de forma
    /// obscura. Nada aqui muda o comportamento: só avisa.
    fn validar(&self) -> Vec<String> {
        let mut avisos = Vec::new();
        if self.provider.base_url.trim().is_empty() {
            avisos.push("provider.base_url vazio (nenhuma requisição seria possível)".into());
        }
        if self.provider.api_key_env.trim().is_empty() {
            avisos
                .push("provider.api_key_env vazio (nenhuma variável de ambiente será lida)".into());
        }
        if self.provider.max_tokens == 0 {
            avisos.push("provider.max_tokens = 0 (o modelo talvez não aceite)".into());
        }
        if self.provider.request_timeout_secs == 0 {
            avisos.push("provider.request_timeout_secs = 0".into());
        }
        if !(0.0..=2.0).contains(&self.provider.temperature) {
            avisos.push(format!(
                "provider.temperature = {} está fora da faixa usual 0.0–2.0",
                self.provider.temperature
            ));
        }
        if self.provider.temperature.is_nan() {
            avisos.push("provider.temperature não é um número válido".into());
        }
        if self.agent.max_history_messages == 0 {
            avisos.push("agent.max_history_messages = 0 (sem histórico)".into());
        }
        if self.agent.default_context_window == 0 {
            avisos.push("agent.default_context_window = 0 (nenhum contexto caberia)".into());
        }
        if self.agent.reserve_response_tokens as usize >= self.agent.default_context_window as usize
        {
            avisos.push(format!(
                "agent.reserve_response_tokens = {} não cabe em default_context_window = {}",
                self.agent.reserve_response_tokens, self.agent.default_context_window
            ));
        }
        if !(0.0..=1.0).contains(&self.agent.context_compact_threshold) {
            avisos.push(format!(
                "agent.context_compact_threshold = {} deveria estar entre 0.0 e 1.0",
                self.agent.context_compact_threshold
            ));
        }
        if self.agent.recovery_timeout_secs == 0 {
            avisos
                .push("agent.recovery_timeout_secs = 0 (recuperação desativada na prática)".into());
        }
        if self.agent.recovery_max_attempts == 0 {
            avisos.push("agent.recovery_max_attempts = 0 (nenhuma tentativa de correção)".into());
        }
        if !matches!(self.web.engine.as_str(), "duckduckgo" | "searx") {
            avisos.push(format!(
                "web.engine = `{}` não é conhecido (usando DuckDuckGo)",
                self.web.engine
            ));
        }
        if self.web.engine == "searx"
            && self
                .web
                .searx_url
                .as_deref()
                .map(str::trim)
                .unwrap_or("")
                .is_empty()
        {
            avisos.push("web.engine = searx sem web.searx_url definido".into());
        }
        if self.web.max_download_bytes == 0 {
            avisos.push("web.max_download_bytes = 0 (nenhum download seria possível)".into());
        }
        if self.web.request_timeout_secs == 0 {
            avisos.push("web.request_timeout_secs = 0".into());
        }
        avisos
    }

    pub fn save(&self, paths: &Paths) -> anyhow::Result<()> {
        paths.ensure()?;
        let text = toml::to_string_pretty(self)?;
        let file = paths.config_file();
        let tmp = file.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &file)?;
        Ok(())
    }

    /// Valor atual da chave de API (nunca exibida na interface nem em logs).
    pub fn api_key(&self) -> Option<String> {
        std::env::var(&self.provider.api_key_env)
            .ok()
            .filter(|v| !v.trim().is_empty())
    }

    pub fn api_key_presente(&self) -> bool {
        self.api_key().is_some()
    }

    pub fn provider_pronto(&self) -> bool {
        self.api_key_presente() && !self.provider.model.trim().is_empty()
    }

    /// Diretório de trabalho efetivo.
    ///
    /// Sem fixação (`pin_workdir = false`), é sempre o diretório atual: quem
    /// abre o Kestrel em `/home/user/projeto` trabalha nesse projeto, não no
    /// último que foi usado. O valor salvo só vale depois de `/projetos`.
    pub fn workdir(&self) -> PathBuf {
        match &self.agent.workdir {
            Some(dir) if !dir.as_os_str().is_empty() && self.agent.pin_workdir => dir.clone(),
            _ => std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }

    /// Fixa (ou solta) o diretório de trabalho entre execuções.
    pub fn fixar_diretorio(&mut self, fixar: bool) {
        self.agent.pin_workdir = fixar;
    }

    /// Raízes permitidas para operações fora do espaço de trabalho.
    pub fn allowed_roots(&self) -> Vec<PathBuf> {
        let mut roots = vec![self.workdir()];
        roots.extend(self.agent.external_dirs.iter().cloned());
        roots
    }

    /// Localiza ou insere uma entrada do catálogo de modelos.
    pub fn model_entry(&self, id: &str) -> Option<&ModelEntry> {
        self.provider.models.iter().find(|m| m.id == id)
    }

    pub fn upsert_model(&mut self, entry: ModelEntry) {
        match self.provider.models.iter_mut().find(|m| m.id == entry.id) {
            Some(existing) => {
                // Preserva informações manuais já conhecidas.
                if entry.source == ModelSource::Manual || existing.source == ModelSource::Descoberto
                {
                    *existing = entry;
                }
            }
            None => self.provider.models.push(entry),
        }
    }

    /// Esforços de raciocínio efetivamente suportados (vazio se desconhecido).
    pub fn supported_efforts(&self) -> Vec<String> {
        match self.model_entry(&self.provider.model) {
            Some(entry) if entry.supports_reasoning == Capability::Sim => {
                if entry.reasoning_efforts.is_empty() {
                    vec!["low".into(), "medium".into(), "high".into()]
                } else {
                    entry.reasoning_efforts.clone()
                }
            }
            _ => Vec::new(),
        }
    }

    /// Janela de contexto presumida para o modelo selecionado.
    pub fn context_window(&self) -> u32 {
        self.model_entry(&self.provider.model)
            .and_then(|m| m.context_window)
            .unwrap_or(self.agent.default_context_window)
    }
}

/// Config corrompido: o original é preservado e o usuário é avisado, em vez de
/// a configuração ser silenciosamente descartada.
#[test]
fn config_corrompido_preserva_o_original() {
    let dir = std::env::temp_dir().join(format!("kestrel-cfg-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let arquivo = dir.join("config").join("config.toml");
    std::fs::create_dir_all(arquivo.parent().unwrap()).unwrap();
    std::fs::write(&arquivo, "isto = nao e toml valido [[[").unwrap();
    let paths = Paths::with_root(&dir);
    let (cfg, aviso) = Config::load(&paths);
    assert!(aviso.is_some(), "deveria avisar do arquivo corrompido");
    assert!(
        aviso.unwrap().contains("preservado"),
        "o aviso deveria mencionar a preservação"
    );
    // O original não é mais o config ativo.
    assert!(
        !arquivo.exists(),
        "o arquivo corrompido deveria ter saído do lugar"
    );
    // E existe uma reserva.
    let reservas: Vec<_> = std::fs::read_dir(dir.join("config"))
        .unwrap()
        .filter_map(|e| e.ok())
        .map(|e| e.file_name().to_string_lossy().to_string())
        .filter(|n| n.contains("corrompido"))
        .collect();
    assert_eq!(
        reservas.len(),
        1,
        "deveria existir uma reserva: {reservas:?}"
    );
    let _ = cfg;
    let _ = std::fs::remove_dir_all(&dir);
}

/// Config válido não gera aviso; valor fora da faixa gera aviso sem quebrar.
#[test]
fn config_invalida_valores_sem_descartar() {
    let dir = std::env::temp_dir().join(format!("kestrel-cfg2-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let arquivo = dir.join("config").join("config.toml");
    std::fs::create_dir_all(arquivo.parent().unwrap()).unwrap();
    std::fs::write(&arquivo, "[agent]\nrecovery_max_attempts = 0\n").unwrap();
    let paths = Paths::with_root(&dir);
    let (_cfg, aviso) = Config::load(&paths);
    let aviso = aviso.expect("recovery_max_attempts = 0 deveria gerar aviso");
    assert!(aviso.contains("recovery_max_attempts"), "{aviso}");
    // O arquivo continua no lugar (nada foi descartado).
    assert!(arquivo.exists(), "a configuração válida deveria permanecer");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Arquivo ausente é a primeira execução: sem aviso.
#[test]
fn config_ausente_nao_gera_aviso() {
    let dir = std::env::temp_dir().join(format!("kestrel-cfg3-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let paths = Paths::with_root(&dir);
    let (cfg, aviso) = Config::load(&paths);
    assert!(aviso.is_none());
    assert!(!cfg.setup_complete);
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::perm::PermissionLevel;

    fn cfg() -> Config {
        Config::default()
    }

    #[test]
    fn padroes_sao_seguros() {
        let c = cfg();
        assert_eq!(c.agent.permission, PermissionLevel::Semi);
        assert!(c.provider.base_url.contains("integrate.api.nvidia.com"));
        assert_eq!(c.provider.api_key_env, "NVIDIA_API_KEY");
        assert!(!c.setup_complete);
    }

    #[test]
    fn salva_e_carrega_arredondado() {
        let dir = std::env::temp_dir().join(format!("kestrel-cfg-{}", uuid::Uuid::new_v4()));
        let paths = Paths::with_root(&dir);
        let mut c = cfg();
        c.provider.model = "meta/llama-3.1-70b-instruct".into();
        c.agent.permission = PermissionLevel::Full;
        c.setup_complete = true;
        c.save(&paths).expect("salvar");
        let (loaded, err) = Config::load(&paths);
        assert!(err.is_none());
        assert_eq!(loaded.provider.model, "meta/llama-3.1-70b-instruct");
        assert_eq!(loaded.agent.permission, PermissionLevel::Full);
        assert!(loaded.setup_complete);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn esforcos_dependem_de_capacidade_conhecida() {
        let mut c = cfg();
        c.provider.model = "x".into();
        assert!(c.supported_efforts().is_empty());
        let mut entry = ModelEntry::new("x");
        entry.supports_reasoning = Capability::Sim;
        c.upsert_model(entry);
        assert_eq!(c.supported_efforts().len(), 3);
    }

    #[test]
    fn catalogo_faz_upsert_sem_duplicar() {
        let mut c = cfg();
        c.upsert_model(ModelEntry::new("a"));
        c.upsert_model(ModelEntry::new("b"));
        assert_eq!(c.provider.models.len(), 2);
        c.upsert_model(ModelEntry::new("a"));
        assert_eq!(c.provider.models.len(), 2);
    }

    /// Os rótulos que a interface/`/permissions` mostram em português também
    /// precisam ser aceitos no TOML: um valor inválido descartava o arquivo
    /// inteiro.
    #[test]
    fn aceita_rotulos_em_portugues() {
        let cfg: Config =
            toml::from_str("[agent]\npermission = \"leitura\"\nmodo = \"conversa\"\n")
                .expect("rótulos em português devem ser aceitos");
        assert_eq!(cfg.agent.permission, PermissionLevel::Read);
        assert_eq!(cfg.agent.modo, ModoOperacao::Chat);

        let cfg: Config = toml::from_str("[agent]\npermission = \"escrita\"\nmodo = \"agente\"\n")
            .expect("rótulos em português devem ser aceitos");
        assert_eq!(cfg.agent.permission, PermissionLevel::Write);
        assert_eq!(cfg.agent.modo, ModoOperacao::Agente);

        let cfg: Config = toml::from_str("[agent]\npermission = \"completo\"\n").unwrap();
        assert_eq!(cfg.agent.permission, PermissionLevel::Full);
        let cfg: Config = toml::from_str("[agent]\npermission = \"semiautônomo\"\n").unwrap();
        assert_eq!(cfg.agent.permission, PermissionLevel::Semi);
    }

    /// Chaves removidas (não fazem mais sentido) não podem derrubar a config
    /// de quem já as tinha no arquivo.
    #[test]
    fn chaves_antigas_nao_derrubam_a_config() {
        let cfg: Config =
            toml::from_str("[agent]\nmax_iterations = 40\n[ui]\nconfirm_destructive = false\n")
                .expect("chaves antigas devem ser ignoradas");
        assert_eq!(cfg.agent.permission, PermissionLevel::Semi);
        assert!(cfg.ui.show_mascot);
    }
}
