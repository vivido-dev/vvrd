//! A minimal in-process Vivid 1.5 presenter for lifecycle and isolation regressions.
//!
//! It runs the real authentication transcript, the real record framing, and the real track-channel
//! handshake, so tests observe exactly what vvrd puts on the wire. It is not a conformant
//! presenter: it validates only what a regression depends on.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use vivid_protocol::auth::{self, Secret32};
use vivid_protocol::cbor::Value;
use vivid_protocol::messages::{
    self, Envelope, Hello, HelloAuthentication, PayloadMap, Welcome, WelcomeAuthentication,
};
use vivid_protocol::registry;
use vivid_protocol::resource::{Resource, ResourceContract};
use vivid_protocol::wire::{HEADER_SIZE, PREFACE_SIZE, Preface, Record, RecordHeader};

/// Root secret the harness accepts. Tests place it in `VIVID_ROOT_SECRET`.
pub const ROOT_SECRET_HEX: &str =
    "5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a5a";

/// One control-connection request the presenter observed, in arrival order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observed {
    pub record_type: u16,
    pub object_id: u64,
    pub payload: PayloadMap,
}

/// What a track connection did, recorded so a test can assert transport ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TrackChannelLog {
    pub track_id: u64,
    pub channel_generation: u64,
    /// Media records accepted before the peer closed.
    pub media_records: u64,
}

/// One observed `DESTROY_TRACK`, with the evidence a relay would use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DestroyObservation {
    pub track_id: u64,
    /// True when the producer closed the track transport before the ordered destroy was serviced.
    /// A relay that sees this removes the track on EOF and then rejects the destroy.
    pub closed_before_destroy: bool,
}

#[derive(Default)]
struct Shared {
    observed: Vec<Observed>,
    channels: Vec<TrackChannelLog>,
    /// Track IDs whose transport reached EOF, in arrival order.
    closed_channels: Vec<u64>,
    destroys: Vec<DestroyObservation>,
    /// Maximum media-record body granted per track, echoed back on CHANNEL_ACCEPTED.
    track_bodies: HashMap<u64, u32>,
}

pub struct FakePresenter {
    endpoint: String,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    control_writer: Arc<Mutex<Option<TcpStream>>>,
    sequence: Arc<Mutex<u64>>,
    join: Option<JoinHandle<io::Result<()>>>,
}

impl FakePresenter {
    /// Start a presenter with the given terminal grid. Each session gets its own numeric ID space,
    /// which lets two owners deliberately reuse the same object numbers.
    pub fn start(cols: u64, rows: u64) -> io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let endpoint = format!("tcp:{}", listener.local_addr()?);
        let shared = Arc::new(Mutex::new(Shared::default()));
        let stop = Arc::new(AtomicBool::new(false));
        let control_writer = Arc::new(Mutex::new(None));
        let sequence = Arc::new(Mutex::new(0));

        let join = {
            let shared = shared.clone();
            let stop = stop.clone();
            let control_writer = control_writer.clone();
            let sequence = sequence.clone();
            thread::Builder::new()
                .name("fake-presenter".to_owned())
                .spawn(move || {
                    serve(listener, shared, stop, control_writer, sequence, cols, rows)
                })?
        };

        Ok(Self {
            endpoint,
            shared,
            stop,
            control_writer,
            sequence,
            join: Some(join),
        })
    }

    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn observed(&self) -> Vec<Observed> {
        self.shared.lock().expect("shared").observed.clone()
    }

    pub fn channels(&self) -> Vec<TrackChannelLog> {
        self.shared.lock().expect("shared").channels.clone()
    }

    pub fn destroys(&self) -> Vec<DestroyObservation> {
        self.shared.lock().expect("shared").destroys.clone()
    }

    /// Push an uncorrelated `TRACK_LOST` for a complete owner identity.
    pub fn lose_track(&self, context_id: u64, surface_id: u64, track_id: u64) -> io::Result<()> {
        let body = Envelope::new(
            0,
            vec![
                (0, Value::Unsigned(context_id)),
                (1, Value::Unsigned(surface_id)),
                (2, Value::Unsigned(track_id)),
                (3, Value::Unsigned(registry::error::DECODER)),
                (4, Value::Unsigned(2)),
                (5, Value::Map(Vec::new())),
                (6, Value::Text("decoder failed".to_owned())),
            ],
        )
        .encode()
        .map_err(io::Error::other)?;
        self.push(messages::TRACK_LOST, track_id, &body)
    }

    fn push(&self, record_type: u16, object_id: u64, body: &[u8]) -> io::Result<()> {
        let mut guard = self.control_writer.lock().expect("control writer");
        let stream = guard
            .as_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::NotConnected, "no control connection"))?;
        let mut sequence = self.sequence.lock().expect("sequence");
        *sequence += 1;
        write_record(stream, *sequence, record_type, 0, object_id, body)
    }
}

