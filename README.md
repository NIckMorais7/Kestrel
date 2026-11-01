# Kestrel

Agente autônomo de codificação para o terminal, escrito em Rust. Conversa,
investiga o projeto, planeja, edita arquivos, executa comandos e valida o que
fez — sempre dentro do nível de permissão escolhido.

## Requisitos

- Linux com terminal interativo (TTY); a interface não funciona com entrada ou
  saída redirecionada.
- Rust estável com Cargo (edition 2024; o projeto é validado com `rustc`
  recente).
- Uma chave de API do provedor configurado, em variável de ambiente. Para o
  provedor padrão (NVIDIA): `NVIDIA_API_KEY`.
- `git` para as ferramentas de repositório (opcionais em outros fluxos).

### Fonte (recomendada: Geist Mono)

O Kestrel é uma aplicação de terminal: a fonte é escolhida pelo **emulador de
terminal**, não pelo app. A Inter não tem variante monoespelhada oficial e
quebra o alinhamento da interface; por isso a recomendação é a **Geist Mono**,
livre e visualmente próxima:

```bash
scripts/instalar_fonte_geist_mono.sh
```

Depois aponte o terminal para ela. No `foot`, por exemplo:

```ini
# ~/.config/foot/foot.ini
font=Geist Mono:size=9
```

O Kestrel usa a métrica real da fonte para posicionar o Pip em 6×2; depois de
trocar a fonte, reabra o terminal para a nova métrica valer.

## Instalação

```bash
scripts/instalar.sh          # compila em release e instala em ~/.local/bin
```

O script não muda nada fora do diretório de destino. Para instalar em outro
lugar:

```bash
KESTREL_INSTALL_DIR=/usr/local/bin sudo -E scripts/instalar.sh
```

Se `~/.local/bin` não estiver no `PATH`, o próprio script mostra a linha que
deve ser acrescentada ao shell. Depois de instalar, `kestrel` funciona a partir
de qualquer diretório:

```bash
cd ~/qualquer/projeto && kestrel
```

O comando roda no diretório atual (o projeto ativo). Para fixar um projeto,
use `/projetos` dentro da interface (a escolha fica salva na configuração).

## Primeira execução

1. Configure a chave e o modelo:

   ```bash
   export NVIDIA_API_KEY="..."        # nunca é gravada em disco
   kestrel --setup                    # assistente de configuração inicial
   ```

   Ou, se a configuração já existe, abra direto:

   ```bash
   kestrel
   ```

2. Digite a tarefa em linguagem natural e pressione Enter. Exemplos:
   `explique a arquitetura deste projeto`, `corrija o bug do parser e rode os testes`.

### Catálogo de modelos

O catálogo é uma **allowlist curada** dos modelos de chat com free endpoint
confirmado no provedor (NVIDIA NIM). A descoberta automática (`/model`)
consulta o `/models` do provedor, mas **só entra na lista o que a allowlist
confirma**: modelos descontinuados (404), sem free endpoint ou que não são
chat (embeddings, guards, OCR, ASR, geração de imagem/vídeo…) ficam de fora.
Na carga da configuração, modelos descobertos que saíram da allowlist são
removidos automaticamente; **modelos adicionados manualmente nunca são
tocados**.

Esforços de raciocínio (`/effort`) seguem o que cada modelo aceita:

- Nemotron 3.5 Super VL, 3 Super, 3 Ultra, 3.5 Lightning, 3 Nano Omni,
  GLM-5.3 (e flash), Step 5 Preview, GPT-OSS-20B: `low`/`medium`/`high`;
- **MiniMax M3.1 Flash**: `low`/`medium`/`high`/`xhigh`/`max` — o
  raciocínio é sempre ligado e **não** aceita `none` (a API responde 400);
- **Mistral Large 4** (`mistralai/mistral-large-4-1t-a52b`): `low`/`medium`/`high`
  — raciocínio em três níveis (o modelo sempre raciocina; `none` não é aceito);
- DeepSeek V4.1 Flash: raciocínio numérico (1–100), fora do seletor de
  esforços (pensa com o padrão);
- Modelos sem esforços configuráveis (Muse Glimmer, Gemma 4, Laguna, Bolt):
  pensam com o padrão do modelo.

