#!/usr/bin/env python3
"""Gera src/ui/pip_textura.rs a partir da imagem original do Pip.

A textura NÃO é redesenhada: a silhueta e os olhos vêm diretamente de
assets/pip.png (cópia fiel de /home/nick/Assets/Pip/pip.png, verificável por
sha256). O desenho original é arte geométrica: uma massa branca opaca com a
"tinta" (contorno e olhos) vazada como transparência. Para o terminal, cada
texel recebe a fração de branco da célula correspondente; no fundo quase preto
do aplicativo os furos aparecem como a tinta do próprio desenho.

Em resoluções minúsculas os olhos podem cair entre texels; por isso eles são
detectados no original e garantidos como texels próprios, com o mesmo tamanho
proporcional da imagem (mínimo de um texel) — é a única adaptação, e ela
preserva a geometria da arte original.

Uso:  python3 scripts/gerar_textura_pip.py [--prever]
"""

from __future__ import annotations

import hashlib
import math
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

try:
    import numpy as np
    from PIL import Image
except ImportError as erro:  # pragma: no cover - caminho de ambiente
    raise SystemExit(
        "este script precisa de Pillow e numpy. Instale com:\n"
        "  python3 -m pip install --user Pillow numpy\n"
        f"(erro original: {erro})"
    )

RAIZ = Path(__file__).resolve().parent.parent
ORIGEM = RAIZ / "assets" / "pip.png"
DESTINO = RAIZ / "src" / "ui" / "pip_textura.rs"

# Resoluções (em texels) usadas pela interface: cada texel vira meia célula.
#
# A miniatura do Pip é derivada por **largura**, não por altura: a célula do
# terminal é 1:2, então a proporção real da imagem (≈1,53:1, mais larga que
# alta) significa que 6 colunas de texels dão 4 texels de altura = 2 linhas —
# e é aí que o rosto, os olhos e as patas continuam reconhecíveis.
#
# Já se mediram: com só 2 texels de altura (1 linha) o desenho virou um bloco
# (`### ###`), porque a silhueta da imagem não cabe em 6 texels. A derivada
# `altura:2` foi o que produziu os "bloquinhos animados" que o usuário
# rejeitou.
TAMANHOS = [
    ("Mini", {"largura": 6}),
]

# Massa branca: fração mínima do texel para ele ficar visível.
LIMIAR_MASSA = 0.45
BRANCO = 0xFFFFFFFF
# Olho fechado na piscada: o furo some no branco do rosto.
COR_PELAGEM = 0xFFFFFFFF
COR_PALPEBRA = 0xFFFFFFFF


