use base64::{engine::general_purpose::STANDARD, Engine};
use bytes::{Bytes, BytesMut};
use chrono::{DateTime, Local};
use futures::StreamExt;
use futures::{stream::Stream, Future};
use rand::seq::SliceRandom;
use rand::Rng;
use reqwest::{
    header::{
        ACCEPT, ACCEPT_LANGUAGE, CACHE_CONTROL, CONTENT_TYPE, ORIGIN, PRAGMA, REFERER, USER_AGENT,
    },
    Client, Proxy,
};
use rustyline::error::ReadlineError;
use serde::{ser::SerializeStruct, Deserialize, Serialize, Serializer};
use sha3::Digest;
use std::{
    collections::HashMap,
    fmt,
    pin::Pin,
    sync::Arc,
    task::{Context, Poll},
};

const OPENAI_ENDPOINT: &str = "https://chat.openai.com";
const OPENAI_API_URL: &str = "https://chat.openai.com/backend-anon/conversation";
const OPENAI_SENTINEL_URL: &str = "https://chat.openai.com/backend-anon/sentinel/chat-requirements";
const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/123.0.0.0 Safari/537.36";

#[derive(Clone)]
pub struct AppState {
    pub proxy: Option<String>,
    pub device_id: String,
    pub code: bool,
    pub model: String,
    pub lang: String,
    pub qusetion: Option<String>,
    pub input_file: Option<String>,
    pub repl: bool,
    pub dump_stats: bool,

    #[cfg(feature = "proxy")]
    pub prefix: String,
    #[cfg(feature = "proxy")]
    pub serve_addr: String,
}

pub type StateRef = Arc<AppState>;

#[derive(Debug)]
pub enum Error {
    Io(String),
    Reqwest(String),
    Serde(String),
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e.to_string())
    }
}

impl From<reqwest::Error> for Error {
    fn from(e: reqwest::Error) -> Self {
        Error::Reqwest(e.to_string())
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Serde(e.to_string())
    }
}

#[cfg(feature = "cli")]
impl From<ReadlineError> for Error {
    fn from(e: ReadlineError) -> Self {
        match e {
            ReadlineError::Eof => Error::Io("EOF".to_string()),
            _ => Error::Io(e.to_string()),
        }
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Error::Io(e) => write!(f, "IO error: {}", e),
            Error::Reqwest(e) => write!(f, "Reqwest error: {}", e),
            Error::Serde(e) => write!(f, "Serde error: {}", e),
        }
    }
}

#[derive(Debug)]
pub struct Session {
    pub start_at: std::time::Instant,
    pub token: String,
    pub proof_seed: String,
    pub proof_difficulty: String,
    pub device_id: String,
}

#[derive(Debug, serde::Deserialize)]
struct ChatRequirementsProofofwork {
    #[allow(unused)]
    pub required: bool,
    pub seed: String,
    pub difficulty: String,
}
#[derive(Debug, serde::Deserialize)]
struct ChatRequirementsResponse {
    pub token: String,
    pub proofofwork: ChatRequirementsProofofwork,
}

#[derive(Deserialize, Debug)]
pub struct Message {
    pub role: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content_type: Option<String>,
}

impl Serialize for Message {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut s = serializer.serialize_struct("Message", 2)?;
        s.serialize_field(
            "author",
            &serde_json::json!({
                "role": &self.role
            }),
        )?;
        s.serialize_field(
            "content",
            &serde_json::json!({
                "content_type": &self.content_type.as_ref().unwrap_or(&"text".to_string()),
                "parts": &serde_json::json!(vec![&self.content]),
            }),
        )?;
        s.end()
    }
}

#[derive(Default, Serialize)]
pub struct CompletionRequest {
    pub action: String,
    pub model: String,
    pub messages: Vec<Message>,
    pub conversation_mode: HashMap<String, String>,
    pub websocket_request_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub conversation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_message_id: Option<String>,
    pub timezone_offset_min: i32,
    pub history_and_training_disabled: bool,
}

