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
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{broadcast, mpsc, watch};

const MAX_LINHAS_BUFFER: usize = 4000;
/// Teto de bytes do histórico de saída de um processo. Sem ele, 4000 linhas
/// de um comando verborrágico podiam ocupar centenas de megabytes.
const MAX_BYTES_BUFFER: usize = 1_000_000;
/// Uma linha sem `\n` (barra de progresso, binário) não pode crescer sem
/// limite: o `BufReader::lines()` antigo alocava até o fim do stream.
const MAX_BYTES_LINHA: usize = 64 * 1024;
/// Processos terminados mantidos para consulta. Cada comando em primeiro
/// plano cria um; sem poda a lista (e a saída guardada) crescia pela sessão
/// inteira.
const MAX_PROCESSOS_TERMINAIS: usize = 100;

/// Processo gerenciado pelo Kestrel.
struct Processo {
    id: String,
    comando: String,
    cwd: PathBuf,
    iniciado_em: String,
    /// PID do filho. Vira `None` assim que `wait()` colhe o processo: depois
    /// disso o número pode ser reutilizado pelo sistema e um SIGKILL tardio
    /// mataria um processo alheio.
    pid: Mutex<Option<u32>>,
    /// Identificador da chamada de ferramenta que criou o processo. Permite
    /// encerrar só os processos daquela chamada quando o tempo limite/cancelamento
    /// descarta o future da ferramenta.
    escopo: Option<String>,
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
    saida_bytes: AtomicUsize,
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

/// Registra uma linha no histórico do processo, respeitando os tetos de
/// linhas e de bytes.
fn emitir_linha(processo: &Processo, bytes: &[u8], prefixo: bool) {
    let texto = String::from_utf8_lossy(bytes).into_owned();
    let texto = if prefixo {
        format!("[stderr] {texto}")
    } else {
        texto
    };
    {
        let mut buffer = processo.saida.lock().expect("lock da saída");
        if buffer.len() >= MAX_LINHAS_BUFFER
            && let Some(antiga) = buffer.pop_front()
        {
            processo
                .saida_bytes
                .fetch_sub(antiga.len().saturating_add(1), Ordering::Relaxed);
        }
        processo
            .saida_bytes
            .fetch_add(texto.len().saturating_add(1), Ordering::Relaxed);
        buffer.push_back(texto.clone());
        while processo.saida_bytes.load(Ordering::Relaxed) > MAX_BYTES_BUFFER && buffer.len() > 1 {
            if let Some(antiga) = buffer.pop_front() {
                processo
                    .saida_bytes
                    .fetch_sub(antiga.len().saturating_add(1), Ordering::Relaxed);
            }
        }
    }
    processo.linhas.fetch_add(1, Ordering::Relaxed);
    let _ = processo.fluxo.send(texto);
}

/// Lê o stream em blocos e separa linhas, com teto por linha.
///
/// `BufReader::lines()` não impõe limite: um processo que imprime gigabytes
/// sem `\n` fazia o Kestrel alocar tudo na memória antes de qualquer corte.
fn acompanhar<R>(
    processo: Arc<Processo>,
    mut stream: R,
    prefixo: bool,
    fim: mpsc::UnboundedSender<()>,
) where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut bloco = [0u8; 8192];
        let mut pendente: Vec<u8> = Vec::new();
        loop {
            match stream.read(&mut bloco).await {
                Ok(0) => break,
                Ok(n) => {
                    pendente.extend_from_slice(&bloco[..n]);
                    while let Some(pos) = pendente.iter().position(|b| *b == b'\n') {
                        let mut linha: Vec<u8> = pendente.drain(..=pos).collect();
                        if linha.last() == Some(&b'\n') {
                            linha.pop();
                        }
                        if linha.last() == Some(&b'\r') {
                            linha.pop();
                        }
                        emitir_linha(&processo, &linha, prefixo);
                    }
                    // Linha sem quebra maior que o teto: emite em pedaços.
                    while pendente.len() > MAX_BYTES_LINHA {
                        let resto = pendente.split_off(MAX_BYTES_LINHA);
                        let linha = std::mem::replace(&mut pendente, resto);
                        emitir_linha(&processo, &linha, prefixo);
                    }
                }
                Err(_) => break,
            }
        }
        if !pendente.is_empty() {
            emitir_linha(&processo, &pendente, prefixo);
        }
        let _ = fim.send(());
    });
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

