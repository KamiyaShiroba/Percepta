//! Owns the connection on its own thread so the UI never waits on the network.

use crate::i18n::tr;
use std::collections::VecDeque;
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::thread;
use std::time::{Duration, Instant};

use super::auth::Pairing;
use super::client::{Client, CspSlots, Error};
use super::{Rgb, Slot};
use crate::platform::{pressing_outside_percepta, ForegroundWatch};

/// Polling rate right after the colour changed, to follow a drag on CSP's wheel.
const ACTIVE_POLL: Duration = Duration::from_millis(16);
const IDLE_POLL: Duration = Duration::from_millis(50);
/// Polling rate once CSP's colour has not changed for `SETTLED_AFTER`.
const SETTLED_POLL: Duration = Duration::from_millis(100);
const SETTLED_AFTER: Duration = Duration::from_secs(10);
/// Polling rate while pressing on CSP without the colour changing, which is mostly drawing: every
/// read is work for CSP in the middle of a stroke.
const PRESSED_POLL: Duration = Duration::from_millis(200);
/// A press with a change this recent is more likely a drag on CSP's wheel, to keep following.
const PRESSED_CHANGE_WINDOW: Duration = Duration::from_millis(200);
const BACKGROUND_POLL: Duration = Duration::from_secs(2);
const ACTIVE_WINDOW: Duration = Duration::from_millis(1500);
const FOREGROUND_CHECK_INTERVAL: Duration = Duration::from_millis(250);
const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(1);
const STATS_INTERVAL: Duration = Duration::from_secs(1);
const RECONNECT_FIRST_WAIT: Duration = Duration::from_secs(1);
const RECONNECT_MAX_WAIT: Duration = Duration::from_secs(15);

pub enum Command {
    /// Writes the active colour slot, or the last one if transparent is selected.
    SetRgb(Rgb),
    SelectSlot(Slot),
    Disconnect,
}

pub enum Event {
    Connected { generation: String, saved: Pairing },
    /// The saved password no longer works, so a fresh QR is needed.
    SessionInvalid,
    /// A connection with a QR password failed; nothing will retry it.
    Failed(String),
    /// The user asked to disconnect.
    Disconnected,
    /// A saved login could not connect, or a connection dropped; this keeps trying.
    Reconnecting(String),
    /// CSP's slots changed. Unchanged polls emit nothing, so the UI stays idle.
    Slots(CspSlots),
    /// Comes every second but matters only to the settings window, which looks by itself, so
    /// this alone doesn't wake the UI and have every window drawn while the user is painting.
    Stats { reads_per_second: u32, read_average: Duration, read_max: Duration },
    Sent { rgb: Rgb, accepted: bool, round_trip: Duration },
    Blocked(bool),
    /// Whether the user is working in CSP.
    Foreground(bool),
    Log(String),
}

/// A write waiting for CSP to accept setters.
enum Write {
    Rgb(Rgb),
    Slot(Slot),
}

pub struct Worker {
    commands: Sender<Command>,
}

impl Worker {
    pub fn spawn(
        pairing: Pairing,
        reconnect: bool,
        events: Sender<Event>,
        wake_ui: impl Fn() + Send + 'static,
    ) -> Self {
        let (commands, inbox) = mpsc::channel();
        thread::spawn(move || {
            let emit = |event| {
                let wake = !matches!(event, Event::Stats { .. });
                let _ = events.send(event);
                if wake {
                    wake_ui();
                }
            };
            keep_connected(pairing, reconnect, &inbox, &emit);
        });
        Self { commands }
    }

    pub fn send(&self, command: Command) {
        let _ = self.commands.send(command);
    }
}

/// Connects, then restores the connection whenever it drops, until the user disconnects
/// or CSP voids the password. Edits made meanwhile wait in `writes` for the next login.
/// A saved login keeps trying from the start, as CSP may simply not be open yet; a QR
/// password that fails is reported at once, since it is already out of date.
fn keep_connected(
    mut pairing: Pairing,
    mut reconnect: bool,
    inbox: &Receiver<Command>,
    emit: &impl Fn(Event),
) {
    let mut writes = VecDeque::new();
    let mut established = reconnect;
    let mut wait = RECONNECT_FIRST_WAIT;
    loop {
        let reason = match Client::connect(&pairing, reconnect) {
            Ok((client, saved)) => {
                established = true;
                wait = RECONNECT_FIRST_WAIT;
                // Only the first login may use a QR password, which works just once.
                reconnect = true;
                pairing = saved.clone();
                emit(Event::Connected { generation: client.server_generation.clone(), saved });
                match run(client, &mut writes, inbox, emit) {
                    Ok(()) => return emit(Event::Disconnected),
                    Err(e) => e.to_string(),
                }
            }
            Err(Error::PasswordMismatch) => return emit(Event::SessionInvalid),
            Err(e) if !established => return emit(Event::Failed(e.to_string())),
            Err(e) => e.to_string(),
        };
        emit(Event::Reconnecting(reason));
        if !wait_before_retry(wait, &mut writes, inbox) {
            return emit(Event::Disconnected);
        }
        wait = (wait * 2).min(RECONNECT_MAX_WAIT);
    }
}

