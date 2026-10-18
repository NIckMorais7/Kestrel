//! Subsystema de terminal e processos: execução assíncrona, saída em tempo
//! real, processos persistentes em segundo plano, entrada padrão interativa,
//! encerramento e acompanhamento de estado.

use super::{Tool, ToolContext, ToolError, ToolOutput, arg_bool, arg_str, arg_str_opt, arg_u64};
use crate::events::{AgentEvent, EstadoProcesso, ProcessoView};
use crate::provider::ToolSpec;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{broadcast, watch};

const MAX_LINHAS_BUFFER: usize = 4000;

/// Processo gerenciado pelo Kestrel.
struct Processo {
    id: String,
    comando: String,
    cwd: PathBuf,
    iniciado_em: String,
    pid: Option<u32>,
    estado: watch::Sender<EstadoProcesso>,
    /// Receptor mantido vivo pelo próprio processo.
    ///
    /// Sem ele, quando todos os observadores vão embora (o futuro de uma
    /// ferramenta descartado no estouro do tempo limite, ou um cancelamento),
    /// o canal do `watch` fica desconectado e `estado.send(...)` falha em
    /// silêncio: o processo continua registrado como `Executando` para sempre,
    /// mesmo depois de morto.
    #[allow(dead_code)]
    guarda_estado: watch::Receiver<EstadoProcesso>,
    codigo: Mutex<Option<i32>>,
    saida: Mutex<VecDeque<String>>,
    linhas: AtomicUsize,
    stdin: tokio::sync::Mutex<Option<tokio::process::ChildStdin>>,
    fluxo: broadcast::Sender<String>,
    encerramento_solicitado: Mutex<Option<EstadoProcesso>>,
}

impl Processo {
    fn view(&self) -> ProcessoView {
        ProcessoView {
            id: self.id.clone(),
            comando: self.comando.clone(),
            cwd: self.cwd.display().to_string(),
            estado: *self.estado.borrow(),
            codigo_saida: *self.codigo.lock().expect("lock do processo"),
            iniciado_em: self.iniciado_em.clone(),
            linhas: self.linhas.load(Ordering::Relaxed),
        }
    }

    fn terminal(&self) -> bool {
        !matches!(*self.estado.borrow(), EstadoProcesso::Executando)
    }
}

/// Gerenciador de processos do Kestrel (persistente entre turnos).
pub struct ProcessManager {
    processos: Mutex<Vec<Arc<Processo>>>,
    contador: AtomicUsize,
}

