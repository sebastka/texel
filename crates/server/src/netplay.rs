//! Netplay rooms: browsers running the same game in step, exchanging their
//! controllers' input. The server gathers them under a short code, routes
//! their messages (to everyone, or to one member) and keeps the room's
//! chat. It does not look at the game messages.
//!
//! Text messages are JSON objects. The server stamps them with `from` (the
//! sender's member id) and sends them to the member in `to`, or to every
//! other member. It answers `ping` itself and keeps `chat` messages, which
//! go to everyone, the sender included. Binary messages start with two
//! bytes, `to` (255: everyone else) and `from`, stamped by the server.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::Json;
use axum::body::Bytes;
use axum::extract::ws::{Message, Utf8Bytes, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::auth::{CurrentUser, Viewer};
use crate::state::AppState;

/// Unambiguous characters: no 0/O, 1/I/L.
const ALPHABET: &[u8] = b"ABCDEFGHJKMNPQRSTUVWXYZ23456789";
const CODE_LEN: usize = 6;
const MAX_ROOMS: usize = 100;
/// Members of a room, host included.
const MAX_MEMBERS: usize = 8;
const MAX_NAME: usize = 24;
const MAX_CHAT: usize = 500;
/// Chat lines kept for members who join later.
const CHAT_HISTORY: usize = 100;
/// The largest message: the game's state, sent to members who join.
pub const MAX_MESSAGE: usize = 32 << 20;
/// Messages queued for a member; one that falls this far behind is dropped.
const QUEUE: usize = 4096;
/// Keeps idle connections open through proxies.
const PING_EVERY: Duration = Duration::from_secs(10);
/// Browsers send something every second: a silent one is gone (or its page
/// frozen in the back/forward cache, where its socket may stay open).
const SILENT: Duration = Duration::from_secs(30);
const EVERYONE: u8 = 255;
const HOST: u8 = 0;

#[derive(Default)]
pub struct Rooms(Mutex<HashMap<String, Room>>);

struct Room {
    game: String,
    bios: String,
    /// Ids are not reused: inputs and controller owners refer to them.
    next_id: u8,
    members: BTreeMap<u8, Member>,
    chat: VecDeque<Value>,
}

struct Member {
    name: String,
    tx: mpsc::Sender<Message>,
}

impl Room {
    fn send(&self, to: u8, msg: Message) -> Result<(), ()> {
        match self.members.get(&to) {
            Some(m) => match m.tx.try_send(msg) {
                Err(mpsc::error::TrySendError::Full(_)) => Err(()),
                _ => Ok(()),
            },
            None => Ok(()),
        }
    }

    /// Sends to every member but `except`. Members too far behind are
    /// returned, to be dropped.
    fn broadcast(&self, except: Option<u8>, msg: &Message) -> Vec<u8> {
        self.members
            .iter()
            .filter(|(id, _)| Some(**id) != except)
            .filter(|(_, m)| matches!(m.tx.try_send(msg.clone()), Err(mpsc::error::TrySendError::Full(_))))
            .map(|(id, _)| *id)
            .collect()
    }

    fn roster(&self) -> Value {
        Value::Array(
            self.members
                .iter()
                .map(|(id, m)| json!({ "id": id, "name": m.name }))
                .collect(),
        )
    }
}

impl Rooms {
    /// Whether room `code` is open and plays this game (or uses this
    /// BIOS): what its code lets a player without an account read.
    pub fn grants(&self, code: &str, game: Option<&str>, bios: Option<&str>) -> bool {
        self.lock().get(&normalise(code)).is_some_and(|r| {
            game.is_none_or(|g| r.game == g) && bios.is_none_or(|b| r.bios == b) && (game.is_some() || bios.is_some())
        })
    }

    fn lock(&self) -> MutexGuard<'_, HashMap<String, Room>> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// "abc-def" → "ABCDEF".
fn normalise(code: &str) -> String {
    code.chars()
        .filter(char::is_ascii_alphanumeric)
        .map(|c| c.to_ascii_uppercase())
        .collect()
}

/// A display name: no control characters, at most MAX_NAME characters.
fn clean_name(name: &str) -> String {
    let name: String = name.chars().filter(|c| !c.is_control()).take(MAX_NAME).collect();
    let name = name.trim();
    if name.is_empty() {
        "Player".to_owned()
    } else {
        name.to_owned()
    }
}

fn new_code() -> String {
    let mut bytes = [0u8; CODE_LEN];
    // Without randomness the code would be guessable: fail loudly.
    getrandom::fill(&mut bytes).expect("the OS random number generator");
    bytes
        .iter()
        .map(|b| char::from(ALPHABET[usize::from(*b) % ALPHABET.len()]))
        .collect()
}

fn text(value: &Value) -> Message {
    Message::Text(Utf8Bytes::from(value.to_string()))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

#[derive(Deserialize)]
pub struct HostQuery {
    game: String,
    bios: String,
}

/// Opens a room for a running game; its creator (signed in) is member 0,
/// the host, under their name.
pub async fn host(
    State(state): State<Arc<AppState>>,
    CurrentUser(user, _): CurrentUser,
    Query(q): Query<HostQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let lib = state.library.read().await.clone();
    if lib.game(&q.game).is_none() || lib.bios(&q.bios).is_none() {
        return (StatusCode::NOT_FOUND, "unknown game or BIOS").into_response();
    }
    if state.rooms.lock().len() >= MAX_ROOMS {
        return (StatusCode::SERVICE_UNAVAILABLE, "too many rooms").into_response();
    }
    ws.max_message_size(MAX_MESSAGE).on_upgrade(move |socket| async move {
        let (tx, rx) = mpsc::channel(QUEUE);
        let code = {
            let mut rooms = state.rooms.lock();
            let code = std::iter::repeat_with(new_code)
                .find(|c| !rooms.contains_key(c))
                .expect("an endless iterator");
            let mut members = BTreeMap::new();
            members.insert(
                HOST,
                Member {
                    name: clean_name(&user.name),
                    tx,
                },
            );
            let room = Room {
                game: q.game,
                bios: q.bios,
                next_id: 1,
                members,
                chat: VecDeque::new(),
            };
            let _ = room.send(HOST, text(&welcome(&code, HOST, &room)));
            rooms.insert(code.clone(), room);
            code
        };
        tracing::info!(%code, "netplay room opened");
        member(socket, rx, &state, &code, HOST).await;
    })
}

fn welcome(code: &str, you: u8, room: &Room) -> Value {
    json!({
        "type": "welcome",
        "code": code,
        "you": you,
        "members": room.roster(),
        "chat": room.chat,
    })
}

#[derive(Serialize)]
pub struct RoomJson {
    game: String,
    bios: String,
    host: String,
    members: usize,
    full: bool,
}

/// What a code leads to, before joining. The answer carries a cookie with
/// the code: it lets a player without an account read the room's BIOS and
/// disc (see `auth::Viewer`).
pub async fn room(State(state): State<Arc<AppState>>, Path(code): Path<String>) -> Response {
    let code = normalise(&code);
    let rooms = state.rooms.lock();
    match rooms.get(&code) {
        Some(r) => (
            [(axum::http::header::SET_COOKIE, state.auth.room_cookie(&code))],
            Json(RoomJson {
                game: r.game.clone(),
                bios: r.bios.clone(),
                host: r.members.get(&HOST).map(|m| m.name.clone()).unwrap_or_default(),
                members: r.members.len(),
                full: r.members.len() >= MAX_MEMBERS || r.next_id == EVERYONE,
            }),
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "no such room").into_response(),
    }
}

#[derive(Deserialize)]
pub struct JoinQuery {
    #[serde(default)]
    name: String,
}

/// Joins a room: under their account's name when signed in, else under the
/// name given.
pub async fn join(
    State(state): State<Arc<AppState>>,
    viewer: Viewer,
    Path(code): Path<String>,
    Query(q): Query<JoinQuery>,
    ws: WebSocketUpgrade,
) -> Response {
    let code = normalise(&code);
    match state.rooms.lock().get(&code) {
        None => return (StatusCode::NOT_FOUND, "no such room").into_response(),
        Some(r) if r.members.len() >= MAX_MEMBERS || r.next_id == EVERYONE => {
            return (StatusCode::CONFLICT, "the room is full").into_response();
        }
        Some(_) => {}
    }
    ws.max_message_size(MAX_MESSAGE).on_upgrade(move |socket| async move {
        let (tx, rx) = mpsc::channel(QUEUE);
        let name = clean_name(viewer.user.as_ref().map_or(&q.name, |u| &u.name));
        let id = {
            let mut rooms = state.rooms.lock();
            let Some(room) = rooms.get_mut(&code) else { return };
            if room.members.len() >= MAX_MEMBERS || room.next_id == EVERYONE {
                return;
            }
            let id = room.next_id;
            room.next_id += 1;
            room.members.insert(id, Member { name: name.clone(), tx });
            let _ = room.send(id, text(&welcome(&code, id, room)));
            room.broadcast(Some(id), &text(&json!({ "type": "joined", "id": id, "name": name })));
            id
        };
        tracing::info!(%code, id, "netplay member joined");
        member(socket, rx, &state, &code, id).await;
    })
}

/// Runs a member's connection, then removes it from the room (closing the
/// room when it is the host).
async fn member(socket: WebSocket, rx: mpsc::Receiver<Message>, state: &AppState, code: &str, id: u8) {
    relay(socket, rx, state, code, id).await;
    leave(state, code, id);
}

fn leave(state: &AppState, code: &str, id: u8) {
    let mut rooms = state.rooms.lock();
    let Some(room) = rooms.get_mut(code) else { return };
    if room.members.remove(&id).is_none() {
        return;
    }
    if id == HOST {
        room.broadcast(None, &text(&json!({ "type": "closed" })));
        rooms.remove(code);
        tracing::info!(%code, "netplay room closed");
    } else {
        room.broadcast(None, &text(&json!({ "type": "left", "id": id })));
        tracing::info!(%code, id, "netplay member left");
    }
}

/// Routes one message from member `from`. Returns members to drop (their
/// queues are full).
fn route(state: &AppState, code: &str, from: u8, msg: Message) -> Vec<u8> {
    let mut rooms = state.rooms.lock();
    let Some(room) = rooms.get_mut(code) else {
        return Vec::new();
    };
    match msg {
        Message::Binary(bytes) if bytes.len() >= 2 => {
            let mut data = bytes.to_vec();
            let to = data[0];
            data[1] = from;
            let msg = Message::Binary(Bytes::from(data));
            if to == EVERYONE {
                room.broadcast(Some(from), &msg)
            } else {
                room.send(to, msg).err().map(|()| vec![to]).unwrap_or_default()
            }
        }
        Message::Text(raw) => {
            let Ok(Value::Object(mut obj)) = serde_json::from_str::<Value>(&raw) else {
                return Vec::new();
            };
            match obj.get("type").and_then(Value::as_str) {
                Some("ping") => {
                    let pong = json!({ "type": "pong", "t": obj.get("t").cloned().unwrap_or(Value::Null) });
                    room.send(from, text(&pong))
                        .err()
                        .map(|()| vec![from])
                        .unwrap_or_default()
                }
                Some("chat") => {
                    let said: String = obj
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .chars()
                        .filter(|c| !c.is_control())
                        .take(MAX_CHAT)
                        .collect();
                    let said = said.trim();
                    if said.is_empty() {
                        return Vec::new();
                    }
                    let name = room.members.get(&from).map(|m| m.name.clone()).unwrap_or_default();
                    let line = json!({ "type": "chat", "from": from, "name": name, "text": said, "at": now_ms() });
                    room.chat.push_back(line.clone());
                    if room.chat.len() > CHAT_HISTORY {
                        room.chat.pop_front();
                    }
                    room.broadcast(None, &text(&line))
                }
                _ => {
                    obj.insert("from".to_owned(), Value::from(from));
                    let to = obj.get("to").and_then(Value::as_u64);
                    let msg = text(&Value::Object(obj));
                    match to.and_then(|t| u8::try_from(t).ok()) {
                        Some(to) => room.send(to, msg).err().map(|()| vec![to]).unwrap_or_default(),
                        None => room.broadcast(Some(from), &msg),
                    }
                }
            }
        }
        _ => Vec::new(),
    }
}

/// Forwards the socket's messages to the room, and the room's to the
/// socket, until the member goes away.
async fn relay(mut socket: WebSocket, mut rx: mpsc::Receiver<Message>, state: &AppState, code: &str, id: u8) {
    let mut ping = tokio::time::interval(PING_EVERY);
    ping.tick().await;
    let mut heard = Instant::now();
    loop {
        tokio::select! {
            incoming = socket.recv() => match incoming {
                Some(Ok(msg @ (Message::Text(_) | Message::Binary(_)))) => {
                    heard = Instant::now();
                    for slow in route(state, code, id, msg) {
                        tracing::warn!(%code, id = slow, "netplay member too slow: dropping");
                        if slow == id {
                            return;
                        }
                        leave(state, code, slow);
                    }
                }
                Some(Ok(Message::Ping(_) | Message::Pong(_))) => {}
                Some(Ok(Message::Close(_)) | Err(_)) | None => break,
            },
            outgoing = rx.recv() => match outgoing {
                Some(msg) => {
                    if socket.send(msg).await.is_err() {
                        break;
                    }
                }
                // Removed from the room (closed, or too slow).
                None => break,
            },
            _ = ping.tick() => {
                if heard.elapsed() > SILENT {
                    tracing::info!(%code, id, "netplay member silent: closing");
                    break;
                }
                if socket.send(Message::Ping(Bytes::new())).await.is_err() {
                    break;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codes() {
        let code = new_code();
        assert_eq!(code.len(), CODE_LEN);
        assert!(code.bytes().all(|b| ALPHABET.contains(&b)));
        assert_eq!(normalise(" abc-d2f "), "ABCD2F");
    }

    #[test]
    fn names() {
        assert_eq!(clean_name("  Ann\u{7}ie  "), "Annie");
        assert_eq!(clean_name(""), "Player");
        assert_eq!(clean_name(&"x".repeat(40)).len(), MAX_NAME);
    }
}
