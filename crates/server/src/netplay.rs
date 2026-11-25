//! Netplay rooms: browsers running the same game in step, exchanging their
//! controllers' input. The server gathers them under a short code, routes
//! their messages (to everyone, or to one member) and keeps the room's
//! chat. It does not look at the game messages.
//!
//! A room is a session: it opens without a game, its host starts one
//! (`load`), changes it or stops it, and when the host leaves, the first
//! member with an account takes over (without one, the room closes). Each
//! change starts a new epoch: the browsers drop messages of earlier ones.
//!
//! Text messages are JSON objects. The server stamps them with `from` (the
//! sender's member id) and sends them to the member in `to`, or to every
//! other member. It answers `ping` itself, keeps `chat` messages, which go
//! to everyone, the sender included, and handles `load`. Binary messages
//! start with two bytes, `to` (255: everyone else) and `from`, stamped by
//! the server.

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
use crate::library::Console;
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
/// The member who opens a room.
const OPENER: u8 = 0;

#[derive(Default)]
pub struct Rooms(Mutex<HashMap<String, Room>>);

/// A game a room plays, and its BIOS: what the room's code grants; and the
/// cheat and patch codes the host plays it with, which everyone runs (they
/// change what the game computes), as the host sent them.
struct Playing {
    game: String,
    bios: String,
    codes: Value,
}

/// The codes a room plays with, at most (as JSON).
const MAX_CODES: usize = 256 << 10;

struct Room {
    /// None while the host chooses.
    playing: Option<Playing>,
    /// The member who changes the game and hands out the controllers.
    host: u8,
    /// Counts the changes of game and of host.
    epoch: u32,
    /// Ids are not reused: inputs and controller owners refer to them.
    next_id: u8,
    members: BTreeMap<u8, Member>,
    chat: VecDeque<Value>,
}

struct Member {
    name: String,
    /// Signed in: may become the host.
    registered: bool,
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
                .map(|(id, m)| json!({ "id": id, "name": m.name, "registered": m.registered }))
                .collect(),
        )
    }

    /// Who takes over from the host: the first other member with an
    /// account.
    fn successor(&self) -> Option<u8> {
        self.members
            .iter()
            .find(|(id, m)| **id != self.host && m.registered)
            .map(|(id, _)| *id)
    }
}

impl Rooms {
    /// Whether room `code` is open and plays this game (or uses this
    /// BIOS): what its code lets a player without an account read.
    pub fn grants(&self, code: &str, game: Option<&str>, bios: Option<&str>) -> bool {
        (game.is_some() || bios.is_some())
            && self.lock().get(&normalise(code)).is_some_and(|r| {
                r.playing
                    .as_ref()
                    .is_some_and(|p| game.is_none_or(|g| p.game == g) && bios.is_none_or(|b| p.bios == b))
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

/// Whether rooms can play this game with this BIOS: the PlayStation's.
fn playable(lib: &crate::library::Library, game: &str, bios: &str) -> bool {
    lib.game(game).is_some_and(|g| g.console == Console::Ps1)
        && lib.bios(bios).is_some_and(|b| b.console == Console::Ps1)
}

/// Opens a room, without a game yet; its creator (signed in) is member 0,
/// the host, under their name.
pub async fn host(
    State(state): State<Arc<AppState>>,
    CurrentUser(user, _): CurrentUser,
    ws: WebSocketUpgrade,
) -> Response {
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
                OPENER,
                Member {
                    name: clean_name(&user.name),
                    registered: true,
                    tx,
                },
            );
            let room = Room {
                playing: None,
                host: OPENER,
                epoch: 0,
                next_id: 1,
                members,
                chat: VecDeque::new(),
            };
            let _ = room.send(OPENER, text(&welcome(&code, OPENER, &room)));
            rooms.insert(code.clone(), room);
            code
        };
        tracing::info!(%code, "netplay room opened");
        member(socket, rx, &state, &code, OPENER).await;
    })
}

fn welcome(code: &str, you: u8, room: &Room) -> Value {
    json!({
        "type": "welcome",
        "code": code,
        "you": you,
        "host": room.host,
        "game": room.playing.as_ref().map(|p| &p.game),
        "bios": room.playing.as_ref().map(|p| &p.bios),
        "codes": room.playing.as_ref().map(|p| &p.codes),
        "epoch": room.epoch,
        "members": room.roster(),
        "chat": room.chat,
    })
}