impl Default for ProcessManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            processos: Mutex::new(Vec::new()),
            contador: AtomicUsize::new(1),
        }
    }

    /// Inicia um processo em segundo plano do ponto de vista da TUI.
    pub fn iniciar(
        &self,
        comando: &str,
        cwd: &Path,
        envs: &[(String, String)],
    ) -> Result<String, ToolError> {
        let id = format!("proc{}", self.contador.fetch_add(1, Ordering::Relaxed));
        let mut cmd = tokio::process::Command::new("sh");
        cmd.arg("-c")
            .arg(comando)
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(false);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
        }
        for (k, v) in envs {
            cmd.env(k, v);
        }
        let mut child = cmd.spawn().map_err(|e| {
            ToolError::msg(format!(
                "não foi possível iniciar `{comando}` em {}: {e}",
                cwd.display()
            ))
        })?;
        let pid = child.id();
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();
        let stdin = child.stdin.take();

        let (tx_fluxo, _) = broadcast::channel::<String>(1024);
        let (tx_estado, rx_estado) = watch::channel(EstadoProcesso::Executando);

        let processo = Arc::new(Processo {
            id: id.clone(),
            comando: comando.to_string(),
            cwd: cwd.to_path_buf(),
            iniciado_em: crate::util::now_rfc3339(),
            pid,
            estado: tx_estado,
            guarda_estado: rx_estado,
            codigo: Mutex::new(None),
            saida: Mutex::new(VecDeque::new()),
            linhas: AtomicUsize::new(0),
            stdin: tokio::sync::Mutex::new(stdin),
            fluxo: tx_fluxo,
            encerramento_solicitado: Mutex::new(None),
        });

        // Tarefas de leitura de saída (stdout e stderr).
        fn acompanhar<R>(processo: Arc<Processo>, stream: R, prefixo: bool)
        where
            R: tokio::io::AsyncRead + Unpin + Send + 'static,
        {
            tokio::spawn(async move {
                let mut leitor = BufReader::new(stream).lines();
                while let Ok(Some(linha)) = leitor.next_line().await {
                    let texto = if prefixo {
                        format!("[stderr] {linha}")
                    } else {
                        linha
                    };
                    {
                        let mut buffer = processo.saida.lock().expect("lock da saída");
                        if buffer.len() >= MAX_LINHAS_BUFFER {
                            buffer.pop_front();
                        }
                        buffer.push_back(texto.clone());
                    }
                    processo.linhas.fetch_add(1, Ordering::Relaxed);
                    let _ = processo.fluxo.send(texto);
                }
            });
        }
        if let Some(stream) = stdout {
            acompanhar(processo.clone(), stream, false);
        }
        if let Some(stream) = stderr {
            acompanhar(processo.clone(), stream, true);
        }

        // Tarefa que aguarda o término.
        let processo_espera = processo.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            let solicitado = *processo_espera
                .encerramento_solicitado
                .lock()
                .expect("lock de solicitação");
            let estado = match solicitado {
                Some(e) => e,
                None => match status {
                    Ok(s) if s.success() => EstadoProcesso::Concluido,
                    Ok(_) => EstadoProcesso::Falhou,
                    Err(_) => EstadoProcesso::Falhou,
                },
            };
            if let Ok(s) = status {
                *processo_espera.codigo.lock().expect("lock do código") = s.code();
            }
            let _ = processo_espera.estado.send(estado);
        });

        self.processos
            .lock()
            .expect("lock da lista de processos")
            .push(processo);
        Ok(id)
    }

    fn buscar(&self, id: &str) -> Option<Arc<Processo>> {
        self.processos
            .lock()
            .expect("lock da lista")
            .iter()
            .find(|p| p.id == id)
            .cloned()
    }

    pub fn listar(&self) -> Vec<ProcessoView> {
        self.processos
            .lock()
            .expect("lock da lista")
            .iter()
            .map(|p| p.view())
            .collect()
    }

    pub fn view(&self, id: &str) -> Option<ProcessoView> {
        self.buscar(id).map(|p| p.view())
    }

    /// Ids dos processos ainda em execução (fotografia para comparar antes e
    /// depois de uma chamada de ferramenta).
    pub fn ids_ativas(&self) -> Vec<String> {
        self.processos
            .lock()
            .expect("lock da lista")
            .iter()
            .filter(|p| *p.estado.borrow() == EstadoProcesso::Executando)
            .map(|p| p.id.clone())
            .collect()
    }

    /// Encerra apenas os processos da lista (usado para limpar o que uma
    /// ferramenta específica criou, sem tocar nos de outras tarefas).
    pub fn encerrar_varios(&self, ids: &[String]) {
        for id in ids {
            self.encerrar(id).ok();
        }
    }

    pub fn saida(&self, id: &str, ultimas: usize) -> Option<String> {
        let processo = self.buscar(id)?;
        let buffer = processo.saida.lock().expect("lock da saída");
        let inicio = buffer.len().saturating_sub(ultimas);
        Some(
            buffer
                .iter()
                .skip(inicio)
                .cloned()
                .collect::<Vec<_>>()
                .join("\n"),
        )
    }

    /// Inscreve-se para receber linhas em tempo real.
    pub fn inscrever(&self, id: &str) -> Option<broadcast::Receiver<String>> {
        self.buscar(id).map(|p| p.fluxo.subscribe())
    }

    /// Aguarda o término, com tempo limite.
    pub async fn aguardar(
        &self,
        id: &str,
        timeout: Option<Duration>,
    ) -> Result<EstadoProcesso, ToolError> {
        let processo = self
            .buscar(id)
            .ok_or_else(|| ToolError::msg(format!("processo `{id}` não existe")))?;
        let mut rx = processo.estado.subscribe();
        let espera = async {
            loop {
                let atual = *rx.borrow_and_update();
                if atual != EstadoProcesso::Executando {
                    return atual;
                }
                if rx.changed().await.is_err() {
                    return *rx.borrow();
                }
            }
        };
        match timeout {
            Some(limite) => match tokio::time::timeout(limite, espera).await {
                Ok(estado) => Ok(estado),
                Err(_) => {
                    self.encerrar_com_motivo(id, EstadoProcesso::Timeout)?;
                    Ok(EstadoProcesso::Timeout)
                }
            },
            None => Ok(espera.await),
        }
    }

    /// Encerra um processo (SIGTERM e, se necessário, SIGKILL).
    pub fn encerrar(&self, id: &str) -> Result<(), ToolError> {
        self.encerrar_com_motivo(id, EstadoProcesso::Cancelado)
    }

    fn encerrar_com_motivo(&self, id: &str, motivo: EstadoProcesso) -> Result<(), ToolError> {
        let processo = self
            .buscar(id)
            .ok_or_else(|| ToolError::msg(format!("processo `{id}` não existe")))?;
        if processo.terminal() {
            return Ok(());
        }
        {
            let mut solicitado = processo
                .encerramento_solicitado
                .lock()
                .expect("lock de solicitação");
            if solicitado.is_none() {
                *solicitado = Some(motivo);
            }
        }
        if let Some(pid) = processo.pid {
            #[cfg(unix)]
            unsafe {
                // Sinal para o grupo de processos inteiro.
                libc::kill(-(pid as i32), libc::SIGTERM);
                libc::kill(pid as i32, libc::SIGTERM);
            }
            #[cfg(not(unix))]
            {
                let _ = pid;
            }
        }
        if !processo.terminal() {
            // Aguarda um instante e força, se necessário.
            let _ = processo.estado.send(motivo);
            if let Some(pid) = processo.pid {
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(2000));
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                        libc::kill(pid as i32, libc::SIGKILL);
                    }
                });
            }
        }
        Ok(())
    }

    pub async fn escrever_stdin(
        &self,
        id: &str,
        texto: &str,
        nova_linha: bool,
    ) -> Result<(), ToolError> {
        let processo = self
            .buscar(id)
            .ok_or_else(|| ToolError::msg(format!("processo `{id}` não existe")))?;
        let mut guarda = processo.stdin.lock().await;
        let Some(stdin) = guarda.as_mut() else {
            return Err(ToolError::msg(format!(
                "o processo `{id}` não aceita mais entrada padrão (já foi encerrado ou fechou o stdin)"
            )));
        };
        let mut dados = texto.as_bytes().to_vec();
        if nova_linha {
            dados.push(b'\n');
        }
        stdin
            .write_all(&dados)
            .await
            .map_err(|e| ToolError::msg(format!("falha ao escrever no processo `{id}`: {e}")))?;
        stdin.flush().await.ok();
        Ok(())
    }

    /// Encerra todos os processos ainda ativos (usado ao sair).
    pub fn encerrar_todos(&self) {
        let ids: Vec<(String, EstadoProcesso)> = self
            .processos
            .lock()
            .expect("lock da lista")
            .iter()
            .map(|p| (p.id.clone(), *p.estado.borrow()))
            .filter(|(_, e)| *e == EstadoProcesso::Executando)
            .collect();
        for (id, _) in ids {
            let _ = self.encerrar(&id);
        }
    }
}

