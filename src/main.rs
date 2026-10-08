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

#[derive(Debug)]
struct Args {
    discover: bool,
}

/// Entrada no grupo que vai virar uma figurinha de boas-vindas.
struct PendingJoin {
    target: Jid,
    notification_id: Option<String>,
    action_index: u32,
    occurred_at: DateTime<Utc>,
}

/// O que fazer com uma entrada detectada no grupo.
#[derive(Debug, PartialEq, Eq)]
enum JoinDecision {
    /// Enviar a figurinha.
    Welcome,
    /// Entrada antiga demais (segundos de atraso), provavelmente vinda de uma
    /// fila liberada após reconexão.
    TooOld(i64),
    /// O mesmo aviso já foi tratado.
    Duplicate,
}

/// Decide se uma entrada merece figurinha. Fica separada do manipulador de
/// eventos para poder ser testada sem uma conexão real.
fn decide_join(join: &PendingJoin, seen: &mut HashSet<String>, now: DateTime<Utc>) -> JoinDecision {
    let age = now.signed_duration_since(join.occurred_at);
    if age.num_seconds() > MAX_JOIN_EVENT_AGE_SECS {
        return JoinDecision::TooOld(age.num_seconds());
    }

    if let Some(notification_id) = &join.notification_id {
        let key = format!("{}:{}:{}", join.target, notification_id, join.action_index);
        if !seen.insert(key) {
            return JoinDecision::Duplicate;
        }
    }

    JoinDecision::Welcome
}

// O bot só espera por I/O, com picos raros e curtos. Duas threads de trabalho
// bastam e mantêm o consumo igual numa VM fraca e numa máquina com muitos
// núcleos (o padrão do tokio criaria uma thread ociosa por núcleo).
#[tokio::main(flavor = "multi_thread", worker_threads = 2)]
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
        let sticker_bytes = load_sticker(&config.sticker_path)?;
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
            eprintln!("Encerrando e salvando a sessão...");
            handle.shutdown().await;
        }
    }

    Ok(())
}

/// Lê os argumentos da linha de comando. Devolve `None` quando a ajuda foi
/// pedida, indicando que o programa deve apenas encerrar.
fn parse_args() -> Result<Option<Args>, Box<dyn std::error::Error>> {
    parse_args_from(std::env::args().skip(1))
}

/// Núcleo de [`parse_args`], separado para testes (não lê `std::env`).
fn parse_args_from<I>(args: I) -> Result<Option<Args>, Box<dyn std::error::Error>>
where
    I: IntoIterator<Item = String>,
{
    let mut parsed = Args { discover: false };
    for arg in args {
        match arg.as_str() {
            "--discover" => parsed.discover = true,
            "-h" | "--help" => {
                print_usage();
                return Ok(None);
            }
            _ => return Err(format!("Opção desconhecida: {arg}. Use --help.").into()),
        }
    }
    Ok(Some(parsed))
}

