//! The target: this PC driven by a source's keyboard and mouse.

mod data;
mod slots;

use data::serve_clipboard;
use slots::{SharedSlots, Slot};

use crate::input::event::InputEvent;
use crate::threads::LockAnyway;
use crate::{
    clipboard, input::emit, monitors, net::discovery, net::identity, net::protocol, net::secure,
    net::transport, net::trust, source, window,
};
use mickey_config as config;
use std::net::SocketAddr;

/// Serves every source that connects, each on its own thread (at most
/// `MAX_CONNECTIONS` at once), forever, injecting what they send.
/// `target_news` hears whether a source's keyboard and mouse control this PC;
/// `found`, when discovery runs, names a PC asking to pair; `keeper`, when
/// the clipboard is reachable, lets paired PCs read and write it.
pub fn target(
    listen: SocketAddr,
    trusted: trust::Shared,
    target_news: std::sync::mpsc::Sender<source::TargetNews>,
    found: Option<discovery::Table>,
    keeper: Option<clipboard::Keeper>,
) -> Result<(), String> {
    use std::io::ErrorKind;
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    let data_dir = identity::data_dir()?;
    let identity = Arc::new(identity::Identity::load_or_create(&data_dir)?);
    let sources = Sources::default();
    let grants = clipboard::sync::Grants::default();
    // One virtual keyboard and mouse for every source that connects.
    let emitter = Arc::new(Mutex::new(emit::Emitter::new()?));
    let listener = TcpListener::bind(listen).map_err(|err| match err.kind() {
        ErrorKind::AddrInUse => format!("{listen} is already in use; is another Mickey running?"),
        ErrorKind::AddrNotAvailable => format!("{listen} is not an address of this machine"),
        _ => format!("cannot listen on {listen}: {err}"),
    })?;
    println!("mickey: listening on {listen}");
    if let Some(keeper) = keeper {
        serve_clipboard(
            listen,
            &identity,
            &trusted,
            keeper,
            &target_news,
            &sources,
            &grants,
        );
    }
    let input_slots = SharedSlots::default();
    // Every source learns when this PC's monitors move (#365).
    let monitors = monitors::keep_latest(std::time::Duration::from_secs(1));
    loop {
        let (stream, peer) = match listener.accept() {
            Ok(connection) => connection,
            Err(err) => {
                eprintln!("mickey: accept failed: {err}");
                continue;
            }
        };
        // Each source on its own thread: a PC asking to pair, its window
        // still open, must not hold up a paired PC that reconnects.
        let (identity, trusted, emitter) = (identity.clone(), trusted.clone(), emitter.clone());
        let around = Around {
            target_news: target_news.clone(),
            found: found.clone(),
            sources: sources.clone(),
            grants: grants.clone(),
            monitors: monitors.clone(),
        };
        let Some(slot) = Slot::take(&input_slots, peer.ip()) else {
            continue;
        };
        crate::threads::try_spawn("serve source", move || {
            let _slot = slot;
            serve_source(stream, peer, &identity, &trusted, &emitter, &around);
        });
    }
}

/// The sources driving this PC, by address: their input links, for what
/// goes back to them outside `serve` (a pick in the switcher shown here).
pub(super) type Sources =
    std::sync::Arc<std::sync::Mutex<std::collections::HashMap<std::net::IpAddr, secure::Link>>>;

/// What every source's thread on the target shares.
pub(super) struct Around {
    target_news: std::sync::mpsc::Sender<source::TargetNews>,
    found: Option<discovery::Table>,
    sources: Sources,
    /// When each source may read the clipboard (#185).
    grants: clipboard::sync::Grants,
    /// This PC's monitors as last read; `None` where they do not follow a
    /// change (GNOME and KDE).
    monitors: Option<monitors::Latest>,
}

/// The switcher window open on this PC for a source, if any, so a later
/// "close" from the source can close it.
pub(super) type Shown = window::Slot;

/// Runs `mickey-ui switcher` with the cards a source sent (PRD 0007), in
/// this PC's own corner: the first line is the focused card, then one card
/// per line. A `mirror` only shows; otherwise this is the PC in use, and
/// the card picked goes back to the source.
pub(super) fn show_switcher_here(
    text: &str,
    from: std::net::IpAddr,
    sources: &Sources,
    shown: &Shown,
    mirror: bool,
) {
    let mut lines = text.lines();
    let Some(focused) = lines.next() else {
        return;
    };
    let interface = config::Config::load()
        .ok()
        .flatten()
        .map_or_else(config::Interface::default, |config| config.interface);
    // This PC's own corner and direction; the order of the cards is the
    // source's.
    let cards: Vec<String> = std::iter::once(focused)
        .chain(lines)
        .map(str::to_string)
        .collect();
    let args = window::switcher_args(&cards, &interface, mirror);
    if mirror {
        // A copy only shows: nobody waits for an answer.
        let slot = shown.clone();
        crate::threads::spawn("switcher pick", move || window::run(&args, &slot));
        return;
    }
    // 255: closed without a pick.
    let card = match window::run(&args, shown) {
        window::Outcome::Answered(text) => text.parse::<u8>().unwrap_or(u8::MAX),
        window::Outcome::Closed | window::Outcome::Failed(_) => u8::MAX,
    };
    let mut sources = sources.lock_anyway();
    if let Some(link) = sources.get_mut(&from) {
        let _ = link.send(&protocol::Message::Picked { card });
    }
}