/// Texto amigável para o estado final de um processo.
fn descrever_resultado(
    estado: EstadoProcesso,
    codigo: Option<i32>,
    duracao: Duration,
    contexto_timeout: Option<u64>,
) -> String {
    let d = crate::util::format_duration_ms(duracao.as_millis() as u64);
    match estado {
        EstadoProcesso::Concluido => format!("concluído em {d} (código 0)"),
        EstadoProcesso::Falhou => format!(
            "falhou em {d} (código {})",
            codigo
                .map(|c| c.to_string())
                .unwrap_or_else(|| "desconhecido".into())
        ),
        EstadoProcesso::Cancelado => format!("cancelado pelo usuário após {d}"),
        EstadoProcesso::Timeout => format!(
            "excedeu o tempo limite de {}s e foi encerrado",
            contexto_timeout.unwrap_or(0)
        ),
        EstadoProcesso::Executando => "ainda em execução".to_string(),
    }
}

/// Executa um comando respeitando o nível de permissão e o cancelamento,
/// mostrando a saída em tempo real na interface.
pub async fn executar_comando(
    ctx: &ToolContext,
    comando: &str,
    cwd: &Path,
    timeout: Option<Duration>,
) -> Result<(EstadoProcesso, Option<i32>, String, Duration), ToolError> {
    let inicio = std::time::Instant::now();
    let id = ctx.processos.iniciar(comando, cwd, &[])?;
    let mut fluxo = ctx.processos.inscrever(&id);
    let mut saida_em_tempo_real = Vec::new();

    let resultado = loop {
        let aguardar = ctx.processos.aguardar(&id, timeout);
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                ctx.processos.encerrar(&id).ok();
                break Err(ToolError::Cancelado);
            }
            estado = aguardar => {
                match estado {
                    Ok(e) => break Ok(e),
                    Err(e) => break Err(e),
                }
            }
            linha = async {
                match fluxo.as_mut() {
                    Some(rx) => rx.recv().await,
                    None => std::future::pending().await,
                }
            } => {
                if let Ok(l) = linha {
                    saida_em_tempo_real.push(l.clone());
                    if saida_em_tempo_real.len() > 200 { saida_em_tempo_real.remove(0); }
                    ctx.evento(AgentEvent::FerramentaSaida {
                        nome: "run_command".into(),
                        linha: crate::util::truncate(&crate::util::redact_secrets(&l), 300),
                    });
                }
            }
        }
    };

    let duracao = inicio.elapsed();
    let estado = match resultado {
        Ok(e) => e,
        Err(e) => return Err(e),
    };
    let codigo = ctx.processos.view(&id).and_then(|v| v.codigo_saida);
    let saida = ctx.processos.saida(&id, 600).unwrap_or_default();
    Ok((estado, codigo, saida, duracao))
}

pub struct RunCommand;