## Modos

- **build** (padrão): investiga, planeja, edita arquivos e executa comandos
  conforme o nível de permissão.
- **chat**: somente conversa e leitura; ferramentas que escrevem, executam ou
  planejam ficam indisponíveis e o modelo é instruído a não alterar nada.

Troque com `/chat` e `/agent`. O modo atual aparece no cabeçalho, é persistido
na configuração e registrado na auditoria.

## Permissões

Quatro níveis (`/permissions` ou Shift+Tab), aplicados em uma única camada
central:

| Nível  | O que permite                                                        |
|--------|----------------------------------------------------------------------|
| read   | **ler** qualquer caminho (dentro ou fora do projeto); nenhuma escrita ou execução |
| write  | ler, criar, editar, excluir e `rm -rf` em alvo comum (ex.: `target/`, `node_modules/`), dentro **e fora** do projeto; operações sensíveis (instalação, rede, privilégios) pedem confirmação |
| semi   | tudo do write **sem confirmação** (inclusive instalação e rede); só o que **perde dado** (git destrutivo) pede confirmação |
| full   | idem ao semi, mais conteúdo baixado da rede executado direto (pipe para interpretador); **só git destrutivo pede confirmação** |

**Fora do projeto não é proibido**: a regra é pela operação. Ler qualquer caminho
funciona em todos os níveis; escrever (criar/editar/excluir) funciona de `write` em
diante e é bloqueado em `read`.

Rodar código do projeto (`node script.js`, `python app.py`, `bun test`, `go run .`, …) é
fluxo de desenvolvimento e passa sem confirmação em `semi`/`full`.

**O que nunca passa** (bloqueado em qualquer nível): formatar disco (`mkfs`, `dd` em
dispositivo), fork bomb, desligar o sistema, escrever ou remover em `/etc`, `/usr`,
`/sys`, `/proc`, `/dev`, `/boot`, `/bin`, `/sbin`, `/var/lib` ou na raiz `/`, e apagar
a home (`~`/`$HOME`), `/home` ou um ancestral do projeto.

**`rm -rf` comum continua liberado a partir do nível `write`** — decisão explícita do
projeto: remover `target/`, `node_modules/` ou uma pasta de build é rotina, não
catástrofe. O bloqueio incondicional vale apenas para os alvos vitais acima.

**Sem limite de iterações**: o agente trabalha até concluir a tarefa, ser interrompido
por você (`Esc` ou `Ctrl+C`) ou a compactação de contexto não conseguir mais manter a
conversa. O progresso aparece na interface como `trabalhando… Nª interação`.

O que ainda pede confirmação em `semi`/`full` é só o que **perde dado sem volta**:
`git reset --hard`, `git push --force`, `git clean -fdx`, `git filter-branch`.

## Comandos da interface

Digite `/` e use Tab/Enter para completar (um comando parcial **não** é
executado por acidente: Enter completa, o segundo Enter executa).

`/help` lista todos os comandos com os argumentos reais: `/model`, `/effort`,
`/person`, `/skin`, `/chat`, `/agent`, `/plan`, `/permissions`, `/thinking`,
`/undo`, `/diff`, `/cost`, `/stop`, `/history`, `/resume`, `/new`, `/projects`,
`/processes`, `/analyze`, `/skills`, `/config`, `/clear`, `/quit`.

## Comps e skins

São duas camadas separadas: **Comps** mudam como o agente fala; **skins** mudam
como a interface desenha.

### Comps (`/person`)

Um Comp é um nome e um bloco de instruções de comportamento (tom, estilo,
prioridades). Ele nunca carrega permissão, modelo ou configuração — essas
continuam sendo decisões suas e das regras de segurança. Um Comp fica ativo por
sessão e entra no system prompt a partir do próximo turno.

```text
/person                    lista os Comps, qual está ativo e o custo em tokens
/person new Lazy           abre o editor dentro do Kestrel
/person use Lazy           ativa
/person see Lazy           mostra as instruções e o custo estimado
/person edit Lazy          reabre o editor com o conteúdo salvo
/person remove Lazy        remove, com confirmação
/person off                volta ao tom padrão
```