impl Drop for FakePresenter {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Ok(mut guard) = self.control_writer.lock() {
            if let Some(stream) = guard.take() {
                let _ = stream.shutdown(std::net::Shutdown::Both);
            }
        }
        // The accept loop wakes on its own connection, so a probe connect unblocks it.
        if let Some(address) = self.endpoint.strip_prefix("tcp:") {
            let _ = TcpStream::connect(address);
        }
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

fn write_record(
    stream: &mut TcpStream,
    sequence: u64,
    record_type: u16,
    flags: u16,
    object_id: u64,
    body: &[u8],
) -> io::Result<()> {
    let header = RecordHeader {
        body_length: u32::try_from(body.len()).map_err(io::Error::other)?,
        record_type,
        flags,
        object_id,
        sequence,
    };
    stream.write_all(&header.encode())?;
    stream.write_all(body)?;
    stream.flush()
}

fn read_record(stream: &mut TcpStream) -> io::Result<Record> {
    let mut header = [0_u8; HEADER_SIZE];
    stream.read_exact(&mut header)?;
    let header = RecordHeader::decode(header);
    let mut body = vec![0_u8; header.body_length as usize];
    stream.read_exact(&mut body)?;
    Ok(Record {
        record_type: header.record_type,
        flags: header.flags,
        object_id: header.object_id,
        sequence: header.sequence,
        body,
    })
}

fn read_preface(stream: &mut TcpStream) -> io::Result<Preface> {
    let mut bytes = [0_u8; PREFACE_SIZE];
    stream.read_exact(&mut bytes)?;
    Preface::decode(bytes)
}

#[allow(clippy::too_many_arguments)]
fn serve(
    listener: TcpListener,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    control_writer: Arc<Mutex<Option<TcpStream>>>,
    sequence: Arc<Mutex<u64>>,
    cols: u64,
    rows: u64,
) -> io::Result<()> {
    let address = listener.local_addr()?;
    let (mut control, _) = listener.accept()?;
    control.set_read_timeout(Some(Duration::from_secs(10)))?;
    control.set_write_timeout(Some(Duration::from_secs(10)))?;
    let mut preface_bytes = [0_u8; PREFACE_SIZE];
    control.read_exact(&mut preface_bytes)?;
    let _ = Preface::decode(preface_bytes)?;

    let hello_record = read_record(&mut control)?;
    if hello_record.record_type != messages::HELLO {
        return Err(io::Error::other("first control record was not HELLO"));
    }
    let (hello_request, hello) = Hello::decode(&hello_record.body).map_err(io::Error::other)?;
    let root = Secret32::from_hex(ROOT_SECRET_HEX).map_err(io::Error::other)?;
    let HelloAuthentication::Root { proof } = &hello.authentication else {
        return Err(io::Error::other("expected root authentication"));
    };
    let authless = hello.authless_payload().map_err(io::Error::other)?;
    if !auth::verify_root_hello_proof(&root, &preface_bytes, &authless, proof) {
        return Err(io::Error::other("root HELLO proof did not verify"));
    }

    let server_nonce = [7_u8; 32];
    let prk = auth::extract_handshake_prk(&root, &hello.client_nonce, &server_nonce, &[0; 32]);
    let mut accepted_profiles = hello.required_profiles.clone();
    accepted_profiles.extend(hello.optional_profiles.iter().cloned());
    accepted_profiles.sort();
    accepted_profiles.dedup();
    let mut welcome = Welcome {
        session_id: 1,
        session_tag: [3; messages::SESSION_TAG_BYTES],
        root_context_id: 1,
        target_generation: 1,
        target_profile: hello.target_profile.clone(),
        target_descriptor: terminal_descriptor(cols, rows),
        accepted_profiles,
        maximum_control_body: vivid_protocol::CONTROL_MAX_RECORD_BODY,
        server_nonce,
        authentication: WelcomeAuthentication {
            kind: messages::AUTHENTICATION_ROOT,
            confirmation: [0; 32],
            lease_state: 0,
            activation_attempt_status: 0,
        },
        session_revision: 1,
        scene_revision: 1,
        resource_contract: generous_contract(),
        establishment_state: 0,
        resume_generation: 0,
        extensions: Vec::new(),
    };
    welcome.confirm(&prk).map_err(io::Error::other)?;
    let welcome_body = welcome.encode(hello_request).map_err(io::Error::other)?;
    {
        let mut guard = sequence.lock().expect("sequence");
        *guard += 1;
        write_record(&mut control, *guard, messages::WELCOME, 0, 0, &welcome_body)?;
    }
    *control_writer.lock().expect("control writer") = Some(control.try_clone()?);

    let channel_key = {
        let (keys, _) = auth::derive_session_keys(
            &prk,
            welcome.session_id,
            welcome.resume_generation,
            &welcome.session_tag,
        );
        Secret32::new(*keys.channel_key())
    };

    // Track connections arrive on the same endpoint; a dedicated acceptor keeps the control loop
    // responsive while a producer opens a channel mid-request.
    let acceptor = {
        let shared = shared.clone();
        let stop = stop.clone();
        let channel_key = Secret32::new(*channel_key.expose());
        thread::spawn(move || accept_track_channels(listener, shared, stop, channel_key))
    };

    let mut scene_revision = 1_u64;
    let mut surface_revisions: HashMap<(u64, u64), (u64, u64)> = HashMap::new();
    let mut track_revisions: HashMap<u64, u64> = HashMap::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let record = match read_record(&mut control) {
            Ok(record) => record,
            Err(_) => break,
        };
        let envelope = messages::decode_control(&record.body).map_err(io::Error::other)?;
        let request_id = envelope.request_id;
        let payload = envelope.payload.clone();
        shared.lock().expect("shared").observed.push(Observed {
            record_type: record.record_type,
            object_id: record.object_id,
            payload: payload.clone(),
        });
        let unsigned = |key: u64| {
            payload
                .iter()
                .find(|(candidate, _)| *candidate == key)
                .and_then(|(_, value)| value.as_u64())
        };

        let (reply_type, reply_body) = match record.record_type {
            messages::CREATE_SURFACE => {
                let key = (unsigned(0).unwrap_or(0), unsigned(1).unwrap_or(0));
                surface_revisions.insert(key, (1, 1));
                (
                    messages::SURFACE_READY,
                    ok_payload(
                        request_id,
                        vec![
                            (0, Value::Unsigned(key.0)),
                            (1, Value::Unsigned(key.1)),
                            (2, Value::Unsigned(1)),
                            (3, Value::Unsigned(1)),
                            (4, Value::Unsigned(unsigned(10).unwrap_or(0))),
                            (5, Value::Map(Vec::new())),
                        ],
                    )?,
                )
            }
            messages::UPDATE_SURFACE => {
                let key = (unsigned(0).unwrap_or(0), unsigned(1).unwrap_or(0));
                if let Some(state) = surface_revisions.get_mut(&key) {
                    state.0 += 1;
                }
                (messages::OK, messages::ok(request_id))
            }
            messages::PROBE_TRACK_CONFIG => (
                messages::TRACK_SUPPORT,
                ok_payload(
                    request_id,
                    vec![
                        (0, Value::Bool(true)),
                        (1, Value::Text("fake-raster".to_owned())),
                        (2, Value::Unsigned(1)),
                        (3, Value::Map(Vec::new())),
                    ],
                )?,
            ),
            messages::CREATE_TRACK => {
                track_revisions.insert(record.object_id, 1);
                let maximum_body = unsigned(7).unwrap_or(1);
                shared.lock().expect("shared").track_bodies.insert(
                    record.object_id,
                    u32::try_from(maximum_body).unwrap_or(u32::MAX),
                );
                let delta_operations = payload
                    .iter()
                    .find(|(key, _)| *key == 12)
                    .and_then(|(_, value)| match value {
                        Value::Map(map) => map
                            .iter()
                            .find(|(key, _)| *key == 5)
                            .and_then(|(_, value)| value.as_u64()),
                        _ => None,
                    })
                    .unwrap_or(0);
                (
                    messages::TRACK_READY,
                    ok_payload(
                        request_id,
                        vec![
                            (0, Value::Unsigned(unsigned(0).unwrap_or(0))),
                            (1, Value::Unsigned(unsigned(1).unwrap_or(0))),
                            (2, Value::Unsigned(record.object_id)),
                            (3, Value::Unsigned(1)),
                            (4, Value::Unsigned(1)),
                            (5, Value::Unsigned(30_000_000)),
                            (6, Value::Unsigned(maximum_body)),
                            (7, Value::Map(Vec::new())),
                            (8, Value::Bool(true)),
                            (9, Value::Unsigned(delta_operations)),
                        ],
                    )?,
                )
            }
            messages::WAIT_TRACK => {
                let revision = *track_revisions.get(&record.object_id).unwrap_or(&1);
                (
                    messages::WAIT_SATISFIED,
                    ok_payload(
                        request_id,
                        vec![
                            (0, Value::Unsigned(unsigned(0).unwrap_or(0))),
                            (1, Value::Unsigned(unsigned(1).unwrap_or(0))),
                            (2, Value::Unsigned(record.object_id)),
                            (3, Value::Unsigned(revision)),
                            (4, Value::Unsigned(unsigned(6).unwrap_or(1))),
                            (5, Value::Unsigned(unsigned(3).unwrap_or(2))),
                        ],
                    )?,
                )
            }
            messages::ACTIVATE_TRACK => {
                let key = (unsigned(0).unwrap_or(0), unsigned(1).unwrap_or(0));
                let revision = surface_revisions
                    .get_mut(&key)
                    .map(|state| {
                        state.0 += 1;
                        state.0
                    })
                    .unwrap_or(2);
                (
                    messages::TRACK_ACTIVATED,
                    ok_payload(
                        request_id,
                        vec![
                            (0, Value::Unsigned(key.0)),
                            (1, Value::Unsigned(key.1)),
                            (2, Value::Array(Vec::new())),
                            (3, Value::Unsigned(revision)),
                            (4, Value::Unsigned(1)),
                        ],
                    )?,
                )
            }
            messages::ADVANCE_CHANNEL => {
                let revision = track_revisions
                    .entry(record.object_id)
                    .and_modify(|value| *value += 1)
                    .or_insert(2);
                (
                    messages::CHANNEL_ADVANCED,
                    ok_payload(
                        request_id,
                        vec![
                            (0, Value::Unsigned(unsigned(0).unwrap_or(0))),
                            (1, Value::Unsigned(unsigned(1).unwrap_or(0))),
                            (2, Value::Unsigned(record.object_id)),
                            (3, Value::Unsigned(unsigned(4).unwrap_or(2))),
                            (4, Value::Unsigned(30_000_000)),
                            (5, Value::Unsigned(*revision)),
                        ],
                    )?,
                )
            }
            messages::COMMIT_TXN => {
                scene_revision += 1;
                (
                    messages::SCENE_PRESENTED,
                    ok_payload(
                        request_id,
                        vec![
                            (0, Value::Unsigned(scene_revision)),
                            (1, Value::Unsigned(1)),
                        ],
                    )?,
                )
            }
            messages::DESTROY_TRACK => {
                // Give a wrongly dropped transport time to reach EOF before the ordered destroy is
                // serviced, exactly as a relay's media worker would observe it.
                thread::sleep(Duration::from_millis(100));
                let mut guard = shared.lock().expect("shared");
                let closed_before_destroy = guard.closed_channels.contains(&record.object_id);
                guard.destroys.push(DestroyObservation {
                    track_id: record.object_id,
                    closed_before_destroy,
                });
                drop(guard);
                (messages::OK, messages::ok(request_id))
            }
            messages::PING => (messages::PONG, messages::ok(request_id)),
            _ => (messages::OK, messages::ok(request_id)),
        };

        let object_id = match reply_type {
            messages::SCENE_PRESENTED => record.object_id,
            _ => record.object_id,
        };
        let mut guard = sequence.lock().expect("sequence");
        *guard += 1;
        write_record(&mut control, *guard, reply_type, 0, object_id, &reply_body)?;
        drop(guard);
        if record.record_type == messages::GOODBYE {
            break;
        }
    }