impl CompletionRequest {
    pub fn new(
        state: StateRef,
        messages: Vec<Message>,
        conversation_id: Option<String>,
        parent_message_id: Option<String>,
    ) -> Self {
        let local: DateTime<Local> = Local::now();
        let offset_minutes = local.offset().local_minus_utc() / 60;

        Self {
            action: "next".to_string(),
            messages,
            model: state.model.clone(),
            conversation_mode: {
                let mut map = HashMap::new();
                map.insert("kind".to_string(), "primary_assistant".to_string());
                map
            },
            websocket_request_id: uuid::Uuid::new_v4().to_string(),
            conversation_id,
            parent_message_id,
            timezone_offset_min: offset_minutes,
            history_and_training_disabled: false,
        }
    }

    pub async fn stream(&self, state: StateRef) -> Result<CompletionStream, Error> {
        let start_at = std::time::Instant::now();
        let session = alloc_session(state.clone()).await?;
        let builder = build_req(
            OPENAI_API_URL,
            &session.device_id,
            Some(&session.token),
            Some(&session.proof_seed),
            Some(&session.proof_difficulty),
            state.clone(),
        )?;
        let body = serde_json::to_string(&self)?;
        let resp = builder.body(body.clone()).send().await?;

        log::debug!(
            "open stream: {} ms, proxy: {:?} body:{:?} -> {:?}",
            start_at.elapsed().as_millis(),
            state.proxy,
            body,
            resp.status()
        );
        self.messages.iter().for_each(|m| log::debug!("{:?}", m));

        if !resp.status().is_success() {
            let resp_body = resp.text().await?;
            return Err(Error::Reqwest(resp_body));
        }
        Ok(CompletionStream {
            response_stream: Box::pin(resp.bytes_stream()),
            buffer: BytesMut::new(),
        })
    }
}

#[derive(Debug)]
pub enum CompletionEvent {
    Data(CompletionResponse),
    Done,
    Heartbeat,
    #[allow(unused)]
    Text(String),
}

impl From<&BytesMut> for CompletionEvent {
    fn from(line: &BytesMut) -> CompletionEvent {
        let line_str = String::from_utf8_lossy(&line).to_string();
        let line_str = line_str.strip_prefix("data: ").unwrap_or(&line_str);
        log::debug!(">> {:?}", line_str);
        if line_str == "[DONE]" {
            return CompletionEvent::Done;
        }
        let heartbeat_re =
            regex::Regex::new(r"^\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}.\d{6}$").unwrap();
        if heartbeat_re.is_match(line_str) {
            CompletionEvent::Heartbeat
        } else {
            match serde_json::from_str(line_str) {
                Ok(data) => CompletionEvent::Data(data),
                Err(e) => {
                    log::error!("parse error: {:?}", e);
                    CompletionEvent::Text(line_str.to_string())
                }
            }
        }
    }
}
#[allow(unused)]
#[derive(Debug, Deserialize)]
pub struct CompletionMessageAuthor {
    pub role: String,
    pub name: Option<String>,
    pub metadata: HashMap<String, serde_json::Value>,
}
#[allow(unused)]
#[derive(Debug, Deserialize)]
pub struct CompletionMessageContent {
    pub content_type: String,
    pub parts: Vec<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct CompletionMessageFinishDetails {
    pub r#type: Option<String>,
}
#[allow(unused)]
#[derive(Debug, Deserialize)]
pub struct CompletionMessageMeta {
    pub citations: Option<Vec<String>>,
    pub gizmo_id: Option<String>,
    pub message_type: Option<String>,
    pub model_slug: Option<String>,
    pub default_model_slug: Option<String>,
    pub pad: Option<String>,
    pub parent_id: Option<String>,
    pub model_switcher_deny: Option<Vec<String>>,
    pub is_visually_hidden_from_conversation: Option<bool>,
    pub finish_details: Option<CompletionMessageFinishDetails>,
}
#[allow(unused)]
#[derive(Debug, Deserialize)]
pub struct CompletionMessage {
    pub id: String,
    pub author: CompletionMessageAuthor,
    pub create_time: Option<f64>,
    pub update_time: Option<f64>,
    pub content: CompletionMessageContent,
    pub status: String,
    pub end_turn: Option<bool>,
    pub weight: Option<f64>,
    pub metadata: CompletionMessageMeta,
    pub recipient: String,
}
#[allow(unused)]
#[derive(Debug, Deserialize)]
pub struct CompletionResponse {
    pub message: Option<CompletionMessage>,
    pub conversation_id: String,
    pub error: Option<String>,
}

impl CompletionResponse {
    pub fn get_finish_reason(&self) -> Option<String> {
        self.message
            .as_ref()?
            .metadata
            .finish_details
            .as_ref()
            .map(|finish_details| {
                if let Some(finish_type) = finish_details.r#type.as_ref() {
                    match finish_type.as_str() {
                        "max_tokens" => "length".to_string(),
                        _ => "stop".to_string(),
                    }
                } else {
                    "stop".to_string()
                }
            })
    }
}

