//! The source side (Linux and Windows): capture local input, run the
//! focus state machine, and send input to the focused peer.

use crate::clipboard::Keeper;
use crate::clipboard::history;
use crate::clipboard::sync::DataLinks;
use crate::control::{self, Control, Order, Request, Status};
use crate::edges;
use crate::focus::{Action, Focus, FocusMachine};
use crate::input::Input;
use crate::input::emit;
use crate::input::event::InputEvent;
use crate::input::hotkey::Hotkey;
use crate::input::record::{Heard, Recording};
use crate::monitors;
use crate::net::discovery;
use crate::net::group;
use crate::net::identity::{self, Identity};
use crate::net::latency;
use crate::net::protocol::Message;
use crate::net::trust;
use crate::threads::LockAnyway;
use crate::tray::{self, Tray};
use crate::update;
use mickey_config as config;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::time::{Duration, Instant};

mod hotkeys;
mod joining;
mod layout;
mod orders;
mod peer;
mod sound;
mod switcher;

use hotkeys::{offer_hotkeys, take_hotkeys};
use joining::{add_peer, join, take_map};
use layout::{Landing, arrange, edge_crossed, edges_matter, landing, save_position};
use orders::{found_text, obey, peers_text};
use switcher::{Switchers, switcher_cards};

use peer::{
    News, Peer, Shared, connect_first, open_data_link, read_peer, reconnect, report_latency,
    save_key,
};
use sound::{audio_target, listen_here, take_audio};

const PING_EVERY: Duration = Duration::from_secs(1);
/// With `MICKEY_BENCH` set, while the keyboard is on another PC: a ping
/// every 10 ms, so the latency report (on coming home) holds enough samples
/// for a p99 while input is flowing (#191). Off by default: one a second is
/// plenty to notice a PC gone silent.
const BENCH_PING_EVERY: Duration = Duration::from_millis(10);
const KEYS_UP_POLL: Duration = Duration::from_millis(10);
const RETRY_EVERY: Duration = Duration::from_secs(5);
/// A peer that has answered nothing (no pong) for this long is treated as
/// gone: frozen or unplugged, it must not keep the keyboard and mouse.
const SILENT_LIMIT: Duration = Duration::from_secs(4);

/// What wakes the main loop.
enum Wake {
    Input(InputEvent),
    /// The switcher window closed: the picked peer, `None` for this PC or
    /// Esc, or why the window could not run.
    Pick(Result<Option<usize>, String>),
    Control(Request),
    /// A peer's keyboard and mouse came over to this PC, or left.
    Controlled(bool),
    /// A pick in the tray icon's menu.
    Tray(tray::Click),
    /// The peer at this address placed itself on this PC's desktop.
    Arranged(std::net::IpAddr, [i32; 2]),
    /// The peer at this address runs this Mickey version.
    Version(std::net::IpAddr, String),
    /// A peer's group map (PRD 0010), and who sent it.
    Group(Vec<group::Member>, [u8; 32]),
    /// A peer's hotkeys, and the address it sent them from.
    Hotkeys(String, std::net::IpAddr),
    /// The person here said yes to a peer's hotkeys (#349).
    HotkeysAccepted(String),
    /// Who listens, from a peer.
    Audio(String),
    /// The person here accepted a PC the group introduced (#184).
    Accepted(group::Member, [u8; 32]),
}

impl From<InputEvent> for Wake {
    fn from(event: InputEvent) -> Wake {
        Wake::Input(event)
    }
}

impl From<Request> for Wake {
    fn from(request: Request) -> Wake {
        Wake::Control(request)
    }
}

/// Whether the keyboard and mouse in use control this PC (PRD 0009). Two
/// things change it: this PC's own keyboard going to a peer or coming home,
/// and a peer's keyboard coming over or leaving. The last change wins.
struct InControl {
    here: bool,
    /// Where this PC's own keyboard is: `None` here, or a peer.
    focus: Option<usize>,
}

impl Default for InControl {
    fn default() -> InControl {
        InControl {
            here: true,
            focus: None,
        }
    }
}

impl InControl {
    fn here(&self) -> bool {
        self.here
    }

    /// This PC's keyboard is on `focus` now. If it moved, returns where it
    /// was, so the peers can be told.
    fn focus_is(&mut self, focus: Option<usize>) -> Option<Option<usize>> {
        if focus == self.focus {
            return None;
        }
        let was = std::mem::replace(&mut self.focus, focus);
        self.here = focus.is_none();
        Some(was)
    }

    /// A peer's keyboard came over (`true`) or left (`false`).
    fn peer_says(&mut self, here: bool) {
        self.here = here;
    }
}

/// Opens `mickey-ui <window>` (settings, history), installed next to this
/// program, and leaves it running on its own.
fn open_window(window: &str) {
    if let Err(err) = crate::window::ui_command(&[window]).and_then(|mut ui| ui.spawn()) {
        eprintln!("mickey: cannot open the {window} window: {err}");
    }
}

/// Uses `config`'s hotkeys at once, if they are valid.
fn use_hotkeys(config: &config::Config, focus_machine: &mut FocusMachine, window: &mut Duration) {
    match config.hotkeys() {
        Ok(set) => {
            let hotkey = Hotkey::from_config(&set);
            *window = hotkey.double_tap_window();
            focus_machine.set_hotkeys(hotkey);
        }
        Err(err) => eprintln!("mickey: hotkeys not changed: {err}"),
    }
}

