//! Editor de entrada de linha de comando com histórico e cursor completo.

/// Teto de caracteres do editor (e de uma única colagem). Escolhido para
/// comportar com folga qualquer mensagem legítima sem deixar cada tecla
/// lenta: acima disso o custo de `byte_pos` por movimento de cursor já se
/// sente, e o que vem depois é lixo de paste acidental.
pub const LIMITE_TEXTO: usize = 200_000;

/// Editor de texto multilinha simples, com histórico de envios.
pub struct Editor {
    texto: String,
    /// Posição do cursor em caracteres (não bytes).
    cursor: usize,
    historico: Vec<String>,
    indice_historico: Option<usize>,
    max_historico: usize,
    rascunho: String,
}

impl Editor {
    pub fn novo(max_historico: usize) -> Self {
        Self {
            texto: String::new(),
            cursor: 0,
            historico: Vec::new(),
            indice_historico: None,
            max_historico: max_historico.max(10),
            rascunho: String::new(),
        }
    }

    pub fn texto(&self) -> &str {
        &self.texto
    }

    pub fn vazio(&self) -> bool {
        self.texto.is_empty()
    }

    pub fn carregar_historico(&mut self, itens: Vec<String>) {
        self.historico = itens;
        if self.historico.len() > self.max_historico {
            let inicio = self.historico.len() - self.max_historico;
            self.historico.drain(..inicio);
        }
    }

    pub fn historico_recente(&self, max: usize) -> Vec<String> {
        let inicio = self.historico.len().saturating_sub(max);
        self.historico[inicio..].to_vec()
    }

    pub fn definir_texto(&mut self, texto: String) {
        self.cursor = texto.chars().count();
        self.texto = texto;
        self.indice_historico = None;
    }

    pub fn limpar(&mut self) {
        self.texto.clear();
        self.cursor = 0;
        self.indice_historico = None;
    }

    /// Remove e devolve o conteúdo, registrando-o no histórico.
    pub fn enviar(&mut self) -> Option<String> {
        let texto = self.texto.trim().to_string();
        self.limpar();
        if texto.is_empty() {
            return None;
        }
        if self.historico.last() != Some(&texto) {
            self.historico.push(texto.clone());
            if self.historico.len() > self.max_historico {
                self.historico.remove(0);
            }
        }
        Some(texto)
    }

    fn byte_pos(&self, indice_chars: usize) -> usize {
        self.texto
            .char_indices()
            .nth(indice_chars)
            .map(|(i, _)| i)
            .unwrap_or(self.texto.len())
    }

    pub fn inserir_char(&mut self, c: char) {
        let pos = self.byte_pos(self.cursor);
        self.texto.insert(pos, c);
        self.cursor += 1;
        self.indice_historico = None;
    }

    /// Insere um texto de uma vez só.
    ///
    /// A versão anterior chamava `inserir_char` por caractere: cada chamada
    /// varria o texto do início para achar o offset e realocava a String, o que
    /// é O(n²) — colar 100 mil caracteres travava a interface por ~10 s.
    ///
    /// A colagem também tem teto: sem limite, um bracketed paste de megabytes
    /// entrava inteiro no editor, onde cada tecla seguinte é O(n) (o offset é
    /// buscado do início do texto a cada movimento de cursor). O excedente é
    /// descartado — perder o fim do paste é melhor que travar a interface.
    pub fn inserir_texto(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let disponivel = LIMITE_TEXTO.saturating_sub(self.texto.chars().count());
        if disponivel == 0 {
            return;
        }
        let fim = s
            .char_indices()
            .nth(disponivel)
            .map(|(i, _)| i)
            .unwrap_or(s.len());
        let recorte = &s[..fim];
        let pos = self.byte_pos(self.cursor);
        self.texto.insert_str(pos, recorte);
        self.cursor += recorte.chars().count();
        self.indice_historico = None;
    }