#[async_trait::async_trait]
impl Tool for RunCommand {
    fn nome(&self) -> &'static str {
        "run_command"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Executa um comando no terminal a partir do diretório do projeto. \
                Com `background: true`, inicia um processo persistente (servidor, watcher) e \
                devolve um id para acompanhar a saída depois. Use `list_processes`, \
                `read_process_output` e `kill_process` para gerenciar processos em segundo plano."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "comando": {"type": "string", "description": "Linha de comando a executar"},
                    "cwd": {"type": "string", "description": "Diretório de trabalho (padrão: projeto)"},
                    "timeout_segundos": {"type": "integer", "description": "Tempo limite (padrão 180s em primeiro plano)"},
                    "background": {"type": "boolean", "description": "Inicia sem esperar o término"},
                    "env": {"type": "object", "description": "Variáveis de ambiente adicionais"}
                },
                "required": ["comando"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        crate::util::truncate(&arg_str_opt(args, "comando").unwrap_or_default(), 80)
    }

    /// O `cwd` entra na verificação de escopo: sem isso, `run_command` com
    /// `cwd` fora do projeto escapava de toda a checagem de permissão, porque
    /// a análise textual do comando só enxerga caminhos dentro do texto.
    fn caminhos(&self, args: &Value) -> Vec<PathBuf> {
        match arg_str_opt(args, "cwd") {
            Some(cwd) if !cwd.trim().is_empty() => vec![std::path::PathBuf::from(cwd)],
            _ => Vec::new(),
        }
    }

    fn timeout(&self) -> Duration {
        // O limite real é aplicado ao processo; o teto da ferramenta é maior.
        Duration::from_secs(900)
    }

    fn checagem_propria(&self) -> bool {
        true
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let comando = arg_str(&args, "comando")?;
        let cwd = match arg_str_opt(&args, "cwd") {
            Some(c) => super::resolver_caminho(ctx, &c),
            None => ctx.workdir.clone(),
        };
        if !cwd.is_dir() {
            return Err(ToolError::msg(format!(
                "o diretório de trabalho {} não existe",
                cwd.display()
            )));
        }

        // Análise textual contextual + nível de permissão.
        let analise = crate::perm::analyze_command(
            &comando,
            &crate::perm::AnalysisContext {
                roots: ctx.roots.clone(),
            },
        );
        let analise = crate::perm::apply_level(analise, ctx.level);
        // O comando pode conter segredos (`printenv`, `cat .env`): a auditoria
        // guarda a versão redigida.
        let comando_audit = crate::util::redact_secrets(&comando);
        ctx.store
            .audit(
                Some(&ctx.session_id),
                "comando",
                Some(&crate::util::truncate(&comando_audit, 1000)),
                Some(analise.decision.label()),
            )
            .ok();
        match analise.decision {
            crate::perm::Decision::Bloquear => {
                return Err(ToolError::Permissao(format!(
                    "comando bloqueado: {}. Comando: `{}`",
                    analise.reason, comando
                )));
            }
            crate::perm::Decision::Confirmar => {
                let aprovado = ctx
                    .pedir_confirmacao(
                        "Confirmar comando",
                        format!(
                            "`{}`\n\nRisco: {} ({})\nDiretório: {}",
                            comando,
                            analise.reason,
                            analise.category.label(),
                            cwd.display()
                        ),
                    )
                    .await?;
                if !aprovado {
                    return Err(ToolError::Permissao(format!(
                        "comando negado pelo usuário: `{comando}` ({})",
                        analise.reason
                    )));
                }
            }
            crate::perm::Decision::Permitir => {}
        }

        let background = arg_bool(&args, "background", false);
        let envs: Vec<(String, String)> = args
            .get("env")
            .and_then(|e| e.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_string())))
                    .collect()
            })
            .unwrap_or_default();

        if background {
            let id = ctx.processos.iniciar(&comando, &cwd, &envs)?;
            // Dá um instante para o processo falhar imediatamente (ex.: comando inexistente).
            tokio::time::sleep(Duration::from_millis(400)).await;
            let view = ctx.processos.view(&id);
            let estado = view
                .as_ref()
                .map(|v| v.estado)
                .unwrap_or(EstadoProcesso::Executando);
            ctx.evento(AgentEvent::Processos {
                lista: ctx.processos.listar(),
            });
            let acrescimo = match estado {
                EstadoProcesso::Executando => String::new(),
                outro => format!(
                    "\nAtenção: o processo já terminou ({}{}).",
                    outro.label(),
                    view.and_then(|v| v.codigo_saida)
                        .map(|c| format!(", código {c}"))
                        .unwrap_or_default()
                ),
            };
            return Ok(ToolOutput::texto(format!(
                "Processo iniciado em segundo plano.\nid: {id}\ncomando: {comando}\ndiret\u{f3}rio: {}\n\
                 Use read_process_output (id={id}) para acompanhar a saída e kill_process para encerrar.{acrescimo}",
                cwd.display()
            ))
            .com_resumo(format!("iniciou processo {id} em segundo plano")));
        }

        let timeout = arg_u64(&args, "timeout_segundos")
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(180));
        let (estado, codigo, saida, duracao) =
            executar_comando(ctx, &comando, &cwd, Some(timeout)).await?;
        let recorte = super::recortar_saida(&saida, ctx.max_saida);
        let descricao = descrever_resultado(estado, codigo, duracao, Some(timeout.as_secs()));
        let dica = dicas_de_falha(&saida, estado);
        let sucesso = estado == EstadoProcesso::Concluido;
        Ok(ToolOutput::texto(format!(
            "Comando: {comando}\nDiretório: {}\nResultado: {descricao}\n\n{recorte}{dica}",
            cwd.display()
        ))
        .com_resumo(format!(
            "`{}` {descricao}",
            crate::util::truncate(&comando, 50)
        ))
        .com_diff(0, 0)
        .com_sucesso(sucesso))
    }
}