/// Returns false if the user asked to disconnect during the wait.
fn wait_before_retry(wait: Duration, writes: &mut VecDeque<Write>, inbox: &Receiver<Command>) -> bool {
    let deadline = Instant::now() + wait;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match inbox.recv_timeout(remaining) {
            Ok(Command::SetRgb(rgb)) => enqueue(writes, Write::Rgb(rgb)),
            Ok(Command::SelectSlot(slot)) => enqueue(writes, Write::Slot(slot)),
            Ok(Command::Disconnect) | Err(RecvTimeoutError::Disconnected) => return false,
            Err(RecvTimeoutError::Timeout) => return true,
        }
    }
}

/// A recent change wins over the foreground state: the sliders can write to CSP
/// while another app is in front, and that write should still read back quickly.
fn poll_interval(since_change: Duration, csp_in_front: bool, pressing: bool) -> Duration {
    if csp_in_front && pressing && since_change >= PRESSED_CHANGE_WINDOW {
        PRESSED_POLL
    } else if since_change < ACTIVE_WINDOW {
        ACTIVE_POLL
    } else if !csp_in_front {
        BACKGROUND_POLL
    } else if since_change < SETTLED_AFTER {
        IDLE_POLL
    } else {
        SETTLED_POLL
    }
}

/// Adds a write to the queue. Only the newest colour matters, as slider drags outrun
/// the network, but it must still land before any slot change queued after it.
fn enqueue(writes: &mut VecDeque<Write>, write: Write) {
    if let (Write::Rgb(_), Some(Write::Rgb(_))) = (&write, writes.back()) {
        writes.pop_back();
    }
    writes.push_back(write);
}

/// Returns whether the write was carried out; if not, it should stay queued.
fn apply_write(
    client: &mut Client,
    write: &Write,
    known: &mut Option<CspSlots>,
    emit: &impl Fn(Event),
) -> Result<bool, Error> {
    // Selecting a slot writes its value back, so that value has to be fresh.
    if known.is_none() || matches!(write, Write::Slot(_)) {
        let Some(fresh) = client.read_slots()? else {
            client.wake()?;
            return Ok(false);
        };
        *known = Some(known.map_or(fresh, |old| fresh.merged_over(&old)));
    }
    let Some(slots) = known.as_mut() else {
        return Ok(false);
    };
    match *write {
        Write::Rgb(rgb) => {
            let start = Instant::now();
            let accepted = if slots.active == Slot::Transparent {
                let target = slots.previous;
                let accepted = client.set_rgb_in_slot(slots.space, rgb, target)?;
                if accepted {
                    slots.active = target;
                }
                accepted
            } else {
                client.set_rgb(rgb)?
            };
            emit(Event::Sent { rgb, accepted, round_trip: start.elapsed() });
        }
        Write::Slot(slot) => {
            if client.select_slot(slots, slot)? {
                slots.active = slot;
            } else {
                emit(Event::Log(tr!("log.slot_failed").into()));
            }
        }
    }
    Ok(true)
}

