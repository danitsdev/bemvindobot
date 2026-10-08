# BemVindoBot

Bot local em Rust que envia uma figurinha `.webp` de boas-vindas quando alguém entra em um grupo do WhatsApp.

Usa o [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust), cliente não oficial do protocolo WhatsApp Web, com pareamento por QR. Sem vínculo com a Meta. Clientes não oficiais podem contrariar os termos do WhatsApp e levar à suspensão da conta.

## Preparar

Requisitos: Rust 1.94 ou mais recente e um compilador C (`cc`/`clang`), usado para compilar o SQLite embutido e o TLS (ring). Não são necessários SQLite nem OpenSSL do sistema.

Na primeira execução o programa cria `bot.json`:

```json
{
  "group_jid": "123456789012345678@g.us",
  "sticker_path": "stickers/bem-vindo.webp"
}
```

Aponte `group_jid` para o JID do seu grupo e `sticker_path` para o WebP desejado: quadrado, 512 × 512, até 100 KiB estático ou 500 KiB animado. O repositório já inclui `stickers/bem-vindo.webp`.

Figurinha salva dentro do WhatsApp não é lida automaticamente. Exporte uma cópia `.webp` e use esse arquivo.

## Descobrir o JID do grupo

```sh
cargo run -- --discover
```

Envie `!grupo-id` no grupo. O JID aparece no terminal; copie para `group_jid` e reinicie. O modo descoberta não envia nada. Veja todas as opções com `cargo run -- --help`.

## Executar

```sh
cargo run
```

Escaneie o QR em **Aparelhos conectados > Conectar aparelho**. Depois disso a sessão fica em `state/whatsapp.db` e o QR não é mais pedido. Encerre com Ctrl+C.

O bot só reage a alterações de grupo no JID configurado. Ao detectar uma entrada, espera 500 ms e envia a figurinha. Entradas com mais de 60 segundos são ignoradas, para não disparar uma rajada de figurinhas atrasadas depois de uma queda. Quedas de rede são reconectadas automaticamente. No Linux a sessão fica com permissão privada.

## Rodar em background

Depois de pareado o bot roda em silêncio, sem QR e sem terminal, observando o grupo até ser encerrado. Como a sessão fica em `state/whatsapp.db`, iniciar a partir da mesma pasta reaproveita o pareamento.

Rode **uma única instância** por sessão. Dois processos no mesmo `state/whatsapp.db` abrem duas conexões como o mesmo aparelho e uma sobrescreve a outra.

O bot encerra e salva a sessão em `SIGINT` (Ctrl+C) e `SIGTERM` (`systemctl stop`, `docker stop`), então não precisa de `kill -9`.

### systemd (VPS)

```ini
# /etc/systemd/system/bemvindobot.service
[Unit]
Description=BemVindoBot
After=network-online.target
Wants=network-online.target

[Service]
WorkingDirectory=/opt/bemvindobot
ExecStart=/opt/bemvindobot/bemvindobot
Restart=on-failure
RestartSec=5

[Install]
WantedBy=multi-user.target
```

```sh
sudo systemctl enable --now bemvindobot
journalctl -u bemvindobot -f
```

### Docker

Monte `bot.json` e `state/` como volumes numa imagem com o binário de `cargo build --release`. O `docker stop` já envia `SIGTERM`.

### Termux (Android)

```sh
pkg install rust clang
cargo build --release
termux-wake-lock
nohup target/release/bemvindobot > bot.log 2>&1 &
```

Também funciona dentro de `tmux`. Para reaproveitar o pareamento de outra máquina, copie a pasta `state/` junto com o `bot.json`.

## Dados locais

`bot.json` guarda a configuração do grupo e `state/` a sessão pareada. Ambos são ignorados pelo Git. Não compartilhe nem publique `state/`: ele permite reutilizar a sessão do aparelho conectado.