pub struct CompletionResult {
    pub textbuf: String,
    pub conversation_id: String,
    pub last_message_id: String,
    pub finish_reason: Option<String>,
}

pub struct CompletionStream {
    response_stream: Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>,
    buffer: BytesMut,
}

impl Stream for CompletionStream {
    type Item = reqwest::Result<CompletionEvent>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        loop {
            match self.response_stream.as_mut().poll_next(cx) {
                Poll::Ready(Some(Ok(data))) => {
                    self.buffer.extend_from_slice(&data);
                    log::debug!("<< {:?}", String::from_utf8_lossy(&data));
                    if let Some(pos) = self.buffer.windows(2).position(|window| window == b"\n\n") {
                        let mut line = self.buffer.split_to(pos + 2);
                        line.truncate(pos);
                        return Poll::Ready(Some(Ok(CompletionEvent::from(&line))));
                    }
                }
                Poll::Ready(Some(Err(e))) => return Poll::Ready(Some(Err(e))),
                Poll::Ready(None) => {
                    log::debug!("<< None");
                    if !self.buffer.is_empty() {
                        if let Some(pos) =
                            self.buffer.windows(2).position(|window| window == b"\n\n")
                        {
                            let mut line = self.buffer.split_to(pos + 2);
                            line.truncate(pos);
                            return Poll::Ready(Some(Ok(CompletionEvent::from(&line))));
                        }
                    } else {
                        return Poll::Ready(None);
                    }
                }
                Poll::Pending => continue,
            }
        }
    }
}

fn build_req(
    url: &str,
    device_id: &str,
    token: Option<&str>,
    seed: Option<&str>,
    difficulty: Option<&str>,
    state: StateRef,
) -> Result<reqwest::RequestBuilder, reqwest::Error> {
    let client = match state.proxy.as_ref() {
        Some(proxy) => match Proxy::all(proxy) {
            Ok(proxy) => Client::builder().proxy(proxy).build().ok(),
            Err(e) => {
                log::warn!("setup proxy error: {:?}, ignore proxy: {}", e, proxy);
                None
            }
        },
        None => None,
    }
    .unwrap_or(Client::new());

    let short_lang = state.lang.split('-').next().unwrap_or("en");
    let mut builder = client
        .post(url)
        .header("oai-language", state.lang.clone())
        .header("oai-device-id", device_id)
        .header(ACCEPT, "*/*")
        .header(
            ACCEPT_LANGUAGE,
            format!("{},{};q=0.9", state.lang.clone(), short_lang),
        )
        .header(CACHE_CONTROL, "no-cache")
        .header(PRAGMA, "no-cache")
        .header(REFERER, OPENAI_ENDPOINT)
        .header(ORIGIN, OPENAI_ENDPOINT)
        .header(CONTENT_TYPE, "application/json")
        .header(
            "sec-ch-ua",
            "\"Google Chrome\";v=\"123\", \"Not:A-Brand\";v=\"8\", \"Chromium\";v=\"123\"",
        )
        .header("sec-ch-ua-mobile", "?0")
        .header("sec-ch-ua-platform", "\"Windows\"")
        .header("sec-fetch-dest", "empty")
        .header("sec-fetch-mode", "cors")
        .header("sec-fetch-site", "same-origin")
        .header(USER_AGENT, UA);

    if let Some(seed) = seed {
        let proof_token = openai_sentinel_proof_token(seed, difficulty.unwrap());
        builder = builder.header("openai-sentinel-proof-token", proof_token);
    }

    if let Some(token) = token {
        Ok(builder.header("openai-sentinel-chat-requirements-token", token))
    } else {
        Ok(builder)
    }
}