fn run(
    mut client: Client,
    writes: &mut VecDeque<Write>,
    inbox: &Receiver<Command>,
    emit: &impl Fn(Event),
) -> Result<(), Error> {
    let mut known: Option<CspSlots> = None;
    let mut last_emitted: Option<CspSlots> = None;
    let mut can_set = true;
    let mut last_can_set = true;
    let mut was_empty = false;
    let mut last_change = Instant::now();
    let mut next_poll = Instant::now();
    let mut next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;

    let mut foreground = ForegroundWatch::default();
    let mut csp_in_front = foreground.csp_in_front();
    emit(Event::Foreground(csp_in_front));
    let mut next_foreground_check = Instant::now() + FOREGROUND_CHECK_INTERVAL;

    let mut stats_start = Instant::now();
    let (mut reads, mut read_total, mut read_max) = (0u32, Duration::ZERO, Duration::ZERO);

    loop {
        let timeout = next_poll
            .min(next_foreground_check)
            .saturating_duration_since(Instant::now());
        match inbox.recv_timeout(timeout) {
            Ok(Command::SetRgb(rgb)) => enqueue(writes, Write::Rgb(rgb)),
            Ok(Command::SelectSlot(slot)) => enqueue(writes, Write::Slot(slot)),
            Ok(Command::Disconnect) | Err(RecvTimeoutError::Disconnected) => return Ok(()),
            Err(RecvTimeoutError::Timeout) => {}
        }
        for command in inbox.try_iter() {
            match command {
                Command::SetRgb(rgb) => enqueue(writes, Write::Rgb(rgb)),
                Command::SelectSlot(slot) => enqueue(writes, Write::Slot(slot)),
                Command::Disconnect => return Ok(()),
            }
        }

        if Instant::now() >= next_foreground_check {
            next_foreground_check = Instant::now() + FOREGROUND_CHECK_INTERVAL;
            let in_front = foreground.csp_in_front();
            if in_front != csp_in_front {
                csp_in_front = in_front;
                emit(Event::Foreground(in_front));
                if in_front {
                    // Catch up at once rather than after the slow background wait.
                    next_poll = Instant::now();
                }
            }
        }

        while can_set {
            let Some(write) = writes.front() else { break };
            if !apply_write(&mut client, write, &mut known, emit)? {
                break;
            }
            writes.pop_front();
            last_change = Instant::now();
            next_poll = next_poll.min(last_change + ACTIVE_POLL);
        }

        if Instant::now() >= next_poll {
            let start = Instant::now();
            let reply = client.read_slots()?;
            let round_trip = start.elapsed();
            reads += 1;
            read_total += round_trip;
            read_max = read_max.max(round_trip);

            match reply {
                Some(fresh) => {
                    was_empty = false;
                    let slots = known.map_or(fresh, |old| fresh.merged_over(&old));
                    known = Some(slots);
                    can_set = slots.can_set;
                    if can_set != last_can_set {
                        emit(Event::Blocked(!can_set));
                        last_can_set = can_set;
                    }
                    if last_emitted != Some(slots) {
                        last_emitted = Some(slots);
                        last_change = Instant::now();
                        emit(Event::Slots(slots));
                    }
                }
                None => {
                    if !was_empty {
                        emit(Event::Log(tr!("log.empty_state").into()));
                    }
                    was_empty = true;
                    client.wake()?;
                }
            }
            let pressing = pressing_outside_percepta();
            next_poll = Instant::now() + poll_interval(last_change.elapsed(), csp_in_front, pressing);
        }

        if Instant::now() >= next_heartbeat {
            client.heartbeat()?;
            next_heartbeat = Instant::now() + HEARTBEAT_INTERVAL;
        }

        if stats_start.elapsed() >= STATS_INTERVAL && reads > 0 {
            emit(Event::Stats {
                reads_per_second: (reads as f32 / stats_start.elapsed().as_secs_f32()).round() as u32,
                read_average: read_total / reads,
                read_max,
            });
            stats_start = Instant::now();
            (reads, read_total, read_max) = (0, Duration::ZERO, Duration::ZERO);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::csp::frame::{self, TYPE_COMMAND, TYPE_SUCCESS};
    use std::io::{Read, Write as _};
    use std::net::{TcpListener, TcpStream};

    /// Answers one connection until `Authenticate` and `polls` polls have been seen.
    fn serve_login(stream: &mut TcpStream, polls: usize) {
        let (mut buf, mut chunk, mut seen) = (Vec::new(), [0u8; 4096], 0);
        loop {
            let n = stream.read(&mut chunk).unwrap_or(0);
            if n == 0 {
                return;
            }
            buf.extend_from_slice(&chunk[..n]);
            while let Some(f) = frame::next_frame(&mut buf) {
                if f.kind != TYPE_COMMAND {
                    continue;
                }
                let reply = match f.command.as_str() {
                    "Authenticate" => r#"{"AuthErrorReason":"Unknown"}"#,
                    "SyncColorCircleUIState" => {
                        seen += 1;
                        r#"{"ColorSelectionModel":"HLS","CurrentColorIndex":0}"#
                    }
                    _ => "",
                };
                let answer = frame::encode(TYPE_SUCCESS, &f.command, f.serial, reply);
                if stream.write_all(&answer).is_err() {
                    return;
                }
                if seen >= polls {
                    return;
                }
            }
        }
    }

    fn start_worker(port: u16, reconnect: bool) -> (Worker, Receiver<Event>) {
        let pairing = Pairing {
            ips: vec![],
            port,
            password: "saved".into(),
            generation: "G".into(),
        };
        let (events_tx, events) = mpsc::channel();
        (Worker::spawn(pairing, reconnect, events_tx, || {}), events)
    }

    fn wait_for(events: &Receiver<Event>, wanted: fn(&Event) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let event = events.recv_timeout(left).expect("the event never came");
            if wanted(&event) {
                return;
            }
        }
    }

    #[test]
    fn reconnects_when_the_connection_drops() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut first, _) = listener.accept().unwrap();
            serve_login(&mut first, 1);
            drop(first);
            let (mut second, _) = listener.accept().unwrap();
            serve_login(&mut second, usize::MAX);
        });

        let (worker, events) = start_worker(port, false);
        wait_for(&events, |e| matches!(e, Event::Connected { .. }));
        wait_for(&events, |e| matches!(e, Event::Reconnecting(_)));
        wait_for(&events, |e| matches!(e, Event::Connected { .. }));
        worker.send(Command::Disconnect);
        wait_for(&events, |e| matches!(e, Event::Disconnected));
    }

    #[test]
    fn disconnecting_on_request_does_not_reconnect() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            serve_login(&mut stream, usize::MAX);
        });

        let (worker, events) = start_worker(port, false);
        wait_for(&events, |e| matches!(e, Event::Connected { .. }));
        worker.send(Command::Disconnect);
        wait_for(&events, |e| {
            assert!(!matches!(e, Event::Reconnecting(_)), "reconnected after a disconnect");
            matches!(e, Event::Disconnected)
        });
    }

    #[test]
    fn a_qr_connection_that_fails_is_not_retried() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (_worker, events) = start_worker(port, false);
        wait_for(&events, |e| matches!(e, Event::Failed(_)));
    }

    #[test]
    fn a_saved_login_waits_for_csp_to_open() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let (worker, events) = start_worker(port, true);
        wait_for(&events, |e| matches!(e, Event::Reconnecting(_)));

        let listener = TcpListener::bind(("127.0.0.1", port)).unwrap();
        std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            serve_login(&mut stream, usize::MAX);
        });
        wait_for(&events, |e| matches!(e, Event::Connected { .. }));
        worker.send(Command::Disconnect);
        wait_for(&events, |e| matches!(e, Event::Disconnected));
    }

    #[test]
    fn polls_by_recent_change_then_foreground() {
        assert_eq!(poll_interval(Duration::ZERO, true, false), ACTIVE_POLL);
        assert_eq!(poll_interval(Duration::ZERO, false, false), ACTIVE_POLL);
        assert_eq!(poll_interval(ACTIVE_WINDOW, true, false), IDLE_POLL);
        assert_eq!(poll_interval(ACTIVE_WINDOW, false, false), BACKGROUND_POLL);
        assert_eq!(poll_interval(SETTLED_AFTER, true, false), SETTLED_POLL);
        assert_eq!(poll_interval(SETTLED_AFTER, false, false), BACKGROUND_POLL);
    }

    #[test]
    fn a_press_on_csp_slows_polling_unless_the_colour_is_changing() {
        assert_eq!(poll_interval(PRESSED_CHANGE_WINDOW, true, true), PRESSED_POLL);
        assert_eq!(poll_interval(SETTLED_AFTER, true, true), PRESSED_POLL);
        assert_eq!(poll_interval(Duration::ZERO, true, true), ACTIVE_POLL);
        // The press is somewhere else, and has nothing to do with CSP.
        assert_eq!(poll_interval(ACTIVE_WINDOW, false, true), BACKGROUND_POLL);
    }

    #[test]
    fn only_the_newest_colour_is_kept_but_order_around_slots_is_not_lost() {
        let mut writes = VecDeque::new();
        enqueue(&mut writes, Write::Rgb([1, 1, 1]));
        enqueue(&mut writes, Write::Rgb([2, 2, 2]));
        enqueue(&mut writes, Write::Slot(Slot::Sub));
        enqueue(&mut writes, Write::Rgb([3, 3, 3]));
        enqueue(&mut writes, Write::Rgb([4, 4, 4]));
        let kinds: Vec<String> = writes
            .iter()
            .map(|write| match write {
                Write::Rgb(rgb) => format!("rgb{}", rgb[0]),
                Write::Slot(_) => "slot".to_string(),
            })
            .collect();
        assert_eq!(kinds, ["rgb2", "slot", "rgb4"]);
    }
}
