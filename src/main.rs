#![recursion_limit = "512"]

//! Bot que envia uma figurinha de boas-vindas quando alguém entra em um grupo
//! do WhatsApp, usando o cliente não oficial `whatsapp-rust`.

use qrcode::{QrCode, render::unicode};
use serde::Deserialize;
use std::collections::HashSet;
use std::fs;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{Mutex, OnceCell};
use whatsapp_rust::chrono::{DateTime, Utc};
use whatsapp_rust::prelude::*;
use whatsapp_rust::upload::{UploadOptions, UploadResponse};
use whatsapp_rust::wacore::download::MediaType;
use whatsapp_rust::wacore::stanza::groups::GroupNotificationAction;

const CONFIG_PATH: &str = "bot.json";
const DATABASE_PATH: &str = "state/whatsapp.db";
const DISCOVERY_COMMAND: &str = "!grupo-id";
/// Entradas registradas há mais tempo que isto são descartadas. Se o bot ficou
/// offline e várias pessoas entraram nesse intervalo, a fila de eventos é
/// liberada de uma vez ao reconectar; sem este limite sairia uma figurinha
/// atrasada para cada pessoa.
const MAX_JOIN_EVENT_AGE_SECS: i64 = 60;
const MAX_STATIC_STICKER_BYTES: usize = 100 * 1024;
const MAX_ANIMATED_STICKER_BYTES: usize = 500 * 1024;

#[derive(Debug, Deserialize)]
struct Config {
    #[serde(default)]
    group_jid: String,
    #[serde(default = "default_sticker_path")]
    sticker_path: String,
}

fn default_sticker_path() -> String {
    "stickers/bem-vindo.webp".to_owned()
}

/// Argumentos aceitos na linha de comando.
struct Args {
    discover: bool,
}

/// Entrada no grupo que ainda vai virar uma figurinha de boas-vindas.
struct PendingJoin {
    /// Grupo que recebe a figurinha.
    target: Jid,
    notification_id: Option<String>,
    action_index: u32,
    occurred_at: DateTime<Utc>,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let Some(args) = parse_args()? else {
        return Ok(());
    };
    let config = load_config()?;

    if !args.discover {
        validate_group_jid(&config.group_jid)?;
    }

    fs::create_dir_all("state")?;
    restrict_permissions("state", 0o700)?;

    let sticker_bytes = if args.discover {
        None
    } else {
        Some(load_sticker(&config.sticker_path)?)
    };

    let store = SqliteStore::new(DATABASE_PATH).await?;
    restrict_permissions(DATABASE_PATH, 0o600)?;

    let builder = Bot::builder()
        .with_backend(store)
        .on_qr_code(|code, timeout| async move {
            eprintln!("QR de pareamento (válido por {}s):", timeout.as_secs());
            match QrCode::new(code.as_bytes()) {
                Ok(qr) => eprintln!(
                    "{}",
                    qr.render::<unicode::Dense1x2>()
                        .dark_color(unicode::Dense1x2::Light)
                        .light_color(unicode::Dense1x2::Dark)
                        .build()
                ),
                Err(error) => eprintln!("Não consegui desenhar o QR no terminal: {error}"),
            }
            eprintln!("Escaneie em WhatsApp > Aparelhos conectados > Conectar aparelho.");
        })
        .on_connected(|_client| async {
            eprintln!("WhatsApp conectado.");
        });

    let builder = if args.discover {
        register_discovery(builder)
    } else {
        let sticker_bytes = sticker_bytes.expect("a figurinha foi carregada");
        register_welcome(builder, &config, sticker_bytes)
    };

    let bot = builder.build().await?;
    let mut handle = bot.spawn();
    tokio::select! {
        _ = &mut handle => {
            eprintln!("A conexão do bot foi encerrada.");
        }
        // Encerra com Ctrl+C (SIGINT) ou com SIGTERM, que é o sinal enviado
        // por `systemctl stop` e `docker stop`. Sem isso, um supervisor mata o
        // processo sem deixar a sessão ser salva.
        _ = shutdown_signal() => {
            eprintln!("Encerrando e salvando a sessão…");
            handle.shutdown().await;
        }
    }