Os arquivos ficam fora do projeto, em `~/.config/kestrel/comps/<nome>.toml`. O
nome vira o slug do arquivo (`Lazy` → `lazy`). Limites: 32 caracteres no nome,
120 na descrição, 4000 nas instruções — um Comp curto e específico funciona
melhor e custa menos contexto. No editor: Enter quebra linha, Ctrl+S salva e Esc
cancela.

O agente também pode criar Comps por pedido em linguagem natural, mas pela
ferramenta dedicada `create_comp` — nunca pelo `write_file` genérico, e nunca
removendo um Comp: remoção é sempre sua, pelo `/person remove`.

### Skins (`/skin`)

```text
/skin                      mostra a skin atual e lista as disponíveis
/skin mono                 troca na hora (a escolha fica salva)
/skin daylight             tema claro
KESTREL_SKIN=mono kestrel  força uma skin só nesta sessão
```

Prontas: `mono` (padrão: **preto/branco**, no estilo Claude Code), `kestrel`
(cinza escuro) e `daylight` (claro). O `/skin` relê o diretório a cada uso,
então skins criadas durante a sessão passam a valer na hora. Skins próprias ficam em
`~/.config/kestrel/skins/<nome>.toml` e usam chaves de cor em `#rrggbb`
(`fundo`, `superficie`, `texto`, `apagado`, `acento`, `acento_fraco`, `borda`,
`borda_ativa`, `usuario`, `raciocinio`, `sucesso`, `falha`, `aviso`, `info`,
`execucao`, `codigo_fundo`, `selecao_fundo`, `selecao_texto`). Campos ausentes
herdam a skin `kestrel`, e o nome da skin é o nome do arquivo. Uma skin própria
inválida é ignorada (não impede a interface de abrir).

Os comandos são em inglês e **todos aceitam apelido em português** (`/modelo`,
`/conversa`, `/agente`, `/limpar`, `/sair`, `/nova`, `/projetos`, `/analisar`,
`/processos`, `/retomar`, `/historico`), então quem digita em português não
perde o hábito.

## Pip

O mascote é a **imagem original do Pip** (`assets/pip.png`, cópia fiel de
`/home/nick/Assets/Pip/pip.png`, conferida por sha256). Não existe textura
intermediária nem script de geração: o PNG é embutido no binário e renderizado
pelo `ratatui-image`.

O espaço reservado é fixo: **6 colunas × 2 linhas**. O PNG é recortado na caixa
opaca, redimensionado para a resolução de pixels dessa área e só então movido
dentro dela. Assim o Pip **nunca passa de duas linhas** e não cobre a conversa.

A renderização usa o `ratatui-image` com o **protocolo gráfico do terminal**
(Sixel, Kitty ou iTerm2) e só cai para halfblocks quando o terminal não oferece
nenhum. O PNG é redimensionado uma única vez para a resolução de pixels de 6×2 e
o quadro é codificado já nessa resolução, sem um segundo resize. A moldura é
opaca na cor do aplicativo, o que evita “rastro”/duplicação ao animar. A arte
continua geométrica: a massa branca do gato com a tinta (contorno e olhos)
vazada; os buracos dos olhos são pintados com a cor de fundo da aplicação, então
não ficam brancos em terminal claro.

O ciclo tem 96 quadros a ~40 ms (~3,8 s) e é determinístico: repouso
respirando (e piscando), passeio leve para a esquerda, pausa, passeio para a
direita e repouso de novo. Cada quadro move no máximo **1 pixel**, então o
deslocamento é contínuo. A moldura é **composta sobre a cor de fundo do tema**
(alfa pré-multiplicado), sem pixels transparentes sobrando — transparência na
moldura virava um bloco branco em alguns protocolos gráficos. Trocar de skin
recodifica os quadros para que a moldura acompanhe a nova cor.

Na v0.9.6 a animação foi reconstruída: quadros a 40 ms, passos de 1 pixel,
piscada só em repouso e animação restrita à recepção (numa conversa longa o Pip
não é desenhado e não força redesenhos). A piscada fecha o olho com uma
pálpebra escura (tinta amostrada do contorno) — preencher com o pelo deixava
um bloco branco no rosto branco. A área do Pip recebe o fundo do tema antes do
protocolo gráfico, eliminando a faixa clara que o `clear_area` do Sixel podia
deixar. A rolagem também ganhou cadência de 60 fps enquanto o alvo está em
movimento e os giros da roda acumulam no alvo, sem “travar” em posições
intermediárias.