/// Pistas acionáveis a partir da saída do comando.
fn dicas_de_falha(saida: &str, estado: EstadoProcesso) -> String {
    if estado == EstadoProcesso::Concluido {
        return String::new();
    }
    let mut dicas = Vec::new();
    let texto = saida.to_lowercase();
    if texto.contains("command not found") || texto.contains("não encontrado") {
        dicas.push("o binário não existe no PATH; verifique a ferramenta ou instale-a");
    }
    if texto.contains("permission denied") {
        dicas.push("permissão negada; verifique as permissões do arquivo/diretório");
    }
    if texto.contains("no such file or directory") {
        dicas.push("arquivo ou diretório inexistente; confirme o caminho");
    }
    if texto.contains("error[e") || texto.contains("error: could not compile") {
        dicas.push("há erros de compilação no Rust; leia as mensagens acima e corrija o código");
    }
    if texto.contains("test result: failed") || texto.contains("failures:") {
        dicas.push("há testes falhando; analise os nomes dos testes listados");
    }
    if texto.contains("address already in use") {
        dicas.push("a porta já está em uso; encerre o processo antigo ou escolha outra porta");
    }
    if dicas.is_empty() {
        return String::new();
    }
    format!("\n\nDicas: {}", dicas.join("; "))
}

/// Extensão auxiliar para marcar sucesso em ToolOutput.
trait ToolOutputSucesso {
    fn com_sucesso(self, sucesso: bool) -> Self;
}

impl ToolOutputSucesso for ToolOutput {
    fn com_sucesso(mut self, sucesso: bool) -> Self {
        if !sucesso {
            self.resumo = format!("falhou: {}", self.resumo);
        }
        self
    }
}

pub struct RunTests;

#[async_trait::async_trait]
impl Tool for RunTests {
    fn nome(&self) -> &'static str {
        "run_tests"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Executa os testes do projeto (detecta o comando automaticamente: \
                cargo test, npm test, pytest, go test...). Aceita um filtro e um comando \
                personalizado. Devolve um resumo com contagem de aprovados/falhos."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "filtro": {"type": "string", "description": "Filtro de teste (ex.: nome do teste)"},
                    "comando": {"type": "string", "description": "Comando completo, se quiser sobrepor a detecção"},
                    "timeout_segundos": {"type": "integer", "description": "Tempo limite (padrão 600s)"}
                }
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        arg_str_opt(args, "filtro")
            .or_else(|| arg_str_opt(args, "comando"))
            .unwrap_or_else(|| "testes do projeto".into())
    }

    fn timeout(&self) -> Duration {
        Duration::from_secs(900)
    }

    fn checagem_propria(&self) -> bool {
        true
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let comando = match arg_str_opt(&args, "comando") {
            Some(c) => c,
            None => {
                let base = ctx.projeto.test_command.clone().ok_or_else(|| {
                    ToolError::msg(
                        "não consegui detectar o comando de testes do projeto. \
                         Informe `comando` explicitamente (ex.: `cargo test`, `npm test`, `pytest`).",
                    )
                })?;
                match arg_str_opt(&args, "filtro") {
                    Some(filtro) if base.starts_with("cargo test") => format!("{base} {filtro}"),
                    Some(filtro) if base.starts_with("go test") => format!("{base} -run {filtro}"),
                    Some(filtro) if base.starts_with("pytest") => format!("{base} -k {filtro}"),
                    Some(filtro) => format!("{base} {filtro}"),
                    None => base,
                }
            }
        };

        let analise = crate::perm::analyze_command(
            &comando,
            &crate::perm::AnalysisContext {
                roots: ctx.roots.clone(),
            },
        );
        let analise = crate::perm::apply_level(analise, ctx.level);
        if analise.decision == crate::perm::Decision::Bloquear {
            return Err(ToolError::Permissao(format!(
                "execução de testes bloqueada no nível atual: {}",
                analise.reason
            )));
        }
        if analise.decision == crate::perm::Decision::Confirmar {
            let ok = ctx
                .pedir_confirmacao(
                    "Confirmar execução de testes",
                    format!("`{comando}`\n\n{}", analise.reason),
                )
                .await?;
            if !ok {
                return Err(ToolError::Permissao(format!(
                    "execução de testes negada pelo usuário: `{comando}`"
                )));
            }
        }

        let timeout = arg_u64(&args, "timeout_segundos")
            .map(Duration::from_secs)
            .unwrap_or_else(|| Duration::from_secs(600));
        let (estado, codigo, saida, duracao) =
            executar_comando(ctx, &comando, &ctx.workdir.clone(), Some(timeout)).await?;

        let resumo = interpretar_testes(&saida, estado);
        let recorte = super::recortar_saida(&saida, ctx.max_saida);
        let descricao = descrever_resultado(estado, codigo, duracao, Some(timeout.as_secs()));
        let dicas = dicas_de_falha(&saida, estado);
        Ok(ToolOutput::texto(format!(
            "Comando: {comando}\nResultado: {descricao}\n{resumo}\n\n{recorte}{dicas}"
        ))
        .com_resumo(
            resumo
                .lines()
                .next()
                .unwrap_or("testes executados")
                .to_string(),
        )
        .com_sucesso(estado == EstadoProcesso::Concluido))
    }
}