/// What the tray and `mickey status` show: where the keyboard is, whether
/// it controls this PC, which peers are online, and an update waiting.
fn status_of(focus: Option<usize>, here: bool, update: Option<String>, peers: &[Peer]) -> Status {
    Status {
        focus,
        here,
        peers: peers
            .iter()
            .map(|peer| (peer.name.clone(), peer.link.is_some()))
            .collect(),
        update,
    }
}

/// What the target passes on to the source.
pub enum TargetNews {
    /// A peer's keyboard and mouse came over to this PC, or left (PRD 0009).
    Controlled(bool),
    /// The peer at this address placed itself on this PC's desktop.
    Arranged {
        from: std::net::IpAddr,
        position: [i32; 2],
    },
    /// A peer's group map (PRD 0010), in its own coordinates, and the key
    /// of the peer that sent it.
    Group {
        members: Vec<group::Member>,
        by: [u8; 32],
    },
    /// A peer's `[hotkeys]` lines, saved there, and where they came from:
    /// they are offered to this PC (#349).
    Hotkeys {
        lines: String,
        from: std::net::IpAddr,
    },
    /// Who listens, as a peer shared it (PRD 0012).
    Audio(String),
    /// The Mickey version the peer at this address runs (PRD 0013).
    Version {
        from: std::net::IpAddr,
        version: String,
    },
}

/// What the rest of Mickey shares with the source. Each is optional:
/// Mickey works without any of them.
pub struct Services {
    /// What the target hears from the peers driving this PC.
    pub target_news: mpsc::Receiver<TargetNews>,
    /// The PCs heard on the network (PRD 0010).
    pub found: Option<discovery::Table>,
    /// The PCs this one trusts, shared with the target.
    pub trusted: trust::Shared,
    /// This PC as the group knows it: its name and the port it listens on.
    pub name: String,
    pub port: u16,
    /// The clipboard, and its history (PRD 0011).
    pub keeper: Option<Keeper>,
    pub history: Option<history::Shared>,
    /// A newer Mickey, found by the daily check or heard from a peer.
    pub updates: Option<update::Updates>,
}