    stop.store(true, Ordering::SeqCst);
    // The track acceptor blocks in accept(); one probe connection releases it.
    let _ = TcpStream::connect(address);
    let _ = acceptor.join();
    Ok(())
}

fn accept_track_channels(
    listener: TcpListener,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    channel_key: Secret32,
) -> io::Result<()> {
    let mut workers = Vec::new();
    while !stop.load(Ordering::SeqCst) {
        let (stream, _) = match listener.accept() {
            Ok(accepted) => accepted,
            Err(_) => break,
        };
        if stop.load(Ordering::SeqCst) {
            break;
        }
        let shared = shared.clone();
        let key = Secret32::new(*channel_key.expose());
        workers.push(thread::spawn(move || {
            serve_track_channel(stream, shared, key)
        }));
    }
    for worker in workers {
        let _ = worker.join();
    }
    Ok(())
}

fn serve_track_channel(
    mut stream: TcpStream,
    shared: Arc<Mutex<Shared>>,
    channel_key: Secret32,
) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    let _ = read_preface(&mut stream)?;
    let open_record = read_record(&mut stream)?;
    if open_record.record_type != messages::CHANNEL_OPEN {
        return Err(io::Error::other("track connection did not open a channel"));
    }
    let open = messages::ChannelOpen::decode(open_record.object_id, &open_record.body)
        .map_err(io::Error::other)?;
    let expected = auth::channel_tag(
        channel_key.expose(),
        open.session_id,
        open.context_id,
        open.surface_id,
        open.track_id,
        open.channel_generation,
        open.track_kind as u32,
        open.lane as u32,
        &open.client_nonce,
    );
    if !auth::verify_tag(&expected, &open.authentication_tag) {
        return Err(io::Error::other("CHANNEL_OPEN authentication tag failed"));
    }

    let maximum_body = shared
        .lock()
        .expect("shared")
        .track_bodies
        .get(&open.track_id)
        .copied()
        .unwrap_or(64 * 1024);
    let accepted = envelope_body(
        1,
        vec![
            (0, Value::Unsigned(open.context_id)),
            (1, Value::Unsigned(open.surface_id)),
            (2, Value::Unsigned(open.track_id)),
            (3, Value::Unsigned(open.channel_generation)),
            (4, Value::Unsigned(u64::from(maximum_body) * 4096)),
            (5, Value::Unsigned(4096)),
            (6, Value::Unsigned(u64::from(maximum_body))),
            (7, Value::Unsigned(2)),
        ],
    )?;
    write_record(
        &mut stream,
        1,
        messages::CHANNEL_ACCEPTED,
        0,
        open.track_id,
        &accepted,
    )?;

    // Log the accepted generation immediately: a producer's channel may legitimately stay open for
    // the whole session, so a test must not have to wait for EOF to see that it was established.
    shared
        .lock()
        .expect("shared")
        .channels
        .push(TrackChannelLog {
            track_id: open.track_id,
            channel_generation: open.channel_generation,
            media_records: 0,
        });

    loop {
        match read_record(&mut stream) {
            Ok(record) if record.record_type == messages::CHANNEL_EOS => break,
            Ok(_) => {
                let mut guard = shared.lock().expect("shared");
                if let Some(entry) = guard.channels.iter_mut().find(|entry| {
                    entry.track_id == open.track_id
                        && entry.channel_generation == open.channel_generation
                }) {
                    entry.media_records += 1;
                }
            }
            Err(_) => break,
        }
    }
    shared
        .lock()
        .expect("shared")
        .closed_channels
        .push(open.track_id);
    Ok(())
}