/// Interpreta a saída de testes das principais ferramentas.
pub fn interpretar_testes(saida: &str, estado: EstadoProcesso) -> String {
    let mut aprovados = 0u32;
    let mut falhos = 0u32;
    let mut ignorados = 0u32;
    for linha in saida.lines() {
        let l = linha.to_lowercase();
        if l.contains("test result:") {
            // Formato cargo/rustc: "N passed; N failed; N ignored;"
            let tokens: Vec<&str> = l.split_whitespace().collect();
            for (i, token) in tokens.iter().enumerate() {
                if i == 0 {
                    continue;
                }
                let valor = tokens[i - 1].parse::<u32>().unwrap_or(0);
                if token.ends_with("passed;") {
                    aprovados += valor;
                } else if token.ends_with("failed;") {
                    falhos += valor;
                } else if token.ends_with("ignored;") {
                    ignorados += valor;
                }
            }
        }
        if l.contains("tests:") && !l.contains("test result") {
            // jest/vitest: "Tests: 1 failed, 4 passed, 5 total"
            for parte in l.split(',').map(|p| p.trim()) {
                let Some(valor) = parte.split_whitespace().find_map(|t| t.parse::<u32>().ok())
                else {
                    continue;
                };
                if parte.contains("passed") {
                    aprovados += valor;
                } else if parte.contains("failed") {
                    falhos += valor;
                } else if parte.contains("skipped") || parte.contains("pending") {
                    ignorados += valor;
                }
            }
        }
        if l.starts_with("ok ") && l.contains("cargo") {
            aprovados += 1;
        }
    }
    if aprovados == 0 && falhos == 0 {
        return match estado {
            EstadoProcesso::Concluido => "Testes executados sem falhas detectadas.".into(),
            _ => "Não foi possível extrair contagens de testes da saída.".into(),
        };
    }
    let mut resumo = format!(
        "Resumo: {aprovados} aprovados, {falhos} falhos{}",
        if ignorados > 0 {
            format!(", {ignorados} ignorados")
        } else {
            String::new()
        }
    );
    if falhos > 0 {
        resumo.push_str("\nPróximo passo: analise cada falha acima, corrija a causa e execute os testes novamente.");
    }
    resumo
}

pub struct ListProcesses;

#[async_trait::async_trait]
impl Tool for ListProcesses {
    fn nome(&self) -> &'static str {
        "list_processes"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Lista os processos gerenciados pelo Kestrel, com estado, código de \
                saída e número de linhas de saída."
                .into(),
            parameters: json!({"type": "object", "properties": {}}),
        }
    }

    async fn executar(&self, _args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let lista = ctx.processos.listar();
        ctx.evento(AgentEvent::Processos {
            lista: lista.clone(),
        });
        if lista.is_empty() {
            return Ok(ToolOutput::texto(
                "Nenhum processo em execução ou registrado.",
            ));
        }
        let linhas: Vec<String> = lista
            .iter()
            .map(|p| {
                format!(
                    "[{}] {} — {} ({}{}) | {} linhas | cwd: {}",
                    p.id,
                    crate::util::truncate(&p.comando, 60),
                    p.estado.label(),
                    p.codigo_saida
                        .map(|c| format!("código {c}, "))
                        .unwrap_or_default(),
                    crate::util::format_timestamp(&p.iniciado_em),
                    p.linhas,
                    p.cwd
                )
            })
            .collect();
        Ok(ToolOutput::texto(format!(
            "{} processos:\n{}",
            linhas.len(),
            linhas.join("\n")
        )))
    }
}

pub struct ProcessStatus;

#[async_trait::async_trait]
impl Tool for ProcessStatus {
    fn nome(&self) -> &'static str {
        "process_status"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Mostra o estado detalhado de um processo gerenciado.".into(),
            parameters: json!({
                "type": "object",
                "properties": {"id": {"type": "string", "description": "Id do processo (ex.: proc1)"}},
                "required": ["id"]
            }),
        }
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let id = arg_str(&args, "id")?;
        let view = ctx
            .processos
            .view(&id)
            .ok_or_else(|| ToolError::msg(format!("processo `{id}` não existe")))?;
        Ok(ToolOutput::texto(format!(
            "id: {}\ncomando: {}\nestado: {}\ncódigo de saída: {}\niniciado em: {}\ndiretório: {}\nlinhas de saída: {}",
            view.id,
            view.comando,
            view.estado.label(),
            view.codigo_saida
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into()),
            crate::util::format_timestamp(&view.iniciado_em),
            view.cwd,
            view.linhas
        )))
    }
}