    Ok(())
}

/// Interpreta os argumentos. Devolve `None` quando só a ajuda foi pedida, o
/// que significa que o programa deve encerrar sem fazer mais nada.
fn parse_args() -> Result<Option<Args>, Box<dyn std::error::Error>> {
    let mut args = Args { discover: false };
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "--discover" => args.discover = true,
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            _ => return Err(format!("Opção desconhecida: {arg}. Use --help.").into()),
        }
    }
    Ok(Some(args))
}

fn print_usage() {
    println!("BemVindoBot — envia uma figurinha quando alguém entra no grupo.");
    println!();
    println!("Uso: bemvindobot [--discover]");
    println!();
    println!("Opções:");
    println!("  --discover  não envia figurinhas; mostra o JID dos grupos onde");
    println!(
        "              `{DISCOVERY_COMMAND}` for enviado, para descobrir o valor de group_jid."
    );
    println!("  -h, --help  mostra esta ajuda.");
}

/// Garante que o `group_jid` foi configurado e tem a forma de um JID de grupo.
fn validate_group_jid(jid: &str) -> Result<(), Box<dyn std::error::Error>> {
    if jid.trim().is_empty() {
        return Err(format!(
            "Configure \"group_jid\" em {CONFIG_PATH}. Para descobrir o JID, rode `cargo run -- --discover` e envie `{DISCOVERY_COMMAND}` no grupo."
        )
        .into());
    }
    if !jid.trim().ends_with("@g.us") {
        return Err("group_jid precisa terminar com @g.us (JID de grupo do WhatsApp).".into());
    }
    Ok(())
}

/// Modo de descoberta: observa mensagens e imprime o JID do grupo onde o
/// comando aparecer. Não envia figurinhas.
fn register_discovery<B, T, H, R>(builder: BotBuilder<B, T, H, R>) -> BotBuilder<B, T, H, R> {
    eprintln!("Modo descoberta: envie {DISCOVERY_COMMAND} em uma conversa de grupo.");
    builder.on_message(|ctx| async move {
        if ctx.message.text_content() == Some(DISCOVERY_COMMAND) {
            let jid = ctx.info.source.chat.to_string();
            if jid.ends_with("@g.us") {
                eprintln!("JID do grupo: {jid}");
            } else {
                eprintln!("Esse comando precisa ser enviado em um grupo.");
            }
        }
    })
}

