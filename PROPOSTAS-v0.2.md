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
| B1 | **Testes de streaming SSE de verdade** (servidor httptest local, casos: `[DONE]` no meio, conexão cortada, chunk dividido, tool_call fragmentada, erro no meio do stream) | `chat_com_stream` é o caminho mais crítico do produto e **não tem nennhum teste**; dois bugs sérios já saíram de lá (`[DONE]` não encerrava o laço; `index` sem limite) | P0 | 1,5d |
| B2 | **Contabilidade de tokens/custo por sessão** (`/custo`): persistir `usage` de cada resposta e somar por sessão/turno | `parse_usage` existe e o resultado é **descartado** — nennhum campo de banco guarda tokens; o usuário não sabe quanto já gastou | P0 | 1d |
| B3 | **Circuit breaker e fallback de modelos**: após N falhas transitárias do modelo ativo, oferecer/trocar automaticamente para o próximo da lista | Hoje `max_retries` repete o mesmo modelo; com 503 de capacidade (que já aconteceu na NVIDIA) o turno simplesmente falha | P1 | 1,5d |
| B4 | **Retomada de sessão offline**: exportar/importar sessão em JSON ou Markdown (`/exportar`, `/importar`) | Sessões vivem só no SQLite local; não há como levar uma conversa para outra máquina ou anexá-la a um issue | P1 | 1,5d |
| B5 | **Telemetria opcional e local**: registro de erros e tempos de turno em arquivo próprio, com `/diagnostico` para anexar em bug report | Não há nennhum registro de falhas fora do `eprintln!`; diagnosticar um problema do usuário é adivinhação | P2 | 1d |
| B6 | **Logs de depuração por nível** (`KESTREL_LOG=trace`) com `tracing` em vez de `eprintln!` | Hoje o único diagnóstico é impressão solta em stderr, sem nível nem destino configurável | P1 | 1d |

**Subtotal B: ~6,5 dias.**

---

## C. Novas capacidades do agente

| # | Proposta | Motivo (evidência) | Prio | Esforço |
|---|----------|--------------------|------|---------|
| C1 | **Enxergar imagens**: `ChatMessage` passa a aceitar partes de imagem; `read_file` (ou `inspect_image`) devolve PNG/JPG em base64; respeitar `supports_vision` do catálogo | Mensagens são **só texto** (`content: String`) e nennhuma ferramenta lê imagem — enquanto a NVIDIA tem modelos de visão confirmados (`nemotron-3.5-super-vl`, `llama-3.2-*-vision`). Sem isso o agente é cego para prints, diagramas e erros de layout | P1 | 3d |
| C2 | **Edição multi-arquivo atômica**: ferramenta `apply_edits` que aplica N edições e reverte tudo se qualquer uma falhar | `edit_file` é um arquivo por chamada; uma refatoração em 6 arquivos fica inconsistente no meio se a 4ª falhar | P1 | 2d |
| C3 | **Desfazer/reverter** (`/desfazer N`, `git restore` como ferramenta): snapshots antes de cada edição + diff reverso | `file_changes` guarda o diff mas **não existe nennhum caminho de desfazer** no código; o usuário precisa recorrer ao git manualmente | P0 | 2d |
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
| D9 | **Reformulação visual** (anexo B): paleta monocromática + 1 acento, 3 faixas de layout, glifos consistentes, 3 skins prontos e `/skin` | A interface atual mistura 3 famílias de cor, põe borda em tudo e empilha metadados sem hierarquia — o usuário a considera feia; o design foi pesquisado contra Hermes, Claude Code e o sistema de design da Anthropic | P1 | 8d |

**Subtotal D: ~22 dias.**

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