pub async fn alloc_session(state: StateRef) -> Result<Session, Error> {
    let start_at = std::time::Instant::now();
    let resp = build_req(
        OPENAI_SENTINEL_URL,
        &state.device_id,
        None,
        None,
        None,
        state.clone(),
    )?
    .send()
    .await;

    let resp = match resp {
        Ok(resp) => resp,
        Err(e) => {
            println!("Alloc session fail, proxy: {:?}", state.proxy);
            println!("If this error persists, your country may not be supported yet.");
            println!("If your country was the issue, please consider using a U.S. VPN.");
            return Err(e.into());
        }
    };

    let data = resp.json::<ChatRequirementsResponse>().await?;

    log::debug!(
        "alloc session: {} ms, proxy: {:?} -> {:?}",
        start_at.elapsed().as_millis(),
        state.proxy,
        data,
    );

    Ok(Session {
        start_at,
        token: data.token,
        proof_seed: data.proofofwork.seed,
        proof_difficulty: data.proofofwork.difficulty,
        device_id: state.device_id.clone(),
    })
}

pub async fn execute_plain<C, Fut>(
    state: StateRef,
    messages: Vec<Message>,
    conversion_id: Option<String>,
    parent_message_id: Option<String>,
    hanlde_delta: C,
) -> Result<CompletionResult, Error>
where
    C: FnOnce(String) -> Fut + std::marker::Copy + 'static,
    Fut: Future<Output = ()> + Send + 'static,
{
    let req = CompletionRequest::new(state.clone(), messages, conversion_id, parent_message_id);
    let mut stream = req.stream(state.clone()).await?;

    let mut textbuf = String::new();
    let mut conversation_id = String::new();
    let mut last_message_id = String::new();
    let mut finish_reason = None;

    while let Some(chunk) = stream.next().await {
        match chunk {
            Ok(crate::fgpt::CompletionEvent::Data(event)) => match event.message.as_ref() {
                Some(message) => {
                    if message.author.role != "assistant" {
                        continue;
                    }
                    let text = message.content.parts.join("\n");
                    if textbuf.len() > text.len() {
                        continue;
                    }
                    finish_reason = event.get_finish_reason();
                    conversation_id = event.conversation_id.clone();
                    last_message_id = message.id.clone();

                    let delta_chars = &text[textbuf.len()..];
                    textbuf = text.clone();

                    hanlde_delta(delta_chars.to_string()).await;
                }
                _ => {
                    if event.error.is_some() {
                        log::error!("Error: {:?}", event.error);
                        break;
                    }
                }
            },
            Ok(crate::fgpt::CompletionEvent::Done) => {
                break;
            }
            Ok(_) => {}
            Err(e) => {
                log::error!("{:?}", e);
                break;
            }
        }
    }
    Ok(CompletionResult {
        textbuf,
        conversation_id,
        last_message_id,
        finish_reason,
    })
}

fn openai_sentinel_proof_token(seed: &str, difficulty: &str) -> String {
    let datetime = Local::now()
        .format("%a %b %-d %Y %T GMT%z (%Z)")
        .to_string();
    let difficulty = difficulty.to_string();
    let diff_len = difficulty.len() / 2;
    let mut hasher = sha3::Sha3_512::new();
    let mut rng = rand::thread_rng();

    loop {
        let first_key = [8, 12, 16, 24].choose(&mut rng).unwrap()
            + [3000, 4000, 6000].choose(&mut rng).unwrap();

        let value = serde_json::json! {
            [first_key, datetime, 4294705152_i64, rng.gen_range(0..100000), UA]
        };
        let value = STANDARD.encode(value.to_string());
        hasher.update(format!("{}{}", seed, value));
        let hash = hasher.finalize_reset();
        if hex::encode(&hash[..diff_len]) <= difficulty {
            return format!("gAAAAAB{}", value);
        }
    }
}
