# BemVindoBot

Bot local em Rust que observa a entrada de pessoas em um grupo do WhatsApp e envia uma figurinha `.webp` de boas-vindas.

O cliente usado é o [`whatsapp-rust`](https://github.com/oxidezap/whatsapp-rust), uma implementação não oficial do protocolo WhatsApp Web. O pareamento é feito como aparelho conectado por QR. Este projeto não é afiliado à Meta ou ao WhatsApp. O uso de clientes não oficiais pode contrariar os termos da Meta e resultar em suspensão da conta.

## Preparar

Requisitos: Rust 1.94 ou mais recente e um compilador C (`cc`/`clang`), usado para compilar o SQLite embutido e o provedor TLS (ring). Não são necessárias bibliotecas de sistema de SQLite nem de OpenSSL.

```sh
cargo run
```

Na primeira execução, o programa cria `bot.json`. O repositório inclui a figurinha `stickers/bem-vindo.webp`; para usar outra, substitua esse arquivo ou altere `sticker_path` para apontar para um WebP local.

Configure o JID do grupo em `bot.json`:

```json
{
  "group_jid": "123456789012345678@g.us",
  "sticker_path": "stickers/bem-vindo.webp"
}
```

Use um arquivo WebP quadrado de 512 × 512. O programa aceita até 100 KiB para figurinha estática e até 500 KiB para animada. Figurinhas favoritas que estão apenas dentro do WhatsApp não são lidas automaticamente; coloque no caminho configurado uma cópia `.webp` da figurinha escolhida.

## Descobrir o JID do grupo

Para ver as opções disponíveis, rode `cargo run -- --help`. Rode em modo de descoberta e escaneie o QR se ainda não pareou este bot:

```sh
cargo run -- --discover
```

Envie `!grupo-id` no grupo desejado. O JID aparecerá no terminal; copie para `group_jid` em `bot.json`, encerre com Ctrl+C e rode `cargo run` novamente. O modo de descoberta não envia mensagens nem figurinhas.

## Executar

```sh
cargo run
```

Escaneie o QR com o WhatsApp em **Aparelhos conectados → Conectar aparelho**. Depois disso, a sessão fica em `state/whatsapp.db` e o QR não será necessário nas próximas execuções. Para encerrar, use Ctrl+C.

O tratamento do bot recebe somente eventos de alteração de grupo e compara o JID antes de agir. Quando chega uma ação de entrada (`add`) no grupo configurado, aguarda 500 ms e envia a figurinha. Entradas registradas há mais de 60 segundos são ignoradas: se o bot ficou offline e várias pessoas entraram nesse período, ele não envia uma rajada de figurinhas atrasadas ao reconectar. Quedas de rede são reconectadas automaticamente pelo cliente, com backoff progressivo. No Linux, o diretório da sessão usa permissão privada.

## Rodar em background (PC, VPS, Docker, Termux)

Depois de pareado, o bot não mostra mais o QR nem precisa de terminal: ele fica em silêncio observando o grupo até ser encerrado. A sessão fica em `state/whatsapp.db`, então rodar a partir da mesma pasta reaproveita o pareamento já existente — não é preciso escanear o QR de novo.

**Rode uma única instância por sessão.** Dois processos usando o mesmo `state/whatsapp.db` abrem duas conexões como o mesmo aparelho e uma sobrescreve a outra. Encerre o processo antigo antes de subir o novo, ou copie `state/` para um diretório separado.

O bot trata `SIGINT` (Ctrl+C) e `SIGTERM` (`systemctl stop`, `docker stop`) encerrando e salvando a sessão — não precisa de `kill -9`.

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
journalctl -u bemvindobot -f   # acompanhar os logs
```

### Docker

Imagem com o binário compilado (`cargo build --release`) e `state/`/`bot.json` montados como volume; `docker stop` já envia `SIGTERM`.

### Termux (Android)

```sh
pkg install rust clang
cargo build --release
termux-wake-lock                      # evita que o Android suspenda o processo
nohup target/release/bemvindobot > bot.log 2>&1 &
```

Alternativamente, rode dentro de `tmux`. Se você já pareou em outra máquina, copie a pasta `state/` junto com o `bot.json` para não parear de novo.

## Dados locais

`bot.json` contém a configuração do grupo e `state/` contém a sessão pareada do WhatsApp. Esses caminhos são ignorados pelo Git. Não compartilhe nem publique a pasta `state/`: ela permite reutilizar a sessão do aparelho conectado.