#[derive(Serialize)]
pub struct RoomJson {
    game: Option<String>,
    bios: Option<String>,
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
                game: r.playing.as_ref().map(|p| p.game.clone()),
                bios: r.playing.as_ref().map(|p| p.bios.clone()),
                host: r.members.get(&r.host).map(|m| m.name.clone()).unwrap_or_default(),
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
        let registered = viewer.user.is_some();
        let id = {
            let mut rooms = state.rooms.lock();
            let Some(room) = rooms.get_mut(&code) else { return };
            if room.members.len() >= MAX_MEMBERS || room.next_id == EVERYONE {
                return;
            }
            let id = room.next_id;
            room.next_id += 1;
            room.members.insert(
                id,
                Member {
                    name: name.clone(),
                    registered,
                    tx,
                },
            );
            let _ = room.send(id, text(&welcome(&code, id, room)));
            let joined = json!({ "type": "joined", "id": id, "name": name, "registered": registered });
            room.broadcast(Some(id), &text(&joined));
            id
        };
        tracing::info!(%code, id, "netplay member joined");
        member(socket, rx, &state, &code, id).await;
    })
}

/// Runs a member's connection, then removes it from the room (handing it
/// over, or closing it, when it is the host).
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
    if id != room.host {
        room.broadcast(None, &text(&json!({ "type": "left", "id": id })));
        tracing::info!(%code, id, "netplay member left");
        return;
    }
    match room.successor() {
        Some(host) => {
            room.host = host;
            room.epoch += 1;
            room.broadcast(None, &text(&json!({ "type": "left", "id": id })));
            room.broadcast(None, &text(&json!({ "type": "host", "id": host, "epoch": room.epoch })));
            tracing::info!(%code, id, host, "netplay host left: handed over");
        }
        None => {
            room.broadcast(None, &text(&json!({ "type": "closed" })));
            rooms.remove(code);
            tracing::info!(%code, "netplay room closed");
        }
    }
}

/// The host starts a game (`game` and `bios`), or stops it (`game` null):
/// everyone follows, in a new epoch.
async fn load(state: &AppState, code: &str, from: u8, msg: &serde_json::Map<String, Value>) -> Vec<u8> {
    let field = |k: &str| msg.get(k).and_then(Value::as_str).map(str::to_owned);
    let codes = msg
        .get("codes")
        .filter(|c| c.to_string().len() <= MAX_CODES)
        .cloned()
        .unwrap_or(Value::Null);
    let playing = field("game").map(|game| Playing {
        game,
        bios: field("bios").unwrap_or_default(),
        codes,
    });
    let ok = match &playing {
        Some(p) => playable(&state.library.read().await.clone(), &p.game, &p.bios),
        None => true,
    };
    let mut rooms = state.rooms.lock();
    let Some(room) = rooms.get_mut(code) else {
        return Vec::new();
    };
    let refusal = if from != room.host {
        Some("Only the host changes the game.")
    } else if !ok {
        Some("Rooms play PlayStation games from the library.")
    } else {
        None
    };
    if let Some(why) = refusal {
        let msg = text(&json!({ "type": "error", "text": why }));
        return room.send(from, msg).err().map(|()| vec![from]).unwrap_or_default();
    }
    room.playing = playing;
    room.epoch += 1;
    let game = room.playing.as_ref().map(|p| p.game.as_str());
    tracing::info!(%code, game = game.unwrap_or("none"), "netplay game changed");
    let bios = room.playing.as_ref().map(|p| p.bios.as_str());
    let codes = room.playing.as_ref().map(|p| &p.codes);
    let change = json!({ "type": "game", "game": game, "bios": bios, "codes": codes, "epoch": room.epoch });
    room.broadcast(None, &text(&change))
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

/// A `load` message, which `load` handles (it reads the library).
fn load_request(msg: &Message) -> Option<serde_json::Map<String, Value>> {
    let Message::Text(raw) = msg else { return None };
    match serde_json::from_str::<Value>(raw) {
        Ok(Value::Object(obj)) if obj.get("type").and_then(Value::as_str) == Some("load") => Some(obj),
        _ => None,
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
                    let slow = match load_request(&msg) {
                        Some(obj) => load(state, code, id, &obj).await,
                        None => route(state, code, id, msg),
                    };
                    for slow in slow {
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
    fn successors_have_accounts() {
        let member = |registered| Member {
            name: String::new(),
            registered,
            tx: mpsc::channel(1).0,
        };
        let mut room = Room {
            playing: None,
            host: OPENER,
            epoch: 0,
            next_id: 4,
            members: BTreeMap::from([(OPENER, member(true)), (1, member(false)), (3, member(true))]),
            chat: VecDeque::new(),
        };
        assert_eq!(room.successor(), Some(3));
        room.members.remove(&3);
        assert_eq!(room.successor(), None);
    }

    #[test]
    fn names() {
        assert_eq!(clean_name("  Ann\u{7}ie  "), "Annie");
        assert_eq!(clean_name(""), "Player");
        assert_eq!(clean_name(&"x".repeat(40)).len(), MAX_NAME);
    }
}
