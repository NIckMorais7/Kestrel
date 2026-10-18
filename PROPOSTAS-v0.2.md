# Propostas para a próxima versão do Kestrel

Documento de planejamento — **nada aqui foi implementado**. Cada item foi
verificado contra o código atual (inventário em anexo no final) e traz o motivo
pelo qual importa. Estimativas em dias de trabalho focado.

Prioridade: **P0** = próximo release · **P1** = release seguinte · **P2** = quando
houver folga.

---

## A. Dívida conhecida (achados das auditorias, ainda abertos)

Corrigir antes de somar recursos novos: sobre fundação frágil, toda feature nova
herda o defeito.

| # | Item | Onde | Impacto | Esforço |
|---|------|------|---------|---------|
| A1 | `break 'turno` descarta resultados de ferramentas já executadas, deixando `tool_calls` sem resposta no histórico enviado à API | `agent/mod.rs` | Alto — o modelo recebe histórico inválido e pode travar em loop | 0,5d |
| A2 | Timeout da ferramenta não mata o processo filho (`kill_on_drop(false)`): processo órfão continua rodando | `tools/mod.rs` / `shell.rs` | Alto — `cargo test`/servidor sobrevive ao corte | 0,5d |
| A3 | Escopo é puramente léxico: symlink dentro do projeto apontando para fora escapa da checagem | `tools/mod.rs` | Alto — furo de segurança direto | 0,5d |
| A4 | `Config::load` descarta a configuração inteira em qualquer erro de parse e não valida faixas (`temperature`, `max_tokens`, limiares) | `config.rs` | Alto — o usuário perde a config sem entender por quê | 1d |
| A5 | Banco sem migração de schema (`CREATE TABLE IF NOT EXISTS` sem `user_version`); `save_plan` incrementa versão sem atomicidade | `db.rs` | Alto — base antiga quebra em runtime | 1d |
| A6 | Escrita atômica perde o modo do arquivo: editar script 0755 remove o bit de execução | `tools/fs.rs` | Médio — silencioso e irritante | 0,25d |
| A7 | `ler_texto` lê arquivo inteiro sem teto; `localizar_bloco` conta `{}` dentro de strings | `tools/fs.rs` | Médio — estouro de memória e corte no lugar errado | 0,5d |
| A8 | `browser_render` sem validação de esquema (`file://` legível) e com `--no-sandbox`; `fetch_url` sem proteção SSRF | `tools/web.rs` | Alto — leitura de arquivos locais e acesso à rede interna | 0,5d |
| A9 | `Interromper` durante confirmação não faz nada; colar texto com modal aberto é descartado em silêncio | `app.rs` | Baixo/Médio | 0,25d |
| A10 | Histórico multilinha corrompido no disco (entrada com `\n` vira N entradas) | `app.rs` | Baixo | 0,25d |
| A11 | Clarear o indicador: `strip_ansi` não remove sequências OSC (saída de ferramenta com hyperlink vaza escape) | `util.rs` | Baixo | 0,25d |

**Subtotal A: ~5,5 dias.**

---

## B. Confiabilidade e observabilidade

| # | Proposta | Motivo (evidência) | Prio | Esforço |
|---|----------|--------------------|------|---------|
| B1 | **Testes de streaming SSE de verdade** (servidor httptest local, casos: `[DONE]` no meio, conexão cortada, chunk dividido, tool_call fragmentada, erro no meio do stream) | `chat_com_stream` é o caminho mais crítico do produto e **não tem nenhum teste**; dois bugs sérios já saíram de lá (`[DONE]` não encerrava o laço; `index` sem limite) | P0 | 1,5d |
| B2 | **Contabilidade de tokens/custo por sessão** (`/custo`): persistir `usage` de cada resposta e somar por sessão/turno | `parse_usage` existe e o resultado é **descartado** — nenhum campo de banco guarda tokens; o usuário não sabe quanto já gastou | P0 | 1d |
| B3 | **Circuit breaker e fallback de modelos**: após N falhas transitárias do modelo ativo, oferecer/trocar automaticamente para o próximo da lista | Hoje `max_retries` repete o mesmo modelo; com 503 de capacidade (que já aconteceu na NVIDIA) o turno simplesmente falha | P1 | 1,5d |
| B4 | **Retomada de sessão offline**: exportar/importar sessão em JSON ou Markdown (`/exportar`, `/importar`) | Sessões vivem só no SQLite local; não há como levar uma conversa para outra máquina ou anexá-la a um issue | P1 | 1,5d |
| B5 | **Telemetria opcional e local**: registro de erros e tempos de turno em arquivo próprio, com `/diagnostico` para anexar em bug report | Não há nenhum registro de falhas fora do `eprintln!`; diagnosticar um problema do usuário é adivinhação | P2 | 1d |
| B6 | **Logs de depuração por nível** (`KESTREL_LOG=trace`) com `tracing` em vez de `eprintln!` | Hoje o único diagnóstico é impressão solta em stderr, sem nível nem destino configurável | P1 | 1d |