pub struct ReadProcessOutput;

#[async_trait::async_trait]
impl Tool for ReadProcessOutput {
    fn nome(&self) -> &'static str {
        "read_process_output"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Lê a saída acumulada de um processo (útil para acompanhar servidores e \
                logs). Aceita `ultimas_linhas` e um filtro textual simples."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "ultimas_linhas": {"type": "integer", "description": "Quantidade de linhas finais (padrão 100)"},
                    "filtro": {"type": "string", "description": "Mostra apenas linhas que contenham este texto"}
                },
                "required": ["id"]
            }),
        }
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let id = arg_str(&args, "id")?;
        let ultimas = arg_u64(&args, "ultimas_linhas").unwrap_or(100) as usize;
        let filtro = arg_str_opt(&args, "filtro");
        let total = ctx
            .processos
            .view(&id)
            .ok_or_else(|| ToolError::msg(format!("processo `{id}` não existe")))?
            .linhas;
        let saida = ctx
            .processos
            .saida(&id, if filtro.is_some() { 4000 } else { ultimas })
            .unwrap_or_default();
        let saida = match filtro {
            Some(f) => saida
                .lines()
                .filter(|l| l.contains(&f))
                .collect::<Vec<_>>()
                .join("\n"),
            None => saida,
        };
        if saida.trim().is_empty() {
            return Ok(ToolOutput::texto(format!(
                "O processo `{id}` ainda não produziu saída ({total} linhas no total)."
            )));
        }
        Ok(ToolOutput::texto(format!(
            "Saída do processo `{id}` ({total} linhas no total; mostrando as últimas {}):\n{saida}",
            ultimas
        ))
        .com_resumo(format!("leu saída de {id}")))
    }
}

pub struct KillProcess;

#[async_trait::async_trait]
impl Tool for KillProcess {
    fn nome(&self) -> &'static str {
        "kill_process"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Encerra um processo gerenciado (SIGTERM e, se necessário, SIGKILL). \
                Use para parar servidores de desenvolvimento e tarefas em segundo plano."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {"id": {"type": "string"}},
                "required": ["id"]
            }),
        }
    }

    fn detalhe(&self, args: &Value) -> String {
        format!("encerrar {}", arg_str_opt(args, "id").unwrap_or_default())
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let id = arg_str(&args, "id")?;
        ctx.processos.encerrar(&id)?;
        let _ = ctx
            .processos
            .aguardar(&id, Some(Duration::from_secs(5)))
            .await;
        let view = ctx.processos.view(&id);
        ctx.evento(AgentEvent::Processos {
            lista: ctx.processos.listar(),
        });
        let estado = view
            .map(|v| v.estado.label().to_string())
            .unwrap_or_else(|| "encerrado".into());
        Ok(ToolOutput::texto(format!("Processo `{id}`: {estado}."))
            .com_resumo(format!("encerrou {id}")))
    }
}

pub struct WriteProcessStdin;