/// Captures local input and sends it to the focused peer. Every peer gets
/// its own secure link; one that is offline is retried in the background
/// and never blocks the others.
pub fn run(
    configured: Vec<config::Peer>,
    hotkeys: Hotkey,
    touchpad_speed: i32,
    mut interface: config::Interface,
    services: Services,
) -> Result<(), String> {
    let Services {
        trusted,
        name: my_name,
        port: my_port,
        target_news,
        found,
        keeper,
        history,
        updates,
    } = services;
    // Two sources would both capture the keyboard and fight over the grab.
    if control::source_running() {
        return Err("another mickey source is already running".into());
    }
    let input = Input::open()?;
    // For text expanders, when asked for (see `input::TypingMirror`).
    #[cfg(target_os = "linux")]
    let mut mirror = crate::input::typing_mirror();
    let data_dir = identity::data_dir()?;
    let identity = Identity::load_or_create(&data_dir)?;
    let start = Instant::now();
    let (pong_sender, pongs) = mpsc::channel();
    let (back_sender, back) = mpsc::channel();

    let mut positions: Vec<Option<[i32; 2]>> =
        configured.iter().map(|peer| peer.position).collect();
    let mut peers: Vec<Peer> = configured
        .into_iter()
        .map(|peer| {
            let key = peer.key.as_deref().and_then(|hex| {
                let key = crate::hex::decode_key(hex);
                if key.is_none() {
                    eprintln!(
                        "mickey: {}'s key in config.toml is not 64 hex digits; ignored",
                        peer.name
                    );
                }
                key
            });
            Peer::new(peer.name, peer.address, key, start)
        })
        .collect();
    let shared = Arc::new(Shared { identity, trusted });
    // Every peer is connected on its own thread, asking to pair where
    // needed: one that hangs (frozen, or waiting for its owner to answer
    // a pair window) never holds up the others. Links arrive on `back`.
    for (index, peer) in peers.iter().enumerate() {
        connect_first(
            index,
            peer.name.clone(),
            peer.address,
            &shared,
            &back_sender,
        );
    }
    // The clipboard follows the keyboard (PRD 0011), over data links.
    let data_links = keeper.clone().map(DataLinks::start);
    // Who listens, to every peer as its data link opens (PRD 0012).
    let mut audio = config::Config::load()
        .ok()
        .flatten()
        .map(|config| config.audio)
        .unwrap_or_default();
    if let Some(data_links) = &data_links {
        data_links.share_audio(&audio.shared_lines());
    }
    // This PC's sound on its way to the listener, while it should be.
    let mut sending: Option<(usize, crate::audio::Sending)> = None;

    // One channel wakes the loop for input, a switcher pick or an order
    // from the control socket, so each is handled the moment it happens.
    let (wake, wakes) = mpsc::channel();
    input.start(wake.clone(), touchpad_speed)?;
    let mut switchers = Switchers::default();
    // Local motion between two questions about where the cursor is.
    let mut edge_check = edges::Throttle::new();
    // Hotkeys another PC shared while the focus was away, to take back home.
    let mut pending_hotkeys: Option<String> = None;
    // The switcher open on this PC for picking, to close it on a second press.
    // The status for bars and commands; without a runtime directory Mickey
    // still works, just without them.
    let control = match Control::start(wake.clone()) {
        Ok(control) => Some(control),
        Err(err) => {
            eprintln!("mickey: no control socket: {err}");
            None
        }
    };
    let mut published = None;
    {
        let wake = wake.clone();
        crate::threads::spawn("target news", move || {
            for news in target_news {
                let woken = match news {
                    TargetNews::Controlled(here) => Wake::Controlled(here),
                    TargetNews::Arranged { from, position } => Wake::Arranged(from, position),
                    TargetNews::Group { members, by } => Wake::Group(members, by),
                    TargetNews::Hotkeys { lines, from } => Wake::Hotkeys(lines, from),
                    TargetNews::Audio(lines) => Wake::Audio(lines),
                    TargetNews::Version { from, version } => Wake::Version(from, version),
                };
                if wake.send(woken).is_err() {
                    return;
                }
            }
        });
    }
    let mut in_control = InControl::default();
    // A hotkey being recorded: what was heard, where to answer, since when.
    let mut recording: Option<(Recording, Sender<String>, Instant)> = None;
    // The tray icon (PRD 0009); Mickey works the same without one.
    let (clicks, picked) = mpsc::channel();
    let first = status_of(None, true, None, &peers);
    let tray = Tray::start(first, clicks)
        .inspect_err(|err| eprintln!("mickey: {err}"))
        .ok();
    {
        let wake = wake.clone();
        crate::threads::spawn("tray clicks", move || {
            for click in picked {
                if wake.send(Wake::Tray(click)).is_err() {
                    return;
                }
            }
        });
    }
    let mut double_tap_window = hotkeys.double_tap_window();
    let mut focus_machine = FocusMachine::with_hotkeys(hotkeys);
    // Dropping a `Grab` gives its device back, on every way out of here,
    // including `?` and panics.
    let mut grabbed = None;
    let mut next_ping = start;
    let bench = std::env::var_os("MICKEY_BENCH").is_some();
    // Names never change; which peers are online is refreshed every turn.
    // The peers' names, apart: `act` holds `peers` while an order is obeyed.
    let mut names: Vec<String> = peers.iter().map(|peer| peer.name.clone()).collect();
    // PCs the group introduced that the person here was asked about, so
    // nobody is asked twice in a run, whatever the answer (#184).
    let mut asked: std::collections::HashSet<[u8; 32]> = std::collections::HashSet::new();
    // Which peers were online when the group map was last shared.
    let mut shared_with: Vec<bool> = Vec::new();
    // Kept up to date by `monitor_changes`, or on GNOME and KDE read again
    // while the portal is not up yet (below).
    let mut this_pc = monitors::this_pc().unwrap_or_else(|err| {
        eprintln!("mickey: {err}; `mickey peers` will not show this PC's monitors");
        Vec::new()
    });
    // Where the cursor is, asked on a thread of its own: on Hyprland the
    // answer took 2.7 ms at p99, and this loop must not wait for it (#187).
    let edge = edges::EdgeWatch::start(this_pc.clone(), crate::input::cursor_now);
    // The monitors again once a second: moving one in the display settings
    // used to need a restart, or the edges stayed where it was (#365). Not
    // on GNOME and KDE, whose edges are the portal's barriers.
    let monitor_changes = monitors::follows_changes()
        .then(|| monitors::watch(this_pc.clone(), PING_EVERY, monitors::this_pc));
    // GNOME and KDE: the InputCapture portal's barriers instead (PRD 0004,
    // phase 2), started once a peer is placed beside this PC.
    #[cfg(target_os = "linux")]
    let mut portal_edges: Option<crate::edges::portal::PortalEdges> = None;
    let mut online: Vec<bool> = Vec::with_capacity(peers.len());
    // PCs that were off when the owner saved a layout: told when they
    // connect (#288).
    let mut arrange_unsent: std::collections::HashSet<String> = std::collections::HashSet::new();
    loop {
        for (index, link) in back.try_iter() {
            // A peer is one PC, known by its key: another trusted PC that
            // took over its address gets nothing meant for it (#184).
            match peers[index].key {
                Some(key) if key != *link.peer_key() => {
                    println!(
                        "mickey: {} answered with another PC's key; refused. If it was reinstalled, remove it in the settings and connect again",
                        peers[index].name
                    );
                    peers[index].new_key = true;
                    continue;
                }
                Some(_) => {}
                None => save_key(&peers[index].name, link.peer_key()),
            }
            read_peer(index, &link, &pong_sender, start);
            if let Some(data_links) = &data_links {
                open_data_link(
                    index,
                    peers[index].address,
                    *link.peer_key(),
                    &shared,
                    data_links,
                );
            }
            peers[index].key = Some(*link.peer_key());
            peers[index].new_key = false;
            peers[index].link = Some(link);
            peers[index].heard = Instant::now();
            println!("mickey: {} is online", peers[index].name);
            // A layout saved while it was off reaches it now (#288).
            if arrange_unsent.remove(&peers[index].name)
                && let (Some([x, y]), Some(link)) = (positions[index], &mut peers[index].link)
                && link.send(&Message::Arrange { x: -x, y: -y }).is_ok()
            {
                println!(
                    "mickey: told {} the layout saved while it was off",
                    peers[index].name
                );
            }
        }
        // Where to put the cursor here, coming back from a peer's edge.
        let mut home_at = None;
        // Or onto another peer placed next to the focused one.
        let mut hop = None;
        // A peer refused the input: bring the focus home.
        let mut refused = false;
        // A card picked in the switcher shown on the focused peer.
        let mut remote_pick = None;
        // Any switcher answered, here or there: the copies can close.
        let mut picked = false;
        let peer_count = peers.len();
        for (index, news) in pongs.try_iter() {
            peers[index].heard = Instant::now();
            match news {
                News::Picked(card) => {
                    picked = true;
                    if focus_machine.focus() == Focus::Remote(index) {
                        remote_pick = Some(card);
                    }
                }
                News::Refused => {
                    if focus_machine.focus() == Focus::Remote(index) {
                        println!(
                            "mickey: {} refused the keyboard and mouse (a window running as administrator in front?); they are back here",
                            peers[index].name
                        );
                        refused = true;
                    }
                }
                News::Leave(x, y) => {
                    if focus_machine.focus() == Focus::Remote(index) {
                        match landing(&this_pc, &positions, &peers, index, (x, y), &input) {
                            Some(Landing::Home(here)) => home_at = Some(here),
                            Some(Landing::Peer(next, there)) => hop = Some((next, there)),
                            None => {}
                        }
                    }
                }
                News::RoundTrip(micros) => peers[index].latency.add_round_trip(micros),
                News::Monitors(monitors) => {
                    let sizes: Vec<String> = monitors
                        .iter()
                        .map(|m| format!("{}x{} at {},{}", m.width, m.height, m.x, m.y))
                        .collect();
                    println!(
                        "mickey: {} has {} monitor(s): {}",
                        peers[index].name,
                        monitors.len(),
                        sizes.join("; ")
                    );
                    peers[index].monitors = monitors;
                }
            }
        }
        if let Focus::WaitingForKeysUp(index) = focus_machine.focus()
            && !input.any_key_down()
        {
            grabbed = Some(input.grab()?);
            focus_machine.grabbed();
            println!("mickey: focus is on {}", peers[index].name);
        }

        // Publish only when the focus or a peer's state changed; comparing
        // first keeps the per-event path free of allocations.
        let focus = match focus_machine.focus() {
            Focus::Remote(peer) => Some(peer),
            Focus::Local | Focus::WaitingForKeysUp(_) => None,
        };
        if let Some(was) = in_control.focus_is(focus) {
            // Motion summed here before the focus moved is about the last
            // stay: kept, it pushed the cursor back through the edge it
            // came home by.
            edge_check = edges::Throttle::new();
            if let Some(data_links) = &data_links {
                data_links.moved(was, focus);
            }
            // Tell the peers the keyboard left and where it went.
            for (index, now_here) in [(was, false), (focus, true)] {
                if let Some(link) = index.and_then(|index| peers[index].link.as_mut()) {
                    let _ = link.send(&Message::Focus { here: now_here });
                }
            }
        }
        let here = in_control.here();
        let waiting_update = updates.as_ref().and_then(update::Updates::waiting);
        let changed = published.as_ref().is_none_or(|last: &Status| {
            last.focus != focus
                || last.here != here
                || last.update != waiting_update
                // A PC the group added makes the list longer.
                || last.peers.len() != peers.len()
                || last
                    .peers
                    .iter()
                    .zip(&peers)
                    .any(|((_, up), peer)| *up != peer.link.is_some())
        });
        if changed {
            let status = status_of(focus, here, waiting_update, &peers);
            if let Some(control) = &control {
                control.publish(status.clone());
            }
            if let Some(tray) = &tray {
                tray.show(&status);
            }
            published = Some(status);
        }

        if let Some(found) = monitor_changes
            .as_ref()
            .and_then(|changes| changes.try_iter().last())
        {
            println!("mickey: this PC's monitors changed; the edges follow them");
            edge.set_monitors(found.clone());
            this_pc = found;
        }

        let mut lost = Vec::new();
        let now = Instant::now();
        // Once a second: the edges that lead to a placed, online peer.
        #[cfg(target_os = "linux")]
        if now >= next_ping
            && matches!(
                crate::desktop::session(),
                crate::desktop::Session::Portal | crate::desktop::Session::Sway
            )
        {
            // The portal may come up after Mickey at login.
            if this_pc.is_empty()
                && let Ok(found) = monitors::this_pc()
            {
                this_pc = found;
            }
            let placed: Vec<(&[monitors::Monitor], [i32; 2])> = (0..peers.len())
                .filter(|&index| peers[index].link.is_some())
                .filter_map(|index| Some((peers[index].monitors.as_slice(), positions[index]?)))
                .collect();
            let lines = edges::barriers(&this_pc, &placed);
            match &portal_edges {
                Some(capture) => capture.set(lines),
                None if !lines.is_empty() => {
                    portal_edges = Some(crate::edges::portal::PortalEdges::start(
                        this_pc.clone(),
                        lines,
                    ));
                }
                None => {}
            }
        }
        if now >= next_ping {
            let ping = Message::Ping {
                id: latency::ping_id(start),
            };
            for (index, peer) in peers.iter_mut().enumerate() {
                let Some(link) = &mut peer.link else {
                    continue;
                };
                if peer.heard.elapsed() > SILENT_LIMIT {
                    println!("mickey: {} stopped answering", peer.name);
                    lost.push(index);
                } else if link.send(&ping).is_err() {
                    lost.push(index);
                }
            }
            let remote = matches!(focus_machine.focus(), Focus::Remote(_));
            next_ping = now
                + if bench && remote {
                    BENCH_PING_EVERY
                } else {
                    PING_EVERY
                };
        }

        // Sleep until something wakes us or the next ping is due; poll often
        // only while waiting for keys to go up, so an idle source wakes once
        // a second.
        let mut wait = next_ping.saturating_duration_since(now);
        if matches!(focus_machine.focus(), Focus::WaitingForKeysUp(_)) {
            wait = wait.min(KEYS_UP_POLL);
        }
        // Reuses one buffer: no allocation per input event.
        online.clear();
        online.extend(peers.iter().map(|peer| peer.link.is_some()));
        let target = audio_target(&audio, shared.identity.public(), &peers, &online);
        if target.map(|(_, to)| to) != sending.as_ref().map(|(_, sending)| sending.to()) {
            // Dropping the old one gives the speakers back.
            sending = None;
            if let (Some((index, to)), Some(data_links)) = (target, &data_links) {
                let data_links = data_links.clone();
                let give_key = move |hex: &str| data_links.give_audio_key(index, hex);
                match crate::audio::Sending::start(to, audio.mode, give_key) {
                    Ok(started) => sending = Some((index, started)),
                    Err(err) => eprintln!("mickey: {err}"),
                }
            }
        }
        let mut open = false;
        // The switcher shown on the focused peer instead (it is where the
        // owner is looking).
        let mut open_on = None;
        let left = focus_machine.focus();
        let mut act = |action| match action {
            Action::OpenSwitcher => open = true,
            Action::OpenSwitcherOn(peer) => open_on = Some(peer),
            Action::Send(index, event) => {
                if let Some(link) = &mut peers[index].link
                    && link.send(&Message::Input(event)).is_err()
                {
                    lost.push(index);
                }
                #[cfg(target_os = "linux")]
                if let Some(mirror) = &mut mirror {
                    mirror.typed(event);
                }
            }
            Action::Ungrab => {
                grabbed = None;
                if let Focus::Remote(index) = left {
                    report_latency(&peers[index]);
                }
            }
        };
        // A hop goes home first, as a pick in the switcher does, so the
        // focus machine moves from one peer to the other the usual way.
        if home_at.is_some() || hop.is_some() || refused {
            focus_machine.go_home(&mut act);
        }
        // 0 is this PC; `i + 1` is peer `i`; anything else is "none".
        match remote_pick.map(usize::from) {
            Some(0) => focus_machine.go_home(&mut act),
            Some(card)
                if card <= peer_count && focus_machine.focus() != Focus::Remote(card - 1) =>
            {
                focus_machine.go_home(&mut act);
                focus_machine.go_to(Some(card - 1), &online);
            }
            _ => {}
        }
        let mut asked_for_peers = None;
        let mut pushed = None;
        let mut quit = false;
        let mut placed = None;
        let mut heard_map = None;
        let mut accepted = None;
        let mut heard_hotkeys = None;
        let mut heard_audio = None;
        let mut heard_version = None;
        // A PC was forgotten from the settings' Remove.
        let mut forgotten = false;
        // A PC to connect to from the settings: handled below, like `join`.
        let mut connecting = None;
        let mut asked_to_arrange = None;
        match wakes.recv_timeout(wait) {
            // While recording a hotkey, keys are named, never acted on.
            Ok(Wake::Input(InputEvent::Key { code, pressed })) if recording.is_some() => {
                let answer = recording.as_mut().and_then(|(record, _, _)| {
                    match record.key(code, pressed, Instant::now()) {
                        Heard::Waiting => None,
                        Heard::Chord(text) => Some(format!("ok: {text}")),
                        Heard::DoubleTap(key) => Some(format!("ok: double {key}")),
                        Heard::Mistake(why) => Some(format!("error: {why}")),
                    }
                });
                if let Some(answer) = answer
                    && let Some((_, reply, _)) = recording.take()
                {
                    let _ = reply.send(answer);
                }
            }
            Ok(Wake::Control(Request {
                order: Order::Connect { address, name },
                reply,
            })) => connecting = Some((address, name, reply)),
            Ok(Wake::Control(request)) if request.order == Order::Record => {
                recording = Some((
                    Recording::new(double_tap_window),
                    request.reply,
                    Instant::now(),
                ));
            }
            Ok(Wake::Input(event)) => {
                focus_machine.handle(event, &online, &mut act);
                if let InputEvent::Motion { dx, dy } = event
                    && focus_machine.focus() == Focus::Local
                {
                    pushed = edge_check.feed((dx, dy));
                }
            }
            Ok(Wake::Controlled(here)) => in_control.peer_says(here),
            // Both handled below, once `act` no longer borrows the peers.
            Ok(Wake::Arranged(from, position)) => placed = Some((from, position)),
            Ok(Wake::Group(members, by)) => heard_map = Some((members, by)),
            Ok(Wake::Accepted(member, by)) => accepted = Some((member, by)),
            Ok(Wake::Hotkeys(lines, from)) => heard_hotkeys = Some((lines, from)),
            Ok(Wake::HotkeysAccepted(lines)) => pending_hotkeys = Some(lines),
            Ok(Wake::Audio(lines)) => heard_audio = Some(lines),
            Ok(Wake::Version(from, version)) => heard_version = Some((from, version)),
            Ok(Wake::Control(request)) if request.order == Order::Arranged => {
                asked_to_arrange = Some(request.reply);
            }
            Ok(Wake::Tray(tray::Click::Quit)) => quit = true,
            Ok(Wake::Control(request)) if request.order == Order::Quit => {
                let _ = request.reply.send("ok: Mickey is quitting".into());
                quit = true;
            }
            Ok(Wake::Control(control::Request {
                order: Order::Forget(fingerprint),
                reply,
            })) => {
                let forgot = shared.trusted.lock_anyway().forget(&fingerprint);
                forgotten = matches!(forgot, Ok(true));
                let _ = reply.send(match forgot {
                    Ok(true) => format!("ok: forgot {fingerprint}; it must pair again to connect"),
                    Ok(false) => {
                        format!("error: no trusted peer has the fingerprint {fingerprint}")
                    }
                    Err(err) => format!("error: {err}"),
                });
            }
            Ok(Wake::Tray(tray::Click::Settings)) => open_window("settings"),
            // Downloading takes a while: on its own thread. A failure opens
            // the settings, whose banner offers the update again.
            Ok(Wake::Tray(tray::Click::Update)) => {
                if let Some(version) = updates.as_ref().and_then(update::Updates::waiting) {
                    crate::threads::spawn("update", move || {
                        if let Err(err) = update::apply(&version) {
                            eprintln!("mickey: the update failed: {err}");
                            open_window("settings");
                        }
                    });
                }
            }
            Ok(Wake::Tray(tray::Click::History)) => open_window("history"),
            Ok(Wake::Tray(tray::Click::ListenHere)) => {
                match listen_here(shared.identity.public()) {
                    Ok(taken) => {
                        if let Some(data_links) = &data_links {
                            data_links.share_audio(&taken.shared_lines());
                        }
                        audio = taken;
                    }
                    Err(err) => {
                        eprintln!("mickey: cannot make this PC the one that listens: {err}")
                    }
                }
            }
            Ok(Wake::Tray(tray::Click::Home)) => focus_machine.go_home(&mut act),
            Ok(Wake::Tray(tray::Click::Go(peer))) => {
                // As from the switcher: only from here, only to a PC online.
                if focus_machine.focus() == Focus::Local && online[peer] {
                    focus_machine.go_to(Some(peer), &online);
                }
            }
            Ok(Wake::Pick(pick)) => {
                switchers.here_open = false;
                picked = true;
                match pick {
                    Ok(peer) => focus_machine.go_to(peer, &online),
                    Err(err) => {
                        // Without the window, behave like the M0 hotkey.
                        eprintln!("mickey: cannot show the switcher: {err}");
                        focus_machine.go_to(online.iter().position(|&up| up), &online);
                    }
                }
            }
            // Answered below, once `act` no longer borrows the peers.
            Ok(Wake::Control(request)) if request.order == Order::History => {
                let answer = match (&history, &keeper) {
                    (Some(history), _) => {
                        let history = history.lock_anyway();
                        history::lines(&history, std::time::SystemTime::now())
                    }
                    (None, Some(_)) => {
                        "error: the history is off on this PC; another clipboard manager keeps it ([clipboard] history in config.toml)".to_string()
                    }
                    (None, None) => "error: the clipboard is not shared on this PC".to_string(),
                };
                let _ = request.reply.send(answer);
            }
            Ok(Wake::Control(Request {
                order: Order::HistoryCopy(id),
                reply,
            })) => {
                let entry = history.as_ref().and_then(|history| {
                    let history = history.lock_anyway();
                    history.get(id).and_then(|entry| entry.content())
                });
                let answer = match (entry, &keeper) {
                    (Some(content), Some(keeper)) => {
                        keeper.write(content);
                        "ok: copied".to_string()
                    }
                    _ => format!("error: no history entry {id}"),
                };
                let _ = reply.send(answer);
            }
            Ok(Wake::Control(request)) if request.order == Order::Discover => {
                let _ = request.reply.send(found_text(found.as_ref()));
            }
            Ok(Wake::Control(request)) if request.order == Order::Peers => {
                asked_for_peers = Some(request.reply);
            }
            Ok(Wake::Control(request)) => {
                let answer = obey(request.order, &names, &online, &mut focus_machine, &mut act);
                let _ = request.reply.send(answer);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
        drop(act);
        // At once, not after a restart: every PC no longer trusted (the one
        // forgotten, and those it introduced) is dropped and listed as never
        // paired, so Connect pairs it again with a code instead of saying it
        // is already one of this PC's PCs.
        if forgotten {
            let trusted = shared.trusted.lock_anyway();
            for peer in peers.iter_mut() {
                if peer.key.is_some_and(|key| !trusted.is_trusted(&key)) {
                    peer.key = None;
                    peer.link = None;
                }
            }
        }
        if recording
            .as_ref()
            .is_some_and(|(_, _, since)| since.elapsed() > control::RECORD_TIMEOUT)
            && let Some((_, reply, _)) = recording.take()
        {
            let _ = reply.send("error: no keys were pressed".into());
        }
        if let Some((from, version)) = heard_version {
            // A peer on a newer Mickey: there is an update for this PC.
            if let Some(updates) = &updates {
                updates.heard(&version);
            }
            for peer in peers.iter_mut().filter(|peer| peer.address.ip() == from) {
                peer.version = Some(version.clone());
            }
        }
        if let Some((from, position)) = placed
            && let Some(index) = peers.iter().position(|peer| peer.address.ip() == from)
        {
            positions[index] = Some(position);
            save_position(&peers[index].name, position);
        }
        if let Some(reply) = asked_to_arrange {
            let _ = reply.send(arrange(&mut positions, &mut peers, &mut arrange_unsent));
            // The hotkeys saved with it: used here at once, and shared.
            if let Ok(Some(config)) = config::Config::load() {
                use_hotkeys(&config, &mut focus_machine, &mut double_tap_window);
                interface = config.interface.clone();
                if let Some(data_links) = &data_links {
                    if config.this_pc.share_hotkeys {
                        data_links.share_hotkeys(&config.hotkeys.to_lines());
                    }
                    data_links.share_audio(&config.audio.shared_lines());
                }
                audio = config.audio.clone();
            }
            // Everyone gets the new places, not only the PCs moved.
            shared_with.clear();
        }
        if let Some((address, name, reply)) = connecting {
            // The owner is connecting a PC now: it may answer back.
            trust::open_pairing();
            let known = peers.iter().position(|peer| peer.address == address);
            let paired = known.is_some_and(|i| peers[i].key.is_some());
            let answer = if let (false, Some(left)) = (paired, trust::refused_for(address.ip())) {
                format!(
                    "error: {address} was refused a moment ago; try again in {} s",
                    left.as_secs() + 1
                )
            } else if let Some(index) = known.filter(|&i| peers[i].key.is_none()) {
                // Listed but never paired (its code was refused, or it
                // never answered): pair again, under the name it has.
                let name = peers[index].name.clone();
                connect_first(index, name.clone(), address, &shared, &back_sender);
                format!("ok: pairing with {name}: check that both PCs show the same code")
            } else if known.is_some() {
                format!("error: {address} is already one of this PC's PCs")
            } else {
                match add_peer(&mut peers, &mut positions, &name, address, None) {
                    Ok(index) => {
                        let name = peers[index].name.clone();
                        names.push(name.clone());
                        // Pairs: the code shows on both PCs.
                        connect_first(index, name.clone(), address, &shared, &back_sender);
                        format!("ok: pairing with {name}: check that both PCs show the same code")
                    }
                    Err(err) => format!("error: {err}"),
                }
            };
            let _ = reply.send(answer);
        }
        if let Some((lines, from)) = heard_hotkeys {
            let who = peers
                .iter()
                .find(|peer| peer.address.ip() == from)
                .map_or_else(|| from.to_string(), |peer| peer.name.clone());
            offer_hotkeys(lines, &who, &wake);
        }
        if let Some(lines) = heard_audio {
            match take_audio(&lines) {
                Ok(taken) => audio = taken,
                Err(err) => eprintln!("mickey: cannot take who listens from another PC: {err}"),
            }
        }
        // Never while the keyboard is on another PC: new hotkeys could leave
        // no way home from there (#182). They wait until it is back.
        if focus_machine.focus() == Focus::Local
            && let Some(lines) = pending_hotkeys.take()
        {
            match take_hotkeys(&lines) {
                Ok(config) => {
                    use_hotkeys(&config, &mut focus_machine, &mut double_tap_window);
                    println!("mickey: took the hotkeys saved on another PC");
                }
                Err(err) => eprintln!("mickey: cannot take another PC's hotkeys: {err}"),
            }
        }
        if let Some((member, by)) = accepted
            && let Some(index) = join(member, &by, &shared, &mut peers, &mut positions)
        {
            names.push(peers[index].name.clone());
            reconnect(index, peers[index].address, &shared, &back_sender);
        }
        if let Some((members, by)) = heard_map {
            let joined = take_map(
                &members,
                by,
                &shared,
                &mut peers,
                &mut positions,
                &mut asked,
                &wake,
            );
            for index in joined {
                names.push(peers[index].name.clone());
                reconnect(index, peers[index].address, &shared, &back_sender);
            }
        }
        // Share the group map when the PCs online changed (one came, or
        // went), so a PC paired with this one reaches every other.
        let now_online: Vec<bool> = peers.iter().map(|peer| peer.link.is_some()).collect();
        if now_online != shared_with
            && let Some(data_links) = &data_links
        {
            let me = group::Member {
                key: *shared.identity.public(),
                name: my_name.clone(),
                address: SocketAddr::from(([0, 0, 0, 0], my_port)),
                position: Some([0, 0]),
            };
            let others: Vec<group::Member> = peers
                .iter()
                .zip(&positions)
                .filter_map(|(peer, position)| {
                    Some(group::Member {
                        key: peer.key?,
                        name: peer.name.clone(),
                        address: peer.address,
                        position: *position,
                    })
                })
                .collect();
            data_links.share_map(&group::encode(&me, &others));
            shared_with = now_online;
        }
        if quit {
            // Returning drops the grab: the devices go back to this PC.
            println!("mickey: quitting, as asked");
            return Ok(());
        }
        if let Some((x, y)) = home_at
            && let Err(err) = emit::place(x, y)
        {
            eprintln!("mickey: cannot place the cursor: {err}");
        }
        if let Some(delta) = pushed
            && edges_matter(&positions, &peers)
        {
            edge.push(delta);
        }
        // An answer about earlier motion: the cursor is still being pushed.
        // On GNOME and KDE, a barrier the cursor pushed into says it.
        #[cfg(target_os = "linux")]
        let hit = portal_edges
            .as_ref()
            .and_then(crate::edges::portal::PortalEdges::hit);
        #[cfg(not(target_os = "linux"))]
        let hit = None;
        // Taken every turn, so an answer is never kept for later.
        let off = edge.answer().flatten().or(hit);
        // An edge answer is about this PC's cursor, and only counts while
        // the focus is here: one arriving after a hop, about motion meant
        // for the peers, sent the cursor onto the wrong peer (moving fast
        // from one laptop to the other landed on the first one's edge).
        // So a hop wins, and a late answer is dropped.
        let crossed = hop.or_else(|| {
            off.filter(|_| focus_machine.focus() == Focus::Local)
                .and_then(|off| edge_crossed(&positions, &peers, off, &input))
        });
        if let Some((index, (x, y))) = crossed {
            focus_machine.go_to(Some(index), &online);
            if let Some(link) = &mut peers[index].link
                && link.send(&Message::Enter { x, y }).is_err()
            {
                lost.push(index);
            }
        }
        if let Some(reply) = asked_for_peers {
            let _ = reply.send(peers_text(&this_pc, &peers, &online));
        }
        if focus_machine.focus() == Focus::Local && left != Focus::Local {
            println!("mickey: focus is back here");
        }
        // The switcher shows on the PC in use, and a copy on every other
        // online PC, so it is seen wherever the owner looks (PRD 0007).
        // The hotkey again while switchers are open closes them all.
        let toggled_off = switchers.out && (open || open_on.is_some());
        let in_use = match open_on {
            _ if toggled_off => None,
            Some(peer) => Some(Some(peer)),
            None if open && !switchers.here_open => Some(None),
            None => None,
        };
        if let Some(in_use) = in_use {
            let cards = switcher_cards(
                in_use.map_or(0, |peer| peer + 1),
                &my_name,
                &peers,
                &positions,
                &interface,
            );
            switchers.show(
                in_use,
                &cards,
                &peers,
                data_links.as_ref(),
                &interface,
                &wake,
            );
        }
        // Closed once a pick is made, or when the focus moved some other way.
        let moved = in_use.is_none() && focus_machine.focus() != left;
        if switchers.out && (picked || toggled_off || moved) {
            switchers.close_all(data_links.as_ref());
        }

        lost.sort_unstable();
        lost.dedup();
        for index in lost {
            peers[index].link = None;
            println!("mickey: lost {}; retrying every 5 s", peers[index].name);
            focus_machine.peer_lost(index, |_ungrab| {
                grabbed = None;
                println!("mickey: focus is back here");
            });
            reconnect(index, peers[index].address, &shared, &back_sender);
        }
    }
    Err("every input device is gone".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio_peers() -> (config::Audio, [u8; 32], Vec<Peer>) {
        let peer = |n: u8| {
            Peer::new(
                format!("PC {n}"),
                "192.0.2.1:4242".parse().unwrap(),
                Some([n; 32]),
                Instant::now(),
            )
        };
        let audio = config::Audio {
            listener: Some(crate::hex::encode(&[2; 32])),
            ..config::Audio::default()
        };
        (audio, [9; 32], vec![peer(1), peer(2)])
    }

    #[test]
    fn the_sound_goes_to_the_listener_when_it_is_online() {
        let (audio, me, peers) = audio_peers();
        let to = audio_target(&audio, &me, &peers, &[true, true]);
        assert_eq!(to.map(|(index, _)| index), Some(1));
        assert_eq!(audio_target(&audio, &me, &peers, &[true, false]), None);
        // This PC listens: nothing to send.
        assert_eq!(audio_target(&audio, &[2; 32], &peers, &[true, true]), None);
    }

    /// #189-style measurement: audio_target runs on every input event.
    /// `cargo test --release audio_target_cost -- --ignored --nocapture`.
    #[test]
    #[ignore = "a measurement"]
    fn audio_target_cost() {
        let (audio, me, peers) = audio_peers();
        let started = Instant::now();
        for _ in 0..100_000 {
            std::hint::black_box(audio_target(&audio, &me, &peers, &[true, true]));
        }
        println!(
            "audio_target: {:.0} ns per call",
            started.elapsed().as_nanos() as f64 / 100_000.0
        );
    }

    #[test]
    fn the_last_change_wins() {
        let mut in_control = InControl::default();
        assert!(in_control.here());
        // This PC's keyboard goes to peer 0: not here; peer 0 is told.
        assert_eq!(in_control.focus_is(Some(0)), Some(None));
        assert!(!in_control.here());
        // Another peer's keyboard comes over: here again.
        in_control.peer_says(true);
        assert!(in_control.here());
        // No move of our own keyboard: nothing to tell, still here.
        assert_eq!(in_control.focus_is(Some(0)), None);
        assert!(in_control.here());
        // Our keyboard comes home: here, and peer 0 is told it left.
        assert_eq!(in_control.focus_is(None), Some(Some(0)));
        assert!(in_control.here());
        in_control.peer_says(false);
        assert!(!in_control.here());
    }
}
