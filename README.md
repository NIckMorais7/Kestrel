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

- **agente** (padrão): investiga, planeja, edita arquivos e executa comandos
  conforme o nível de permissão.
- **chat**: somente conversa e leitura; ferramentas que escrevem, executam ou
  planejam ficam indisponíveis e o modelo é instruído a não alterar nada.

Troque com `/chat` e `/agente`. O modo atual aparece no cabeçalho, é persistido
na configuração e registrado na auditoria.

## Permissões

Quatro níveis (`/permissions` ou Shift+Tab), aplicados em uma única camada
central:

| Nível  | O que permite                                                        |
|--------|----------------------------------------------------------------------|
| read   | **ler** qualquer caminho (dentro ou fora do projeto); nenhuma escrita ou execução |
| write  | ler, criar, editar e excluir — dentro **e fora** do projeto; `rm -rf` em caminho comum; operações sensíveis (instalação, rede, privilégios) pedem confirmação |
| semi   | tudo do write sem confirmação, mais `rm -rf`, instalações e rede; só o que **perde dado** (git destrutivo) pede confirmação |
| full   | idem ao semi, mais conteúdo baixado da rede executado direto (pipe para interpretador); **só git destrutivo pede confirmação** |

**Fora do projeto não é proibido**: a regra é pela operação. Ler qualquer caminho
funciona em todos os níveis; escrever (criar/editar/excluir) funciona de `write` em
diante e é bloqueado em `read`.

Rodar código do projeto (`node script.js`, `python app.py`, `bun test`, `go run .`, …) é
fluxo de desenvolvimento e passa sem confirmação em `semi`/`full`.

**O que nunca passa** (bloqueado em qualquer nível): formatar disco (`mkfs`, `dd` em
dispositivo), fork bomb, desligar o sistema, e escrever ou remover em `/etc`, `/usr`,
`/sys`, `/proc`, `/dev`, `/boot`, `/bin`, `/sbin`, `/var/lib` ou na raiz `/`.

**Sem limite de iterações**: o agente trabalha até concluir a tarefa, ser interrompido
por você (`Esc` ou `Ctrl+C`) ou a compactação de contexto não conseguir mais manter a
conversa. O progresso aparece na interface como `trabalhando… Nª interação`.

O que ainda pede confirmação em `semi`/`full` é só o que **perde dado sem volta**:
`git reset --hard`, `git push --force`, `git clean -fdx`, `git filter-branch`.

## Comandos da interface

Digite `/` e use Tab/Enter para completar (um comando parcial **não** é
executado por acidente: Enter completa, o segundo Enter executa).

`/help` lista todos os comandos com os argumentos reais: `/model`, `/chat`,
`/agent`, `/plan`, `/permissions`, `/thinking`, `/history`, `/resume`, `/new`,
`/projects`, `/processes`, `/skills`, `/config`, `/clear`, `/quit`.

Os comandos são em inglês e **todos aceitam apelido em português** (`/modelo`,
`/conversa`, `/agente`, `/limpar`, `/sair`, `/nova`, `/projetos`, `/analisar`,
`/processos`, `/retomar`, `/historico`), então quem digita em português não
perde o hábito.

## Pip

O mascote é a imagem original do Pip (`assets/pip.png`, cópia fiel de
`/home/nick/Assets/Pip/pip.png`, conferida por sha256), reamostrada pelo script
`scripts/gerar_textura_pip.py` para a textura em `src/ui/pip_textura.rs`.
Nenhum pixel é redesenhado à mão.

A renderização usa meias-células (`▀`) com cor verdadeira, o que preserva a
proporção do desenho em terminais comuns. A arte é geométrica: a massa branca
do gato com a tinta (contorno e olhos) vazada em transparência — no terminal os
furos deixam aparecer o fundo escuro do aplicativo, ou seja, a própria tinta do
desenho.

Há **uma única resolução**, escolhida a dedo: 3×2 texels = 3 colunas × **1
linha**, a metade do tamanho anterior (que já era reflexo de uma redução ainda
maior). A progressão inteira do mascote:

| versão | tamanho | onde aparecia |
|--------|---------|----------------|
| original | 28×34 texels = 17 linhas | companheiro fixo da conversa (coluna direita) |
| 2ª | 20×24 texels = 12 linhas | assistente de configuração |
| 3ª | 20×13 texels = 7 linhas | assistente de configuração |
| 4ª | 6×4 texels = 2 linhas | indicador de trabalho |
| **atual** | **3×2 texels = 1 linha** | indicador de trabalho e assistente |

Hoje ele cabe em uma linha em qualquer lugar da interface:

- **junto ao campo de mensagem**, como indicador de trabalho: enquanto o modelo
  pensa, executa ferramentas ou redige a resposta, o Pip respira e pisca no
  lugar da antiga animação de pontos; quando o turno termina, ele sai de cena;
- **no assistente de configuração**, com o estado (`contemplando`,
  `raciocinando`, `ocioso`, …) logo abaixo.

O desenho é mínimo por decisão: nada de mascote grande ocupando a conversa.
Limitações documentadas:

- não há suporte a protocolos gráficos (sixel/kitty/iterm); nesses terminais o
  Pip aparece como blocos coloridos, equivalentes aos demais;
- terminais sem cor verdadeira aproximam as cores;
- com altura de terminal abaixo de 14 linhas o indicador dá lugar ao status
  compacto, para não espremer a conversa;
- a animação tem somente respiração (deslocamento vertical de meio caractere,
  sem mudar a altura ocupada) e piscada (os furos dos olhos fecham com a cor do
  pelo, derivada dos olhos da própria imagem), desligada com
  `ui.show_mascot = false`;
- o fundo do aplicativo é um azul-acinzentado escuro (RGB 24,27,34), não
  preto — preto absoluto parecia OLED e cansava a vista em sessões longas; e
  todos os blocos usam cantos arredondados.

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
- ou clique no atalho `↓ voltar para o último`, que aparece no canto superior
  direito do campo de mensagem enquanto você estiver fora do fim.

## Diretório inicial

O Kestrel começa **no diretório de onde foi chamado**. Fixar um projeto
(`/projetos`, escolha explícita) grava `agent.pin_workdir = true` e passa a
abrir sempre nesse diretório — para voltar ao comportamento "onde estou",
desligue a fixação em `/config` ou apague os dois campos do `config.toml`.

## Dados e privacidade

- Configuração: `~/.config/kestrel/config.toml`
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

A regeneração da textura do Pip é opcional (só quando `assets/pip.png` mudar):

```bash
python3 scripts/gerar_textura_pip.py    # requer Pillow e numpy; aplica rustfmt sozinho
```

Se `rustfmt` não estiver no `PATH`, rode `cargo fmt` depois de regenerar,
para o arquivo seguir o mesmo estilo do restante do código.