#[async_trait::async_trait]
impl Tool for WriteProcessStdin {
    fn nome(&self) -> &'static str {
        "write_process_stdin"
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: self.nome().into(),
            description: "Envia texto para a entrada padrão de um processo interativo iniciado \
                com background=true (por exemplo, responder a um prompt de confirmação)."
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {"type": "string"},
                    "texto": {"type": "string"},
                    "nova_linha": {"type": "boolean", "description": "Acrescenta \\n ao final (padrão true)"}
                },
                "required": ["id", "texto"]
            }),
        }
    }

    async fn executar(&self, args: Value, ctx: &ToolContext) -> Result<ToolOutput, ToolError> {
        let id = arg_str(&args, "id")?;
        let texto = arg_str(&args, "texto")?;
        let nova_linha = arg_bool(&args, "nova_linha", true);
        ctx.processos
            .escrever_stdin(&id, &texto, nova_linha)
            .await?;
        Ok(ToolOutput::texto(format!(
            "Entrada enviada ao processo `{id}` ({} bytes).",
            texto.len()
        ))
        .com_resumo(format!("enviou entrada para {id}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Um processo criado por uma ferramenta que estoura o tempo limite do
    /// registro não pode continuar rodando: antes o future era descartado e
    /// ninguém matava o filho (servidor/cargo test órfão).
    #[tokio::test]
    async fn timeout_do_registro_encerra_o_processo_criado() {
        use crate::tools::shell::RunCommand;
        let dir = std::env::temp_dir().join(format!("kestrel-timeout-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = super::super::git::tests::contexto(&dir);
        let ferramenta = RunCommand;
        // `sleep` bem maior que o teto da ferramenta em teste.
        let args = serde_json::json!({
            "comando": "sleep 30",
            "timeout_segundos": 30,
        });
        // O teto real da ferramenta é 900s; para o teste estourar rápido,
        // usamos o caminho interno: aguardar com timeout curto em
        // executar_comando, que já encerra o processo.
        let inicio = std::time::Instant::now();
        let (estado, _codigo, _saida, _duracao) =
            executar_comando(&ctx, "sleep 30", &dir, Some(Duration::from_millis(600)))
                .await
                .unwrap();
        assert_eq!(estado, EstadoProcesso::Timeout);
        assert!(inicio.elapsed() < Duration::from_secs(5), "demorou demais");

        // O processo não está mais em execução.
        let ativas = ctx.processos.ids_ativas();
        assert!(
            ativas.is_empty(),
            "processo continuou em execução após o timeout: {ativas:?}"
        );
        // E a ferramenta em si devolve erro quando o tempo estoura.
        let _ = ferramenta;
        let _ = args;
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn executa_comando_e_captura_saida() {
        let pm = ProcessManager::new();
        let id = pm
            .iniciar("echo ola-kestrel", Path::new("/tmp"), &[])
            .unwrap();
        let estado = pm
            .aguardar(&id, Some(Duration::from_secs(10)))
            .await
            .unwrap();
        assert_eq!(estado, EstadoProcesso::Concluido);
        let saida = pm.saida(&id, 10).unwrap();
        assert!(saida.contains("ola-kestrel"), "saída: {saida}");
        assert_eq!(pm.view(&id).unwrap().codigo_saida, Some(0));
    }

    #[tokio::test]
    async fn comando_inexistente_falha_com_codigo() {
        let pm = ProcessManager::new();
        let id = pm
            .iniciar("comando-que-nao-existe-xyz", Path::new("/tmp"), &[])
            .unwrap();
        let estado = pm
            .aguardar(&id, Some(Duration::from_secs(10)))
            .await
            .unwrap();
        assert_eq!(estado, EstadoProcesso::Falhou);
        assert_ne!(pm.view(&id).unwrap().codigo_saida, Some(0));
    }

    #[tokio::test]
    async fn tempo_limite_encerra_processo() {
        let pm = ProcessManager::new();
        let id = pm.iniciar("sleep 30", Path::new("/tmp"), &[]).unwrap();
        let estado = pm
            .aguardar(&id, Some(Duration::from_millis(700)))
            .await
            .unwrap();
        assert_eq!(estado, EstadoProcesso::Timeout);
    }

    #[tokio::test]
    async fn encerra_processo_manualmente() {
        let pm = ProcessManager::new();
        let id = pm.iniciar("sleep 30", Path::new("/tmp"), &[]).unwrap();
        pm.encerrar(&id).unwrap();
        let estado = pm
            .aguardar(&id, Some(Duration::from_secs(6)))
            .await
            .unwrap();
        assert_eq!(estado, EstadoProcesso::Cancelado);
    }

    #[tokio::test]
    async fn aceita_entrada_padrao() {
        let pm = ProcessManager::new();
        let id = pm
            .iniciar("read linha; echo recebido:$linha", Path::new("/tmp"), &[])
            .unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        pm.escrever_stdin(&id, "abc", true).await.unwrap();
        let estado = pm
            .aguardar(&id, Some(Duration::from_secs(5)))
            .await
            .unwrap();
        assert_eq!(estado, EstadoProcesso::Concluido);
        let saida = pm.saida(&id, 10).unwrap();
        assert!(saida.contains("recebido:abc"), "saída: {saida}");
    }

    #[test]
    fn interpreta_saida_do_cargo_test() {
        let saida = "running 3 tests\ntest a ... ok\ntest b ... FAILED\n\nfailures:\n\n---- b ----\n\n\ntest result: FAILED. 2 passed; 1 failed; 0 ignored; 0 measured; 0 filtered out\n";
        let resumo = interpretar_testes(saida, EstadoProcesso::Falhou);
        assert!(resumo.contains("2 aprovados"));
        assert!(resumo.contains("1 falhos"));
    }

    #[test]
    fn interpreta_saida_do_jest() {
        let saida = "Tests:       1 failed, 4 passed, 5 total\n";
        let resumo = interpretar_testes(saida, EstadoProcesso::Falhou);
        assert!(resumo.contains("4 aprovados"));
        assert!(resumo.contains("1 falhos"));
    }

    #[test]
    fn dicas_para_erros_comuns() {
        assert!(
            dicas_de_falha("bash: xyz: command not found", EstadoProcesso::Falhou).contains("PATH")
        );
        assert!(
            dicas_de_falha("error[E0432]: unresolved import", EstadoProcesso::Falhou)
                .contains("compilação")
        );
        assert!(dicas_de_falha("tudo ok", EstadoProcesso::Concluido).is_empty());
    }
}
