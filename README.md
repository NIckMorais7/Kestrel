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

A renderização é feita pelo `ratatui-image`, sempre em **halfblocks**
(`▀`/`▄`) no tamanho fixo de 6×2. Halfblocks evita o “rastro”/duplicação que
protocolos gráficos estado-dependentes causavam ao redesenhar a animação. A arte
continua geométrica: a massa branca do gato com a tinta (contorno e olhos)
vazada; os buracos dos olhos são pintados com a cor de fundo da aplicação, então
não ficam brancos em terminal claro.

O ciclo tem 128 tiques e é determinístico: repouso respirando (e piscando),
caminhada leve para a esquerda e volta, caminhada leve para a direita e volta,
repouso de novo.

A partir da v0.5.0, o Pip **não fica mais no rodapé** nem no assistente. Ele
aparece somente na **recepção de nova sessão**, no chat, junto de instruções
curtas. Ao enviar a primeira mensagem, o Pip e as instruções somem juntos e a
conversa segue limpa.

Durante o turno, o progresso aparece no chat como uma **estrela branca** que
cresce e diminuindo, fluida, acima do campo de mensagem.

Limitações documentadas:

- terminais sem cor verdadeira aproximam as cores;
- a animação é sempre contida dentro da mesma caixa de 6×2 e pode ser desligada
  com `ui.show_mascot = false`;
- o fundo do aplicativo é **preto** e o acento é branco, no estilo Claude Code
  (a skin `daylight` mantém um tema claro separado); a conversa não usa caixa, e
  apenas o campo de mensagem e os modais têm cantos arredondados.

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

## Diretório inicial

O Kestrel começa **no diretório de onde foi chamado**. Fixar um projeto
(`/projetos`, escolha explícita) grava `agent.pin_workdir = true` e passa a
abrir sempre nesse diretório — para voltar ao comportamento "onde estou",
desligue a fixação em `/config` ou apague os dois campos do `config.toml`.

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