**v0.2.5 — "agradável"** (~10,5 dias) — *a pedido do usuário*
D8 (`/person`) e D9 (reformulação visual + skins), que compartilham a mesma
origem (`theme.rs` ganha as chaves de cor; o prompt ganha o bloco do Comp).
Resultado: a interface deixa de parecer um debug tool e o agente passa a ter
personalidades escolhíveis.

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
instruções de nennhum Comp substituem as regras de segurança (permissão, modo,
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
| Presets de fábrica | **começar vazio** | A lista nasce vazia; `/person` sem Comps mostra "nennhum Comp criado" |
| Implementação | **só design agora** | Nada de código nesta sessão |

### Criação pelo agente: ferramenta dedicada, não `write_file`

O agente escreve arquivos, mas **só dentro do projeto**; a pasta de Comps está
fora dele — então `write_file` não alcança, e é isso que queremos: o único
caminho de escrita é uma ferramenta própria, com validação:

- nome: slug `[a-z0-9-_]`, no máximo 32 caracteres e único (sobrescrever exige
  confirmação);
- `descricao` até 120 caracteres; `instrucoes` até 4000 (o teto também protege
  o orçamento de contexto);
- grava exclusivamente em `comps/<slug>.toml`, em nennhum outro lugar;
- **remover** Comp fica só na interface (`/person remove`) — o agente não remove
  personalidades, para não perder uma por engano;
- a confirmação reutiliza o canal existente (`PedirConfirmacao`), então o texto
  aparece na sua tela antes de qualquer gravação.

### Custo de contexto

Cada Comp soma tokens ao system prompt em **todo** turno. `/person see` mostra
o tamanho, e um Comp gigante e vago é o pior dos dois mundos: caro e ineficaz.

---

## Anexo B — reformulação visual (v0.2.0)

### O que pesquisei (18/10/2026)

| Fonte | O que aproveitei |
|---|---|
| **Hermes CLI** (layout visto em imagem + docs completos) | fundo quase preto, **um** acento dourado, respostas do agente em verde, caixas arredondadas, glifo `❯` no input, chamadas de ferramenta com prefixo `┊`, barra de status fixa acima do input |
| **Hermes — barra de status** | `modelo │ 12.4k/200k │ [██████░░░░] 6% │ $0.06 │ 15m`, com selos (`🗜 N` compactações, `▶ N` tarefas, `⚠ YOLO`), e **truncamento por largura**: completo ≥76 col., compacto 52–75, mínimo <52 |
| **Hermes — skins** | separação explícita entre **skin** (visual) e **personality** (tom); 9 skins prontos incluindo `mono`, `slate` e `daylight` (tema claro); detecção de terminal claro (OSC 11 / `COLORFGBG`); lista completa de chaves de cor |
| **Claude (Anthropic) — design system** | paleta quente: terracota `#c96442`, coral `#d97757`, quase-preto **quente** `#141413`, superfícies `#30302e`, prata morna `#b0aea5`; **nenhum cinza azulado** — todos os neutros têm tom amarelado; profundidade por anel (1px) e raios generosos |
| **tui-design-skill** (princípios) | *começar monocromático*; 4–5 cores no máximo; escala de espaçamento (1/2/4/6/8 células); **mínimo 1 célula de padding dentro de borda, preferir 2**; 3–4 níveis de hierarquia; não usar borda em tudo (espaço vazio separa melhor); tabela de símbolos (`● ○ ▸ ✓ ✗ ⚠ … ■ ◆`) |

**Limitação honesta:** as telas do Claude Code e do Codex CLI estão atrás de URLs assinadas expiradas ou bloqueadas — o que sei delas vem das docs e do sistema de design, não de captura de tela. O Hermes eu vi layout de verdade.

### Diagnóstico: por que o Kestrel hoje parece feio

1. **Três famílias de cor competindo** — azul (`#96b4e6` rótulos, `#78b4ff` borda ativa), âmbar (`#ffc46e` títulos) e cinzas frios. Nenhuma conversa com as outras. O guia é claro: 4–5 cores, no máximo.
2. **Borda em tudo** — conversa, campo de mensagem, modais, seletor, todos com caixa arredondada. O guia: borda agrupa; espaço vazio separa. Com borda em tudo, nada se destaca.
3. **A linha de status é uma gaveta** — estado + "Ctrl+C interrompe" + "enter envia · /help" + "skills 0" + Pip, tudo numa linha, sem hierarquia entre eles.
4. **Densidade plana, sem escala de espaço** — nennhum padding interno consistente; leitura cansativa justamente pelo que o usuário notou ("alto contraste").
5. **Um único visual, nennhum tema** — as cores estão fixas em `theme.rs`; não existe tema claro, nem alternativa minimal.

### Proposta: 3 camadas

#### 1. Paleta (monocromática + 1 acento + semântica)

| Papel | Cor proposta | Uso |
|---|---|---|
| fundo | `#1f2026` (quente, não preto) | aplicação inteira |
| superfície | `#26272e` | barras e realces |
| texto | `#d9d6cd` | corpo |
| apagado | `#8b8a83` | dicas, metadados |
| **acento** | **`#e0a458`** (âmbar) | foco, seleção, prompt, links |
| borda | `#3a3b42` | contornos discretos |
| ok / aviso / erro | `#7cb87b` / `#d9b45a` / `#cf7a72` | estados |

Regras: **encerrar o azul** (`#96b4e6`/`#78b4ff` viram acento); no máximo 4 cores por tela; símbolo sempre acompanha cor (`✓`, `✗`, `⚠`, `●`).

#### 2. Estrutura (3 faixas, 1 centro)

```
 ~/Projetos/Kestrel (master)                                  ← 1 linha, sem caixa
╭ conversa ────────────────────────────────────────────────╮
│  …                                                    ↑  │  ← borda só aqui
╰──────────────────────────────────────────────────────────╯
 ❯ z-ai/glm-5.3 · pensando   12.4k/128k ▓▓▓░░░░ 9%  4m      ← 1 linha, agrupada
╭──────────────────────────────────────────────────────────╮
│ ❯ descreva a tarefa…                                     │  ← glifo ❯ + padding 1
╰──────────────────────────────────────────────────────────╯
```

- **campo de mensagem**: glifo `❯` no acento, 1 célula de padding interno, borda só quando focado;
- **faixa de estado**: uma linha agrupada por ` · ` (modelo · estado · contexto · tempo), truncando por largura como o Hermes (≥76 completa, 52–75 compacta, <52 mínima);
- **chamadas de ferramenta**: prefixo vertical `┊` no acento, como o Hermes;
- **resposta do agente**: rótulo `◆ Kestrel` (ou o nome do Comp ativo — amarra com o `/person`).

#### 3. Skins (o sistema, não só um visual)

Separado de `/person`, exatamente como o Hermes separa **skin** de **personality**:

```
/skin                → lista e mostra o atual
/skin mono           → aplica na hora (sessão)
/skin daylight       → tema claro
```

`~/.config/kestrel/skins/<nome>.toml` com as chaves de cor acima; 3 prontos:
**kestrel** (escuro quente, padrão), **mono** (só cinzas, para gravar tela),
**daylight** (claro, para terminal claro). Detecção de terminal claro via
`COLORFGBG`/OSC 11, com `KESTREL_SKIN` para forçar.

### Custo

| Item | Esforço |
|---|---|
| paleta + reestruturação das 3 faixas + glifos | 3d |
| sistema de skins (3 prontos, `/skin`, detecção claro/escuro, testes) | 3d |
| acabamentos (padding, símbolos, estado vazio com painel de boas-vindas) | 2d |
| **total** | **~8d** |

Roda **junto** com o `/person` (2,5d) na v0.2.0: os dois compartilham a mesma origem
(`theme.rs` ganha as chaves; `/person` ganha o bloco de instruções).

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
Lacunas: **streaming SSE sem nennhum teste**, db 5 (para 10 tabelas), custo/tokens
sem cobertura, setup quase sem cobertura.