def fundo_externo(transparente: np.ndarray, fator: int = 8) -> np.ndarray:
    """Marque os pixels transparentes ligados à borda (o fundo de fora).

    A propagação é feita numa grade reduzida (qualquer transparência na célula
    conta) e devolvida ampliada; sobras de borda são inofensivas porque os
    furos procurados são bem maiores que o fator.
    """
    altura, largura = transparente.shape
    linhas = -(-altura // fator)
    colunas = -(-largura // fator)
    grade = np.zeros((linhas * fator, colunas * fator), dtype=bool)
    grade[:altura, :largura] = transparente
    grade = grade.reshape(linhas, fator, colunas, fator).any(axis=(1, 3))

    pilha = [
        (y, x)
        for y in range(linhas)
        for x in range(colunas)
        if grade[y, x] and (y in (0, linhas - 1) or x in (0, colunas - 1))
    ]
    fora = np.zeros_like(grade)
    for y, x in pilha:
        fora[y, x] = True
    while pilha:
        y, x = pilha.pop()
        for dy, dx in ((1, 0), (-1, 0), (0, 1), (0, -1)):
            ny, nx = y + dy, x + dx
            if 0 <= ny < linhas and 0 <= nx < colunas and grade[ny, nx] and not fora[ny, nx]:
                fora[ny, nx] = True
                pilha.append((ny, nx))
    ampliado = np.repeat(np.repeat(fora, fator, axis=0), fator, axis=1)
    return ampliado[:altura, :largura]


def carregar():
    """Silhueta (massa branca) e retângulos dos olhos a partir do original."""
    imagem = Image.open(ORIGEM).convert("RGBA")
    dados = np.array(imagem)
    opaco = dados[..., 3] >= 128
    ys, xs = np.where(opaco)
    x0, x1, y0, y1 = xs.min(), xs.max(), ys.min(), ys.max()
    massa = opaco[y0:y1 + 1, x0:x1 + 1]

    # Os olhos são os furos da arte: trechos transparentes fechados dentro da
    # massa, sem ligação com o fundo de fora. Grandes retângulos verticais.
    buraco = ~massa & ~fundo_externo(~massa)
    altura, largura = massa.shape
    visto = np.zeros_like(buraco)
    olhos = []
    for y in range(altura):
        for x in np.where(buraco[y] & ~visto[y])[0]:
            pilha = [(y, int(x))]
            visto[y, x] = True
            minx = maxx = int(x)
            miny = maxy = y
            total = 0
            while pilha:
                cy, cx = pilha.pop()
                total += 1
                minx = min(minx, cx)
                maxx = max(maxx, cx)
                miny = min(miny, cy)
                maxy = max(maxy, cy)
                for dy, dx in ((1, 0), (-1, 0), (0, 1), (0, -1)):
                    ny, nx = cy + dy, cx + dx
                    if (
                        0 <= ny < altura
                        and 0 <= nx < largura
                        and buraco[ny, nx]
                        and not visto[ny, nx]
                    ):
                        visto[ny, nx] = True
                        pilha.append((ny, nx))
            if total >= 2000 and maxy - miny + 1 >= 60 and maxx - minx + 1 >= 30:
                olhos.append((minx, miny, maxx, maxy))
    olhos.sort()
    if len(olhos) != 2:
        raise SystemExit(
            f"esperava exatamente dois olhos na imagem, encontrei {len(olhos)}: {olhos}"
        )
    return massa, olhos


def quantizar(massa: np.ndarray, olhos, restricao: dict):
    altura_orig, largura_orig = massa.shape
    if "largura" in restricao:
        largura = restricao["largura"]
        altura = max(1, round(altura_orig * largura / largura_orig))
    else:
        altura = restricao["altura"]
        largura = max(1, round(largura_orig * altura / altura_orig))

    # Fração de branco por texel (reamostragem por área, determinística).
    fracao = np.zeros((altura, largura))
    contagem = np.zeros((altura, largura))
    mapa_y = (np.arange(altura_orig) * altura // altura_orig).clip(0, altura - 1)
    mapa_x = (np.arange(largura_orig) * largura // largura_orig).clip(0, largura - 1)
    np.add.at(fracao, (mapa_y[:, None], mapa_x[None, :]), massa)
    np.add.at(contagem, (mapa_y[:, None], mapa_x[None, :]), np.ones_like(massa, dtype=float))
    grade = np.where(fracao / np.maximum(contagem, 1) >= LIMIAR_MASSA, 1, 0)

    # Olhos: retângulo limpo, com o tamanho proporcional ao do original
    # (mínimo de um texel), centrado no centro do olho mapeado para a grade.
    # Texels em que o olho cobre pelo menos metade da célula também entram na
    # conta, de modo que o retângulo final cobre exatamente o que a
    # reamostragem já indicava como furo — nunca menos que a proporção.
    caixas = []
    for (ox0, oy0, ox1, oy1) in olhos:
        largura_o = (ox1 - ox0 + 1) * largura / largura_orig
        altura_o = (oy1 - oy0 + 1) * altura / altura_orig
        largura_t = max(1, round(largura_o))
        altura_t = max(1, round(altura_o))
        centro_x = (ox0 + ox1) / 2 * largura / largura_orig
        centro_y = (oy0 + oy1) / 2 * altura / altura_orig
        x_ini = max(0, min(largura - largura_t, math.floor(centro_x - largura_t / 2 + 0.5)))
        y_ini = max(0, min(altura - altura_t, math.floor(centro_y - altura_t / 2 + 0.5)))
        x_fim = x_ini + largura_t - 1
        y_fim = y_ini + altura_t - 1

        pixels = np.zeros((altura, largura))
        for y in range(oy0, oy1 + 1):
            for x in range(ox0, ox1 + 1):
                pixels[
                    min(altura - 1, y * altura // altura_orig),
                    min(largura - 1, x * largura // largura_orig),
                ] += 1
        pixels /= np.maximum(contagem, 1)
        for ty, tx in np.argwhere(pixels >= 0.5):
            ty, tx = int(ty), int(tx)
            if y_ini - 1 <= ty <= y_fim + 1 and x_ini - 1 <= tx <= x_fim + 1:
                x_ini, x_fim = min(tx, x_ini), max(tx, x_fim)
                y_ini, y_fim = min(ty, y_ini), max(ty, y_fim)

        for y in range(y_ini, y_fim + 1):
            for x in range(x_ini, x_fim + 1):
                grade[y, x] = 0
        caixas.append((x_ini, y_ini, x_fim, y_fim))
    return grade, caixas


def escrever(vetores) -> None:
    linhas = [
        "//! Textura do Pip, derivada da imagem original (assets/pip.png).",
        "//!",
        "//! Este arquivo é GERADO por scripts/gerar_textura_pip.py — não edite à mão.",
        "//! Os pixels vêm da imagem oficial em /home/nick/Assets/Pip/pip.png; nada",
        "//! é redesenhado aqui. Cada texel ocupa meia célula do terminal (blocos",
        "//! ▀/▄ com cor verdadeira) e o fundo escuro do aplicativo aparece pelos",
        "//! furos da arte (contorno e olhos).",
        "",
        "/// Um olho do Pip em coordenadas de texel, com as cores do fechamento.",
        "#[derive(Clone, Copy, Debug)]",
        "pub struct OlhoTexel {",
        "    pub x0: usize,",
        "    pub y0: usize,",
        "    pub x1: usize,",
        "    pub y1: usize,",
        "    /// Cor do pelo que cobre o olho durante a piscada.",
        "    pub pelagem: u32,",
        "    /// Cor dos texels da pálpebra fechada.",
        "    pub palpebra: u32,",
        "}",
        "",
    ]
    for nome, dados in vetores.items():
        grade, caixas = dados
        altura, largura = grade.shape
        linhas.append(f"/// Largura da textura {nome.lower()} em texels.")
        linhas.append(f"pub const LARGURA_{nome.upper()}: usize = {largura};")
        linhas.append(f"/// Altura da textura {nome.lower()} em texels.")
        linhas.append(f"pub const ALTURA_{nome.upper()}: usize = {altura};")
        linhas.append("")
        linhas.append(f"/// Pixels (0xAARRGGBB; alfa 0 = vazado) da textura {nome.lower()}.")
        linhas.append(f"pub static PIXELS_{nome.upper()}: [u32; {largura * altura}] = [")
        for y in range(altura):
            valores = [
                f"0x{BRANCO:08X}" if grade[y, x] else "0x00000000" for x in range(largura)
            ]
            linhas.append("    " + ", ".join(valores) + ",")
        linhas.append("];")
        linhas.append("")
        linhas.append(
            f"/// Olhos da textura {nome.lower()}, na proporção da imagem original."
        )
        linhas.append(f"pub const OLHOS_{nome.upper()}: [OlhoTexel; {len(caixas)}] = [")
        for (x0, y0, x1, y1) in caixas:
            linhas.append("    OlhoTexel {")
            linhas.append(f"        x0: {x0}, y0: {y0}, x1: {x1}, y1: {y1},")
            linhas.append(f"        pelagem: 0x{COR_PELAGEM:08X},")
            linhas.append(f"        palpebra: 0x{COR_PALPEBRA:08X},")
            linhas.append("    },")
        linhas.append("];")
        linhas.append("")
    DESTINO.write_text("\n".join(linhas) + "\n")
    formatar()
    print(f"gerado {DESTINO.relative_to(RAIZ)}")


def formatar() -> None:
    """Aplica rustfmt ao arquivo gerado, mantendo `cargo fmt --check` verde.

    Sem rustfmt no PATH o arquivo continua válido; aí quem regenerar deve
    rodar `cargo fmt` manualmente antes de commitar.
    """
    rustfmt = shutil.which("rustfmt")
    if rustfmt is None:
        print("aviso: rustfmt não encontrado; rode `cargo fmt` depois")
        return
    resultado = subprocess.run(
        [rustfmt, "--edition", "2024", str(DESTINO)],
        capture_output=True,
        text=True,
    )
    if resultado.returncode != 0:
        raise SystemExit(f"rustfmt falhou: {resultado.stderr.strip()}")


def texto(grade) -> None:
    for y in range(grade.shape[0]):
        print("   ", "".join("#" if v else "." for v in grade[y]))


def prever(grade, destino: Path, escala: int = 26) -> None:
    """Prévia ampliada (texel = quadrado) para conferência visual."""
    altura, largura = grade.shape
    imagem = Image.new(
        "RGB", (largura * escala + 8, altura * escala + 8), (7, 8, 11)
    )
    for y in range(altura):
        for x in range(largura):
            if not grade[y, x]:
                continue
            for dy in range(escala - 1):
                for dx in range(escala - 1):
                    imagem.putpixel(
                        (4 + x * escala + dx, 4 + y * escala + dy), (255, 255, 255)
                    )
    destino.parent.mkdir(parents=True, exist_ok=True)
    imagem.save(destino)
    print(f"prévia {destino}")


def main() -> None:
    if not ORIGEM.is_file():
        raise SystemExit(f"imagem não encontrada: {ORIGEM}")
    assinatura = hashlib.sha256(ORIGEM.read_bytes()).hexdigest()
    print(f"origem: {ORIGEM} (sha256 {assinatura})")
    massa, olhos = carregar()
    print(f"massa: {massa.shape[1]}x{massa.shape[0]} px; olhos: {olhos}")

    vetores = {}
    prever_saida = "--prever" in sys.argv
    pasta_previa = Path(tempfile.gettempdir()) / "pip"
    for nome, restricao in TAMANHOS:
        grade, caixas = quantizar(massa, olhos, restricao)
        print(f"{nome}: {grade.shape[1]}x{grade.shape[0]} texels; olhos em {caixas}")
        texto(grade)
        if prever_saida:
            prever(grade, pasta_previa / f"novo_{nome.lower()}.png")
        vetores[nome] = (grade, caixas)
    escrever(vetores)


if __name__ == "__main__":
    sys.exit(main())