    pub fn nova_linha(&mut self) {
        self.inserir_char('\n');
    }

    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        let inicio = self.byte_pos(self.cursor - 1);
        let fim = self.byte_pos(self.cursor);
        self.texto.replace_range(inicio..fim, "");
        self.cursor -= 1;
        self.indice_historico = None;
    }

    pub fn apagar(&mut self) {
        if self.cursor >= self.texto.chars().count() {
            return;
        }
        let inicio = self.byte_pos(self.cursor);
        let fim = self.byte_pos(self.cursor + 1);
        self.texto.replace_range(inicio..fim, "");
        self.indice_historico = None;
    }

    /// Posição do cursor como (linha, coluna), ambos com base zero.
    ///
    /// A coluna é em **células** de terminal, não em caracteres: o terminal
    /// posiciona o cursor por célula, então um emoji ou um CJK (2 células)
    /// deixava o cursor visual atrás do texto, e um combinante (0 células)
    /// adiantava. Quem conta por caractere discorda do que se vê na tela.
    pub fn posicao_cursor(&self) -> (usize, usize) {
        use unicode_width::UnicodeWidthChar;
        let mut linha = 0;
        let mut coluna = 0;
        for (i, c) in self.texto.chars().enumerate() {
            if i == self.cursor {
                break;
            }
            if c == '\n' {
                linha += 1;
                coluna = 0;
            } else {
                coluna += UnicodeWidthChar::width(c).unwrap_or(0);
            }
        }
        (linha, coluna)
    }

    /// Índice (em caracteres) em que a colona em células fecha dentro da
    /// linha. Usado por `cima`/`baixo`, que navegam pela coluna visual.
    fn indice_por_coluna(&self, linha: usize, coluna_celulas: usize) -> usize {
        use unicode_width::UnicodeWidthChar;
        let (inicio, fim) = self.limites_da_linha(linha);
        let mut celulas = 0usize;
        for (i, c) in self
            .texto
            .chars()
            .enumerate()
            .skip(inicio)
            .take(fim.saturating_sub(inicio))
        {
            if celulas >= coluna_celulas {
                return i;
            }
            celulas += UnicodeWidthChar::width(c).unwrap_or(0);
        }
        fim
    }

    pub fn total_linhas(&self) -> usize {
        self.texto.chars().filter(|c| *c == '\n').count() + 1
    }

    fn limites_da_linha(&self, linha: usize) -> (usize, usize) {
        let mut inicio = 0usize;
        let mut atual = 0usize;
        for (i, c) in self.texto.chars().enumerate() {
            if atual == linha && c == '\n' {
                return (inicio, i);
            }
            if c == '\n' {
                atual += 1;
                inicio = i + 1;
            }
        }
        (inicio, self.texto.chars().count())
    }

    pub fn esquerda(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    pub fn direita(&mut self) {
        if self.cursor < self.texto.chars().count() {
            self.cursor += 1;
        }
    }

    pub fn home(&mut self) {
        let (linha, _) = self.posicao_cursor();
        let (inicio, _) = self.limites_da_linha(linha);
        self.cursor = inicio;
    }

    pub fn fim(&mut self) {
        let (linha, _) = self.posicao_cursor();
        let (_, fim) = self.limites_da_linha(linha);
        self.cursor = fim;
    }

    /// Move o cursor uma linha para cima; devolve `false` se já estava na
    /// primeira (o chamador pode então navegar no histórico).
    pub fn cima(&mut self) -> bool {
        let (linha, coluna) = self.posicao_cursor();
        if linha == 0 {
            return false;
        }
        self.cursor = self.indice_por_coluna(linha - 1, coluna);
        true
    }

    /// Move o cursor uma linha para baixo; devolve `false` se já estava na última.
    pub fn baixo(&mut self) -> bool {
        let (linha, coluna) = self.posicao_cursor();
        if linha + 1 >= self.total_linhas() {
            return false;
        }
        self.cursor = self.indice_por_coluna(linha + 1, coluna);
        true
    }

    /// Navega para a entrada anterior do histórico (mais recente primeiro).
    pub fn historico_anterior(&mut self) {
        if self.historico.is_empty() {
            return;
        }
        let indice = match self.indice_historico {
            Some(0) => 0,
            Some(i) => i - 1,
            None => {
                self.rascunho = self.texto.clone();
                self.historico.len() - 1
            }
        };
        self.indice_historico = Some(indice);
        let texto = self.historico[indice].clone();
        self.definir_texto(texto);
        self.indice_historico = Some(indice);
    }

    /// Navega para a entrada seguinte do histórico; ao final, devolve o rascunho.
    pub fn historico_proximo(&mut self) {
        let Some(indice) = self.indice_historico else {
            return;
        };
        if indice + 1 >= self.historico.len() {
            self.indice_historico = None;
            let rascunho = std::mem::take(&mut self.rascunho);
            self.definir_texto(rascunho);
            self.indice_historico = None;
        } else {
            self.indice_historico = Some(indice + 1);
            let texto = self.historico[indice + 1].clone();
            self.definir_texto(texto);
            self.indice_historico = Some(indice + 1);
        }
    }

    /// Sugestões de comando quando a entrada começa com `/`.
    pub fn sugestoes(&self) -> Vec<&'static ComandoSlash> {
        let texto = self.texto.trim_start();
        if !texto.starts_with('/') || texto.contains(' ') {
            return Vec::new();
        }
        let prefixo = texto.to_lowercase();
        COMANDOS_SLASH
            .iter()
            .filter(|c| c.nome.starts_with(&prefixo))
            .collect()
    }

    /// Completa o comando atualmente digitado com o primeiro da lista.
    pub fn completar(&mut self, comando: &'static ComandoSlash) {
        self.definir_texto(format!("{} ", comando.nome));
        self.indice_historico = None;
    }
}