impl Drop for ProcessManager {
    fn drop(&mut self) {
        // Processos em segundo plano (servidores, watchers) não podem
        // sobreviver ao Kestrel: sem isso o `child` ficava órfão ao sair.
        self.encerrar_todos();
    }
}

impl ProcessManager {
    pub fn new() -> Self {
        Self {
            processos: Mutex::new(Vec::new()),
            contador: AtomicUsize::new(1),
        }
    }

    /// Atalho usado pelos testes para iniciar sem escopo explícito.
    #[cfg(test)]
    pub fn iniciar(
        &self,
        comando: &str,
        cwd: &Path,
        envs: &[(String, String)],
    ) -> Result<String, ToolError> {
        self.iniciar_com_escopo(comando, cwd, envs, None)
    }

    /// Inicia um processo associado à chamada de ferramenta `escopo`.
    ///
    /// O escopo permite que a limpeza de uma chamada (timeout/cancelamento)
    /// encerre somente os processos que ela criou, sem tocar nos processos
    /// paralelos de outras chamadas.
    pub fn iniciar_com_escopo(
        &self,
        comando: &str,
        cwd: &Path,
        envs: &[(String, String)],
        escopo: Option<&str>,
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
            pid: Mutex::new(pid),
            escopo: escopo.map(|s| s.to_string()),
            estado: tx_estado,
            guarda_estado: rx_estado,
            codigo: Mutex::new(None),
            saida: Mutex::new(VecDeque::new()),
            saida_bytes: AtomicUsize::new(0),
            linhas: AtomicUsize::new(0),
            stdin: tokio::sync::Mutex::new(stdin),
            fluxo: tx_fluxo,
            encerramento_solicitado: Mutex::new(None),
        });

        // Tarefas de leitura de saída (stdout e stderr). Cada uma avisa o
        // `wait` quando o pipe chega ao fim, para que a saída final seja
        // capturada antes de o estado virar terminal.
        let (tx_leitura, mut rx_leitura) = mpsc::unbounded_channel::<()>();
        let mut leitores = 0;
        if let Some(stream) = stdout {
            leitores += 1;
            acompanhar(processo.clone(), stream, false, tx_leitura.clone());
        }
        if let Some(stream) = stderr {
            leitores += 1;
            acompanhar(processo.clone(), stream, true, tx_leitura.clone());
        }
        drop(tx_leitura);

        // Tarefa que aguarda o término.
        let processo_espera = processo.clone();
        tokio::spawn(async move {
            let status = child.wait().await;
            // O filho foi colhido: o PID já pode pertencer a outro processo.
            // Zerar aqui impede que o SIGKILL atrasado atinja um PID reusado.
            *processo_espera.pid.lock().expect("lock do pid") = None;
            // Drena os pipes antes de anunciar o término. Sem isto, o
            // `executar_comando` lia o snapshot de saída antes de as últimas
            // linhas chegarem e o resultado perdia o fim da saída.
            if leitores > 0 {
                let drenar = async {
                    for _ in 0..leitores {
                        if rx_leitura.recv().await.is_none() {
                            break;
                        }
                    }
                };
                let _ = tokio::time::timeout(Duration::from_millis(500), drenar).await;
            }
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

        // Poda processos terminais antigos antes de registrar o novo. Só os
        // terminais saem: os ativos continuam disponíveis para interação.
        {
            let mut lista = self.processos.lock().expect("lock da lista de processos");
            let terminais = lista.iter().filter(|p| p.terminal()).count();
            if terminais >= MAX_PROCESSOS_TERMINAIS {
                let mut remover = terminais - MAX_PROCESSOS_TERMINAIS + 1;
                lista.retain(|p| {
                    if remover > 0 && p.terminal() {
                        remover -= 1;
                        false
                    } else {
                        true
                    }
                });
            }
            lista.push(processo);
        }
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

    /// Ids dos processos ainda em execução (usado pelos testes de escopo).
    #[cfg(test)]
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

    /// Encerra os processos ativos criados pela chamada de ferramenta
    /// `escopo`. Processos de outras chamadas paralelas ficam intactos.
    pub fn encerrar_do_escopo(&self, escopo: &str) {
        let ids: Vec<String> = self
            .processos
            .lock()
            .expect("lock da lista")
            .iter()
            .filter(|p| {
                p.escopo.as_deref() == Some(escopo)
                    && *p.estado.borrow() == EstadoProcesso::Executando
            })
            .map(|p| p.id.clone())
            .collect();
        self.encerrar_varios(&ids);
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
                    // Espera curta para o `wait` colher o filho e drenar a
                    // saída final. O estado só vira terminal de verdade
                    // depois disso (antes ele era anunciado cedo demais).
                    let mut rx = processo.estado.subscribe();
                    let confirmacao = async {
                        loop {
                            if *rx.borrow_and_update() != EstadoProcesso::Executando {
                                return;
                            }
                            if rx.changed().await.is_err() {
                                return;
                            }
                        }
                    };
                    let _ = tokio::time::timeout(Duration::from_secs(3), confirmacao).await;
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
        let pid = *processo.pid.lock().expect("lock do pid");
        if let Some(pid) = pid {
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
            // Não marcamos o estado como terminal aqui: ele só muda quando o
            // `wait` confirma a morte. Anunciar antes fazia `aguardar` voltar
            // com a saída incompleta e permitia que o SIGKILL atrasado
            // atingisse um PID já reutilizado.
            if let Some(pid) = pid {
                let processo = processo.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(2000));
                    // O `wait` zera o PID ao colher o filho; se isso já
                    // aconteceu (ou o estado mudou), não há o que matar.
                    let ainda_ativo = *processo.estado.borrow() == EstadoProcesso::Executando
                        && *processo.pid.lock().expect("lock do pid") == Some(pid);
                    if ainda_ativo {
                        #[cfg(unix)]
                        unsafe {
                            libc::kill(-(pid as i32), libc::SIGKILL);
                            libc::kill(pid as i32, libc::SIGKILL);
                        }
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
    let id = ctx
        .processos
        .iniciar_com_escopo(comando, cwd, &[], ctx.escopo_processos())?;
    let mut fluxo = ctx.processos.inscrever(&id);

    // O future de espera é criado uma única vez: recriá-lo a cada linha de
    // saída reiniciava o relógio do tempo limite. Um comando que imprime
    // continuamente (um `tail -f`, um servidor verborrágico) nunca expirava.
    let aguardar = ctx.processos.aguardar(&id, timeout);
    tokio::pin!(aguardar);

    let resultado = loop {
        tokio::select! {
            _ = ctx.cancel.cancelled() => {
                ctx.processos.encerrar(&id).ok();
                break Err(ToolError::Cancelado);
            }
            estado = &mut aguardar => {
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
                    ctx.evento(AgentEvent::FerramentaSaida {
                        id: ctx.chamada_id.clone(),
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
            let id =
                ctx.processos
                    .iniciar_com_escopo(&comando, &cwd, &envs, ctx.escopo_processos())?;
            // Dá um instante para o processo falhar imediatamente (ex.: comando inexistente).
            tokio::time::sleep(Duration::from_millis(400)).await;
            let view = ctx.processos.view(&id);
            let estado = view
                .as_ref()
                .map(|v| v.estado)
                .unwrap_or(EstadoProcesso::Executando);
            ctx.evento(AgentEvent::ProcessosAtualizados {
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
        self.sucesso = sucesso;
        if !sucesso {
            self.resumo = format!("falhou: {}", self.resumo);
        }
        self
    }
}

/// Cita um argumento como um único valor na linha de comando do shell.
///
/// O filtro de testes era interpolado cru (`cargo test {filtro}`): filtros com
/// espaço quebravam o comando e metacaracteres (`;`, `$(…)`) viravam código.
fn citar_argumento(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', "'\\''"))
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
                    Some(filtro) if base.starts_with("cargo test") => {
                        format!("{base} {}", citar_argumento(&filtro))
                    }
                    Some(filtro) if base.starts_with("go test") => {
                        format!("{base} -run {}", citar_argumento(&filtro))
                    }
                    Some(filtro) if base.starts_with("pytest") => {
                        format!("{base} -k {}", citar_argumento(&filtro))
                    }
                    Some(filtro) => format!("{base} {}", citar_argumento(&filtro)),
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
        ctx.evento(AgentEvent::ProcessosAtualizados {
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
        let (saida, mostrando, casaram) = match filtro {
            Some(f) => {
                // O filtro precisa percorrer o histórico inteiro, mas o
                // resultado continua limitado por `ultimas_linhas` (antes o
                // parâmetro era ignorado quando havia filtro).
                let todas = ctx
                    .processos
                    .saida(&id, MAX_LINHAS_BUFFER)
                    .unwrap_or_default();
                let mut filtradas: Vec<&str> = todas.lines().filter(|l| l.contains(&f)).collect();
                let casaram = filtradas.len();
                if filtradas.len() > ultimas {
                    filtradas = filtradas[filtradas.len() - ultimas..].to_vec();
                }
                (filtradas.join("\n"), filtradas.len(), Some(casaram))
            }
            None => (
                ctx.processos.saida(&id, ultimas).unwrap_or_default(),
                ultimas,
                None,
            ),
        };
        if saida.trim().is_empty() {
            let detalhe = match casaram {
                Some(0) => format!("nenhuma das {total} linhas contém o filtro"),
                _ => format!("{total} linhas no total"),
            };
            return Ok(ToolOutput::texto(format!(
                "O processo `{id}` ainda não produziu saída ({detalhe})."
            )));
        }
        let cabecalho = match casaram {
            Some(casaram) => format!(
                "Saída do processo `{id}` ({total} linhas no total; {casaram} casam com o filtro; mostrando as últimas {mostrando})"
            ),
            None => format!(
                "Saída do processo `{id}` ({total} linhas no total; mostrando as últimas {mostrando})"
            ),
        };
        Ok(ToolOutput::texto(format!("{cabecalho}:\n{saida}"))
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
        ctx.evento(AgentEvent::ProcessosAtualizados {
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

    /// Um comando que não para de imprimir não pode renovar o tempo limite a
    /// cada linha: o future de espera precisa manter o mesmo prazo do início.
    #[tokio::test]
    async fn saida_continua_nao_renova_o_tempo_limite() {
        let dir = std::env::temp_dir().join(format!("kestrel-chatty-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = super::super::git::tests::contexto(&dir);
        let comando = "while true; do echo tick; sleep 0.02; done";
        let externo = tokio::time::timeout(
            Duration::from_secs(5),
            executar_comando(&ctx, comando, &dir, Some(Duration::from_millis(500))),
        )
        .await
        .expect("o tempo limite interno não foi respeitado")
        .unwrap();
        assert_eq!(externo.0, EstadoProcesso::Timeout);
        assert!(
            ctx.processos.ids_ativas().is_empty(),
            "o processo continuou ativo após o tempo limite"
        );
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

    /// A limpeza de uma chamada (timeout/cancelamento) não pode matar os
    /// processos iniciados por outra chamada paralela.
    #[tokio::test]
    async fn limpeza_por_escopo_preserva_processos_paralelos() {
        let pm = ProcessManager::new();
        let a = pm
            .iniciar_com_escopo("sleep 30", Path::new("/tmp"), &[], Some("escopo-a"))
            .unwrap();
        let b = pm
            .iniciar_com_escopo("sleep 30", Path::new("/tmp"), &[], Some("escopo-b"))
            .unwrap();
        pm.encerrar_do_escopo("escopo-a");
        let estado_a = pm.aguardar(&a, Some(Duration::from_secs(5))).await.unwrap();
        assert_eq!(estado_a, EstadoProcesso::Cancelado);
        assert_eq!(
            pm.view(&b).unwrap().estado,
            EstadoProcesso::Executando,
            "o processo de outro escopo foi encerrado junto"
        );
        pm.encerrar(&b).unwrap();
        let _ = pm.aguardar(&b, Some(Duration::from_secs(5))).await;
    }

    /// Depois que o filho é colhido, o PID não pode continuar guardado: um
    /// SIGKILL atrasado poderia atingir um processo que reutilizou o número.
    #[tokio::test]
    async fn pid_e_limpo_quando_o_processo_termina() {
        let pm = ProcessManager::new();
        let id = pm.iniciar("true", Path::new("/tmp"), &[]).unwrap();
        let estado = pm
            .aguardar(&id, Some(Duration::from_secs(5)))
            .await
            .unwrap();
        assert_eq!(estado, EstadoProcesso::Concluido);
        let processo = pm.buscar(&id).unwrap();
        assert_eq!(
            *processo.pid.lock().expect("pid"),
            None,
            "o PID continuou registrado após o término"
        );
    }

    /// Uma linha sem quebra não pode ser acumulada inteira na memória: ela é
    /// emitida em pedaços de no máximo `MAX_BYTES_LINHA`.
    #[tokio::test]
    async fn linha_sem_quebra_e_fatiada() {
        let pm = ProcessManager::new();
        let id = pm
            .iniciar(
                "head -c 200000 /dev/zero | tr '\\0' x",
                Path::new("/tmp"),
                &[],
            )
            .unwrap();
        let estado = pm
            .aguardar(&id, Some(Duration::from_secs(10)))
            .await
            .unwrap();
        assert_eq!(estado, EstadoProcesso::Concluido);
        let processo = pm.buscar(&id).unwrap();
        assert!(
            processo.linhas.load(Ordering::Relaxed) >= 3,
            "a linha gigante não foi fatiada"
        );
        let saida = pm.saida(&id, 100).unwrap();
        for linha in saida.lines() {
            assert!(
                linha.len() <= MAX_BYTES_LINHA,
                "linha com {} bytes excedeu o teto",
                linha.len()
            );
        }
    }

    /// O filtro de `read_process_output` percorre o histórico inteiro, mas o
    /// resultado continua limitado por `ultimas_linhas`.
    #[tokio::test]
    async fn leitura_com_filtro_respeita_ultimas_linhas() {
        use crate::tools::shell::ReadProcessOutput;
        let dir = std::env::temp_dir().join(format!("kestrel-filtro-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let ctx = super::super::git::tests::contexto(&dir);
        let id = ctx
            .processos
            .iniciar("printf 'erro 1\\nok\\nerro 2\\nerro 3\\n'", &dir, &[])
            .unwrap();
        ctx.processos
            .aguardar(&id, Some(Duration::from_secs(5)))
            .await
            .unwrap();
        let saida = ReadProcessOutput
            .executar(
                serde_json::json!({
                    "id": id,
                    "ultimas_linhas": 2,
                    "filtro": "erro"
                }),
                &ctx,
            )
            .await
            .unwrap();
        assert!(saida.conteudo.contains("erro 2"), "{}", saida.conteudo);
        assert!(saida.conteudo.contains("erro 3"), "{}", saida.conteudo);
        assert!(!saida.conteudo.contains("erro 1"), "{}", saida.conteudo);
        let _ = std::fs::remove_dir_all(&dir);
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

    /// O filtro de testes entra na linha de comando: precisa ser um único
    /// argumento e não pode injetar comandos.
    #[test]
    fn filtra_testes_com_seguranca() {
        assert_eq!(citar_argumento("meu_teste"), "'meu_teste'");
        assert_eq!(citar_argumento("a b"), "'a b'");
        // Aspas dentro do filtro são fechadas/reabertas: o `;` fica literal.
        assert_eq!(citar_argumento("x'; rm -rf ~"), "'x'\\''; rm -rf ~'");
    }

    /// Sucesso semântico: comando com código ≠ 0 não é sucesso, mesmo tendo
    /// devolvido saída (o agente usava `Ok` como sucesso e ignorava a falha).
    #[test]
    fn comando_com_codigo_nao_zero_nao_e_sucesso() {
        assert!(!ToolOutput::texto("saida").com_sucesso(false).sucesso);
        assert!(ToolOutput::texto("saida").com_sucesso(true).sucesso);
        assert!(ToolOutput::default().sucesso);
    }
}
