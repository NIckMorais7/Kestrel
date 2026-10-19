#!/usr/bin/env bash
# Instala a fonte Geist Mono para o usuário atual.
#
# A Inter não tem variante mono oficial; usá-la em um terminal TUI quebra o
# alinhamento das células. A Geist Mono é a alternativa livre mais próxima
# visualmente e é verdadeiramente monoespelhada.
#
# Uso:
#   scripts/instalar_fonte_geist_mono.sh
#
# Não altera nada fora de ~/.local/share/fonts/GeistMono (ou do diretório
# definido em KESTREL_FONT_DIR).

set -euo pipefail

versao="${GEIST_FONT_VERSAO:-1.8.1}"
url="https://github.com/vercel/geist-font/releases/download/v${versao}/geist-font-v${versao}.zip"
destino="${KESTREL_FONT_DIR:-${XDG_DATA_HOME:-$HOME/.local/share}/fonts}/GeistMono"

echo "kestrel: baixando Geist Mono ${versao}…"
tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
curl -fL --retry 3 -o "$tmp/geist.zip" "$url"

echo "kestrel: instalando em $destino…"
mkdir -p "$destino"
unzip -q "$tmp/geist.zip" -d "$tmp/geist"

# A release traz TTF e OTF. Para terminal, os TTF estáticos são os mais
# compatíveis; o resto da release não é necessário.
instalados=0
while IFS= read -r -d '' arquivo; do
    install -m 644 "$arquivo" "$destino/$(basename "$arquivo")"
    instalados=$((instalados + 1))
done < <(find "$tmp/geist" -path '*/GeistMono/ttf/*.ttf' -print0)

if [ "$instalados" -eq 0 ]; then
    echo "kestrel: erro: nenhum TTF da Geist Mono encontrado na release." >&2
    exit 1
fi

if command -v fc-cache >/dev/null 2>&1; then
    fc-cache -f "$destino" >/dev/null
fi

echo "kestrel: $instalados arquivos da Geist Mono instalados."
echo "kestrel: aponte o terminal para a fonte. Exemplos:"
echo "  foot (~/.config/foot/foot.ini):        font=Geist Mono:size=9"
echo "  kitty (~/.config/kitty/kitty.conf):    font_family Geist Mono"
echo "                                         font_size 9.0"
echo "  alacritty (~/.config/alacritty.toml):  [font.normal]"
echo "                                         family = \"Geist Mono\""
