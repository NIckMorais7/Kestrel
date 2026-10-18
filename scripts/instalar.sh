#!/usr/bin/env bash
# Instala o Kestrel para o usuário atual.
#
# Compila em modo release e copia o binário para ~/.local/bin (ou para o
# diretório definido em KESTREL_INSTALL_DIR). Não altera nada fora do
# diretório de destino; o build usa o Cargo já presente no sistema.
#
# Uso:
#   scripts/instalar.sh
#   KESTREL_INSTALL_DIR=/usr/local/bin sudo -E scripts/instalar.sh

set -euo pipefail

raiz="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
destino="${KESTREL_INSTALL_DIR:-$HOME/.local/bin}"

echo "kestrel: compilando em modo release…"
cargo build --release --manifest-path "$raiz/Cargo.toml"

mkdir -p "$destino"
install -m 755 "$raiz/target/release/kestrel" "$destino/kestrel"
echo "kestrel: binário instalado em $destino/kestrel"

case ":$PATH:" in
    *":$destino:"*) ;;
    *)
        echo "kestrel: aviso: $destino não está no PATH."
        echo "         acrescente ao seu shell (~/.bashrc, ~/.zshrc ou equivalente):"
        echo "         export PATH=\"$destino:\$PATH\""
        ;;
esac

"$destino/kestrel" --version