/// Comando de barra suportado pela interface.
pub struct ComandoSlash {
    pub nome: &'static str,
    pub argumentos: &'static str,
    pub descricao: &'static str,
}

pub const COMANDOS_SLASH: &[ComandoSlash] = &[
    ComandoSlash {
        nome: "/help",
        argumentos: "",
        descricao: "mostra a ajuda completa com todos os comandos",
    },
    ComandoSlash {
        nome: "/keys",
        argumentos: "",
        descricao: "mostra os atalhos atuais e como personalizá-los",
    },
    ComandoSlash {
        nome: "/branch",
        argumentos: "[nome]",
        descricao: "cria um ramo/checkpoint da sessão atual com histórico e estado",
    },
    ComandoSlash {
        nome: "/lang",
        argumentos: "[pt|en]",
        descricao: "troca o idioma da interface (pt-BR ou en-US)",
    },
    ComandoSlash {
        nome: "/model",
        argumentos: "[nome]",
        descricao: "lista os modelos do catálogo ou troca o modelo atual",
    },
    ComandoSlash {
        nome: "/effort",
        argumentos: "[nível]",
        descricao: "escolhe o esforço de raciocínio quando o modelo suporta",
    },
    ComandoSlash {
        nome: "/undo",
        argumentos: "",
        descricao: "desfaz a última alteração de arquivo feita nesta sessão",
    },
    ComandoSlash {
        nome: "/diff",
        argumentos: "",
        descricao: "mostra as alterações de arquivo da sessão, uma a uma",
    },
    ComandoSlash {
        nome: "/cost",
        argumentos: "",
        descricao: "mostra os tokens gastos na sessão (entrada, saída e raciocínio)",
    },
    ComandoSlash {
        nome: "/thinking",
        argumentos: "[on|off]",
        descricao: "mostra ou oculta o raciocínio do modelo na conversa (padrão: oculto)",
    },
    ComandoSlash {
        nome: "/person",
        argumentos: "[new|use|edit|see|remove|off] [nome]",
        descricao: "Comps (personalidades): cria, edita, ativa e remove comportamentos do agente",
    },
    ComandoSlash {
        nome: "/skin",
        argumentos: "[nome]",
        descricao: "lista ou troca a skin visual (kestrel, mono, daylight)",
    },
    ComandoSlash {
        nome: "/permissions",
        argumentos: "[nível]",
        descricao: "mostra ou altera o nível de permissão (read/write/semi/full)",
    },
    ComandoSlash {
        nome: "/chat",
        argumentos: "",
        descricao: "modo conversa: só leitura, nada no projeto é modificado ou executado",
    },
    ComandoSlash {
        nome: "/agent",
        argumentos: "",
        descricao: "modo build: trabalha no projeto conforme as permissões",
    },
    ComandoSlash {
        nome: "/clear",
        argumentos: "",
        descricao: "limpa a conversa exibida (o histórico continua salvo)",
    },
    ComandoSlash {
        nome: "/skills",
        argumentos: "",
        descricao: "lista as skills instaladas e o que cada uma faz",
    },
    ComandoSlash {
        nome: "/plan",
        argumentos: "",
        descricao: "mostra o plano e as subtarefas da sessão atual",
    },
    ComandoSlash {
        nome: "/stop",
        argumentos: "",
        descricao: "interrompe o turno em execução, preservando alterações",
    },
    ComandoSlash {
        nome: "/history",
        argumentos: "[termo]",
        descricao: "lista sessões salvas ou busca no histórico de mensagens",
    },
    ComandoSlash {
        nome: "/projects",
        argumentos: "",
        descricao: "lista projetos recentes e permite trocar o diretório de trabalho",
    },
    ComandoSlash {
        nome: "/analyze",
        argumentos: "",
        descricao: "reanalisa o projeto ativo (linguagens, build, testes)",
    },
    ComandoSlash {
        nome: "/processes",
        argumentos: "",
        descricao: "lista os processos gerenciados e o estado de cada um",
    },
    ComandoSlash {
        nome: "/config",
        argumentos: "",
        descricao: "mostra o resumo da configuração e o caminho do arquivo",
    },
    ComandoSlash {
        nome: "/resume",
        argumentos: "<id|last>",
        descricao: "retoma uma sessão salva de onde parou",
    },
    ComandoSlash {
        nome: "/new",
        argumentos: "",
        descricao: "inicia uma nova sessão sem apagar o histórico",
    },
    ComandoSlash {
        nome: "/quit",
        argumentos: "",
        descricao: "encerra o Kestrel com segurança",
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edicao_basica() {
        let mut e = Editor::novo(50);
        e.inserir_texto("olá mundo");
        assert_eq!(e.texto(), "olá mundo");
        e.backspace();
        assert_eq!(e.texto(), "olá mund");
        e.home();
        e.apagar();
        assert_eq!(e.texto(), "lá mund");
        e.fim();
        e.inserir_char('!');
        assert_eq!(e.texto(), "lá mund!");
    }

    #[test]
    fn cursor_e_linhas() {
        let mut e = Editor::novo(50);
        e.inserir_texto("abc\ndefg");
        assert_eq!(e.posicao_cursor(), (1, 4));
        assert!(e.cima());
        assert_eq!(e.posicao_cursor(), (0, 3));
        e.home();
        assert_eq!(e.posicao_cursor(), (0, 0));
        assert!(!e.cima());
        assert!(e.baixo());
        assert_eq!(e.posicao_cursor(), (1, 0));
    }

    #[test]
    fn multilinha_insere_quebra() {
        let mut e = Editor::novo(50);
        e.inserir_texto("linha1");
        e.nova_linha();
        e.inserir_texto("linha2");
        assert_eq!(e.texto(), "linha1\nlinha2");
        assert_eq!(e.total_linhas(), 2);
    }

    /// A coluna do cursor é em CÉLULAS de terminal, não em caracteres: o
    /// terminal posiciona o cursor por célula, e um emoji/CJK (2 células)
    /// deixava o cursor visual atrás do texto digitado.
    #[test]
    fn posicao_do_cursor_conta_celulas_nao_caracteres() {
        let mut e = Editor::novo(50);
        e.inserir_texto("Olá 🚀");
        // 3 células de "Olá", 1 do espaço e 2 do emoji.
        assert_eq!(e.posicao_cursor(), (0, 6));
        let mut cjk = Editor::novo(50);
        cjk.inserir_texto("日本");
        assert_eq!(cjk.posicao_cursor(), (0, 4));
    }

    /// `cima`/`baixo` navegam pela coluna VISUAL: descer depois de um emoji
    /// precisa parar na mesma coluna de células, não no mesmo índice de char.
    #[test]
    fn navegacao_vertical_usa_a_coluna_visual() {
        let mut e = Editor::novo(50);
        e.inserir_texto("🚀🚀");
        e.nova_linha();
        e.inserir_texto("abcdef");
        // Cursor no fim da segunda linha: coluna 6 em células.
        assert_eq!(e.posicao_cursor(), (1, 6));
        assert!(e.cima());
        // A primeira linha só tem 4 colunas de células: encosta no fim dela.
        assert_eq!(e.posicao_cursor(), (0, 4));
    }

    /// Colar sem limite deixava o editor com megabytes de texto, onde cada
    /// tecla seguinte é O(n): o excedente é descartado com o teto do editor.
    #[test]
    fn colagem_respeita_o_limite_do_editor() {
        let mut e = Editor::novo(50);
        e.inserir_texto("a");
        let gigante = "b".repeat(LIMITE_TEXTO * 2);
        e.inserir_texto(&gigante);
        assert_eq!(e.texto().chars().count(), LIMITE_TEXTO);
        // Com o editor cheio, nova colagem não entra.
        e.inserir_texto("mais");
        assert_eq!(e.texto().chars().count(), LIMITE_TEXTO);
    }

    #[test]
    fn historico_navega_e_restaura_rascunho() {
        let mut e = Editor::novo(50);
        e.definir_texto("primeira".into());
        e.enviar();
        e.definir_texto("segunda".into());
        e.enviar();
        e.definir_texto("rascunho".into());
        e.historico_anterior();
        assert_eq!(e.texto(), "segunda");
        e.historico_anterior();
        assert_eq!(e.texto(), "primeira");
        e.historico_anterior();
        assert_eq!(e.texto(), "primeira");
        e.historico_proximo();
        e.historico_proximo();
        assert_eq!(e.texto(), "rascunho");
    }

    #[test]
    fn sugestoes_de_comando() {
        let mut e = Editor::novo(50);
        e.definir_texto("/mo".into());
        let sugestoes = e.sugestoes();
        assert_eq!(sugestoes.len(), 1);
        assert_eq!(sugestoes[0].nome, "/model");
        e.completar(sugestoes[0]);
        assert_eq!(e.texto(), "/model ");
        assert!(e.sugestoes().is_empty());
        e.definir_texto("/xyz".into());
        assert!(e.sugestoes().is_empty());
    }

    /// Todos os comandos são em inglês: um comando principal em português
    /// quebraria a regra (e a tabulação completaria algo que ninguém digita).
    #[test]
    fn comandos_sao_todos_em_ingles() {
        for c in COMANDOS_SLASH {
            assert!(
                c.nome.starts_with('/'),
                "comando sem barra inicial: {}",
                c.nome
            );
            let corpo = &c.nome[1..];
            assert!(
                corpo.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'),
                "comando `{}` não está em inglês minúsculo",
                c.nome
            );
            assert!(!c.descricao.is_empty(), "{} sem descrição", c.nome);
        }
        // Os apelidos em português continuam existindo no despacho — a regra é
        // sobre o nome canônico, não sobre o que ainda pode ser digitado.
        let nomes: Vec<&str> = COMANDOS_SLASH.iter().map(|c| c.nome).collect();
        for esperado in [
            "/help",
            "/model",
            "/effort",
            "/thinking",
            "/permissions",
            "/chat",
            "/agent",
            "/clear",
            "/skills",
            "/plan",
            "/stop",
            "/history",
            "/projects",
            "/analyze",
            "/processes",
            "/config",
            "/resume",
            "/new",
            "/quit",
        ] {
            assert!(nomes.contains(&esperado), "faltou {esperado}");
        }
    }

    #[test]
    fn enviar_limpa_e_registra() {
        let mut e = Editor::novo(50);
        e.definir_texto("  conteúdo  ".into());
        assert_eq!(e.enviar().as_deref(), Some("conteúdo"));
        assert!(e.vazio());
        assert_eq!(e.enviar(), None);
    }
}