A partir da v0.5.0, o Pip **não fica mais no rodapé** nem no assistente. Ele
aparece somente na **recepção de nova sessão**, no chat, junto de instruções
curtas. Ao enviar a primeira mensagem, o Pip e as instruções somem juntos e a
conversa segue limpa.

Durante o turno, o trabalho aparece em dois lugares discretos: o **spinner** no
campo de mensagem e o **estado** no rodapé (com o indicador de atividade fixo,
sem piscar). Não existe mais estrela de progresso no chat.

Limitações documentadas:

- terminais sem cor verdadeira aproximam as cores;
- a animação é sempre contida dentro da mesma caixa de 6×2 e pode ser desligada
  com `ui.show_mascot = false`;
- o fundo do aplicativo é **preto** e o acento é branco, no estilo Claude Code
  (a skin `daylight` mantém um tema claro separado); a conversa não usa caixa, e
  apenas o campo de mensagem e os modais têm cantos arredondados.

## Ícone do Kestrel

A partir da v0.9.6 o cabeçalho **não exibe mais o ícone** no canto superior
esquerdo: ele começa direto em `Kestrel v0.9.6`. Os arquivos de imagem
continuam no repositório como material de marca, mas não são carregados nem
codificados na inicialização.

## Contexto no rodapé

O rodapé inferior mostra o **contexto real do modelo**, não a soma de tokens da
sessão: usado/total e percentual, sem barrinhas — só o texto. Exemplo:

```text
contexto 390.000/1.000.000 (39%)
```

O rodapé e o cabeçalho são **discretos**: estado e contexto em texto apagado,
indicador de atividade em um único círculo (discreto quando ocioso, branco ao
concluir e piscando devagar durante o trabalho), processos ativos só como
`⚙ N`, modelo e Comp no cabeçalho sem destaque. Tokens acumulados continuam
disponíveis em `/cost`.

## Raciocínio do modelo (`/thinking`)

`/thinking` (ou `/thinking on` / `/thinking off`) decide se o raciocínio do
modelo aparece na conversa. **Vem desativado por padrão**: com ele desligado, o
pensamento não entra no chat e o trabalho do agente é sinalizado apenas pelo
estado ao lado do Pip (`contemplando`, `raciocinando`, `executando ferramenta`,
`escrevendo`). Ligado, os blocos de raciocínio aparecem inline, como antes.

A escolha é gravada em `ui.show_reasoning` e vale para as próximas sessões. F2
faz o mesmo que o comando.

## Conversa, rolagem e estado mais recente

A conversa **não volta sozinha para o fim**: enquanto o agente trabalha, você
pode rolar (PageUp/PageDown ou roda do mouse) e ler o histórico com calma. Para
voltar ao estado mais recente:

- role até o fim (com a roda ou PageDown) — isso já religa o acompanhamento;
- ou clique no atalho `↓ voltar para o último`, que aparece no canto direito do
  rodapé enquanto você estiver fora do fim.

A rolagem é animada a 60 fps enquanto há alvo pendente; cada giro da roda ou
PageUp/PageDown soma ao **alvo** (não à posição já animada), então gestos
rápidos não se perdem.

## Diretório inicial

O Kestrel começa **no diretório de onde foi chamado**. Fixar um projeto
(`/projetos`, escolha explícita) grava `agent.pin_workdir = true` e passa a
abrir sempre nesse diretório — para voltar ao comportamento "onde estou",
desligue a fixação em `/config` ou apague os dois campos do `config.toml`.

## MCP (servidores locais e remotos)

O Kestrel sobe servidores MCP declarados em `~/.config/kestrel/config.toml` e
expõe as ferramentas deles ao modelo, com o prefixo `mcp__<servidor>__<nome>`:

```toml
[[mcp]]
nome = "github"
comando = "npx"
args = ["-y", "@modelcontextprotocol/server-github"]

[[mcp]]
nome = "remoto"
transporte = "http"                 # "stdio" (padrão) ou "http"
url = "https://mcp.exemplo/mcp"
headers = { "X-Ambiente" = "dev" }
token_env = "MCP_TOKEN"             # bearer token por variável de ambiente
# OAuth2 client-credentials (sem segredo no TOML):
# [mcp.oauth]
# token_url = "https://auth.exemplo/token"
# client_id = "kestrel"
# client_secret_env = "MCP_CLIENT_SECRET"
# scopes = ["mcp.read", "mcp.write"]
```

- **stdio e HTTP/SSE**: o transporte HTTP aceita JSON ou `text/event-stream`,
  preserva `Mcp-Session-Id`, faz OAuth client-credentials e respeita timeout.
- **Recursos e prompts**: quando o servidor anuncia as capacidades, o modelo
  recebe também `list_resources`, `read_resource`, `list_prompts` e
  `get_prompt`. Notificações do servidor (inclusive `list_changed`) são lidas.
- **Assinatura**: dá para exigir integridade do plugin. `mcp_trusted_keys`
  mapeia id → chave pública Ed25519 (hex); o manifesto assinado inclui nome,
  transporte, comando/URL, args, env, headers, cwd e o SHA-256 do executável
  (stdio). Configure `assinatura`, `chave_id` e, se quiser fixar o binário,
  `sha256` no servidor:

  ```toml
  [mcp_trusted_keys]
  "equipe" = "hex-da-chave-publica-ed25519"

  [[mcp]]
  nome = "plugin"
  comando = "/opt/plugin/mcp"
  assinatura = "hex-da-assinatura"
  chave_id = "equipe"
  sha256 = "hex-do-digest"
  ```
- **Mesmas regras.** Ferramentas MCP passam pela verificação de permissão, pelo
  escopo de caminhos e pela auditoria, como qualquer ferramenta nativa.
- **Sem travar.** Um servidor que falha não impede os outros nem a abertura do
  Kestrel: o aviso aparece na conversa e o resto continua funcionando.

## Busca web e fallback (0.9.7)

`web_search` usa o **DuckDuckGo** por padrão. Se o mecanismo falhar (rede, HTTP
ou captcha) **ou não devolver nenhum resultado**, o Kestrel tenta, na ordem:

1. DuckDuckGo (`duckduckgo.com/html`, seguindo redirecionamento)
2. DuckDuckGo Lite (`lite.duckduckgo.com/lite`)
3. DuckDuckGo HTML (`html.duckduckgo.com/html`)
4. **Exa** (`https://api.exa.ai/search`), quando a chave está disponível

O resultado informa qual mecanismo respondeu e quais falharam/voltaram vazios.
Se todos falharem, o erro lista as tentativas; se algum respondeu sem
resultados, o modelo recebe essa informação para reformular a consulta.

```toml
[web]
engine = "duckduckgo"          # ou "searx" (SearxNG primeiro, DDG como fallback)
# exa_api_key_env = "EXA_API_KEY"
# exa_url = "https://api.exa.ai/search"
```

A chave do Exa é lida **apenas** da variável de ambiente (padrão
`EXA_API_KEY`) e nunca é gravada no TOML.

## Tokens, fallback e contexto (0.9.6)

- **Tokenizer real**: modelos com codificação conhecida usam `tiktoken`
  (`o200k_base`, `cl100k_base`, `p50k_base`, `r50k_base`), declarada em
  `provider.models[].tokenizer` ou reconhecida pelo id. O `usage` real do
  provedor calibra a estimativa por modelo; modelos sem tokenizer caem na
  estimativa por caracteres.
- **Fallback entre modelos**:

  ```toml
  [provider]
  fallback_models = ["nvidia/nemotron-3-super-120b-a12b", "meta/muse-glimmer-30b"]
  ```

  Se o modelo principal falhar por indisponibilidade, limite, rede ou 5xx, o
  próximo é tentado para aquele turno (o modelo padrão em disco não muda).
- **`search_project`**: retrieval BM25 local, respeitando `.gitignore`, que
  ranqueia arquivos e devolve trechos com números de linha.