/// Modo normal: envia a figurinha quando alguém entra no grupo configurado.
fn register_welcome<B, T, H, R>(
    builder: BotBuilder<B, T, H, R>,
    config: &Config,
    sticker_bytes: Vec<u8>,
) -> BotBuilder<B, T, H, R> {
    let group_jid = config.group_jid.trim().to_owned();
    let sticker_bytes = Arc::new(sticker_bytes);
    let is_animated = looks_animated_webp(&sticker_bytes);
    let uploaded_sticker = Arc::new(OnceCell::<UploadResponse>::new());
    let seen_notifications = Arc::new(Mutex::new(HashSet::<String>::new()));

    builder.on_event_for(&[EventKind::GroupUpdate], move |event, client| {
        let pending = match &*event {
            Event::GroupUpdate(update)
                if update.group_jid.to_string() == group_jid
                    && matches!(&update.action, GroupNotificationAction::Add { .. }) =>
            {
                Some(PendingJoin {
                    target: update.group_jid.clone(),
                    notification_id: update.notification_id.clone(),
                    action_index: update.action_index,
                    occurred_at: update.timestamp,
                })
            }
            _ => None,
        };

        let sticker_bytes = Arc::clone(&sticker_bytes);
        let uploaded_sticker = Arc::clone(&uploaded_sticker);
        let seen_notifications = Arc::clone(&seen_notifications);

        async move {
            let Some(PendingJoin {
                target,
                notification_id,
                action_index,
                occurred_at,
            }) = pending
            else {
                return;
            };

            let age = Utc::now().signed_duration_since(occurred_at);
            if age.num_seconds() > MAX_JOIN_EVENT_AGE_SECS {
                eprintln!(
                    "Entrada de {}s atrás ignorada (limite: {MAX_JOIN_EVENT_AGE_SECS}s).",
                    age.num_seconds()
                );
                return;
            }

            if let Some(notification_id) = notification_id {
                let key = format!("{target}:{notification_id}:{action_index}");
                let is_new = seen_notifications.lock().await.insert(key);
                if !is_new {
                    return;
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;

            let upload = match uploaded_sticker
                .get_or_try_init(|| async {
                    client
                        .upload(
                            sticker_bytes.as_ref().clone(),
                            MediaType::Sticker,
                            UploadOptions::default(),
                        )
                        .await
                })
                .await
            {
                Ok(upload) => upload.clone(),
                Err(error) => {
                    eprintln!("Não consegui enviar a figurinha ao CDN: {error}");
                    return;
                }
            };

            let message = build_sticker_message(upload, is_animated);
            match client.send_message(target, message).await {
                Ok(_) => eprintln!("Figurinha de boas-vindas enviada."),
                Err(error) => eprintln!("Falha ao enviar a figurinha: {error}"),
            }
        }
    })
}

fn load_config() -> Result<Config, Box<dyn std::error::Error>> {
    if !Path::new(CONFIG_PATH).exists() {
        fs::write(CONFIG_PATH, include_str!("../bot.example.json"))?;
        eprintln!("Criei {CONFIG_PATH}. Confira group_jid e sticker_path antes de iniciar o bot.");
    }

    let content = fs::read_to_string(CONFIG_PATH)?;
    Ok(serde_json::from_str(&content)?)
}

fn load_sticker(path: &str) -> Result<Vec<u8>, Box<dyn std::error::Error>> {
    let bytes = fs::read(path)?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        return Err(format!("{path} não parece ser um arquivo WebP válido.").into());
    }
    let is_animated = looks_animated_webp(&bytes);
    let max_bytes = if is_animated {
        MAX_ANIMATED_STICKER_BYTES
    } else {
        MAX_STATIC_STICKER_BYTES
    };
    if bytes.len() > max_bytes {
        return Err(format!(
            "A figurinha tem {} KiB; o limite é {} KiB para este tipo de WebP. Reduza o arquivo.",
            bytes.len() / 1024,
            max_bytes / 1024
        )
        .into());
    }
    Ok(bytes)
}

fn looks_animated_webp(bytes: &[u8]) -> bool {
    bytes
        .windows(4)
        .any(|chunk| chunk == b"ANIM" || chunk == b"ANMF")
}

fn build_sticker_message(upload: UploadResponse, is_animated: bool) -> wa::Message {
    wa::Message {
        sticker_message: MessageField::some(wa::message::StickerMessage {
            url: Some(upload.url),
            file_sha256: Some(upload.file_sha256.to_vec()),
            file_enc_sha256: Some(upload.file_enc_sha256.to_vec()),
            media_key: Some(upload.media_key.to_vec()),
            mimetype: Some("image/webp".to_owned()),
            height: Some(512),
            width: Some(512),
            direct_path: Some(upload.direct_path),
            file_length: Some(upload.file_length),
            media_key_timestamp: Some(upload.media_key_timestamp),
            is_animated: Some(is_animated),
            ..Default::default()
        }),
        ..Default::default()
    }
}

/// Restringe `path` ao dono (sessão do WhatsApp, banco). No Unix aplica `mode`;
/// nas demais plataformas não há equivalente e a função não faz nada.
#[cfg(unix)]
fn restrict_permissions(path: &str, mode: u32) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(mode))
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &str, _mode: u32) -> std::io::Result<()> {
    Ok(())
}