fn print_usage() {
    println!("BemVindoBot: envia uma figurinha quando alguém entra no grupo.");
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
            let Some(join) = pending else {
                return;
            };

            let decision = {
                let mut seen = seen_notifications.lock().await;
                decide_join(&join, &mut seen, Utc::now())
            };
            match decision {
                JoinDecision::Welcome => {}
                JoinDecision::TooOld(seconds) => {
                    eprintln!(
                        "Entrada de {seconds}s atrás ignorada (limite: {MAX_JOIN_EVENT_AGE_SECS}s)."
                    );
                    return;
                }
                JoinDecision::Duplicate => return,
            }

            let target = join.target;

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

/// Detecta se o WebP é animado lendo o cabeçalho de verdade: o bit de animação
/// (0x02) do bloco `VP8X`. WebP simples (`VP8 `/`VP8L`) não tem `VP8X` e é
/// sempre estático.
fn looks_animated_webp(bytes: &[u8]) -> bool {
    // Formato RIFF: "RIFF" + tamanho + "WEBP", depois blocos de 4 bytes de
    // nome, 4 de tamanho (little-endian) e os dados, com preenchimento par.
    let mut offset = 12;
    while offset + 8 <= bytes.len() {
        let name = &bytes[offset..offset + 4];
        let size = u32::from_le_bytes(bytes[offset + 4..offset + 8].try_into().unwrap()) as usize;
        let data = offset + 8;
        if name == b"VP8X" {
            return bytes.get(data).is_some_and(|flags| flags & 0x02 != 0);
        }
        offset = data + size + (size & 1);
    }
    false
}

/// Monta a mensagem de figurinha com os dados devolvidos pelo upload no CDN.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    /// WebP mínimo: cabeçalho `RIFF`/`WEBP` válido, o bloco `nome` no começo e
    /// preenchimento até `tamanho`.
    fn webp(marcador: &[u8], tamanho: usize) -> Vec<u8> {
        let mut bytes = webp_com_bloco(marcador, &[]);
        bytes.resize(tamanho.max(bytes.len()), 0);
        bytes
    }

    /// WebP com um bloco RIFF no começo: nome + tamanho (LE) + dados, com
    /// preenchimento par quando os dados têm tamanho ímpar.
    fn webp_com_bloco(nome: &[u8], dados: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"RIFF");
        bytes.extend_from_slice(&[0u8; 4]);
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(nome);
        bytes.extend_from_slice(&(dados.len() as u32).to_le_bytes());
        bytes.extend_from_slice(dados);
        if !dados.len().is_multiple_of(2) {
            bytes.push(0);
        }
        bytes
    }

    /// WebP animado: bloco `VP8X` com o bit de animação (0x02) ligado,
    /// preenchido até `tamanho`.
    fn webp_animado(tamanho: usize) -> Vec<u8> {
        let mut bytes = webp_com_bloco(b"VP8X", &[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        bytes.resize(tamanho.max(bytes.len()), 0);
        bytes
    }

    /// Arquivo temporário removido automaticamente ao sair do escopo.
    struct ArquivoTemporario {
        caminho: PathBuf,
    }

    impl Drop for ArquivoTemporario {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.caminho);
        }
    }

    fn arquivo_temporario(nome: &str, conteudo: &[u8]) -> ArquivoTemporario {
        static CONTADOR: AtomicU64 = AtomicU64::new(0);
        let id = CONTADOR.fetch_add(1, Ordering::Relaxed);
        let caminho = std::env::temp_dir().join(format!(
            "bemvindobot-{}-{nome}-{id}.webp",
            std::process::id()
        ));
        fs::write(&caminho, conteudo).expect("escrever arquivo temporário");
        ArquivoTemporario { caminho }
    }

    /// Atalho para testar [`parse_args_from`] com uma lista fixa de argumentos.
    fn parse_args_de(args: &[&str]) -> Result<Option<Args>, Box<dyn std::error::Error>> {
        parse_args_from(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[test]
    fn looks_animated_webp_le_bit_do_vp8x() {
        assert!(looks_animated_webp(&webp_animado(64)));
        let estatico = webp_com_bloco(b"VP8X", &[0x00, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert!(!looks_animated_webp(&estatico));
    }

    #[test]
    fn looks_animated_webp_ignora_webp_simples() {
        // WebP simples (VP8 / VP8L) não tem bloco VP8X: é sempre estático.
        assert!(!looks_animated_webp(&webp(b"VP8 ", 64)));
        assert!(!looks_animated_webp(&webp(b"VP8L", 64)));
        assert!(!looks_animated_webp(b"RIFF....WEBP"));
        assert!(!looks_animated_webp(&[]));
    }

    #[test]
    fn load_sticker_aceita_webp_dentro_do_limite() {
        let conteudo = webp(b"VP8 ", 64);
        let arquivo = arquivo_temporario("estatico", &conteudo);
        let lido = load_sticker(arquivo.caminho.to_str().unwrap()).unwrap();
        assert_eq!(lido, conteudo);
    }

    #[test]
    fn load_sticker_aceita_a_figurinha_padrao_do_repositorio() {
        // Garante que o arquivo enviado junto com o projeto passa na validação.
        assert!(load_sticker("stickers/bem-vindo.webp").is_ok());
    }

    #[test]
    fn load_sticker_rejeita_arquivo_que_nao_e_webp() {
        let arquivo = arquivo_temporario("invalido", b"isto nao e um webp");
        let erro = load_sticker(arquivo.caminho.to_str().unwrap()).unwrap_err();
        assert!(erro.to_string().contains("WebP"));
    }

    #[test]
    fn load_sticker_rejeita_arquivo_curto_demais() {
        let arquivo = arquivo_temporario("curto", b"RIFF");
        let erro = load_sticker(arquivo.caminho.to_str().unwrap()).unwrap_err();
        assert!(erro.to_string().contains("WebP"));
    }

    #[test]
    fn load_sticker_permite_animado_acima_do_limite_estatico() {
        // Acima do limite estático, mas ainda dentro do limite animado.
        let conteudo = webp_animado(MAX_STATIC_STICKER_BYTES + 1);
        let arquivo = arquivo_temporario("animado-ok", &conteudo);
        assert!(load_sticker(arquivo.caminho.to_str().unwrap()).is_ok());
    }

    #[test]
    fn load_sticker_rejeita_estatico_acima_do_limite() {
        let conteudo = webp(b"VP8 ", MAX_STATIC_STICKER_BYTES + 1);
        let arquivo = arquivo_temporario("estatico-grande", &conteudo);
        let erro = load_sticker(arquivo.caminho.to_str().unwrap()).unwrap_err();
        assert!(erro.to_string().contains("100 KiB"));
    }

    #[test]
    fn load_sticker_rejeita_animado_acima_do_limite() {
        let conteudo = webp_animado(MAX_ANIMATED_STICKER_BYTES + 1);
        let arquivo = arquivo_temporario("animado-grande", &conteudo);
        let erro = load_sticker(arquivo.caminho.to_str().unwrap()).unwrap_err();
        assert!(erro.to_string().contains("500 KiB"));
    }

    /// Entrada de teste com os campos que importam para a decisão.
    fn entrada(
        notification_id: Option<&str>,
        atraso_segundos: i64,
        action_index: u32,
    ) -> PendingJoin {
        PendingJoin {
            target: Jid::group("123"),
            notification_id: notification_id.map(str::to_owned),
            action_index,
            occurred_at: Utc::now() - whatsapp_rust::chrono::Duration::seconds(atraso_segundos),
        }
    }

    #[test]
    fn decide_join_aceita_entrada_recente_e_inedita() {
        let mut seen = HashSet::new();
        let join = entrada(Some("aviso-1"), 5, 0);
        assert_eq!(
            decide_join(&join, &mut seen, Utc::now()),
            JoinDecision::Welcome
        );
    }

    #[test]
    fn decide_join_ignora_entrada_antiga() {
        let mut seen = HashSet::new();
        let join = entrada(Some("aviso-1"), MAX_JOIN_EVENT_AGE_SECS + 10, 0);
        assert!(matches!(
            decide_join(&join, &mut seen, Utc::now()),
            JoinDecision::TooOld(_)
        ));
    }

    #[test]
    fn decide_join_ignora_aviso_repetido() {
        let mut seen = HashSet::new();
        let agora = Utc::now();
        let join = entrada(Some("aviso-1"), 5, 0);
        assert_eq!(decide_join(&join, &mut seen, agora), JoinDecision::Welcome);
        assert_eq!(
            decide_join(&join, &mut seen, agora),
            JoinDecision::Duplicate
        );
    }

    #[test]
    fn decide_join_trata_mesmo_aviso_com_acoes_diferentes_como_entradas_distintas() {
        let mut seen = HashSet::new();
        let agora = Utc::now();
        let primeira = entrada(Some("aviso-1"), 5, 0);
        let segunda = entrada(Some("aviso-1"), 5, 1);
        assert_eq!(
            decide_join(&primeira, &mut seen, agora),
            JoinDecision::Welcome
        );
        assert_eq!(
            decide_join(&segunda, &mut seen, agora),
            JoinDecision::Welcome
        );
    }

    #[test]
    fn decide_join_sem_identificador_nunca_e_repetido() {
        let mut seen = HashSet::new();
        let agora = Utc::now();
        let join = entrada(None, 5, 0);
        assert_eq!(decide_join(&join, &mut seen, agora), JoinDecision::Welcome);
        assert_eq!(decide_join(&join, &mut seen, agora), JoinDecision::Welcome);
    }

    #[test]
    fn parse_args_sem_argumentos_usa_padrao() {
        let args = parse_args_de(&[]).unwrap().unwrap();
        assert!(!args.discover);
    }

    #[test]
    fn parse_args_reconhece_discover() {
        let args = parse_args_de(&["--discover"]).unwrap().unwrap();
        assert!(args.discover);
    }

    #[test]
    fn parse_args_ajuda_encerra_sem_configurar() {
        assert!(parse_args_de(&["-h"]).unwrap().is_none());
        assert!(parse_args_de(&["--help"]).unwrap().is_none());
    }

    #[test]
    fn parse_args_rejeita_opcao_desconhecida() {
        let erro = parse_args_de(&["--bogus"]).unwrap_err();
        assert!(erro.to_string().contains("--bogus"));
    }
}