fn envelope_body(request_id: u64, payload: PayloadMap) -> io::Result<Vec<u8>> {
    ok_payload(request_id, payload)
}

fn ok_payload(request_id: u64, payload: PayloadMap) -> io::Result<Vec<u8>> {
    Envelope::correlated(request_id, payload)
        .and_then(|envelope| envelope.encode())
        .map_err(io::Error::other)
}

fn terminal_descriptor(cols: u64, rows: u64) -> PayloadMap {
    vec![
        (0, Value::Unsigned(cols * 10)),
        (1, Value::Unsigned(rows * 20)),
        (2, Value::Unsigned(cols)),
        (3, Value::Unsigned(rows)),
        (4, Value::Unsigned(10)),
        (5, Value::Unsigned(20)),
        (6, Value::Bool(true)),
        (7, Value::Unsigned(3)),
        (8, Value::Unsigned(64)),
    ]
}

fn generous_contract() -> ResourceContract {
    let mut contract =
        ResourceContract::new([u64::MAX / 4; vivid_protocol::resource::RESOURCE_COUNT]);
    contract.set(
        Resource::ControlRecordBody,
        u64::from(vivid_protocol::CONTROL_MAX_RECORD_BODY),
    );
    contract.set(Resource::MediaRecordBody, 16 * 1024 * 1024);
    contract
}
