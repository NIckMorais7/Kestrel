//! Testes de ponta a ponta do binário: contrato da linha de comando.
//!
//! Estes testes executam o binário real em um terminal inexistente e
//! verificam apenas os caminhos que retornam antes de abrir a interface
//! (ajuda, versão e validações de argumentos). O diretório de configuração
//! e o de dados apontam para locais temporários, preservando a configuração
//! real do usuário.

use std::process::Command;

fn bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_kestrel"))
}

fn dirs_de_teste(nome: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let base = std::env::temp_dir().join(format!("kestrel-cli-{nome}-{}", std::process::id()));
    (base.join("config"), base.join("data"))
}

#[test]
fn ajuda_e_versao_em_portugues() {
    let saida = bin().arg("--help").output().unwrap();
    assert!(saida.status.success());
    let texto = String::from_utf8_lossy(&saida.stdout);
    assert!(texto.contains("Kestrel analisa o projeto"));
    assert!(texto.contains("--workdir"));
    assert!(texto.contains("--permissions"));
    assert!(texto.contains("NVIDIA_API_KEY"));

    let saida = bin().arg("--version").output().unwrap();
    assert!(saida.status.success());
    assert!(String::from_utf8_lossy(&saida.stdout).contains("kestrel"));
}

#[test]
fn nivel_de_permissao_invalido_falha_antes_da_interface() {
    let (config, data) = dirs_de_teste("permissao");
    let saida = bin()
        .args([
            "--config-dir",
            config.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
            "--permissions",
            "turbo",
        ])
        .output()
        .unwrap();
    assert!(!saida.status.success());
    let erro = String::from_utf8_lossy(&saida.stderr);
    assert!(
        erro.contains("nível de permissão inválido"),
        "stderr inesperado: {erro}"
    );
    let _ = std::fs::remove_dir_all(config.parent().unwrap());
}

#[test]
fn workdir_invalido_falha_com_mensagem_clara() {
    let (config, data) = dirs_de_teste("workdir");
    let inexistente = config.parent().unwrap().join("nao-existe");
    let saida = bin()
        .args([
            "--config-dir",
            config.to_str().unwrap(),
            "--data-dir",
            data.to_str().unwrap(),
            "-C",
            inexistente.to_str().unwrap(),
        ])
        .output()
        .unwrap();
    assert!(!saida.status.success());
    let erro = String::from_utf8_lossy(&saida.stderr);
    assert!(
        erro.contains("diretório de trabalho inválido"),
        "stderr inesperado: {erro}"
    );
    let _ = std::fs::remove_dir_all(config.parent().unwrap());
}