**Subtotal B: ~6,5 dias.**

---

## C. Novas capacidades do agente

| # | Proposta | Motivo (evidência) | Prio | Esforço |
|---|----------|--------------------|------|---------|
| C1 | **Enxergar imagens**: `ChatMessage` passa a aceitar partes de imagem; `read_file` (ou `inspect_image`) devolve PNG/JPG em base64; respeitar `supports_vision` do catálogo | Mensagens são **só texto** (`content: String`) e nenhuma ferramenta lê imagem — enquanto a NVIDIA tem modelos de visão confirmados (`nemotron-3.5-super-vl`, `llama-3.2-*-vision`). Sem isso o agente é cego para prints, diagramas e erros de layout | P1 | 3d |
| C2 | **Edição multi-arquivo atômica**: ferramenta `apply_edits` que aplica N edições e reverte tudo se qualquer uma falhar | `edit_file` é um arquivo por chamada; uma refatoração em 6 arquivos fica inconsistente no meio se a 4ª falhar | P1 | 2d |
| C3 | **Desfazer/reverter** (`/desfazer N`, `git restore` como ferramenta): snapshots antes de cada edição + diff reverso | `file_changes` guarda o diff mas **não existe nenhum caminho de desfazer** no código; o usuário precisa recorrer ao git manualmente | P0 | 2d |
| C4 | **Modo patch** (`apply_patch` no estilo OpenAI/Codex): formato compacto e confiável para大中型 edições | `edit_file` tem três modos ad-hoc (`trecho`/`linhas`/`bloco`) e o `bloco` já erra por contar `{}` dentro de strings; o formato patch é padrão de mercado e testado | P2 | 3d |
| C5 | **Ferramentas de qualidade**: `lint`, `format`, `typecheck` que detectam o que existe no projeto e devolvem diagnóstico estruturado | Hoje só `run_command`/`run_tests`; o agente precisa adivinhar o comando e depois parsear texto solto | P2 | 2d |
| C6 | **MCP (Model Context Protocol)**: cliente MCP para ferramentas externas, com allowlist por servidor | Extensibilidade hoje é só ferramenta compilada + skill em Markdown; MCP é o padrão do ecossistema e multiplica o valor sem inflar o binário | P2 | 5d |
| C7 | **Memória persistente de projeto**: fato aprendido (`MEMORY.md` por projeto) que o agente consulta e atualiza | A "memória" atual é o dossiê de compactação, que descreve a conversa, não o projeto (convenções, decisões, armadilhas) | P1 | 2d |
| C8 | **Fila de tarefas em background**: enfileirar N pedidos e acompanhar (`/fila`) | Só existe `pendente` (uma mensagem) e `spawn_subagent`; não há como mandar "faça A, depois B, depois C" e sair | P2 | 2d |

**Subtotal C: ~19 dias.**

---

## D. Experiência no terminal

