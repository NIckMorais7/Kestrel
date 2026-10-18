//! Editor de entrada de linha de comando com histórico e cursor completo.

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
    pub fn inserir_texto(&mut self, s: &str) {
        if s.is_empty() {
            return;
        }
        let pos = self.byte_pos(self.cursor);
        self.texto.insert_str(pos, s);
        self.cursor += s.chars().count();
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
    pub fn posicao_cursor(&self) -> (usize, usize) {
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
                coluna += 1;
            }
        }
        (linha, coluna)
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
        let (inicio_anterior, fim_anterior) = self.limites_da_linha(linha - 1);
        self.cursor = (inicio_anterior + coluna).min(fim_anterior);
        true
    }

    /// Move o cursor uma linha para baixo; devolve `false` se já estava na última.
    pub fn baixo(&mut self) -> bool {
        let (linha, coluna) = self.posicao_cursor();
        if linha + 1 >= self.total_linhas() {
            return false;
        }
        let (inicio_seguinte, fim_seguinte) = self.limites_da_linha(linha + 1);
        self.cursor = (inicio_seguinte + coluna).min(fim_seguinte);
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
        nome: "/thinking",
        argumentos: "[on|off]",
        descricao: "mostra ou oculta o raciocínio do modelo na conversa (padrão: oculto)",
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
        descricao: "modo agente: trabalha no projeto conforme as permissões",
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