/// Pairs with one source if needed, then plays what it sends until it
/// leaves.
pub(super) fn serve_source(
    stream: std::net::TcpStream,
    peer: SocketAddr,
    identity: &identity::Identity,
    trusted: &std::sync::Mutex<trust::TrustedPeers>,
    emitter: &std::sync::Mutex<emit::Emitter>,
    around: &Around,
) {
    let Around {
        target_news,
        found,
        sources,
        grants,
        monitors: latest,
    } = around;
    let found = found.as_ref();
    let mut link = match transport::accept(stream, identity) {
        Ok(link) => link,
        Err(err) => {
            eprintln!("mickey: {peer} failed the secure handshake: {err}");
            return;
        }
    };
    let peer_key = *link.peer_key();
    let lock = || trusted.lock_anyway();
    // The list is not held while the person decides.
    if !lock().is_trusted(&peer_key) {
        if !trust::pairing_open(trusted) {
            println!(
                "mickey: {peer} asked to pair while pairing is closed; refused (open it in the settings' PCs tab, or `mickey pairing`)"
            );
            return;
        }
        let who = found
            .and_then(|table| table.name_of(&peer_key))
            .unwrap_or_else(|| peer.ip().to_string());
        match trust::ask_once(&peer_key, peer, &link.pairing_code(), &who) {
            Ok(true) => {
                if let Err(err) = lock().add(peer_key, &peer.to_string()) {
                    eprintln!("mickey: {err}");
                    return;
                }
                println!("mickey: paired with {who}");
            }
            Ok(false) => {
                println!("mickey: refused {peer}");
                return;
            }
            Err(err) => {
                eprintln!("mickey: {err}");
                return;
            }
        }
    }
    println!("mickey: source {peer} connected");
    // Kept so a switcher shown here can send the pick back to this source.
    if let Ok(clone) = link.try_clone() {
        sources.lock_anyway().insert(peer.ip(), clone);
    }
    let mut reported = false;
    // Asked per connection: monitors may have changed since the last one.
    let monitors = monitors::this_pc().unwrap_or_else(|err| {
        eprintln!("mickey: {err}; the source will not see this PC's monitors");
        Vec::new()
    });
    let output = |effect| {
        let mut emitter = emitter.lock_anyway();
        match effect {
            transport::Effect::Input(event) => {
                if let InputEvent::Key { pressed: true, .. }
                | InputEvent::Button { pressed: true, .. } = event
                {
                    clipboard::sync::pressed(grants, peer.ip());
                }
                emitter.emit(event)
            }
            transport::Effect::Place { x, y } => emitter.place(x, y),
            // Nobody listens when this PC only waits to be controlled.
            transport::Effect::Controlled(here) => {
                clipboard::sync::focus_changed(grants, peer.ip(), here);
                // The keyboard and mouse went elsewhere: the cursor too.
                if here {
                    emit::unpark();
                } else {
                    emit::park();
                }
                let _ = target_news.send(source::TargetNews::Controlled(here));
                Ok(())
            }
            transport::Effect::Arranged(position) => {
                let from = peer.ip();
                let _ = target_news.send(source::TargetNews::Arranged { from, position });
                Ok(())
            }
        }
    };
    // Newer monitors than the last sent, for `serve` to pass on.
    let mut sent = monitors.clone();
    let moved = || {
        let now = latest.as_ref()?.lock_anyway().clone();
        if now.is_empty() || now == sent {
            return None;
        }
        sent = now.clone();
        Some(now)
    };
    let result = transport::serve(
        &mut link,
        &monitors,
        moved,
        |effect| match output(effect) {
            Ok(()) => true,
            Err(err) => {
                // omakvm: the X server went away (a kiosk restarting X with
                // a new cookie). This connection never works again, and the
                // cursor is no longer seen, so no edge leads home: end, and
                // let the service start again with the new cookie.
                if x_is_gone(&err) {
                    eprintln!("mickey: the X server went away ({err}); ending so the service starts again");
                    std::process::exit(1);
                }
                if !reported {
                    eprintln!("mickey: input injection failed: {err} (not reported again)");
                    reported = true;
                }
                false
            }
        },
        emit::cursor,
        || lock().is_trusted(&peer_key),
    );
    sources.lock_anyway().remove(&peer.ip());
    match result {
        Ok(()) => println!("mickey: source {peer} disconnected"),
        Err(err) => eprintln!("mickey: source {peer} dropped: {err}"),
    }
}

/// Whether an injection error says the X connection is gone for good.
fn x_is_gone(err: &std::io::Error) -> bool {
    std::env::var_os("DISPLAY").is_some()
        && std::env::var_os("WAYLAND_DISPLAY").is_none()
        && (err.kind() == std::io::ErrorKind::BrokenPipe
            || err.kind() == std::io::ErrorKind::ConnectionReset
            || err.to_string().contains("Broken pipe"))
}
