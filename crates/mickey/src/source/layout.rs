//! Where each peer sits around this PC's desktop: the edges the cursor
//! crosses, and the layout saved in the settings window.

use super::Peer;
use crate::edges;
use crate::input::Input;
use crate::monitors::Monitor;
use crate::net::protocol::Message;
use mickey_config as config;

/// Whether a placed peer is online: only then is the edge worth checking.
pub(super) fn edges_matter(positions: &[Option<[i32; 2]>], peers: &[Peer]) -> bool {
    (0..peers.len()).any(|index| positions[index].is_some() && peers[index].link.is_some())
}

/// The peer, and its point, that the cursor pushed onto at `off` (a pixel
/// just off this desktop, from `EdgeWatch`). Never while a mouse button is
/// down: dragging a window to the edge must not throw it to another PC.
pub(super) fn edge_crossed(
    positions: &[Option<[i32; 2]>],
    peers: &[Peer],
    off: (i32, i32),
    input: &Input,
) -> Option<(usize, (i32, i32))> {
    let placed = |index: usize| positions[index].filter(|_| peers[index].link.is_some());
    let (index, there) = (0..peers.len()).find_map(|index| {
        let there = edges::onto(&peers[index].monitors, placed(index)?, off)?;
        Some((index, there))
    })?;
    (!input.button_down()).then_some((index, there))
}

/// Where the cursor goes after it pushed off a peer's desktop.
pub(super) enum Landing {
    /// Onto this PC's desktop, at this point.
    Home((i32, i32)),
    /// Onto another placed peer, at this point of its desktop.
    Peer(usize, (i32, i32)),
}

/// Where the cursor lands after pushing off peer `from` at `off` (a pixel
/// just off its desktop, in its coordinates, from its `Leave`): this PC
/// first, then any other placed peer whose desktop touches there. Without
/// the second, two peers side by side were reached only through this PC.
pub(super) fn landing(
    this_pc: &[Monitor],
    positions: &[Option<[i32; 2]>],
    peers: &[Peer],
    from: usize,
    off: (i32, i32),
    input: &Input,
) -> Option<Landing> {
    let position = positions[from]?;
    if let Some(here) = edges::back(this_pc, position, off) {
        return Some(Landing::Home(here));
    }
    let here = edges::in_layout(position, off);
    if let Some((index, there)) = edge_crossed(positions, peers, here, input) {
        return Some(Landing::Peer(index, there));
    }
    // Part of that edge sticks out past this PC's monitors: home anyway,
    // to the nearest point along the side it pushed through.
    edges::back_near(this_pc, &peers[from].monitors, position, off).map(Landing::Home)
}

/// After the owner placed PCs in the settings window: takes the positions
/// saved in config.toml and tells every placed peer online where this PC
/// sits on its desktop (the mirror: if it is at `x, y` here, this PC is at
/// `-x, -y` there), so both PCs agree. The PCs offline go into `unsent`,
/// told as soon as they connect (#288): a layout saved while a PC was off
/// must reach it.
pub(super) fn arrange(
    positions: &mut [Option<[i32; 2]>],
    peers: &mut [Peer],
    unsent: &mut std::collections::HashSet<String>,
) -> String {
    let config = match config::Config::load() {
        Ok(Some(config)) => config,
        Ok(None) => return "error: no config.toml".into(),
        Err(err) => return format!("error: {err}"),
    };
    let mut told = 0;
    for (index, peer) in peers.iter_mut().enumerate() {
        let saved = config.peers.iter().find(|saved| saved.name == peer.name);
        let Some([x, y]) = saved.and_then(|saved| saved.position) else {
            continue;
        };
        positions[index] = Some([x, y]);
        if let Some(link) = &mut peer.link
            && link.send(&Message::Arrange { x: -x, y: -y }).is_ok()
        {
            told += 1;
            unsent.remove(&peer.name);
        } else {
            unsent.insert(peer.name.clone());
        }
    }
    let later = if unsent.is_empty() {
        String::new()
    } else {
        format!("; {} more once online", unsent.len())
    };
    format!("ok: told {told} PC(s) about the layout{later}")
}

/// Writes where a peer placed itself into config.toml, so the settings
/// window and the next start see it too.
pub(super) fn save_position(name: &str, position: [i32; 2]) {
    let saved = config::path().and_then(|path| {
        let text = std::fs::read_to_string(&path).map_err(|err| err.to_string())?;
        let text = config::set_position(&text, name, position)?;
        config::write(&path, &text)
    });
    match saved {
        Ok(()) => println!("mickey: {name} placed itself on this desktop at {position:?}"),
        Err(err) => eprintln!("mickey: cannot save where {name} sits: {err}"),
    }
}