- **`apply_patch`**: patch nativo com validação de contexto (`*** Begin Patch`
  ou diff unificado); se um hunk não bate, nada é escrito. `/branch` (ou
  `/checkpoint`) cria um ramo da sessão com histórico, plano, tarefas, decisões
  e alterações de arquivo.

## Sandbox de execução

Processos filhos podem ser confinados com **Landlock**:

```toml
[agent]
sandbox = "auto"            # auto | off | require
sandbox_allow_network = true
sandbox_extra_writable = ["/caminho/extra"]
```

- Leitura do sistema continua liberada (compiladores e bibliotecas).
- Escrita permitida apenas no projeto, `external_dirs`, `/tmp`, caches usuais
  (`~/.cache`, `~/.cargo`, `~/.npm`) e caminhos extras.
- `sandbox_allow_network = false` nega TCP no domínio Landlock.
- `require` falha ao iniciar se o kernel não oferecer Landlock; `auto` avisa e
  segue sem confinamento.

## Atalhos, busca, cópia e idioma (0.9.6)

- `Ctrl+F` busca na conversa; `Enter`/`↓` vão à próxima ocorrência e `↑` à
  anterior.
- `Ctrl+Y` copia a última resposta via OSC 52.
- `F1` ajuda, `F2` raciocínio, `Shift+Tab` permissões, `PageUp/PageDown` rolam.
- Tudo isso é personalizável:

  ```toml
  [ui]
  reduced_motion = false
  language = "pt-BR"        # ou "en-US"
  [ui.keybindings]
  buscar = "ctrl+g"
  copiar = "ctrl+y"
  ```

  Ações: `buscar`, `copiar`, `ajuda`, `raciocinio`, `permissoes`,
  `rolar_cima`, `rolar_baixo`, `interromper`. `/keys` mostra os atalhos atuais;
  `/lang en` e `/lang pt` trocam o idioma.
- A conversa é **virtualizada**: só as linhas visíveis são renderizadas, com
  cache de alturas por largura/versão.
- `/skills reload` relê os diretórios de skills sem reiniciar.


## Conversa

- **Trilha colorida** em cada bloco: leitura, escrita, comandos, web, falha e
  subagente têm cor própria — a conversa é lida por cor, não por glifo.
- **Clique esquerdo** sobre um bloco recolhe ou expande: saídas longas ficam em
  uma linha e abrem quando você quiser conferir.
- **Avisos temporários** no topo para confirmações rápidas (modo, permissão,
  modelo, skin, thinking) em vez de blocos permanentes na conversa.
- **Contexto discreto** no rodapé: usado/total e percentual em texto, sem
  barrinhas; processos ativos como um simples `⚙ N`.
- **Subagentes visíveis**: o trabalho delegado abre um grupo com a subtarefa, e
  cada ferramenta dele aparece recuada e identificada.
- **Histórico de sessões com exclusão em lote**: `/historico` marca várias
  sessões com espaço, `d` pede confirmação e exclui tudo de uma vez (a sessão
  atual nunca é excluída).
- **Atualização automática do catálogo**: ao abrir, o Kestrel consulta
  `/models` do provedor em segundo plano e mescla os modelos novos no catálogo
  local, preservando entradas manuais; quando há novidades, um aviso temporário
  aparece (`modelos novos — veja /model`). A descoberta é filtrada pela
  allowlist: apenas chat/free confirmados entram (veja “Catálogo de modelos”).

## Dados e privacidade

- Configuração: `~/.config/kestrel/config.toml`
- Comps: `~/.config/kestrel/comps/<nome>.toml`
- Skins próprias: `~/.config/kestrel/skins/<nome>.toml`
- Banco (sessões, mensagens, auditoria, planos): `~/.local/share/kestrel/kestrel.db`
- Histórico de entrada: `~/.local/share/kestrel/historico_entrada.txt`
- Chaves de API são lidas apenas de variáveis de ambiente (`NVIDIA_API_KEY` por
  padrão; o nome é configurável) e são redigidas de qualquer texto exibido ou
  gravado.

## Desenvolvimento

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

O PNG do Pip é embutido no binário em tempo de compilação. Se `assets/pip.png`
mudar, basta recompilar — não há textura gerada nem passo extra.