| # | Proposta | Motivo (evidência) | Prio | Esforço |
|---|----------|--------------------|------|---------|
| D1 | **Revisor de mudanças na sessão** (`/diff`): lista das edições do turno/sessão com diff real e navegação | O agente edita arquivos e o usuário só vê o resumo numérico ("+12 -3"); não há como revisar o que foi feito sem sair do Kestrel | P0 | 2d |
| D2 | **Seleção e cópia**: seleção por mouse + `Ctrl+Shift+C`, e `/copiar` para o último bloco | Não há absolutamente nada de clipboard ou seleção no código; copiar uma saída hoje depende do terminal | P1 | 1,5d |
| D3 | **Inspeção de contexto** (`/contexto`): tokens por seção (sistema, projeto, skills, plano, histórico) e o que será enviado | A compactação é automática e opaca; quando o agente "esquece" algo, o usuário não tem como ver por quê | P1 | 1,5d |
| D4 | **Editor de plano na interface**: editar passos/subtarefas sem falar com o modelo | `update_plan` é ferramenta do modelo; o usuário não pode ajustar o plano sem reenviar uma instrução | P2 | 2d |
| D5 | **Atalhos configuráveis** (`keybindings` no config.toml) | Todas as teclas estão fixas no código (`F2` = thinking, `Shift+Tab` = permissão, …) | P2 | 1d |
| D6 | **Modo watch**: observar arquivos e re-rodar build/teste a cada salvamento | Nenhum watcher de arquivos existe; o laço de build→corrigir é sempre manual | P2 | 2d |
| D7 | **Acessibilidade**: tema de alto contraste, respeito a `NO_COLOR`/`TERM=dumb`, e modo "sem mascote" já existe — revisar contraste de todos os pares de cor | O fundo foi clareado agora (24,27,34) e os contrastes não foram medidos; não há alternativa para daltonismo | P2 | 1,5d |
| D8 | **`/person` — Comps (personalidades)**: criar, editar, alternar e remover comportamentos do agente, cada um com seu bloco de instruções no prompt; um Comp ativo por sessão, gravado em `comps/<nome>.toml` | Hoje a personalidade é fixa em `prompt.rs`: não há como trocar o estilo (amigável, técnico, conciso, mentor) sem editar código e recompilar — e o usuário quer **criar** personalidades pelo próprio Kestrel | P1 | 2,5d |

**Subtotal D: ~14 dias.**

---

## E. Ecossistema e distribuição

| # | Proposta | Motivo (evidência) | Prio | Esforço |
|---|----------|--------------------|------|---------|
| E1 | **Pacotes e instalação**: `cargo install kestrel`, AUR/deb/rpm, `shell completions` (bash/zsh/fish) e man page | Só existe `scripts/instalar.sh`, que compila no lugar; um agente de terminal sem completions é doloroso de usar | P1 | 2d |
| E2 | **Site/documentação viva**: gerar docs dos comandos e ferramentas a partir do próprio código (fonte única) | O README é escrito à mão e **já divergiu** (`history.txt`, `Pillow`, tabela de permissões) — três divergências encontradas em uma auditoria | P2 | 2d |
| E3 | **Skills como pacotes**: `kestrel skills install <origem>` com versionamento | Skills hoje são arquivos soltos em dois diretórios; não há como compartilhar ou versionar | P2 | 2d |
| E4 | **Provedores além da NVIDIA**: preset OpenAI/Anthropic/Groq/Ollama com o mesmo catálogo híbrido | O provider é único e "compatível OpenAI"; `id = "nvidia"` está no centro da config | P1 | 2d |
| E5 | **Modo serviço**: rodar uma tarefa longa destacado (`kestrel run "tarefa"`) e consultar depois | A TUI exige terminal interativo (não funciona com saída redirecionada); não há uso em CI ou por script | P2 | 3d |

**Subtotal E: ~11 dias.**

---

## F. Sugestão de roadmap

**v0.2.0 — "confiável"** (~14 dias)
A1–A5 (dívida crítica), B1 (testes de streaming), B2 (custo), C3 (desfazer),
D1 (`/diff`). Resultado: o agente para de falhar de formas silenciosas e o
usuário passa a ver o que ele fez e o quanto custou.

**v0.3.0 — "capaz"** (~20 dias)
A6–A11, C1 (imagens), C2 (edições atômicas), C7 (memória de projeto), D2 (copiar),
D3 (contexto), B3 (fallback de modelos), E4 (provedores).

**v0.4.0 — "ecossistema"** (~22 dias)
C4 (patch), C5 (qualidade), C6 (MCP), D4–D7, E1–E5, B4–B6.

---

## Anexo A — design de `/person` (Comps)

### O conceito

Um **Comp** é uma personalidade do agente: um nome e um bloco de instruções que
entra no system prompt. Você cria, edita, remove e alterna sem recompilar.

**Exemplo de uso:**

```
/person                    → lista os Comps, qual está ativo, e ativa/desativa
/person new Lazy           → abre o editor de texto dentro do Kestrel
/person use Lazy           → ativa a partir do próximo turno
/person edit Lazy          → reabre o editor com o conteúdo atual
/person see Lazy           → mostra as instruções e o tamanho (custo em tokens)
/person remove Lazy        → remove, com confirmação
```

E por conversa: *"crie um Comp Técnico, sério e preciso, que pesquise na web
sempre que dúvidar de uma API"* → o agente redige, **mostra o texto**, pede
confirmação e grava.

### Onde entra no prompt

Bloco novo, **depois** das Regras de trabalho e **antes** de Ambiente:

```
## Regras de trabalho        ← autoridade máxima, sempre aplicada
## Comp — Lazy               ← instruções da personalidade ativa
## Ambiente                  ← permissão, modo, diretório
```

A ordem não é estética: o bloco de Regras termina com uma linha dizendo que
instruções de nenhum Comp substituem as regras de segurança (permissão, modo,
nunca inventar resultados, nunca pedir segredos). Sem isso, um Comp colado da
internet poderia, em tese, instruir o agente a ignorar o nível de permissão.

### Formato de arquivo

Um arquivo por Comp em `~/.config/kestrel/comps/` — fácil de editar à mão,
copiar e versionar:

```toml
nome = "Tecnico"
descricao = "seco, preciso e cético; pesquisa antes de afirmar"
instrucoes = """
Responda de forma técnica e direta. Sem elogios, sem rodeios.
Quando não tiver certeza de uma API ou versão, pesquise na web antes de
afirmar, e diga qual fonte usou.
"""
```

### Decisões tomadas (18/10/2026)

| Decisão | Escolha | Consequência no design |
|---|---|---|
| O que um Comp controla | **só comportamento** — nada de config | Nenhum campo de modelo, esforço ou permissão no arquivo; um Comp nunca mexe em configuração |
| Como criar | **dois caminhos**: `/person new <nome>` (editor dentro do Kestrel) **e** por pedido em linguagem natural, com o agente criando | Exige ferramenta dedicada e validada, não `write_file` genérico |
| Presets de fábrica | **começar vazio** | A lista nasce vazia; `/person` sem Comps mostra "nenhum Comp criado" |
| Implementação | **só design agora** | Nada de código nesta sessão |

### Criação pelo agente: ferramenta dedicada, não `write_file`

O agente escreve arquivos, mas **só dentro do projeto**; a pasta de Comps está
fora dele — então `write_file` não alcança, e é isso que queremos: o único
caminho de escrita é uma ferramenta própria, com validação:

- nome: slug `[a-z0-9-_]`, no máximo 32 caracteres e único (sobrescrever exige
  confirmação);
- `descricao` até 120 caracteres; `instrucoes` até 4000 (o teto também protege
  o orçamento de contexto);
- grava exclusivamente em `comps/<slug>.toml`, em nenhum outro lugar;
- **remover** Comp fica só na interface (`/person remove`) — o agente não remove
  personalidades, para não perder uma por engano;
- a confirmação reutiliza o canal existente (`PedirConfirmacao`), então o texto
  aparece na sua tela antes de qualquer gravação.

### Custo de contexto

Cada Comp soma tokens ao system prompt em **todo** turno. `/person see` mostra
o tamanho, e um Comp gigante e vago é o pior dos dois mundos: caro e ineficaz.

---

## Anexo — inventário verificado (base das propostas)

**31 ferramentas:** `list_files`, `read_file`, `inspect_file_metadata`,
`write_file`, `create_file`, `edit_file` (modos `trecho`/`linhas`/`bloco`),
`delete_file`, `search_code`, `analyze_project`, `run_command`, `run_tests`,
`list_processes`, `process_status`, `read_process_output`, `kill_process`,
`write_process_stdin`, `git_status`, `git_diff`, `git_log`, `git_stage`,
`git_commit`, `git_branch`, `web_search`, `fetch_url`, `download_file`,
`browser_render`, `list_skills`, `load_skill`, `update_plan`, `task_status`,
`spawn_subagent`.

**19 comandos:** `/help`, `/model`, `/effort`, `/thinking`, `/permissions`,
`/chat`, `/agente`, `/plan`, `/clear`, `/skills`, `/stop`, `/historico`,
`/projetos`, `/analisar`, `/processos`, `/config`, `/retomar`, `/nova`, `/sair`.

**Configuração:** `provider` (id, base_url, api_key_env, model, reasoning_effort,
max_tokens, temperature, timeouts, retries, stream, models), `agent` (permission,
modo, max_iterations, limiares de contexto, workdir/pin, external_dirs, histórico,
recuperação), `ui` (mascote, histórico, confirmação, raciocínio, idioma), `web`.

**Banco:** sessions, messages, tool_runs, plans, tasks, file_changes, decisions,
audit, context_state, kv.

**Testes (187):** tools 43, ui 31, agent 24, perm 21, provider 16, app 16, util 6,
diff 6, html 5, db 5, skills 4, config 4, project 3, cli 3.
Lacunas: **streaming SSE sem nenhum teste**, db 5 (para 10 tabelas), custo/tokens
sem cobertura, setup quase sem cobertura.
